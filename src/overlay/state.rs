use crate::utils::{create_drm_modifier_image, fourcc_to_vk_format};

use super::{OverlayFrame, OverlaySlot, OverlayState};
use anyhow::{Context as _, Result, ensure};
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::{Buffer as _, Fourcc, Modifier};
use smithay::backend::drm::DrmDeviceFd;
use smithay::delegate_compositor;
use smithay::delegate_dmabuf;
use smithay::delegate_drm_syncobj;
use smithay::delegate_seat;
use smithay::delegate_shm;
use smithay::delegate_xdg_shell;
use smithay::input::{SeatHandler, SeatState};
use smithay::reexports::calloop::ping;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_seat;
use smithay::reexports::wayland_server::protocol::{wl_buffer, wl_output, wl_surface::WlSurface};
use smithay::reexports::wayland_server::{Client, DisplayHandle};
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
use smithay::wayland::shm::{ShmHandler, ShmState};
use std::cell::Cell;
use std::collections::HashMap;
use std::fs::File;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::time::Instant;
use tracing::warn;
use vulkano::device::Device;
use vulkano::image::Image;
use vulkano::image::ImageUsage;
use vulkano::memory::allocator::StandardMemoryAllocator;
use vulkano::memory::{
    DedicatedAllocation, DeviceMemory, ExternalMemoryHandleType, MemoryAllocateInfo,
    MemoryImportInfo, ResourceMemory,
};

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

    pub device: Arc<Device>,
    pub allocator: Arc<StandardMemoryAllocator>,
    pub image_cache: HashMap<Dmabuf, Arc<Image>>,

    pub slot: OverlaySlot,
    pub flush_ping: ping::Ping,
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

impl State {
    pub fn new(
        dh: DisplayHandle,
        device: Arc<Device>,
        allocator: Arc<StandardMemoryAllocator>,
        slot: OverlaySlot,
        flush_ping: ping::Ping,
    ) -> Result<Self> {
        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let seat_state = SeatState::new();

        let props = device.physical_device().properties();
        let major = props.render_major.context("no render_major on device")? as u64;
        let minor = props.render_minor.context("no render_minor on device")? as u64;

        let dev_t = nix::sys::stat::makedev(major, minor);
        let phys = device.physical_device();
        let format_props = phys
            .format_properties(vulkano::format::Format::B8G8R8A8_SRGB)
            .context("format_properties for B8G8R8A8_SRGB")?;
        let modifiers: Vec<Modifier> = format_props
            .drm_format_modifier_properties
            .iter()
            .filter(|p| {
                p.drm_format_modifier_plane_count == 1
                    && p.drm_format_modifier_tiling_features
                        .intersects(vulkano::format::FormatFeatures::SAMPLED_IMAGE)
            })
            .map(|p| Modifier::from(p.drm_format_modifier))
            .collect();
        let formats: Vec<Format> = [Fourcc::Argb8888, Fourcc::Xrgb8888]
            .into_iter()
            .flat_map(|code| {
                modifiers
                    .iter()
                    .map(move |&modifier| Format { code, modifier })
            })
            .collect();
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

            device,
            allocator,
            image_cache: HashMap::new(),

            slot,
            flush_ping,
            toplevels: Vec::new(),
            size: Size::from((1280, 720)),
            start: Instant::now(),
        })
    }

    fn import_dmabuf(&mut self, dmabuf: &Dmabuf) -> Result<Arc<Image>> {
        if let Some(image) = self.image_cache.get(dmabuf) {
            return Ok(image.clone());
        }

        let fmt = dmabuf.format();
        let size = dmabuf.size();
        let num_planes = dmabuf.num_planes();
        ensure!(
            num_planes == 1,
            "multi-plane dmabufs not supported ({num_planes} planes)"
        );
        let vk_format = fourcc_to_vk_format(fmt.code)?;
        let modifier = u64::from(fmt.modifier);

        let raw_image = create_drm_modifier_image(
            self.device.clone(),
            vk_format,
            size.w as u32,
            size.h as u32,
            ImageUsage::TRANSFER_SRC | ImageUsage::SAMPLED,
            vec![modifier],
        )?;

        let req = raw_image.memory_requirements()[0];

        // dup the fd since vulkano takes ownership
        let borrowed_fd = dmabuf.handles().next().context("dmabuf has no planes")?;
        let owned_fd = borrowed_fd
            .try_clone_to_owned()
            .context("failed to dup dmabuf fd")?;
        let file = File::from(owned_fd);

        let fd_props = unsafe {
            self.device
                .memory_fd_properties(ExternalMemoryHandleType::DmaBuf, file)
        }
        .context("memory_fd_properties")?;

        // dup again for the actual import (previous call consumed the fd)
        let borrowed_fd = dmabuf.handles().next().context("dmabuf has no planes")?;
        let owned_fd = borrowed_fd
            .try_clone_to_owned()
            .context("failed to dup dmabuf fd")?;
        let file = File::from(owned_fd);

        let memory_type_index = req.memory_type_bits & fd_props.memory_type_bits;
        let memory_type_index = memory_type_index.trailing_zeros();

        let alloc = unsafe {
            DeviceMemory::import(
                self.device.clone(),
                MemoryAllocateInfo {
                    allocation_size: req.layout.size(),
                    memory_type_index,
                    dedicated_allocation: Some(DedicatedAllocation::Image(&raw_image)),
                    ..Default::default()
                },
                MemoryImportInfo::Fd {
                    handle_type: ExternalMemoryHandleType::DmaBuf,
                    file,
                },
            )
        }
        .context("DeviceMemory::import")?;

        let image = Arc::new(
            raw_image
                .bind_memory([ResourceMemory::new_dedicated(alloc)])
                .map_err(|(e, _, _)| e)?,
        );

        self.image_cache.insert(dmabuf.clone(), image.clone());
        Ok(image)
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
        with_states(surface, |states| {
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

            match current.buffer.take() {
                Some(BufferAssignment::NewBuffer(buffer)) => match get_dmabuf(&buffer) {
                    Ok(dmabuf) => {
                        let Some(image) = self.image_cache.get(dmabuf).cloned() else {
                            warn!("committed buffer not in image cache");
                            return;
                        };
                        let mut sync = states.cached_state.get::<DrmSyncobjCachedState>();
                        let sync_current = sync.current();
                        let (Some(acquire_point), Some(release_point)) = (
                            sync_current.acquire_point.clone(),
                            sync_current.release_point.clone(),
                        ) else {
                            warn!("committed buffer without explicit sync points");
                            return;
                        };
                        let frame_callbacks = current.frame_callbacks.drain(..).collect();
                        let frame = OverlayFrame {
                            image,
                            acquire_point,
                            release_point,
                            buffer,
                            frame_callbacks: Cell::new(frame_callbacks),
                            start: self.start,
                            flush_ping: self.flush_ping.clone(),
                        };
                        *self.slot.lock().unwrap() = OverlayState::Frame(frame);
                    }
                    Err(_) => {
                        warn!("non-dmabuf buffer (shm?) ignored");
                    }
                },
                Some(BufferAssignment::Removed) => {}
                None => {}
            }
        });
    }
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, buffer: &wl_buffer::WlBuffer) {
        if let Ok(dmabuf) = get_dmabuf(buffer)
            && self.image_cache.remove(dmabuf).is_some()
        {}
    }
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
        self.toplevels.push(surface);
        let mut guard = self.slot.lock().unwrap();
        if matches!(*guard, OverlayState::Inactive) {
            *guard = OverlayState::Pending;
        }
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        self.toplevels.retain(|t| t != &surface);
        if self.toplevels.is_empty() {
            *self.slot.lock().unwrap() = OverlayState::Inactive;
        }
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

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {}
}

impl DmabufHandler for State {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: smithay::backend::allocator::dmabuf::Dmabuf,
        notifier: ImportNotifier,
    ) {
        match self.import_dmabuf(&dmabuf) {
            Ok(_) => {
                let _ = notifier.successful::<Self>();
            }
            Err(e) => {
                warn!("dmabuf import failed: {e:#}");
                notifier.failed();
            }
        }
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
