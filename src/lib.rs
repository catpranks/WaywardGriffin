#![allow(clippy::new_without_default)]

mod bridge;
mod config;
pub mod control;
mod display;
pub mod monitor;
pub mod overlay;
mod render;
pub mod sizer;
mod source;
pub mod stats;
mod utils;

use crate::config::Config;
use crate::monitor::{Mode, Monitor, MonitorHandle};
use crate::sizer::{SharedSizer, Sizer};
use crate::source::{BackendType, CaptureOpts};
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
            // Mesa 26.2 keeps SDMA queues behind this flag. Set it before Vulkan
            // initialization and before spawning threads; preserve other flags.
            let mut experimental = std::env::var("RADV_EXPERIMENTAL").unwrap_or_default();
            if !experimental.split(',').any(|flag| flag == "transfer_queue") {
                if !experimental.is_empty() {
                    experimental.push(',');
                }
                experimental.push_str("transfer_queue");
                std::env::set_var("RADV_EXPERIMENTAL", experimental);
            }
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
    let monitor = Monitor::new(global_state.clone(), sizer.clone());
    init_tracing(monitor.handle(), opts.tdelay.is_some());

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
    control::spawn(monitor.snapshot(), global_state.clone(), control_tx)?;

    let mon = monitor.handle();
    let opts2 = opts.clone();
    let sizer2 = sizer.clone();
    std::thread::Builder::new()
        .name("display".into())
        .spawn(move || {
            mon.fatal(
                display::run(opts2, global_state, mon.clone(), sizer2, config, control_rx)
                    .context("display thread"),
            );
        })?;

    let mode = match opts.tdelay {
        Some(delay) => Mode::Tui { delay },
        None => Mode::Headless {
            stats_every: opts.stats_every,
        },
    };
    monitor.run(mode)
}

/// Route tracing output through the monitor so the TUI can show it.
fn init_tracing(mon: MonitorHandle, tui: bool) {
    let crate_name = env!("CARGO_PKG_NAME");
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("info,{crate_name}=debug")));
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .compact()
        .with_writer(move || MonitorWriter {
            mon: mon.clone(),
            buf: vec![],
        });
    let registry = tracing_subscriber::registry().with(filter);
    if tui {
        registry.with(fmt_layer.without_time()).init();
    } else {
        registry.with(fmt_layer).init();
    }
}

pub type GlobalState = Arc<ArcSwap<GlobalStateInner>>;

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

/// Flip one or more flags atomically: `state.update(|s| s.confine = true)`.
pub trait GlobalStateExt {
    fn update(&self, f: impl Fn(&mut GlobalStateInner));
}

impl GlobalStateExt for ArcSwap<GlobalStateInner> {
    fn update(&self, f: impl Fn(&mut GlobalStateInner)) {
        self.rcu(|s| {
            let mut s = (**s).clone();
            f(&mut s);
            s
        });
    }
}

struct MonitorWriter {
    mon: MonitorHandle,
    buf: Vec<u8>,
}

impl std::io::Write for MonitorWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for MonitorWriter {
    fn drop(&mut self) {
        self.mon.log(String::from_utf8_lossy(&self.buf).to_string());
    }
}
