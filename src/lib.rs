#![allow(clippy::new_without_default)]

mod capture;
mod display;
pub mod overlay;
pub mod plotter;
pub mod sizer;
mod utils;

use crate::plotter::{Plotter, PlotterHandle};
use crate::capture::source::BackendType;
use crate::sizer::{SharedSizer, Sizer};
use anyhow::{Context as _, Result};
use arc_swap::ArcSwap;
use clap::Parser;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

#[derive(Debug, Clone, Parser)]
#[command(version, about, long_about = None)]
struct Opts {
    /// Delay between TUI frames
    #[arg(long, value_parser = humantime::parse_duration)]
    tdelay: Option<std::time::Duration>,

    /// confine from start
    #[arg(long)]
    confine: bool,

    /// don't capture from start
    #[arg(long)]
    nocapture: bool,

    #[command(flatten)]
    capture_opts: capture::CaptureOpts,
}

pub fn run() -> Result<()> {
    let opts = Opts::parse();
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

    let crate_name = env!("CARGO_PKG_NAME");

    let global_state = Arc::new(ArcSwap::from_pointee(GlobalStateInner {
        cursor_visible: true,
        confine: opts.confine,
        capture: !opts.nocapture,
        force_relative: false,
    }));
    let sizer: SharedSizer = Arc::new(ArcSwap::from_pointee(Sizer::default()));
    let plotter = Plotter::new(global_state.clone(), sizer.clone());
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("info,{}=debug", crate_name)));
    let ph = plotter.handle();
    let fmt_layer = tracing_subscriber::fmt::layer()
        .without_time()
        .with_ansi(false)
        .compact()
        .with_writer(move || PlotterWriter {
            ph: ph.clone(),
            buf: vec![],
        });
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .init();
    let ph = plotter.handle();
    let opts2 = opts.clone();
    let sizer2 = sizer.clone();
    std::thread::Builder::new()
        .name("display".into())
        .spawn(move || {
            ph.fatal(
                display::run(opts2, global_state, ph.clone(), sizer2).context("display thread"),
            );
        })
        .unwrap();
    plotter.run(opts.tdelay)
}

type GlobalState = Arc<ArcSwap<GlobalStateInner>>;

#[derive(Clone, Default)]
pub struct GlobalStateInner {
    pub cursor_visible: bool,
    pub confine: bool,
    pub capture: bool,
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
