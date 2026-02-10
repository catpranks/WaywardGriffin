mod dmabuf_probe;
mod image_copy;
mod screencopy;

use super::{BackendType, CaptureBackend, CaptureEnv, DeviceId, SpawnResult};
use crate::GlobalState;
use crate::OwningWlBuffer;
use crate::capture::SwapchainRenderer;
use crate::capture::input::dummy::DummyInput;
use crate::capture::plotter::{FrameInfo, PlotterHandle};
use anyhow::anyhow;
use anyhow::{Context as _, Result, bail};
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
use std::mem::MaybeUninit;
use std::os::fd::AsFd as _;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::info;
use vulkano::VulkanObject as _;
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
use smithay_client_toolkit::reexports::protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;
use smithay_client_toolkit::reexports::protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::{self, ExtImageCopyCaptureManagerV1};
use smithay_client_toolkit::reexports::protocols::ext::image_capture_source::v1::client::ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1;

fn connect(display: &str) -> Result<Connection> {
    let stream = UnixStream::connect(display)
        .with_context(|| format!("Failed to connect to Wayland socket: {display}"))?;
    Connection::from_socket(stream).context("Failed to create Wayland connection from socket")
}

pub struct Backend {
    display: String,
    feedback: DmabufFeedback,
}

impl Backend {
    pub fn new(display: &str) -> Result<Self> {
        let feedback = dmabuf_probe::query_dmabuf_feedback(display)?;
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
            let backend = env.backend;
            let ph = env.ph.clone();
            move || {
                ph.fatal(
                    run(env, &display, calloop_rx).context(format!("capture thread ({backend:?})")),
                );
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
    screencopy_manager: Option<ZwlrScreencopyManagerV1>,
    image_copy: Option<ImageCopyState>,
    qh: QueueHandle<State>,
    output: Option<WlOutput>,

    frame_state: Option<FrameState>,
    pool: Vec<Buffer>,

    mode: CaptureMode,
    done: Option<Result<()>>,
    loop_handle: LoopHandle<'static, State>,
    last_render: Instant,
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
        if self.screencopy_manager.is_some() {
            self.screencopy_issue_capture();
        } else if self.image_copy.is_some() {
            self.image_copy_issue_capture();
        } else {
            unreachable!();
        }
    }

    fn is_capturing(&self) -> bool {
        self.global_state.load().capture
    }

    fn do_render(&mut self, mut buf: Buffer, info: FrameInfo) -> CaptureMode {
        match self.renderer.render(buf.image.clone(), info) {
            Ok(fence) => {
                buf.fence = Some(fence);
                self.pool.push(buf);
                self.last_render = Instant::now();
                CaptureMode::Capturing
            }
            Err(e) => {
                self.pool.push(buf);
                self.done = Some(Err(e.context("render failed")));
                CaptureMode::Idle
            }
        }
    }

    fn handle_ready(&mut self, info: FrameInfo, buf: Buffer) {
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
        let safety = self.last_render.elapsed() >= Duration::from_secs(1);
        self.mode = match mode {
            CaptureMode::DisplayWaiting => self.do_render(buf, info),
            CaptureMode::Capturing if safety => self.do_render(buf, info),
            CaptureMode::Capturing => CaptureMode::FrameBuffered(PendingFrame { info, buf }),
            CaptureMode::FrameBuffered(old) if safety => {
                self.pool.push(old.buf);
                self.do_render(buf, info)
            }
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
            self.mode = mode;
            if let Err(e) = self.loop_handle.insert_source(
                Timer::from_duration(Duration::from_millis(100)),
                |_, _, state| {
                    state.issue_capture();
                    TimeoutAction::Drop
                },
            ) {
                self.done = Some(Err(anyhow!("failed to insert retry timer: {e}")));
            }
        } else if let CaptureMode::FrameBuffered(pending) = mode {
            self.pool.push(pending.buf);
        }
    }

    fn handle_wakeup(&mut self) {
        if !self.is_capturing() {
            if let Err(e) = self.renderer.blank() {
                self.done = Some(Err(e.context("render blank failed")));
            }
            self.last_render = Instant::now();
            return;
        }

        let mode = std::mem::replace(&mut self.mode, CaptureMode::Idle);
        self.mode = match mode {
            CaptureMode::Idle => {
                self.issue_capture();
                CaptureMode::DisplayWaiting
            }
            CaptureMode::FrameBuffered(pending) => self.do_render(pending.buf, pending.info),
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
        backend,
    } = env;
    let use_screencopy = backend == BackendType::Screencopy;
    let conn = connect(display)?;

    let (globals, mut event_queue) = registry_queue_init::<State>(&conn)?;
    let qh = event_queue.handle();

    let mut event_loop: EventLoop<State> = EventLoop::try_new()?;

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
        renderer,
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

        mode: CaptureMode::Idle,
        done: None,
        loop_handle: event_loop.handle(),
        last_render: Instant::now(),
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
        .insert_source(calloop_rx, |event, _, state| match event {
            calloop_channel::Event::Msg(()) => state.handle_wakeup(),
            calloop_channel::Event::Closed => state.done = Some(Ok(())),
        })
        .map_err(|e| anyhow!("{}", e.error))?;

    state.renderer.blank()?;

    loop {
        event_loop.dispatch(Some(Duration::from_secs(1)), &mut state)?;
        if let Some(result) = state.done.take() {
            return result;
        }
        if !state.is_capturing() && state.last_render.elapsed() >= Duration::from_secs(1) {
            state.renderer.blank()?;
            state.last_render = Instant::now();
        }
    }
}

struct Buffer {
    wl_buffer: OwningWlBuffer,
    image: Arc<Image>,
    fence: Option<Arc<Fence>>,
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
        let vk_format = fourcc_to_format(format)?;

        let tiling = if modifiers.is_empty() {
            ImageTiling::Linear
        } else {
            ImageTiling::DrmFormatModifier
        };

        let raw_image = if tiling == ImageTiling::DrmFormatModifier {
            // Workaround: vulkano 0.35.2 doesn't chain
            // VkImageDrmFormatModifierListCreateInfoEXT into pNext.
            // Create the VkImage ourselves and wrap it.
            let handle = {
                let mut modifier_list = ash::vk::ImageDrmFormatModifierListCreateInfoEXT::default()
                    .drm_format_modifiers(&modifiers);
                let mut external_mem = ash::vk::ExternalMemoryImageCreateInfo::default()
                    .handle_types(ash::vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
                let create_info_vk = ash::vk::ImageCreateInfo::default()
                    .image_type(ash::vk::ImageType::TYPE_2D)
                    .format(vk_format.into())
                    .extent(ash::vk::Extent3D {
                        width,
                        height,
                        depth: 1,
                    })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(ash::vk::SampleCountFlags::TYPE_1)
                    .tiling(ash::vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                    .usage(
                        ash::vk::ImageUsageFlags::TRANSFER_SRC | ash::vk::ImageUsageFlags::SAMPLED,
                    )
                    .sharing_mode(ash::vk::SharingMode::EXCLUSIVE)
                    .initial_layout(ash::vk::ImageLayout::UNDEFINED)
                    .push_next(&mut modifier_list)
                    .push_next(&mut external_mem);
                let mut output = MaybeUninit::uninit();
                unsafe {
                    (device.fns().v1_0.create_image)(
                        device.handle(),
                        &create_info_vk,
                        std::ptr::null(),
                        output.as_mut_ptr(),
                    )
                }
                .result()
                .map_err(|e| anyhow!("vkCreateImage: {e:?}"))?;
                unsafe { output.assume_init() }
            };
            unsafe {
                RawImage::from_handle(
                    device.clone(),
                    handle,
                    ImageCreateInfo {
                        format: vk_format,
                        extent: [width, height, 1],
                        usage: ImageUsage::TRANSFER_SRC | ImageUsage::SAMPLED,
                        external_memory_handle_types: ExternalMemoryHandleTypes::DMA_BUF,
                        tiling: ImageTiling::DrmFormatModifier,
                        drm_format_modifiers: modifiers,
                        ..Default::default()
                    },
                )
            }
            .context("RawImage::from_handle")?
        } else {
            RawImage::new(
                device.clone(),
                ImageCreateInfo {
                    format: vk_format,
                    extent: [width, height, 1],
                    usage: ImageUsage::TRANSFER_SRC | ImageUsage::SAMPLED,
                    external_memory_handle_types: ExternalMemoryHandleTypes::DMA_BUF,
                    tiling,
                    ..Default::default()
                },
            )?
        };

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
