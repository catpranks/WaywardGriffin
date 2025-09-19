#![allow(clippy::new_without_default)]

mod capture;
mod display;
pub mod sizer;

use crate::capture::plotter::{Plotter, PlotterHandle};
use anyhow::Result;
use arc_swap::ArcSwap;
use clap::Parser;
use smithay_client_toolkit::reexports::client::protocol::wl_buffer::WlBuffer;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

#[derive(Debug, Clone, Parser)]
#[command(version, about, long_about = None)]
struct Opts {
    /// The display to capture
    #[arg(long)]
    display: String,

    /// Delay between TUI frames
    #[arg(long, value_parser = humantime::parse_duration)]
    tdelay: Option<std::time::Duration>,

    /// confine from start
    #[arg(long)]
    confine: bool,

    /// don't capture from start
    #[arg(long)]
    nocapture: bool,
}

pub fn run() -> Result<()> {
    let opts = Opts::parse();
    unsafe { std::env::set_var("DISPLAY", opts.display.clone()) };
    let crate_name = env!("CARGO_PKG_NAME");

    let global_state = Arc::new(ArcSwap::from_pointee(GlobalStateInner {
        cursor_visible: true,
        confine: opts.confine,
        capture: !opts.nocapture,
        force_relative: false,
    }));
    let plotter = Plotter::new(global_state.clone());
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
    std::thread::spawn(move || display::run(opts2, global_state, ph));
    plotter.run(opts.tdelay)
}

type GlobalState = Arc<ArcSwap<GlobalStateInner>>;

#[derive(Clone)]
pub struct GlobalStateInner {
    pub cursor_visible: bool,
    pub confine: bool,
    pub capture: bool,
    pub force_relative: bool,
}

macro_rules! with_field {
    ($field:ident) => {
        pastey::paste! {
        pub fn [<with_ $field>](&self, $field: bool) -> Self {
                let mut new_state = self.clone();
                new_state.$field = $field;
                new_state
            }
        }
    };
}

impl GlobalStateInner {
    with_field!(cursor_visible);
    with_field!(confine);
    with_field!(capture);
    with_field!(force_relative);
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

macro_rules! impl_owning_wrapper {
    ($wrapper_name:ident, $inner_type:ty) => {
        pub struct $wrapper_name(pub $inner_type);

        impl std::ops::Deref for $wrapper_name {
            type Target = $inner_type;
            fn deref(&self) -> &Self::Target {
                &self.0
            }
        }

        impl std::ops::DerefMut for $wrapper_name {
            fn deref_mut(&mut self) -> &mut Self::Target {
                &mut self.0
            }
        }

        impl Drop for $wrapper_name {
            fn drop(&mut self) {
                self.0.destroy();
            }
        }
    };
}

impl_owning_wrapper!(OwningWlBuffer, WlBuffer);
