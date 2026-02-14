use super::{OverlayEnv, OverlayFrame, OverlaySlot};
use anyhow::{Context as _, Result};
use smithay::backend::allocator::{Buffer as _, Fourcc, Modifier};
use smithay::backend::drm::DrmDeviceFd;
use smithay::backend::renderer::utils::on_commit_buffer_handler;
use smithay::delegate_compositor;
use smithay::delegate_dmabuf;
use smithay::delegate_drm_syncobj;
use smithay::delegate_seat;
use smithay::delegate_shm;
use smithay::delegate_xdg_shell;
use smithay::input::{SeatHandler, SeatState};
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::{wl_buffer, wl_output, wl_surface::WlSurface};
use smithay::reexports::wayland_server::{Client, DisplayHandle};
use smithay::reexports::wayland_server::protocol::wl_seat;
use smithay::utils::{Logical, Serial, Size};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    BufferAssignment, CompositorClientState, CompositorHandler, CompositorState, SurfaceAttributes,
    with_states,
};
use smithay::wayland::dmabuf::{
    DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier, get_dmabuf,
};
use smithay::wayland::drm_syncobj::{DrmSyncobjCachedState, DrmSyncobjHandler, DrmSyncobjState};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::wayland::shm::{ShmHandler, ShmState};
use std::fs::File;
use std::os::fd::OwnedFd;
use std::time::Instant;
use tracing::{debug, warn};
use vulkano::device::physical::PhysicalDevice;

use smithay::backend::allocator::Format;
use smithay::utils::DeviceFd;

pub struct State {
    pub display_handle: DisplayHandle,
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    pub seat_state: SeatState<Self>,
    pub dmabuf_state: DmabufState,
    pub dmabuf_global: DmabufGlobal,
    pub syncobj_state: DrmSyncobjState,

    pub slot: OverlaySlot,
    pub toplevels: Vec<ToplevelSurface>,
    pub size: Size<i32, Logical>,
    start: Instant,
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

fn render_dev(physical_device: &PhysicalDevice) -> Option<(u64, u64)> {
    let props = physical_device.properties();
    Some((props.render_major? as u64, props.render_minor? as u64))
}

impl State {
    pub fn new(dh: DisplayHandle, env: &OverlayEnv, slot: OverlaySlot) -> Result<Self> {
        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let seat_state = SeatState::new();

        let (major, minor) =
            render_dev(&env.physical_device).context("no render major/minor on device")?;

        let dev_t = nix::sys::stat::makedev(major, minor);
        let formats = vec![
            Format {
                code: Fourcc::Argb8888,
                modifier: Modifier::Linear,
            },
            Format {
                code: Fourcc::Xrgb8888,
                modifier: Modifier::Linear,
            },
        ];
        let feedback = DmabufFeedbackBuilder::new(dev_t, formats)
            .build()
            .context("failed to build dmabuf feedback")?;

        let mut dmabuf_state = DmabufState::new();
        let dmabuf_global =
            dmabuf_state.create_global_with_default_feedback::<Self>(&dh, &feedback);

        let render_path = format!("/dev/dri/renderD{minor}");
        let file = File::open(&render_path)
            .with_context(|| format!("failed to open render node {render_path}"))?;
        let owned_fd: OwnedFd = file.into();
        let device_fd = DeviceFd::from(owned_fd);
        let drm_fd = DrmDeviceFd::new(device_fd);
        let syncobj_state = DrmSyncobjState::new::<Self>(&dh, drm_fd);

        Ok(Self {
            display_handle: dh,
            compositor_state,
            xdg_shell_state,
            shm_state,
            seat_state,
            dmabuf_state,
            dmabuf_global,
            syncobj_state,

            slot,
            toplevels: Vec::new(),
            size: Size::from((1280, 720)),
            start: Instant::now(),
        })
    }
}

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);

        let start = self.start;

        with_states(surface, |states| {
            {
                let mut attrs = states.cached_state.get::<SurfaceAttributes>();
                let current = attrs.current();

                if current.buffer_scale != 1 {
                    warn!(scale = current.buffer_scale, "buffer scale != 1 ignored");
                }
                if current.buffer_transform != wl_output::Transform::Normal {
                    warn!(transform = ?current.buffer_transform, "buffer transform ignored");
                }
                if let Some(ref delta) = current.buffer_delta {
                    warn!(?delta, "buffer delta ignored");
                }
                if current.opaque_region.is_some() {
                    warn!("opaque region ignored");
                }
                if current.input_region.is_some() {
                    warn!("input region ignored");
                }

                match current.buffer {
                    Some(BufferAssignment::NewBuffer(ref buffer)) => {
                        match get_dmabuf(buffer) {
                            Ok(dmabuf) => {
                                let mut sync =
                                    states.cached_state.get::<DrmSyncobjCachedState>();
                                let sync_current = sync.current();
                                let size = dmabuf.size();
                                // TODO: does this need to retain the Buffer in order to avoid smithay automatically calling release while we're sampling the texture?
                                let frame = OverlayFrame {
                                    dmabuf: dmabuf.clone(),
                                    acquire_point: sync_current.acquire_point.clone(),
                                    release_point: sync_current.release_point.clone(),
                                    size: (size.w, size.h),
                                };
                                debug!(w = size.w, h = size.h, "overlay frame committed");
                                *self.slot.lock().unwrap() = Some(frame);
                            }
                            Err(_) => {
                                warn!("non-dmabuf buffer (shm?) ignored");
                            }
                        }
                    }
                    Some(BufferAssignment::Removed) => {
                        warn!("buffer removal ignored");
                    }
                    None => {}
                }
            }

            {
                let mut attrs = states.cached_state.get::<SurfaceAttributes>();
                let current = attrs.current();
                let time_ms = start.elapsed().as_millis() as u32;
                for callback in current.frame_callbacks.drain(..) {
                    callback.done(time_ms);
                }
            }
        });
    }
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl SeatHandler for State {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }
}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let size = self.size;
        surface.with_pending_state(|state| {
            state.size = Some(size);
            state.states.set(xdg_toplevel::State::Fullscreen);
            state.states.set(xdg_toplevel::State::Activated);
        });
        surface.send_configure();
        debug!("new overlay toplevel (total: {})", self.toplevels.len() + 1);
        self.toplevels.push(surface);
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        self.toplevels.retain(|t| t != &surface);
        debug!("overlay toplevel destroyed (remaining: {})", self.toplevels.len());
    }

    fn new_popup(&mut self, _surface: PopupSurface, _positioner: PositionerState) {
        warn!("overlay compositor: popup rejected");
    }

    fn reposition_request(
        &mut self,
        _surface: PopupSurface,
        _positioner: PositionerState,
        _token: u32,
    ) {
    }

    fn grab(
        &mut self,
        _surface: PopupSurface,
        _seat: wl_seat::WlSeat,
        _serial: Serial,
    ) {
    }
}

impl DmabufHandler for State {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        _dmabuf: smithay::backend::allocator::dmabuf::Dmabuf,
        notifier: ImportNotifier,
    ) {
        let _ = notifier.successful::<Self>();
    }
}

impl DrmSyncobjHandler for State {
    fn drm_syncobj_state(&mut self) -> Option<&mut DrmSyncobjState> {
        Some(&mut self.syncobj_state)
    }
}

delegate_compositor!(State);
delegate_shm!(State);
delegate_seat!(State);
delegate_xdg_shell!(State);
delegate_dmabuf!(State);
delegate_drm_syncobj!(State);
