use super::{CaptureBackend, CaptureBackendBuilder, CapturedFrame};
use crate::OwningWlBuffer;
use crate::capture::input::InputBridge;
use crate::capture::input::dummy::DummyInput;
use crate::capture::plotter::FrameInfo;
use anyhow::{Context as _, Result, bail};
use drm_fourcc::DrmFourcc;
use smithay_client_toolkit::dmabuf::{DmabufFeedback, DmabufHandler, DmabufState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::EventLoop;
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::wl_buffer::WlBuffer;
use smithay_client_toolkit::reexports::client::protocol::wl_output::WlOutput;
use smithay_client_toolkit::reexports::client::{Connection, Dispatch, EventQueue, QueueHandle};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::{
    delegate_dmabuf, delegate_output, delegate_registry, registry_handlers,
};
use std::os::fd::AsFd as _;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
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

struct State {
    registry_state: RegistryState,
    output_state: OutputState,
    dmabuf_state: DmabufState,
    screencopy_manager: ZwlrScreencopyManagerV1,
    feedback: Option<DmabufFeedback>,
    qh: QueueHandle<State>,
    output: Option<WlOutput>,
    pending_linux_dmabuf: Option<(u32, u32, u32)>,
    shared: Arc<Mutex<BufferPool>>,
    device: Option<Arc<Device>>,
    allocator: Option<Arc<StandardMemoryAllocator>>,
    in_flight: Option<PooledBuffer>,
}

impl State {
    fn start_capture(&self) {
        let output = self.output.as_ref().unwrap();
        self.screencopy_manager
            .capture_output(1, output, &self.qh, ());
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
                state.pending_linux_dmabuf = Some((format, width, height));
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => {
                let (format, width, height) = state.pending_linux_dmabuf.take().unwrap();
                let vk_format = fourcc_to_format(format).unwrap();

                let mut buf = {
                    let mut pool = state.shared.lock().unwrap();
                    pool.pool.pop()
                };

                let reuse = buf.as_ref().is_some_and(|b| {
                    let ext = b.image.extent();
                    b.image.format() == vk_format && (ext[0], ext[1]) == (width, height)
                });
                if !reuse {
                    if let Some(old) = &mut buf
                        && let Some(fence) = old.fence.take()
                    {
                        fence.wait(None).unwrap();
                    }
                    buf = Some(
                        PooledBuffer::new(
                            state.device.as_ref().unwrap(),
                            state.allocator.as_ref().unwrap().as_ref(),
                            &state.dmabuf_state,
                            &state.qh,
                            format,
                            width,
                            height,
                        )
                        .unwrap(),
                    );
                }
                let mut buf = buf.unwrap();

                if let Some(fence) = buf.fence.take() {
                    fence.wait(None).unwrap();
                }

                frame.copy(&buf.wl_buffer);
                state.in_flight = Some(buf);
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => {
                let buf = state.in_flight.take().unwrap();
                // TODO: proper timing (start at capture_output, wait at fence, obtain here)
                // and plotter capture/capture_miss counters
                let now = std::time::Instant::now();
                let captured = CapturedFrame {
                    image: buf.image,
                    info: FrameInfo {
                        start: now,
                        wait: now,
                        obtain: now,
                        commit: None,
                        capture_mono_ns: 0,
                        present: None,
                        cursor_visible: true,
                    },
                    handle: Box::new(BufferHandle {
                        wl_buffer: buf.wl_buffer,
                    }),
                };
                let mut pool = state.shared.lock().unwrap();
                if let Some(old) = pool.ready.take() {
                    let handle: BufferHandle = *old.handle.downcast().expect("invalid handle type");
                    pool.pool.push(PooledBuffer {
                        wl_buffer: handle.wl_buffer,
                        image: old.image,
                        fence: None,
                    });
                }
                pool.ready = Some(captured);
                frame.destroy();
                state.start_capture();
            }
            zwlr_screencopy_frame_v1::Event::Failed => {
                // TODO: counter, return the pending buffer
                frame.destroy();
                state.start_capture();
            }
            _ => {}
        }
    }
}

delegate_registry!(State);
delegate_output!(State);
delegate_dmabuf!(State);

pub struct Builder {
    conn: Connection,
    event_queue: EventQueue<State>,
    state: State,
}

impl Builder {
    pub fn new(display: &str) -> Result<Self> {
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
            registry_state: RegistryState::new(&globals),
            output_state: OutputState::new(&globals, &qh),
            dmabuf_state: DmabufState::new(&globals, &qh),
            screencopy_manager,
            feedback: None,
            qh: qh.clone(),
            output: None,
            pending_linux_dmabuf: None,
            shared: Arc::new(Mutex::new(BufferPool {
                pool: Vec::new(),
                ready: None,
            })),
            device: None,
            allocator: None,
            in_flight: None,
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

        state.dmabuf_state.get_default_feedback(&qh)?;
        event_queue.roundtrip(&mut state)?;

        if state.feedback.is_none() {
            bail!("Compositor did not provide dmabuf feedback");
        }

        Ok(Self {
            conn,
            event_queue,
            state,
        })
    }
}

impl CaptureBackendBuilder for Builder {
    fn device_id(&self) -> super::DeviceId {
        let dev = self.state.feedback.as_ref().unwrap().main_device();
        super::DeviceId::DevMajorMinor(nix::sys::stat::major(dev), nix::sys::stat::minor(dev))
    }

    fn build(
        self: Box<Self>,
        device: Arc<Device>,
        allocator: Arc<StandardMemoryAllocator>,
        _display: &str,
    ) -> Result<(Box<dyn CaptureBackend>, Box<dyn InputBridge>)> {
        let Self {
            conn,
            event_queue,
            mut state,
        } = *self;

        let shared = state.shared.clone();
        state.device = Some(device);
        state.allocator = Some(allocator);

        std::thread::Builder::new()
            .name("wlr-screencopy".into())
            .spawn(move || event_loop_thread(conn, event_queue, state))?;

        Ok((Box::new(Backend { shared }), Box::new(DummyInput::new())))
    }
}

struct PooledBuffer {
    wl_buffer: OwningWlBuffer,
    image: Arc<Image>,
    fence: Option<Arc<Fence>>,
}

impl PooledBuffer {
    fn new(
        device: &Arc<Device>,
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
            device.clone(),
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

struct BufferPool {
    pool: Vec<PooledBuffer>,
    ready: Option<CapturedFrame>,
}

struct BufferHandle {
    wl_buffer: OwningWlBuffer,
}

struct Backend {
    shared: Arc<Mutex<BufferPool>>,
}

impl CaptureBackend for Backend {
    fn capture(&mut self) -> Result<Option<CapturedFrame>> {
        let mut pool = self.shared.lock().unwrap();
        Ok(pool.ready.take())
    }

    fn release(&mut self, frame: CapturedFrame, fence: Option<Arc<Fence>>) {
        let handle: BufferHandle = *frame.handle.downcast().expect("invalid handle type");
        let mut pool = self.shared.lock().unwrap();
        assert!(pool.pool.len() < 4, "buffer pool bloated");
        pool.pool.push(PooledBuffer {
            wl_buffer: handle.wl_buffer,
            image: frame.image,
            fence,
        });
    }
}

fn event_loop_thread(conn: Connection, event_queue: EventQueue<State>, mut state: State) {
    let mut event_loop: EventLoop<State> = EventLoop::try_new().unwrap();
    let loop_handle = event_loop.handle();

    WaylandSource::new(conn, event_queue)
        .insert(loop_handle)
        .unwrap();

    state.start_capture();

    loop {
        event_loop.dispatch(None, &mut state).unwrap();
    }
}
