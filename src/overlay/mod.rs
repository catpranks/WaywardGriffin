pub mod state;

use crate::plotter::PlotterHandle;
use anyhow::{Context as _, Result, anyhow};
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{EventLoop, Interest, Mode, PostAction};
use smithay::reexports::wayland_server::Display;
use smithay::wayland::drm_syncobj::DrmSyncPoint;
use smithay::wayland::socket::ListeningSocketSource;
use state::State;
use std::sync::{Arc, Mutex};
use tracing::info;
use vulkano::device::physical::PhysicalDevice;

pub struct OverlayFrame {
    pub dmabuf: Dmabuf,
    pub acquire_point: Option<DrmSyncPoint>,
    pub release_point: Option<DrmSyncPoint>,
    pub size: (i32, i32),
}

pub type OverlaySlot = Arc<Mutex<Option<OverlayFrame>>>;

pub struct OverlayHandle {
    pub slot: OverlaySlot,
    pub socket_name: String,
}

pub struct OverlayEnv {
    pub physical_device: Arc<PhysicalDevice>,
    pub ph: PlotterHandle,
}

pub fn spawn(env: OverlayEnv) -> Result<OverlayHandle> {
    let slot: OverlaySlot = Arc::new(Mutex::new(None));

    let display: Display<State> = Display::new().context("failed to create wayland display")?;
    let dh = display.handle();
    let state = State::new(dh, &env, slot.clone())?;

    let listening_socket =
        ListeningSocketSource::with_name("waygriff-0").context("failed to bind socket")?;
    let socket_name = listening_socket
        .socket_name()
        .to_str()
        .context("socket name not utf-8")?
        .to_owned();
    info!(socket = %socket_name, "overlay compositor listening");

    let ph = env.ph.clone();
    std::thread::Builder::new()
        .name("overlay".into())
        .spawn(move || {
            ph.fatal(run(display, state, listening_socket).context("overlay thread"));
        })
        .unwrap();

    Ok(OverlayHandle { slot, socket_name })
}

fn run(display: Display<State>, mut state: State, listening_socket: ListeningSocketSource) -> Result<()> {
    let mut event_loop: EventLoop<State> = EventLoop::try_new()?;

    event_loop
        .handle()
        .insert_source(
            listening_socket,
            move |client_stream, _, state: &mut State| {
                state
                    .display_handle
                    .insert_client(client_stream, Arc::new(state::ClientState::default()))
                    .unwrap();
            },
        )
        .map_err(|e| anyhow!("{}", e.error))?;

    event_loop
        .handle()
        .insert_source(
            Generic::new(display, Interest::READ, Mode::Level),
            |_, display, state| {
                unsafe {
                    display.get_mut().dispatch_clients(state).unwrap();
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| anyhow!("{}", e.error))?;

    loop {
        event_loop.dispatch(None, &mut state)?;
    }
}
