pub mod state;

use crate::plotter::PlotterHandle;
use anyhow::{Context as _, Result, anyhow};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::ping::Ping;
use smithay::reexports::calloop::{EventLoop, Interest, Mode, PostAction};
use smithay::reexports::wayland_server::Display;
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_callback::WlCallback;
use smithay::wayland::drm_syncobj::DrmSyncPoint;
use smithay::wayland::socket::ListeningSocketSource;
use state::State;
use std::cell::Cell;
use std::fs::File;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tracing::{info, warn};
use vulkano::device::Device;
use vulkano::image::Image;
use vulkano::memory::allocator::StandardMemoryAllocator;
use vulkano::sync::semaphore::{
    ExternalSemaphoreHandleType, ImportSemaphoreFdInfo, Semaphore, SemaphoreCreateInfo,
    SemaphoreImportFlags,
};

pub struct OverlayFrame {
    pub image: Arc<Image>,
    acquire_point: DrmSyncPoint,
    release_point: DrmSyncPoint,
    buffer: WlBuffer,
    frame_callbacks: Cell<Vec<WlCallback>>,
    start: Instant,
    flush_ping: Ping,
}

impl OverlayFrame {
    fn send_frame_callbacks(&self) {
        let time_ms = self.start.elapsed().as_millis() as u32;
        for cb in self.frame_callbacks.take() {
            cb.done(time_ms);
        }
    }

    /// Send frame callbacks to the client, indicating it's a good time to render.
    pub fn presented(&self) {
        self.send_frame_callbacks();
        self.flush_ping.ping();
    }

    /// Export the acquire point as a Vulkan semaphore for GPU-side waiting.
    pub fn acquire_semaphore(&self, device: &Arc<Device>) -> Result<Arc<Semaphore>> {
        let sync_fd = self.acquire_point.export_sync_file()?;
        let file = File::from(sync_fd);
        let semaphore = Semaphore::new(device.clone(), SemaphoreCreateInfo::default())?;
        let mut import_info =
            ImportSemaphoreFdInfo::handle_type(ExternalSemaphoreHandleType::SyncFd);
        import_info.flags = SemaphoreImportFlags::TEMPORARY;
        import_info.file = Some(file);
        unsafe { semaphore.import_fd(import_info) }?;
        Ok(Arc::new(semaphore))
    }
}

impl Drop for OverlayFrame {
    fn drop(&mut self) {
        if let Err(e) = self.release_point.signal() {
            warn!("failed to signal release point: {e}");
        }
        self.buffer.release();
        self.send_frame_callbacks();
        self.flush_ping.ping();
    }
}

pub enum OverlayState {
    /// New frame from overlay compositor.
    Frame(OverlayFrame),
    /// Toplevels exist but no new frame since last take.
    Pending,
    /// No toplevels connected.
    Inactive,
}

pub type OverlaySlot = Arc<Mutex<OverlayState>>;

pub struct OverlayHandle {
    slot: OverlaySlot,
}

impl OverlayHandle {
    pub fn new(
        device: Arc<Device>,
        allocator: Arc<StandardMemoryAllocator>,
        ph: PlotterHandle,
    ) -> Result<Self> {
        let slot: OverlaySlot = Arc::new(Mutex::new(OverlayState::Inactive));

        let (flush_ping, flush_ping_source) = smithay::reexports::calloop::ping::make_ping()?;

        let display: Display<State> = Display::new().context("failed to create wayland display")?;
        let dh = display.handle();
        let state = State::new(dh, device, allocator, slot.clone(), flush_ping)?;

        let listening_socket =
            ListeningSocketSource::with_name("waygriff-0").context("failed to bind socket")?;
        let socket_name = listening_socket
            .socket_name()
            .to_str()
            .context("socket name not utf-8")?
            .to_owned();
        info!(socket = %socket_name, "overlay compositor listening");

        let ph = ph.clone();
        std::thread::Builder::new()
            .name("overlay".into())
            .spawn(move || {
                ph.fatal(
                    run(display, state, listening_socket, flush_ping_source)
                        .context("overlay thread"),
                );
            })?;

        Ok(OverlayHandle { slot })
    }

    pub fn take(&self) -> OverlayState {
        let mut guard = self.slot.lock().unwrap();
        match &*guard {
            OverlayState::Frame(_) => std::mem::replace(&mut *guard, OverlayState::Pending),
            OverlayState::Pending => OverlayState::Pending,
            OverlayState::Inactive => OverlayState::Inactive,
        }
    }
}

fn run(
    display: Display<State>,
    mut state: State,
    listening_socket: ListeningSocketSource,
    flush_ping_source: smithay::reexports::calloop::ping::PingSource,
) -> Result<()> {
    let mut event_loop: EventLoop<State> = EventLoop::try_new()?;

    event_loop
        .handle()
        .insert_source(flush_ping_source, |_, _, _| {})
        .map_err(|e| anyhow!("{}", e.error))?;

    event_loop
        .handle()
        .insert_source(
            listening_socket,
            move |client_stream, _, state: &mut State| {
                if let Err(err) = state
                    .display_handle
                    .insert_client(client_stream, Arc::new(state::ClientState::default()))
                {
                    warn!("error adding wayland client: {err}");
                }
            },
        )
        .map_err(|e| anyhow!("{}", e.error))?;

    event_loop
        .handle()
        .insert_source(
            Generic::new(display, Interest::READ, Mode::Level),
            |_, display, state| {
                unsafe {
                    display.get_mut().dispatch_clients(state)?;
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| anyhow!("{}", e.error))?;

    loop {
        event_loop.dispatch(None, &mut state)?;
        state.display_handle.flush_clients()?;
    }
}
