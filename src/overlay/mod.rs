pub mod state;

use anyhow::{Context as _, Result};
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{EventLoop, Interest, Mode, PostAction};
use smithay::reexports::wayland_server::Display;
use smithay::wayland::drm_syncobj::DrmSyncPoint;
use smithay::wayland::socket::ListeningSocketSource;
use state::State;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
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
}

pub fn spawn(env: OverlayEnv) -> Result<OverlayHandle> {
    let slot: OverlaySlot = Arc::new(Mutex::new(None));

    // Channel to receive socket name (or error) from the thread after setup.
    let (setup_tx, setup_rx) = mpsc::channel::<Result<String>>();

    let slot2 = slot.clone();
    std::thread::Builder::new()
        .name("overlay".into())
        .spawn(move || {
            let result = run(env, slot2, setup_tx);
            if let Err(e) = result {
                tracing::error!("overlay thread exited: {e:#}");
            }
        })
        .unwrap();

    let socket_name = setup_rx
        .recv()
        .context("overlay thread died during setup")??;

    Ok(OverlayHandle { slot, socket_name })
}

fn run(env: OverlayEnv, slot: OverlaySlot, setup_tx: mpsc::Sender<Result<String>>) -> Result<()> {
    let display: Display<State> = Display::new().context("failed to create wayland display")?;
    let dh = display.handle();

    let state = match State::new(dh, &env, slot) {
        Ok(s) => s,
        Err(e) => {
            let _ = setup_tx.send(Err(anyhow::anyhow!("{e:#}")));
            return Err(e);
        }
    };

    let mut event_loop: EventLoop<State> = EventLoop::try_new()?;

    let listening_socket =
        ListeningSocketSource::with_name("waygriff-0").context("failed to bind socket")?;
    let socket_name = listening_socket
        .socket_name()
        .to_str()
        .context("socket name not utf-8")?
        .to_owned();
    tracing::info!(socket = %socket_name, "overlay compositor listening");

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
        .map_err(|e| anyhow::anyhow!("{}", e.error))?;

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
        .map_err(|e| anyhow::anyhow!("{}", e.error))?;

    // Signal setup complete
    let _ = setup_tx.send(Ok(socket_name));

    let mut state = state;
    loop {
        event_loop.dispatch(None, &mut state)?;
    }
}
