mod dmabuf_probe;
mod image_copy;
mod screencopy;

use super::{
    BackendType, CaptureBackendBuilder, CaptureEnv, CapturedFrame, DeviceId, ReclaimedBuffer,
};
use crate::GlobalState;
use crate::capture::input::InputBridge;
use crate::capture::input::wayland::WaylandInput;
use crate::capture::source::CaptureBackend;
use crate::plotter::{FrameInfo, PlotterHandle};
use crate::sizer::SharedSizer;
use crate::utils::wayland_connect;
use crate::utils::{OwningWlBuffer, create_drm_modifier_image, fourcc_to_vk_format};
use anyhow::anyhow;
use anyhow::{Context as _, Result};
use drm_fourcc::DrmFourcc;
use image_copy::ImageCopyState;
use smithay_client_toolkit::dmabuf::{DmabufFeedback, DmabufHandler, DmabufState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::channel::{self as calloop_channel, Channel};
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay_client_toolkit::reexports::calloop::{EventLoop, LoopHandle};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::wl_buffer::WlBuffer;
use smithay_client_toolkit::reexports::client::protocol::wl_output::WlOutput;
use smithay_client_toolkit::reexports::client::{Connection, QueueHandle};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::{
    delegate_dmabuf, delegate_output, delegate_registry, registry_handlers,
};
use std::os::fd::AsFd as _;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::info;
use vulkano::device::Device;
use vulkano::image::{Image, ImageAspect, ImageUsage};
use vulkano::memory::allocator::{MemoryAllocator, MemoryTypeFilter, StandardMemoryAllocator};
use vulkano::memory::{
    DedicatedAllocation, DeviceMemory, ExternalMemoryHandleType, ExternalMemoryHandleTypes,
    MemoryAllocateInfo, ResourceMemory,
};

// breaks rustfmt import sorting for some reason
use smithay_client_toolkit::reexports::protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1;
use smithay_client_toolkit::reexports::protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1};
use smithay_client_toolkit::reexports::protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;
use smithay_client_toolkit::reexports::protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::{self, ExtImageCopyCaptureManagerV1};
use smithay_client_toolkit::reexports::protocols::ext::image_capture_source::v1::client::ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1;

pub struct Builder {
    display: String,
    backend: BackendType,
    feedback: DmabufFeedback,
}

impl Builder {
    pub fn new(display: &str, backend: BackendType) -> Result<Self> {
        let feedback = dmabuf_probe::query_dmabuf_feedback(display)?;
        Ok(Self {
            display: display.to_owned(),
            backend,
            feedback,
        })
    }
}

impl CaptureBackendBuilder for Builder {
    fn device_id(&self) -> Result<DeviceId> {
        let dev = self.feedback.main_device();
        Ok(DeviceId::DevMajorMinor(
            nix::sys::stat::major(dev),
            nix::sys::stat::minor(dev),
        ))
    }

    fn build(
        self: Box<Self>,
        env: CaptureEnv,
        sizer: SharedSizer,
    ) -> Result<(Box<dyn CaptureBackend>, Box<dyn InputBridge>)> {
        let bridge = Box::new(WaylandInput::new(&self.display, sizer, env.ph.clone())?);
        let slot: Arc<Mutex<Option<CapturedFrame>>> = Arc::new(Mutex::new(None));
        let (ping_tx, ping_rx) = calloop_channel::channel();

        std::thread::Builder::new()
            .name("capture-wayland".into())
            .spawn({
                let slot = slot.clone();
                move || {
                    let ph = env.ph.clone();
                    ph.fatal(
                        run(env, &self.display, self.backend, slot, ping_rx)
                            .context(format!("capture thread ({:?})", self.backend)),
                    );
                }
            })?;

        Ok((Box::new(Backend { slot, ping_tx }), bridge))
    }
}

pub struct Backend {
    slot: Arc<Mutex<Option<CapturedFrame>>>,
    ping_tx: calloop_channel::Sender<()>,
}

impl CaptureBackend for Backend {
    fn capture(&mut self) -> Result<Option<CapturedFrame>> {
        let _ = self.ping_tx.send(());
        let frame = self.slot.lock().unwrap().take();
        Ok(frame)
    }
}

struct State {
    ph: PlotterHandle,
    global_state: GlobalState,
    device: Arc<Device>,
    allocator: Arc<StandardMemoryAllocator>,

    registry_state: RegistryState,
    output_state: OutputState,
    dmabuf_state: DmabufState,
    screencopy_manager: Option<ZwlrScreencopyManagerV1>,
    image_copy: Option<ImageCopyState>,
    qh: QueueHandle<State>,
    output: Option<WlOutput>,

    frame_state: Option<FrameState>,
    pool: Vec<Buffer>,

    slot: Arc<Mutex<Option<CapturedFrame>>>,
    reclaim_tx: mpsc::Sender<ReclaimedBuffer>,
    reclaim_rx: mpsc::Receiver<ReclaimedBuffer>,

    done: Option<Result<()>>,
    loop_handle: LoopHandle<'static, State>,
}

enum FrameState {
    Requested {
        start: Instant,
    },
    Described {
        start: Instant,
        format: u32,
        width: u32,
        height: u32,
    },
    Copying {
        buf: Buffer,
        start: Instant,
        wait: Instant,
    },
}

impl State {
    fn issue_capture(&mut self) {
        if self.screencopy_manager.is_some() {
            self.screencopy_issue_capture();
        } else if self.image_copy.is_some() {
            self.image_copy_issue_capture();
        } else {
            unreachable!();
        }
    }

    fn capturing(&self) -> bool {
        self.global_state.load().capture
    }

    fn drain_reclaimed(&mut self) {
        while let Ok(rbuf) = self.reclaim_rx.try_recv() {
            self.pool.push(Buffer::from(rbuf));
        }
    }

    fn handle_ready(&mut self, info: FrameInfo, buf: Buffer) {
        if !self.capturing() {
            self.pool.push(buf);
            return;
        }

        self.ph.capture();

        if self.global_state.load().cursor_visible != info.cursor_visible {
            self.global_state
                .rcu(|s| s.with_cursor_visible(info.cursor_visible));
        }

        let Buffer { wl_buffer, image } = buf;
        let frame = CapturedFrame {
            image,
            backend_data: Some(Box::new(wl_buffer)),
            info: Some(info),
            reclaim_tx: self.reclaim_tx.clone(),
        };
        {
            let mut slot = self.slot.lock().unwrap();
            if slot.is_some() {
                self.ph.capture_miss();
            }
            *slot = Some(frame);
        }

        self.issue_capture();
    }

    fn handle_failed(&mut self) {
        if !self.capturing() {
            return;
        }

        if let Err(e) = self.loop_handle.insert_source(
            Timer::from_duration(Duration::from_millis(100)),
            |_, _, state| {
                if state.capturing() && state.frame_state.is_none() {
                    state.issue_capture();
                }
                TimeoutAction::Drop
            },
        ) {
            self.done = Some(Err(anyhow!("failed to insert retry timer: {e}")));
        }
    }

    fn handle_ping(&mut self) {
        if self.capturing() && self.frame_state.is_none() {
            self.issue_capture();
        }
    }
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
}

impl DmabufHandler for State {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_feedback(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _proxy: &ZwpLinuxDmabufFeedbackV1,
        _feedback: DmabufFeedback,
    ) {
    }

    fn created(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _params: &ZwpLinuxBufferParamsV1,
        _buffer: WlBuffer,
    ) {
    }

    fn failed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _params: &ZwpLinuxBufferParamsV1,
    ) {
    }

    fn released(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _buffer: &WlBuffer) {}
}

delegate_registry!(State);
delegate_output!(State);
delegate_dmabuf!(State);

fn run(
    env: CaptureEnv,
    display: &str,
    backend: BackendType,
    slot: Arc<Mutex<Option<CapturedFrame>>>,
    ping_rx: Channel<()>,
) -> Result<()> {
    let CaptureEnv {
        ph,
        global_state,
        device,
        allocator,
    } = env;
    let use_screencopy = backend == BackendType::Screencopy;
    let conn = wayland_connect(display)?;

    let (globals, mut event_queue) = registry_queue_init::<State>(&conn)?;
    let qh = event_queue.handle();

    let mut event_loop: EventLoop<State> = EventLoop::try_new()?;

    let (reclaim_tx, reclaim_rx) = mpsc::channel();

    let screencopy_manager: Option<ZwlrScreencopyManagerV1> = if use_screencopy {
        Some(
            globals
                .bind(&qh, 3..=3, ())
                .context("zwlr_screencopy_manager_v1 v3 not available")?,
        )
    } else {
        None
    };

    let mut state = State {
        ph,
        global_state,
        device,
        allocator,

        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        dmabuf_state: DmabufState::new(&globals, &qh),
        screencopy_manager,
        image_copy: None,
        qh: qh.clone(),
        output: None,

        frame_state: None,
        pool: Vec::new(),

        slot,
        reclaim_tx,
        reclaim_rx,

        done: None,
        loop_handle: event_loop.handle(),
    };

    event_queue.roundtrip(&mut state)?;

    let output = state
        .output_state
        .outputs()
        .next()
        .context("no outputs available")?
        .clone();
    state.output = Some(output.clone());

    if !use_screencopy {
        let capture_manager: ExtImageCopyCaptureManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .context("ext_image_copy_capture_manager_v1 not available")?;
        let source_manager: ExtOutputImageCaptureSourceManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .context("ext_output_image_capture_source_manager_v1 not available")?;
        let source = source_manager.create_source(&output, &qh, ());
        let session = capture_manager.create_session(
            &source,
            ext_image_copy_capture_manager_v1::Options::PaintCursors,
            &qh,
            (),
        );
        state.image_copy = Some(ImageCopyState::new(session, source));
    }

    WaylandSource::new(conn, event_queue)
        .insert(event_loop.handle())
        .map_err(|e| anyhow!("{}", e.error))?;

    event_loop
        .handle()
        .insert_source(ping_rx, |event, _, state| match event {
            calloop_channel::Event::Msg(()) => state.handle_ping(),
            calloop_channel::Event::Closed => state.done = Some(Ok(())),
        })
        .map_err(|e| anyhow!("{}", e.error))?;

    loop {
        event_loop.dispatch(None, &mut state)?;
        if let Some(result) = state.done.take() {
            return result;
        }
    }
}

struct Buffer {
    wl_buffer: OwningWlBuffer,
    image: Arc<Image>,
}

impl Buffer {
    #[allow(clippy::too_many_arguments)]
    fn new(
        device: Arc<Device>,
        allocator: &impl MemoryAllocator,
        dmabuf_state: &DmabufState,
        qh: &QueueHandle<State>,
        format: u32,
        modifiers: Vec<u64>,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        let vk_format =
            fourcc_to_vk_format(DrmFourcc::try_from(format).context("unknown fourcc")?)?;
        let raw_image = create_drm_modifier_image(
            device.clone(),
            vk_format,
            width,
            height,
            ImageUsage::TRANSFER_SRC | ImageUsage::SAMPLED,
            modifiers,
        )?;

        let (modifier, layout_aspect) =
            if let Some((modifier, _planes)) = raw_image.drm_format_modifier() {
                info!(
                    "buffer: {:?} modifier {:#x}",
                    DrmFourcc::try_from(format).ok(),
                    modifier,
                );
                (modifier, ImageAspect::MemoryPlane0)
            } else {
                (0, ImageAspect::Color)
            };

        let req = raw_image.memory_requirements()[0];
        let alloc = DeviceMemory::allocate(
            device,
            MemoryAllocateInfo {
                allocation_size: req.layout.size(),
                memory_type_index: allocator
                    .find_memory_type_index(req.memory_type_bits, MemoryTypeFilter::PREFER_DEVICE)
                    .context("No suitable memory type found for image")?,
                dedicated_allocation: Some(DedicatedAllocation::Image(&raw_image)),
                export_handle_types: ExternalMemoryHandleTypes::DMA_BUF,
                ..Default::default()
            },
        )?;

        let fd = alloc.export_fd(ExternalMemoryHandleType::DmaBuf)?;
        // SAFETY: vulkano's subresource_layout validation has a bug where the
        // generic format-aspect check doesn't account for DRM format modifiers
        // (MemoryPlane0 is valid but not in format_aspects for COLOR formats).
        let layout = unsafe { raw_image.subresource_layout_unchecked(layout_aspect, 0, 0) };

        let image = Arc::new(
            raw_image
                .bind_memory([ResourceMemory::new_dedicated(alloc)])
                .map_err(|(e, _, _)| e)?,
        );

        let params = dmabuf_state.create_params(qh)?;
        params.add(
            fd.as_fd(),
            0,
            layout.offset as u32,
            layout.row_pitch as u32,
            modifier,
        );
        let (wl_buffer, _params) = params.create_immed(
            width as i32,
            height as i32,
            format,
            zwp_linux_buffer_params_v1::Flags::empty(),
            qh,
        );

        Ok(Self {
            wl_buffer: OwningWlBuffer(wl_buffer),
            image,
        })
    }
}

impl From<ReclaimedBuffer> for Buffer {
    fn from(rbuf: ReclaimedBuffer) -> Self {
        Self {
            wl_buffer: *rbuf.backend_data.downcast().unwrap(),
            image: rbuf.image,
        }
    }
}
