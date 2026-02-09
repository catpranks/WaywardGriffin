use super::{CaptureBackend, CaptureEnv, DeviceId, SpawnResult};
use crate::GlobalState;
use crate::OwningWlBuffer;
use crate::capture::SwapchainRenderer;
use crate::capture::input::dummy::DummyInput;
use crate::capture::plotter::{FrameInfo, PlotterHandle};
use anyhow::{Context as _, Result, bail};
use drm_fourcc::DrmFourcc;
use smithay_client_toolkit::dmabuf::{DmabufFeedback, DmabufHandler, DmabufState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::EventLoop;
use smithay_client_toolkit::reexports::calloop::channel::{self as calloop_channel, Channel};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::wl_buffer::WlBuffer;
use smithay_client_toolkit::reexports::client::protocol::wl_output::WlOutput;
use smithay_client_toolkit::reexports::client::{Connection, Dispatch, QueueHandle};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::{
    delegate_dmabuf, delegate_output, delegate_registry, registry_handlers,
};
use std::os::fd::AsFd as _;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Instant;
use vulkano::device::Device;
use vulkano::format::Format;
use vulkano::image::sys::RawImage;
use vulkano::image::{Image, ImageAspect, ImageCreateInfo, ImageTiling, ImageUsage};
use vulkano::memory::allocator::{MemoryAllocator, MemoryTypeFilter, StandardMemoryAllocator};
use vulkano::memory::{
    DedicatedAllocation, DeviceMemory, ExternalMemoryHandleType, ExternalMemoryHandleTypes,
    MemoryAllocateInfo, ResourceMemory,
};
use vulkano::sync::fence::Fence;

// breaks rustfmt import sorting for some reason
use smithay_client_toolkit::reexports::protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1;
use smithay_client_toolkit::reexports::protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1};
use smithay_client_toolkit::reexports::protocols_wlr::screencopy::v1::client::zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1};
use smithay_client_toolkit::reexports::protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1};

pub struct Backend {
    display: String,
    feedback: DmabufFeedback,
}

impl Backend {
    pub fn new(display: &str) -> Result<Self> {
        let stream = UnixStream::connect(display)
            .with_context(|| format!("Failed to connect to Wayland socket: {display}"))?;
        let conn = Connection::from_socket(stream)
            .context("Failed to create Wayland connection from socket")?;

        let (globals, mut event_queue) = registry_queue_init::<PreInitState>(&conn)?;
        let qh = event_queue.handle();

        let mut state = PreInitState {
            registry_state: RegistryState::new(&globals),
            dmabuf_state: DmabufState::new(&globals, &qh),
            feedback: None,
        };

        state.dmabuf_state.get_default_feedback(&qh)?;
        event_queue.roundtrip(&mut state)?;

        let feedback = state
            .feedback
            .context("Compositor did not provide dmabuf feedback")?;

        Ok(Self {
            display: display.to_owned(),
            feedback,
        })
    }
}

impl CaptureBackend for Backend {
    fn device_id(&self) -> DeviceId {
        let dev = self.feedback.main_device();
        DeviceId::DevMajorMinor(nix::sys::stat::major(dev), nix::sys::stat::minor(dev))
    }

    fn spawn(self: Box<Self>, env: CaptureEnv) -> Result<SpawnResult> {
        let injector = Box::new(DummyInput::new());
        let (calloop_tx, calloop_rx) = calloop_channel::channel();
        std::thread::spawn({
            let display = self.display;
            let ph = env.ph.clone();
            move || {
                ph.fatal(run(env, &display, calloop_rx).context("capture thread (screencopy)"));
            }
        });
        Ok(SpawnResult {
            injector,
            wake: Box::new(move || {
                let _ = calloop_tx.send(());
            }),
        })
    }
}

// Minimal state for device_id pre-init
struct PreInitState {
    registry_state: RegistryState,
    dmabuf_state: DmabufState,
    feedback: Option<DmabufFeedback>,
}

impl ProvidesRegistryState for PreInitState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![];
}

impl DmabufHandler for PreInitState {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_feedback(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _proxy: &ZwpLinuxDmabufFeedbackV1,
        feedback: DmabufFeedback,
    ) {
        self.feedback = Some(feedback);
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

delegate_registry!(PreInitState);
delegate_dmabuf!(PreInitState);

// Main backend state for the calloop event loop

struct State {
    renderer: SwapchainRenderer,
    ph: PlotterHandle,
    global_state: GlobalState,
    device: Arc<Device>,
    allocator: Arc<StandardMemoryAllocator>,

    registry_state: RegistryState,
    output_state: OutputState,
    dmabuf_state: DmabufState,
    screencopy_manager: ZwlrScreencopyManagerV1,
    qh: QueueHandle<State>,
    output: Option<WlOutput>,

    frame_state: Option<FrameState>,
    pool: Vec<Buffer>,

    mode: CaptureMode,
    done: Option<Result<()>>,
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

struct PendingFrame {
    info: FrameInfo,
    buf: Buffer,
}

enum CaptureMode {
    Idle,
    Capturing,
    DisplayWaiting,
    FrameBuffered(PendingFrame),
}

impl State {
    fn issue_capture(&mut self) {
        self.frame_state = Some(FrameState::Requested {
            start: Instant::now(),
        });
        let output = self.output.as_ref().unwrap();
        self.screencopy_manager
            .capture_output(1, output, &self.qh, ());
    }

    fn is_capturing(&self) -> bool {
        self.global_state.load().capture
    }

    fn handle_ready(&mut self, info: FrameInfo, mut buf: Buffer) {
        if !self.is_capturing() {
            self.pool.push(buf);
            if let CaptureMode::FrameBuffered(pending) =
                std::mem::replace(&mut self.mode, CaptureMode::Idle)
            {
                self.pool.push(pending.buf);
            }
            return;
        }

        self.ph.capture();

        if self.global_state.load().cursor_visible != info.cursor_visible {
            self.global_state
                .rcu(|s| s.with_cursor_visible(info.cursor_visible));
        }

        let mode = std::mem::replace(&mut self.mode, CaptureMode::Idle);
        self.mode = match mode {
            CaptureMode::DisplayWaiting => match self.renderer.render(buf.image.clone(), info) {
                Ok(fence) => {
                    buf.fence = Some(fence);
                    self.pool.push(buf);
                    CaptureMode::Capturing
                }
                Err(e) => {
                    self.pool.push(buf);
                    self.done = Some(Err(e.context("render failed")));
                    CaptureMode::Idle
                }
            },
            CaptureMode::Capturing => CaptureMode::FrameBuffered(PendingFrame { info, buf }),
            CaptureMode::FrameBuffered(old) => {
                self.ph.capture_miss();
                self.pool.push(old.buf);
                CaptureMode::FrameBuffered(PendingFrame { info, buf })
            }
            CaptureMode::Idle => unreachable!("handle_ready called while Idle"),
        };
        self.issue_capture();
    }

    fn handle_failed(&mut self) {
        let mode = std::mem::replace(&mut self.mode, CaptureMode::Idle);

        if matches!(mode, CaptureMode::Idle) {
            unreachable!("handle_failed called while Idle");
        }

        if self.is_capturing() {
            self.issue_capture();
            self.mode = mode;
        } else if let CaptureMode::FrameBuffered(pending) = mode {
            self.pool.push(pending.buf);
        }
    }

    fn handle_buffer_done(&mut self, frame: &ZwlrScreencopyFrameV1) -> Result<()> {
        let Some(FrameState::Described {
            start,
            format,
            width,
            height,
        }) = self.frame_state.take()
        else {
            bail!("BufferDone without prior LinuxDmabuf");
        };
        let vk_format = fourcc_to_format(format)?;

        let mut buf = self.pool.pop();

        let reuse = buf.as_ref().is_some_and(|b| {
            let ext = b.image.extent();
            b.image.format() == vk_format && (ext[0], ext[1]) == (width, height)
        });
        if !reuse {
            if let Some(old) = &mut buf
                && let Some(fence) = old.fence.take()
            {
                fence.wait(None)?;
            }
            buf = Some(Buffer::new(
                self.device.clone(),
                self.allocator.as_ref(),
                &self.dmabuf_state,
                &self.qh,
                format,
                width,
                height,
            )?);
        }
        let mut buf = buf.unwrap();

        if let Some(fence) = buf.fence.take() {
            fence.wait(None)?;
        }
        let wait = Instant::now();

        frame.copy(&buf.wl_buffer);
        self.frame_state = Some(FrameState::Copying { buf, start, wait });
        Ok(())
    }

    fn handle_wakeup(&mut self) {
        if !self.is_capturing() {
            if let Err(e) = self.renderer.blank() {
                self.done = Some(Err(e.context("render blank failed")));
            }
            return;
        }

        let mode = std::mem::replace(&mut self.mode, CaptureMode::Idle);
        self.mode = match mode {
            CaptureMode::Idle => {
                self.issue_capture();
                CaptureMode::DisplayWaiting
            }
            CaptureMode::FrameBuffered(mut pending) => {
                match self
                    .renderer
                    .render(pending.buf.image.clone(), pending.info)
                {
                    Ok(fence) => {
                        pending.buf.fence = Some(fence);
                        self.pool.push(pending.buf);
                        CaptureMode::Capturing
                    }
                    Err(e) => {
                        self.pool.push(pending.buf);
                        self.done = Some(Err(e.context("render failed")));
                        CaptureMode::Idle
                    }
                }
            }
            CaptureMode::Capturing | CaptureMode::DisplayWaiting => CaptureMode::DisplayWaiting,
        };
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

impl Dispatch<ZwlrScreencopyManagerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrScreencopyManagerV1,
        _event: zwlr_screencopy_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        frame: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::LinuxDmabuf {
                format,
                width,
                height,
            } => {
                let start = match state.frame_state {
                    Some(FrameState::Requested { start } | FrameState::Described { start, .. }) => {
                        start
                    }
                    _ => return,
                };
                state.frame_state = Some(FrameState::Described {
                    start,
                    format,
                    width,
                    height,
                });
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => {
                if let Err(e) = state.handle_buffer_done(frame) {
                    state.done = Some(Err(e));
                }
            }
            zwlr_screencopy_frame_v1::Event::Ready {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
            } => {
                let Some(FrameState::Copying { buf, start, wait }) = state.frame_state.take()
                else {
                    assert!(state.done.is_some());
                    return;
                };
                let obtain = Instant::now();
                let capture_mono_ns = ((tv_sec_hi as u64) << 32 | tv_sec_lo as u64)
                    * 1_000_000_000
                    + tv_nsec as u64;
                let info = FrameInfo {
                    start,
                    wait,
                    obtain,
                    commit: None,
                    capture_mono_ns,
                    present: None,
                    cursor_visible: true,
                };
                frame.destroy();
                state.handle_ready(info, buf);
            }
            zwlr_screencopy_frame_v1::Event::Failed => {
                if let Some(FrameState::Copying { buf, .. }) = state.frame_state.take() {
                    state.pool.push(buf);
                }
                frame.destroy();
                state.handle_failed();
            }
            _ => {}
        }
    }
}

delegate_registry!(State);
delegate_output!(State);
delegate_dmabuf!(State);

fn run(env: CaptureEnv, display: &str, calloop_rx: Channel<()>) -> Result<()> {
    let CaptureEnv {
        renderer,
        ph,
        global_state,
        device,
        allocator,
    } = env;
    let stream = UnixStream::connect(display)
        .with_context(|| format!("Failed to connect to Wayland socket: {display}"))?;
    let conn = Connection::from_socket(stream)
        .context("Failed to create Wayland connection from socket")?;

    let (globals, mut event_queue) = registry_queue_init::<State>(&conn)?;
    let qh = event_queue.handle();

    let screencopy_manager: ZwlrScreencopyManagerV1 = globals
        .bind(&qh, 3..=3, ())
        .context("zwlr_screencopy_manager_v1 v3 not available")?;

    let mut state = State {
        renderer,
        ph,
        global_state,
        device,
        allocator,

        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        dmabuf_state: DmabufState::new(&globals, &qh),
        screencopy_manager,
        qh: qh.clone(),
        output: None,

        frame_state: None,
        pool: Vec::new(),

        mode: CaptureMode::Idle,
        done: None,
    };

    event_queue.roundtrip(&mut state)?;

    state.output = Some(
        state
            .output_state
            .outputs()
            .next()
            .context("no outputs available")?
            .clone(),
    );

    let mut event_loop: EventLoop<State> = EventLoop::try_new()?;

    WaylandSource::new(conn, event_queue)
        .insert(event_loop.handle())
        .map_err(|e| anyhow::anyhow!("{}", e.error))?;

    event_loop
        .handle()
        .insert_source(calloop_rx, |event, _, state| match event {
            calloop_channel::Event::Msg(()) => state.handle_wakeup(),
            calloop_channel::Event::Closed => state.done = Some(Ok(())),
        })
        .map_err(|e| anyhow::anyhow!("{}", e.error))?;

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
    fence: Option<Arc<Fence>>,
}

impl Buffer {
    fn new(
        device: Arc<Device>,
        allocator: &impl MemoryAllocator,
        dmabuf_state: &DmabufState,
        qh: &QueueHandle<State>,
        format: u32,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        let vk_format = fourcc_to_format(format)?;

        let raw_image = RawImage::new(
            device.clone(),
            ImageCreateInfo {
                format: vk_format,
                extent: [width, height, 1],
                usage: ImageUsage::TRANSFER_SRC | ImageUsage::SAMPLED,
                external_memory_handle_types: ExternalMemoryHandleTypes::DMA_BUF,
                tiling: ImageTiling::Linear,
                ..Default::default()
            },
        )?;

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
        let layout = raw_image.subresource_layout(ImageAspect::Color, 0, 0)?;

        let image = Arc::new(
            raw_image
                .bind_memory([ResourceMemory::new_dedicated(alloc)])
                .map_err(|(e, _, _)| e)?,
        );

        let params = dmabuf_state.create_params(qh)?;
        params.add(fd.as_fd(), 0, 0, layout.row_pitch as u32, 0);
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
            fence: None,
        })
    }
}

fn fourcc_to_format(fourcc: u32) -> Result<Format> {
    match DrmFourcc::try_from(fourcc).context("unknown fourcc")? {
        DrmFourcc::Argb8888 | DrmFourcc::Xrgb8888 => Ok(Format::B8G8R8A8_SRGB),
        DrmFourcc::Abgr8888 | DrmFourcc::Xbgr8888 => Ok(Format::R8G8B8A8_SRGB),
        other => bail!("unsupported fourcc: {other:?}"),
    }
}
