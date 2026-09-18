#![allow(clippy::new_without_default)]

mod capture;
mod config;
pub mod control;
mod display;
pub mod overlay;
pub mod plotter;
pub mod sizer;
pub mod stats;
mod utils;

use crate::capture::CaptureOpts;
use crate::capture::source::BackendType;
use crate::config::Config;
use crate::plotter::{Mode, Plotter, PlotterHandle};
use crate::sizer::{SharedSizer, Sizer};
use anyhow::{Context as _, Result};
use arc_swap::ArcSwap;
use clap::Parser;
use smithay_client_toolkit::reexports::calloop::channel;
use std::sync::Arc;
use std::time::Duration;
use tracing::info;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use xdg::BaseDirectories;

#[derive(Debug, Clone, Parser)]
#[command(version, about, long_about = None)]
struct Opts {
    /// Show the TUI, redrawn at this interval (e.g. 1000ms). Without it, logs go to stdout.
    #[arg(long, value_parser = humantime::parse_duration)]
    tdelay: Option<Duration>,

    /// In headless mode, log a JSON stats line at this interval (e.g. 5s)
    #[arg(long, value_parser = humantime::parse_duration)]
    stats_every: Option<Duration>,

    /// confine from start
    #[arg(long)]
    confine: bool,

    /// don't capture from start
    #[arg(long)]
    nocapture: bool,

    #[command(flatten)]
    capture_opts: CaptureOpts,
}

pub fn run() -> Result<()> {
    let opts = Opts::parse();
    // A panicking thread would otherwise leave the rest of the process running headless.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        std::process::exit(101);
    }));
    // NVFBC only reads DISPLAY from env var; there's no API to pass it explicitly.
    // Other backends don't need it set globally.
    match opts.capture_opts.backend {
        BackendType::Nvfbc => unsafe {
            std::env::set_var("DISPLAY", &opts.capture_opts.display);
        },
        _ => unsafe {
            std::env::remove_var("DISPLAY");
        },
    }

    let global_state = Arc::new(ArcSwap::from_pointee(GlobalStateInner {
        cursor_visible: true,
        confine: opts.confine,
        capture: !opts.nocapture,
        force_relative: false,
    }));
    let sizer: SharedSizer = Arc::new(ArcSwap::from_pointee(Sizer::default()));
    let plotter = Plotter::new(global_state.clone(), sizer.clone());
    init_tracing(plotter.handle(), opts.tdelay.is_some());

    let config = {
        let dirs = BaseDirectories::with_prefix("waygriff");
        match dirs.find_config_file("config.toml") {
            Some(path) => {
                info!("loading config: {}", path.display());
                Config::load(&path)?
            }
            None => Config::default(),
        }
    };

    let (control_tx, control_rx) = channel::channel();
    control::spawn(plotter.snapshot(), global_state.clone(), control_tx)?;

    let ph = plotter.handle();
    let opts2 = opts.clone();
    let sizer2 = sizer.clone();
    std::thread::Builder::new()
        .name("display".into())
        .spawn(move || {
            ph.fatal(
                display::run(opts2, global_state, ph.clone(), sizer2, config, control_rx)
                    .context("display thread"),
            );
        })?;

    let mode = match opts.tdelay {
        Some(delay) => Mode::Tui { delay },
        None => Mode::Headless {
            stats_every: opts.stats_every,
        },
    };
    plotter.run(mode)
}

/// Route tracing output through the plotter so the TUI can show it.
fn init_tracing(ph: PlotterHandle, tui: bool) {
    let crate_name = env!("CARGO_PKG_NAME");
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("info,{crate_name}=debug")));
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .compact()
        .with_writer(move || PlotterWriter {
            ph: ph.clone(),
            buf: vec![],
        });
    let registry = tracing_subscriber::registry().with(filter);
    if tui {
        registry.with(fmt_layer.without_time()).init();
    } else {
        registry.with(fmt_layer).init();
    }
}

type GlobalState = Arc<ArcSwap<GlobalStateInner>>;

/// Flags shared by every thread. Replace wholesale with `rcu`; never mutate in place.
#[derive(Clone, Default)]
pub struct GlobalStateInner {
    /// Source reports a visible cursor (drives absolute vs relative pointer forwarding).
    pub cursor_visible: bool,
    /// Input grab is on: pointer confined, keys forwarded, compositor shortcuts inhibited.
    pub confine: bool,
    /// Capture and render frames (off: blank window, input still forwarded).
    pub capture: bool,
    /// Forward relative pointer motion even while the source cursor is visible.
    pub force_relative: bool,
}

impl GlobalStateInner {
    pub fn with_cursor_visible(&self, v: bool) -> Self {
        Self {
            cursor_visible: v,
            ..self.clone()
        }
    }
    pub fn with_confine(&self, v: bool) -> Self {
        Self {
            confine: v,
            ..self.clone()
        }
    }
    pub fn with_capture(&self, v: bool) -> Self {
        Self {
            capture: v,
            ..self.clone()
        }
    }
    pub fn with_force_relative(&self, v: bool) -> Self {
        Self {
            force_relative: v,
            ..self.clone()
        }
    }
}

struct PlotterWriter {
    ph: PlotterHandle,
    buf: Vec<u8>,
}

impl std::io::Write for PlotterWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for PlotterWriter {
    fn drop(&mut self) {
        self.ph.log(String::from_utf8_lossy(&self.buf).to_string());
    }
}
