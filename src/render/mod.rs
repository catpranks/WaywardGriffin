mod shaders;

use crate::display::DisplayCtx;
use crate::overlay::{OverlayFrame, OverlayHandle, OverlayState};
use crate::sizer::{SharedSizer, Sizer};
use crate::source::{CaptureBackend, CapturedFrame, DeviceId};
use anyhow::{Context as _, Result, bail};
use shaders::{
    BorderPushConstants, ScreenshotPushConstants, TexturedPushConstants, cs_screenshot, fs_border,
    fs_textured, vs,
};
use smallvec::smallvec;
use smithay_client_toolkit::compositor::Region;
use smithay_client_toolkit::reexports::client::{Connection, Proxy as _};
use std::io::Write as _;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tracing::{info, warn};
use vulkano::buffer::{Buffer, BufferCreateInfo, BufferUsage};
use vulkano::command_buffer::allocator::StandardCommandBufferAllocator;
use vulkano::command_buffer::{
    AutoCommandBufferBuilder, ClearColorImageInfo, CommandBufferBeginInfo, CommandBufferLevel,
    CommandBufferSubmitInfo, CommandBufferUsage, CopyBufferToImageInfo, RecordingCommandBuffer,
    RenderPassBeginInfo, SemaphoreSubmitInfo, SubmitInfo, SubpassBeginInfo,
};
use vulkano::descriptor_set::allocator::StandardDescriptorSetAllocator;
use vulkano::descriptor_set::{DescriptorImageViewInfo, DescriptorSet, WriteDescriptorSet};
use vulkano::device::physical::{DriverId, PhysicalDeviceType};
use vulkano::device::{
    Device, DeviceCreateInfo, DeviceExtensions, DeviceFeatures, Queue, QueueCreateInfo, QueueFlags,
};
use vulkano::format::Format;
use vulkano::image::sampler::{Filter, Sampler, SamplerAddressMode, SamplerCreateInfo};
use vulkano::image::view::ImageView;
use vulkano::image::{Image, ImageAspects, ImageLayout, ImageSubresourceRange, ImageUsage};
use vulkano::instance::{Instance, InstanceCreateInfo, InstanceExtensions};
use vulkano::memory::allocator::{AllocationCreateInfo, MemoryTypeFilter, StandardMemoryAllocator};
use vulkano::pipeline::compute::ComputePipelineCreateInfo;
use vulkano::pipeline::graphics::GraphicsPipelineCreateInfo;
use vulkano::pipeline::graphics::color_blend::{
    AttachmentBlend, ColorBlendAttachmentState, ColorBlendState,
};
use vulkano::pipeline::graphics::input_assembly::{InputAssemblyState, PrimitiveTopology};
use vulkano::pipeline::graphics::multisample::MultisampleState;
use vulkano::pipeline::graphics::rasterization::RasterizationState;
use vulkano::pipeline::graphics::vertex_input::VertexInputState;
use vulkano::pipeline::graphics::viewport::{Scissor, Viewport, ViewportState};
use vulkano::pipeline::layout::PipelineDescriptorSetLayoutCreateInfo;
use vulkano::pipeline::{
    ComputePipeline, DynamicState, GraphicsPipeline, Pipeline, PipelineBindPoint, PipelineLayout,
    PipelineShaderStageCreateInfo,
};
use vulkano::render_pass::{Framebuffer, FramebufferCreateInfo, RenderPass, Subpass};
use vulkano::shader::EntryPoint;
use vulkano::swapchain::{
    AcquireNextImageInfo, AcquiredImage, ColorSpace, CompositeAlpha, PresentInfo, PresentMode,
    SemaphorePresentInfo, Surface, Swapchain, SwapchainCreateInfo, SwapchainPresentInfo,
};
use vulkano::sync::fence::{Fence, FenceCreateFlags, FenceCreateInfo};
use vulkano::sync::semaphore::{Semaphore, SemaphoreCreateInfo};
use vulkano::sync::{
    AccessFlags, DependencyInfo, ImageMemoryBarrier, PipelineStages, QueueFamilyOwnershipTransfer,
};
use vulkano::{VulkanLibrary, VulkanObject as _, single_pass_renderpass};

struct InFlight {
    acquire: Arc<Semaphore>,
    present: Arc<Semaphore>,
    uploaded: Arc<Semaphore>,
    upload_command: Option<Arc<dyn Send + Sync>>,
    fence: Arc<Fence>,
    last_command_buffer: Option<Arc<dyn Send + Sync>>,
    frames: Vec<Arc<CapturedFrame>>,
    descriptor_sets: Vec<Arc<DescriptorSet>>,
    overlays: Vec<Arc<OverlayFrame>>,
    overlay_acquire: Option<Arc<Semaphore>>,
}

pub struct Renderer {
    dc: DisplayCtx,
    sizer: SharedSizer,
    last_committed_sizer: Option<Sizer>,
    needs_recreate: bool,

    frame_idx: usize,
    in_flight: Vec<InFlight>,
    current_frame: Option<Arc<CapturedFrame>>,
    images: Vec<Arc<Framebuffer>>,
    textured_pipeline: Arc<GraphicsPipeline>,
    border_pipeline: Arc<GraphicsPipeline>,
    screenshot_pipeline: Arc<ComputePipeline>,
    sampler: Arc<Sampler>,
    render_pass: Arc<RenderPass>,
    swapchain: Arc<Swapchain>,

    command_buffer_allocator: Arc<StandardCommandBufferAllocator>,
    descriptor_set_allocator: Arc<StandardDescriptorSetAllocator>,

    current_overlay: Option<Arc<OverlayFrame>>,

    pub allocator: Arc<StandardMemoryAllocator>,
    pub queue: Arc<Queue>,
    transfer_queue: Arc<Queue>,
    pub device: Arc<Device>,
    _surface: Arc<Surface>,
    start_time: Instant,
    scanout_pacing: bool,
}

impl Renderer {
    pub fn new(
        conn: &Connection,
        device_id: DeviceId,
        dc: DisplayCtx,
        sizer: SharedSizer,
    ) -> Result<Self> {
        let library = VulkanLibrary::new().context("no Vulkan library")?;
        let instance = Instance::new(
            library,
            InstanceCreateInfo {
                enabled_extensions: InstanceExtensions {
                    khr_wayland_surface: true,
                    khr_external_memory_capabilities: true,
                    ..InstanceExtensions::empty()
                },
                ..Default::default()
            },
        )?;

        let host_capture = matches!(device_id, DeviceId::RadvIntegrated);
        let physical_device = match device_id {
            DeviceId::RadvIntegrated => instance
                .enumerate_physical_devices()?
                .find(|p| {
                    p.properties().driver_id == Some(DriverId::MesaRADV)
                        && p.properties().device_type == PhysicalDeviceType::IntegratedGpu
                })
                .context("NVFBC presentation requires a RADV integrated GPU")?,
            DeviceId::DevMajorMinor(major, minor) => instance
                .enumerate_physical_devices()?
                .find(|p| {
                    let props = p.properties();
                    let major = major as i64;
                    let minor = minor as i64;
                    (props.primary_major == Some(major) && props.primary_minor == Some(minor))
                        || (props.render_major == Some(major) && props.render_minor == Some(minor))
                })
                .context("no physical device with matching major/minor")?,
        };

        info!(device = %physical_device.properties().device_name, "render/present device");

        let surface = unsafe {
            Surface::from_wayland(
                instance,
                conn.backend().display_ptr() as _,
                dc.surface.id().as_ptr() as _,
                None,
            )?
        };

        let device_extensions = DeviceExtensions {
            khr_swapchain: true,
            ext_external_memory_host: host_capture,
            khr_external_memory: true,
            khr_external_memory_fd: true,
            ext_external_memory_dma_buf: true,
            khr_external_semaphore_fd: true,
            khr_timeline_semaphore: true,
            ext_image_drm_format_modifier: true,
            ..DeviceExtensions::empty()
        };

        let queue_family_index = physical_device
            .queue_family_properties()
            .iter()
            .enumerate()
            .position(|(i, q)| {
                q.queue_flags.intersects(QueueFlags::GRAPHICS)
                    && physical_device
                        .surface_support(i as u32, &surface)
                        .unwrap_or(false)
            })
            .map(|i| i as u32)
            .context("No graphics queue family found on the device")?;

        let transfer_index = physical_device
            .queue_family_properties()
            .iter()
            .position(|q| {
                host_capture
                    && q.queue_flags.contains(QueueFlags::TRANSFER)
                    && !q
                        .queue_flags
                        .intersects(QueueFlags::GRAPHICS | QueueFlags::COMPUTE)
            })
            .map(|i| i as u32)
            .unwrap_or(queue_family_index);
        let mut queue_create_infos = vec![QueueCreateInfo {
            queue_family_index,
            ..Default::default()
        }];
        if transfer_index != queue_family_index {
            queue_create_infos.push(QueueCreateInfo {
                queue_family_index: transfer_index,
                ..Default::default()
            });
        }
        info!(queue_family_index, transfer_index, "queue families");
        let (device, mut queues) = Device::new(
            physical_device.clone(),
            DeviceCreateInfo {
                enabled_extensions: device_extensions,
                queue_create_infos,
                enabled_features: DeviceFeatures {
                    timeline_semaphore: true,
                    ..Default::default()
                },
                ..Default::default()
            },
        )?;
        let queue = queues.next().unwrap();
        let transfer_queue = queues.next().unwrap_or_else(|| queue.clone());
        let allocator = Arc::new(StandardMemoryAllocator::new_default(device.clone()));

        let (swapchain, swapchain_images) = {
            let surface_capabilities =
                physical_device.surface_capabilities(&surface, Default::default())?;

            Swapchain::new(
                device.clone(),
                surface.clone(),
                SwapchainCreateInfo {
                    image_sharing: if transfer_index != queue_family_index {
                        vulkano::sync::Sharing::Concurrent(smallvec![
                            queue_family_index,
                            transfer_index
                        ])
                    } else {
                        vulkano::sync::Sharing::Exclusive
                    },
                    min_image_count: surface_capabilities.min_image_count.max(2),
                    image_format: Format::B8G8R8A8_SRGB,
                    image_color_space: ColorSpace::SrgbNonLinear,
                    image_extent: [1, 1],
                    image_usage: ImageUsage::COLOR_ATTACHMENT | ImageUsage::TRANSFER_DST,
                    composite_alpha: CompositeAlpha::Opaque,
                    present_mode: PresentMode::Mailbox,
                    ..Default::default()
                },
            )?
        };

        let render_pass = single_pass_renderpass!(
            device.clone(),
            attachments: {
                color: {
                    format: swapchain.image_format(),
                    samples: 1,
                    load_op: Load,
                    store_op: Store,
                },
            },
            pass: {
                color: [color],
                depth_stencil: {},
            },
        )?;
        let vs = vs::load(device.clone())?.entry_point("main").unwrap();

        let make_pipeline = |vs: &EntryPoint, fs_entry| -> Result<Arc<GraphicsPipeline>> {
            let stages = [
                PipelineShaderStageCreateInfo::new(vs.clone()),
                PipelineShaderStageCreateInfo::new(fs_entry),
            ];
            let layout = PipelineLayout::new(
                device.clone(),
                PipelineDescriptorSetLayoutCreateInfo::from_stages(&stages)
                    .into_pipeline_layout_create_info(device.clone())
                    .unwrap(),
            )
            .unwrap();
            let subpass = Subpass::from(render_pass.clone(), 0).unwrap();
            Ok(GraphicsPipeline::new(
                device.clone(),
                None,
                GraphicsPipelineCreateInfo {
                    stages: stages.into_iter().collect(),
                    vertex_input_state: Some(VertexInputState::default()),
                    input_assembly_state: Some(InputAssemblyState {
                        topology: PrimitiveTopology::TriangleList,
                        ..Default::default()
                    }),
                    viewport_state: Some(ViewportState::default()),
                    rasterization_state: Some(RasterizationState::default()),
                    multisample_state: Some(MultisampleState::default()),
                    color_blend_state: Some(ColorBlendState::with_attachment_states(
                        subpass.num_color_attachments(),
                        ColorBlendAttachmentState {
                            blend: Some(AttachmentBlend::alpha()),
                            ..Default::default()
                        },
                    )),
                    dynamic_state: [DynamicState::Viewport, DynamicState::Scissor]
                        .into_iter()
                        .collect(),
                    subpass: Some(subpass.into()),
                    ..GraphicsPipelineCreateInfo::layout(layout)
                },
            )?)
        };

        let textured_pipeline = make_pipeline(
            &vs,
            fs_textured::load(device.clone())?
                .entry_point("main")
                .unwrap(),
        )?;
        let border_pipeline = make_pipeline(
            &vs,
            fs_border::load(device.clone())?
                .entry_point("main")
                .unwrap(),
        )?;
        let screenshot_cs = cs_screenshot::load(device.clone())?
            .entry_point("main")
            .unwrap();
        let screenshot_layout = PipelineLayout::new(
            device.clone(),
            PipelineDescriptorSetLayoutCreateInfo::from_stages(&[
                PipelineShaderStageCreateInfo::new(screenshot_cs.clone()),
            ])
            .into_pipeline_layout_create_info(device.clone())
            .unwrap(),
        )?;
        let screenshot_pipeline = ComputePipeline::new(
            device.clone(),
            None,
            ComputePipelineCreateInfo::stage_layout(
                PipelineShaderStageCreateInfo::new(screenshot_cs),
                screenshot_layout,
            ),
        )?;

        let command_buffer_allocator = Arc::new(StandardCommandBufferAllocator::new(
            device.clone(),
            Default::default(),
        ));
        let descriptor_set_allocator = Arc::new(StandardDescriptorSetAllocator::new(
            device.clone(),
            Default::default(),
        ));
        let sampler = Sampler::new(
            device.clone(),
            SamplerCreateInfo {
                mag_filter: Filter::Linear,
                min_filter: Filter::Linear,
                address_mode: [SamplerAddressMode::ClampToEdge; 3],
                ..Default::default()
            },
        )?;

        let in_flight = (0..2)
            .map(|_| {
                let acquire = Arc::new(Semaphore::new(
                    device.clone(),
                    SemaphoreCreateInfo::default(),
                )?);
                let present = Arc::new(Semaphore::new(
                    device.clone(),
                    SemaphoreCreateInfo::default(),
                )?);
                let fence = Arc::new(Fence::new(
                    device.clone(),
                    FenceCreateInfo {
                        flags: FenceCreateFlags::SIGNALED,
                        ..Default::default()
                    },
                )?);
                Ok(InFlight {
                    uploaded: Arc::new(Semaphore::new(
                        device.clone(),
                        SemaphoreCreateInfo::default(),
                    )?),
                    upload_command: None,
                    acquire,
                    present,
                    fence,
                    last_command_buffer: None,
                    frames: vec![],
                    descriptor_sets: vec![],
                    overlays: vec![],
                    overlay_acquire: None,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(Self {
            dc,
            sizer,
            last_committed_sizer: None,
            needs_recreate: false,
            frame_idx: 0,
            in_flight,
            current_frame: None,
            images: Self::build_framebuffers(render_pass.clone(), swapchain_images)?,
            textured_pipeline,
            border_pipeline,
            screenshot_pipeline,
            sampler,
            render_pass,
            swapchain,
            current_overlay: None,
            command_buffer_allocator,
            descriptor_set_allocator,
            allocator,
            queue,
            transfer_queue,
            device,
            _surface: surface,
            start_time: Instant::now(),
            scanout_pacing: false,
        })
    }

    fn build_framebuffers(
        render_pass: Arc<RenderPass>,
        images: Vec<Arc<Image>>,
    ) -> Result<Vec<Arc<Framebuffer>>> {
        images
            .into_iter()
            .map(|image| {
                let view = ImageView::new_default(image)?;
                let fb = Framebuffer::new(
                    render_pass.clone(),
                    FramebufferCreateInfo {
                        attachments: vec![view],
                        ..Default::default()
                    },
                )?;
                Ok(fb)
            })
            .collect()
    }

    fn configure(&mut self, sizer: &Sizer) -> Result<()> {
        let [sw, sh] = self.swapchain.image_extent();
        if self.needs_recreate || (sw, sh) != sizer.render_size {
            // Raw command buffers do not retain the old framebuffers.
            for frame in &self.in_flight {
                frame.fence.wait(None)?;
            }
            self.needs_recreate = false;
            let (r_w, r_h) = sizer.render_size;
            let (swapchain, images) = self.swapchain.recreate(SwapchainCreateInfo {
                image_extent: [r_w, r_h],
                ..self.swapchain.create_info()
            })?;
            self.swapchain = swapchain;
            self.images = Self::build_framebuffers(self.render_pass.clone(), images)?;
        }

        if self.last_committed_sizer.as_ref() != Some(sizer) {
            self.last_committed_sizer = Some(sizer.clone());
            let (w_w, w_h) = sizer.window_size;
            let (r_w, r_h) = sizer.render_size;
            self.dc
                .viewport
                .set_source(0.0, 0.0, r_w as f64, r_h as f64);
            self.dc.viewport.set_destination(w_w as i32, w_h as i32);
            let rect = sizer.window_sizing.content;
            let region = Region::new(&self.dc.compositor_state).context("create region")?;
            region.add(
                rect.x as i32,
                rect.y as i32,
                rect.width as i32,
                rect.height as i32,
            );
            self.dc.surface.set_opaque_region(Some(region.wl_region()));
            self.dc.surface.set_input_region(Some(region.wl_region()));
        }

        Ok(())
    }

    pub fn blank(&mut self) -> Result<()> {
        let sizer = self.sizer.load();
        self.configure(&sizer)?;

        let ifli = self.frame_idx % self.in_flight.len();
        let ifl = &mut self.in_flight[ifli];
        ifl.fence.wait(None)?;
        ifl.descriptor_sets.clear();
        ifl.frames.clear();
        ifl.overlays.clear();
        ifl.overlay_acquire = None;

        let AcquiredImage {
            image_index,
            is_suboptimal,
        } = unsafe {
            self.swapchain.acquire_next_image(&AcquireNextImageInfo {
                semaphore: Some(ifl.acquire.clone()),
                ..Default::default()
            })
        }?;
        unsafe { ifl.fence.reset() }?;

        let mut builder = AutoCommandBufferBuilder::primary(
            self.command_buffer_allocator.clone(),
            self.queue.queue_family_index(),
            CommandBufferUsage::OneTimeSubmit,
        )?;

        builder.clear_color_image(ClearColorImageInfo {
            clear_value: [0.3, 0.3, 0.3, 1.0].into(),
            ..ClearColorImageInfo::image(
                self.images[image_index as usize].attachments()[0]
                    .image()
                    .clone(),
            )
        })?;
        let command_buffer = builder.build()?;
        ifl.last_command_buffer = Some(command_buffer.clone() as Arc<dyn Send + Sync>);

        let submit_info = SubmitInfo {
            wait_semaphores: vec![SemaphoreSubmitInfo {
                stages: PipelineStages::COLOR_ATTACHMENT_OUTPUT,
                ..SemaphoreSubmitInfo::new(ifl.acquire.clone())
            }],
            command_buffers: vec![CommandBufferSubmitInfo::new(command_buffer)],
            signal_semaphores: vec![SemaphoreSubmitInfo::new(ifl.present.clone())],
            ..Default::default()
        };

        let present_suboptimal = self.queue.with(|mut guard| unsafe {
            guard.submit(&[submit_info], Some(&ifl.fence.clone()))?;
            guard
                .present(&PresentInfo {
                    wait_semaphores: vec![SemaphorePresentInfo::new(ifl.present.clone())],
                    swapchain_infos: vec![SwapchainPresentInfo::swapchain_image_index(
                        self.swapchain.clone(),
                        image_index,
                    )],
                    ..Default::default()
                })?
                .next()
                .unwrap()
                .context("present")
        })?;

        self.frame_idx += 1;
        if is_suboptimal || present_suboptimal {
            info!("suboptimal");
            self.needs_recreate = true;
        }
        Ok(())
    }

    pub fn render(&mut self, mut new: Option<CapturedFrame>, overlay: OverlayState) -> Result<()> {
        let has_new_frame = new.is_some();
        let mut info = new.as_mut().and_then(|f| f.info.take());
        if let Some(info) = info.as_mut() {
            info.latency.render_queue = info.obtain.elapsed();
        }
        let old_frame = new.and_then(|f| self.current_frame.replace(Arc::new(f)));
        let Some(current_frame) = self.current_frame.clone() else {
            return self.blank();
        };
        let image = current_frame.image.clone();

        let (has_new_overlay, old_overlay) = match overlay {
            OverlayState::Frame(ol) => (true, self.current_overlay.replace(Arc::new(ol))),
            OverlayState::Pending => (false, None),
            OverlayState::Inactive => (false, self.current_overlay.take()),
        };

        let frame_size = image.extent();
        let frame_size = (frame_size[0], frame_size[1]);
        if self.sizer.load().source_size != frame_size {
            self.sizer.rcu(|s| s.with_source_size(frame_size));
        }

        let sizer = self.sizer.load();
        self.configure(&sizer)?;

        let ifli = self.frame_idx % self.in_flight.len();
        let ifl = &mut self.in_flight[ifli];
        let fence_start = Instant::now();
        ifl.fence.wait(None)?;
        if let Some(info) = info.as_mut() {
            info.latency.fence_wait = fence_start.elapsed();
        }
        ifl.descriptor_sets.clear();
        ifl.frames.clear();
        if let Some(ref old) = old_frame {
            ifl.frames.push(old.clone());
        }
        ifl.frames.push(current_frame.clone());
        ifl.overlays.clear();
        if let Some(ref old) = old_overlay {
            ifl.overlays.push(old.clone());
        }
        if let Some(ref ol) = self.current_overlay {
            ifl.overlays.push(ol.clone());
        }

        let overlay_acquire = if has_new_overlay {
            self.current_overlay
                .as_ref()
                .map(|ol| ol.acquire_semaphore(&self.device))
                .transpose()?
        } else {
            None
        };
        ifl.overlay_acquire = overlay_acquire.clone();

        let acquire_start = Instant::now();
        let AcquiredImage {
            image_index,
            is_suboptimal,
        } = unsafe {
            self.swapchain.acquire_next_image(&AcquireNextImageInfo {
                semaphore: Some(ifl.acquire.clone()),
                ..Default::default()
            })
        }?;
        if let Some(info) = info.as_mut() {
            info.latency.acquire = acquire_start.elapsed();
        }
        unsafe { ifl.fence.reset() }?;

        let fb = self.images[image_index as usize].clone();
        let swapchain_image = self.images[image_index as usize].attachments()[0]
            .image()
            .clone();
        let qfi = self.queue.queue_family_index();
        let content = sizer.render_sizing.content;
        let copy_base = current_frame.host_buffer.is_some()
            && image.extent() == swapchain_image.extent()
            && content.x == 0
            && content.y == 0
            && content.width == image.extent()[0]
            && content.height == image.extent()[1];
        // The 5 ms presentation budget is for the measured SDMA path. Scaling
        // uses the slower sampled path and keeps immediate submission.
        self.scanout_pacing = copy_base && self.transfer_queue.queue_family_index() != qfi;
        let upload = if copy_base {
            let mut upload = RecordingCommandBuffer::new(
                self.command_buffer_allocator.clone(),
                self.transfer_queue.queue_family_index(),
                CommandBufferLevel::Primary,
                CommandBufferBeginInfo {
                    usage: CommandBufferUsage::OneTimeSubmit,
                    ..Default::default()
                },
            )?;
            unsafe {
                upload.pipeline_barrier(&DependencyInfo {
                    memory_barriers: smallvec![vulkano::sync::MemoryBarrier {
                        src_stages: PipelineStages::HOST,
                        src_access: AccessFlags::HOST_WRITE,
                        dst_stages: PipelineStages::ALL_TRANSFER,
                        dst_access: AccessFlags::TRANSFER_READ,
                        ..Default::default()
                    }],
                    ..Default::default()
                })?;
                upload.pipeline_barrier(&color_barrier(
                    swapchain_image.clone(),
                    ImageLayout::Undefined,
                    ImageLayout::TransferDstOptimal,
                    (PipelineStages::TOP_OF_PIPE, AccessFlags::empty()),
                    (PipelineStages::ALL_TRANSFER, AccessFlags::TRANSFER_WRITE),
                    None,
                ))?;
                let host = current_frame.host_buffer.as_ref().unwrap();
                let mut copy = CopyBufferToImageInfo::buffer_image(
                    host.bytes.clone(),
                    swapchain_image.clone(),
                );
                copy.regions[0].buffer_row_length = host.row_length;
                upload.copy_buffer_to_image(&copy)?;
                upload.pipeline_barrier(&color_barrier(
                    swapchain_image.clone(),
                    ImageLayout::TransferDstOptimal,
                    ImageLayout::ColorAttachmentOptimal,
                    (PipelineStages::ALL_TRANSFER, AccessFlags::TRANSFER_WRITE),
                    (PipelineStages::BOTTOM_OF_PIPE, AccessFlags::empty()),
                    None,
                ))?;
            }
            Some(Arc::new(unsafe { upload.end() }?))
        } else {
            None
        };
        let mut cmd = RecordingCommandBuffer::new(
            self.command_buffer_allocator.clone(),
            qfi,
            CommandBufferLevel::Primary,
            CommandBufferBeginInfo {
                usage: CommandBufferUsage::OneTimeSubmit,
                ..Default::default()
            },
        )?;
        unsafe {
            // Release old capture image to external (if replaced)
            if let Some(ref old) = old_frame
                && !old
                    .image
                    .external_memory_handle_types()
                    .contains(vulkano::memory::ExternalMemoryHandleTypes::HOST_ALLOCATION)
            {
                cmd.pipeline_barrier(&release_to_external(old.image.clone(), qfi))?;
            }
            // Release old overlay image to external (if replaced)
            if let Some(ref old) = old_overlay {
                cmd.pipeline_barrier(&release_to_external(old.image.clone(), qfi))?;
            }
            // Acquire new capture image from external
            if has_new_frame {
                let host_image = image
                    .external_memory_handle_types()
                    .contains(vulkano::memory::ExternalMemoryHandleTypes::HOST_ALLOCATION);
                let barrier = if host_image {
                    // CUDA has completed the D2H write; no foreign Vulkan queue owns
                    // these pages. Slots stay in General for host writes and sampling.
                    color_barrier(
                        image.clone(),
                        ImageLayout::General,
                        ImageLayout::General,
                        (PipelineStages::HOST, AccessFlags::HOST_WRITE),
                        (
                            PipelineStages::FRAGMENT_SHADER | PipelineStages::COMPUTE_SHADER,
                            AccessFlags::SHADER_SAMPLED_READ,
                        ),
                        None,
                    )
                } else {
                    acquire_from_external(image.clone(), qfi)
                };
                cmd.pipeline_barrier(&barrier)?;
            }
            // Acquire new overlay image from external
            if has_new_overlay && let Some(ref overlay) = self.current_overlay {
                cmd.pipeline_barrier(&acquire_from_external(overlay.image.clone(), qfi))?;
            }
            if !copy_base {
                cmd.pipeline_barrier(&color_barrier(
                    swapchain_image.clone(),
                    ImageLayout::Undefined,
                    ImageLayout::TransferDstOptimal,
                    (PipelineStages::TOP_OF_PIPE, AccessFlags::empty()),
                    (PipelineStages::ALL_TRANSFER, AccessFlags::TRANSFER_WRITE),
                    None,
                ))?;
                cmd.clear_color_image(&ClearColorImageInfo {
                    image_layout: ImageLayout::TransferDstOptimal,
                    clear_value: [0.0, 0.0, 0.0, 1.0].into(),
                    ..ClearColorImageInfo::image(swapchain_image.clone())
                })?;
                cmd.pipeline_barrier(&color_barrier(
                    swapchain_image.clone(),
                    ImageLayout::TransferDstOptimal,
                    ImageLayout::ColorAttachmentOptimal,
                    (PipelineStages::ALL_TRANSFER, AccessFlags::TRANSFER_WRITE),
                    (
                        PipelineStages::COLOR_ATTACHMENT_OUTPUT,
                        AccessFlags::COLOR_ATTACHMENT_READ | AccessFlags::COLOR_ATTACHMENT_WRITE,
                    ),
                    None,
                ))?;
            }
            cmd.begin_render_pass(
                &RenderPassBeginInfo {
                    clear_values: vec![None],
                    ..RenderPassBeginInfo::framebuffer(fb.clone())
                },
                &SubpassBeginInfo::default(),
            )?;
            cmd.set_scissor(
                0,
                &[Scissor {
                    offset: [0, 0],
                    extent: self.swapchain.image_extent(),
                }],
            )?;
            cmd.bind_pipeline_graphics(&self.textured_pipeline)?;
            let content = sizer.render_sizing.content;
            cmd.set_viewport(
                0,
                &[Viewport {
                    offset: [content.x as f32, content.y as f32],
                    extent: [content.width as f32, content.height as f32],
                    depth_range: 0.0..=1.0,
                }],
            )?;

            if !copy_base {
                // Raw command recording does not retain descriptors or their images.
                // Keep each set with this submission until its fence completes.
                let base_set = textured_set(
                    &self.descriptor_set_allocator,
                    &self.textured_pipeline,
                    &self.sampler,
                    image.clone(),
                )?;
                cmd.bind_descriptor_sets(
                    PipelineBindPoint::Graphics,
                    self.textured_pipeline.layout(),
                    0,
                    &[base_set.as_raw()],
                    &[],
                )?;
                ifl.descriptor_sets.push(base_set);
                cmd.push_constants(
                    self.textured_pipeline.layout(),
                    0,
                    &TexturedPushConstants { opaque: 1 },
                )?;
                cmd.draw(3, 1, 0, 0)?;
            }
            // Draw 2: Overlay (alpha-blended)
            if let Some(ref overlay) = self.current_overlay {
                cmd.bind_pipeline_graphics(&self.textured_pipeline)?;
                let overlay_set = textured_set(
                    &self.descriptor_set_allocator,
                    &self.textured_pipeline,
                    &self.sampler,
                    overlay.image.clone(),
                )?;
                cmd.bind_descriptor_sets(
                    PipelineBindPoint::Graphics,
                    self.textured_pipeline.layout(),
                    0,
                    &[overlay_set.as_raw()],
                    &[],
                )?;
                ifl.descriptor_sets.push(overlay_set);
                cmd.push_constants(
                    self.textured_pipeline.layout(),
                    0,
                    &TexturedPushConstants { opaque: 0 },
                )?;
                cmd.draw(3, 1, 0, 0)?;
            }

            // Draw 3: Border (procedural)
            if !self.dc.global_state.load().confine {
                cmd.bind_pipeline_graphics(&self.border_pipeline)?;
                cmd.push_constants(
                    self.border_pipeline.layout(),
                    0,
                    &BorderPushConstants {
                        time: self.start_time.elapsed().as_secs_f32(),
                        content_width: content.width as f32,
                        content_height: content.height as f32,
                    },
                )?;
                // The shader's maximum width is 25 pixels. Four disjoint strips
                // retain its breathing animation without shading the interior.
                let x = content.x;
                let y = content.y;
                let w = content.width;
                let h = content.height;
                let top = 25.min(h);
                let bottom = 25.min(h - top);
                let sides = h - top - bottom;
                let left = 25.min(w);
                let right = 25.min(w - left);
                for (offset, extent) in [
                    ([x, y], [w, top]),
                    ([x, y + h - bottom], [w, bottom]),
                    ([x, y + top], [left, sides]),
                    ([x + w - right, y + top], [right, sides]),
                ] {
                    cmd.set_scissor(0, &[Scissor { offset, extent }])?;
                    cmd.draw(3, 1, 0, 0)?;
                }
            }

            cmd.end_render_pass(&Default::default())?;

            // Transition swapchain back to present
            cmd.pipeline_barrier(&color_barrier(
                swapchain_image,
                ImageLayout::ColorAttachmentOptimal,
                ImageLayout::PresentSrc,
                (
                    PipelineStages::COLOR_ATTACHMENT_OUTPUT,
                    AccessFlags::COLOR_ATTACHMENT_WRITE,
                ),
                (PipelineStages::BOTTOM_OF_PIPE, AccessFlags::empty()),
                None,
            ))?;
        }
        let command_buffer = Arc::new(unsafe { cmd.end() }?);

        let command_buffer_handle = vec![command_buffer.handle()];

        ifl.last_command_buffer = Some(command_buffer as Arc<dyn Send + Sync>);

        let mut wait_semaphores = vec![if copy_base {
            ifl.uploaded.handle()
        } else {
            ifl.acquire.handle()
        }];
        let mut wait_stages = vec![ash::vk::PipelineStageFlags::TOP_OF_PIPE];
        if let Some(ref sem) = overlay_acquire {
            wait_semaphores.push(sem.handle());
            wait_stages.push(ash::vk::PipelineStageFlags::FRAGMENT_SHADER);
        }
        let present_semaphore = [ifl.present.handle()];
        let submit_info = ash::vk::SubmitInfo::default()
            .command_buffers(&command_buffer_handle)
            .wait_semaphores(&wait_semaphores)
            .wait_dst_stage_mask(&wait_stages)
            .signal_semaphores(&present_semaphore);

        let feedback = info.map(|mut info| {
            info.mark_commit();
            self.dc.request_feedback(info)
        });
        let submit_start = Instant::now();
        if let Some(upload) = upload {
            let cmds = [upload.handle()];
            let wait = [ifl.acquire.handle()];
            let signal = [ifl.uploaded.handle()];
            let stages = [ash::vk::PipelineStageFlags::TOP_OF_PIPE];
            let submit = ash::vk::SubmitInfo::default()
                .command_buffers(&cmds)
                .wait_semaphores(&wait)
                .wait_dst_stage_mask(&stages)
                .signal_semaphores(&signal);
            self.transfer_queue.with(|_| unsafe {
                (self.device.fns().v1_0.queue_submit)(
                    self.transfer_queue.handle(),
                    1,
                    &submit,
                    ash::vk::Fence::null(),
                )
                .result()
            })?;
            ifl.upload_command = Some(upload);
        }
        let present_suboptimal = self.queue.with(|mut guard| unsafe {
            (self.device.fns().v1_0.queue_submit)(
                self.queue.handle(),
                1,
                &submit_info as *const _,
                ifl.fence.handle(),
            )
            .result()?;

            let submit_elapsed = submit_start.elapsed();
            self.dc.mon.submit();
            let present_start = Instant::now();
            let result = guard
                .present(&PresentInfo {
                    wait_semaphores: vec![SemaphorePresentInfo::new(ifl.present.clone())],
                    swapchain_infos: vec![SwapchainPresentInfo::swapchain_image_index(
                        self.swapchain.clone(),
                        image_index,
                    )],
                    ..Default::default()
                })?
                .next()
                .unwrap()
                .context("present");
            let present_elapsed = present_start.elapsed();
            let returned = crate::utils::clock_monotonic_ns();
            if let Some(id) = feedback {
                self.dc
                    .finish_present(id, submit_elapsed, present_elapsed, returned);
            }
            result
        })?;

        if has_new_overlay && let Some(ref overlay) = self.current_overlay {
            overlay.presented();
        }

        self.frame_idx += 1;
        if is_suboptimal || present_suboptimal {
            info!("suboptimal");
            self.needs_recreate = true;
        }
        Ok(())
    }

    pub fn screenshot(&mut self) -> Result<ScreenshotData> {
        let Some(ref current_frame) = self.current_frame else {
            bail!("no current frame");
        };
        let image = current_frame.image.clone();
        let extent = image.extent();
        let (width, height) = (extent[0], extent[1]);
        let stride = width * 4;

        // Wait for all in-flight GPU work so the image is idle
        for ifl in &self.in_flight {
            ifl.fence.wait(None)?;
        }

        let staging = Buffer::new_slice::<u32>(
            self.allocator.clone(),
            BufferCreateInfo {
                usage: BufferUsage::STORAGE_BUFFER,
                ..Default::default()
            },
            AllocationCreateInfo {
                memory_type_filter: MemoryTypeFilter::PREFER_HOST
                    | MemoryTypeFilter::HOST_RANDOM_ACCESS,
                ..Default::default()
            },
            (width as u64) * (height as u64),
        )?;

        let descriptor_set = DescriptorSet::new(
            self.descriptor_set_allocator.clone(),
            self.screenshot_pipeline.layout().set_layouts()[0].clone(),
            [
                WriteDescriptorSet::sampler(0, self.sampler.clone()),
                sampled_image(image)?,
                WriteDescriptorSet::buffer(2, staging.clone()),
            ],
            [],
        )?;

        let qfi = self.queue.queue_family_index();
        let mut cmd = RecordingCommandBuffer::new(
            self.command_buffer_allocator.clone(),
            qfi,
            CommandBufferLevel::Primary,
            CommandBufferBeginInfo {
                usage: CommandBufferUsage::OneTimeSubmit,
                ..Default::default()
            },
        )?;
        unsafe {
            cmd.bind_pipeline_compute(&self.screenshot_pipeline)?;
            cmd.bind_descriptor_sets(
                PipelineBindPoint::Compute,
                self.screenshot_pipeline.layout(),
                0,
                &[descriptor_set.as_raw()],
                &[],
            )?;
            cmd.push_constants(
                self.screenshot_pipeline.layout(),
                0,
                &ScreenshotPushConstants { width, height },
            )?;
            cmd.dispatch([width.div_ceil(16), height.div_ceil(16), 1])?;
        }
        let command_buffer = Arc::new(unsafe { cmd.end() }?);

        let fence = Fence::new(self.device.clone(), FenceCreateInfo::default())?;
        let cb_handle = vec![command_buffer.handle()];
        let submit_info = ash::vk::SubmitInfo::default().command_buffers(&cb_handle);
        self.queue.with(|_guard| unsafe {
            (self.device.fns().v1_0.queue_submit)(
                self.queue.handle(),
                1,
                &submit_info as *const _,
                fence.handle(),
            )
            .result()
            .context("screenshot queue_submit")
        })?;
        fence.wait(None)?;
        drop(command_buffer);
        let data = bytemuck::cast_slice(&staging.read()?).to_vec();

        Ok(ScreenshotData {
            width,
            height,
            stride,
            data,
        })
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        // Raw queue submissions do not carry Vulkano's automatic lifetime tracking.
        // Complete all reads before the capture slots and overlay releases drop.
        unsafe { self.device.wait_idle() }.unwrap();
        for frame in &mut self.in_flight {
            frame.last_command_buffer = None;
            frame.upload_command = None;
            frame.descriptor_sets.clear();
        }
    }
}

fn sampled_image(image: Arc<Image>) -> Result<WriteDescriptorSet> {
    let image_layout = if image
        .external_memory_handle_types()
        .contains(vulkano::memory::ExternalMemoryHandleTypes::HOST_ALLOCATION)
    {
        ImageLayout::General
    } else {
        ImageLayout::ShaderReadOnlyOptimal
    };
    Ok(WriteDescriptorSet::image_view_with_layout(
        1,
        DescriptorImageViewInfo {
            image_view: ImageView::new_default(image)?,
            image_layout,
        },
    ))
}

/// Sampler + texture set for the textured pipeline.
fn textured_set(
    allocator: &Arc<StandardDescriptorSetAllocator>,
    pipeline: &GraphicsPipeline,
    sampler: &Arc<Sampler>,
    image: Arc<Image>,
) -> Result<Arc<DescriptorSet>> {
    Ok(DescriptorSet::new(
        allocator.clone(),
        pipeline.layout().set_layouts()[0].clone(),
        [
            WriteDescriptorSet::sampler(0, sampler.clone()),
            sampled_image(image)?,
        ],
        [],
    )?)
}

/// Single-image barrier over the color aspect, mip 0, layer 0.
fn color_barrier(
    image: Arc<Image>,
    old_layout: ImageLayout,
    new_layout: ImageLayout,
    (src_stages, src_access): (PipelineStages, AccessFlags),
    (dst_stages, dst_access): (PipelineStages, AccessFlags),
    queue_family_ownership_transfer: Option<QueueFamilyOwnershipTransfer>,
) -> DependencyInfo {
    DependencyInfo {
        image_memory_barriers: smallvec![ImageMemoryBarrier {
            old_layout,
            new_layout,
            src_stages,
            src_access,
            dst_stages,
            dst_access,
            queue_family_ownership_transfer,
            subresource_range: ImageSubresourceRange {
                aspects: ImageAspects::COLOR,
                mip_levels: 0..1,
                array_layers: 0..1,
            },
            ..ImageMemoryBarrier::image(image)
        }],
        ..Default::default()
    }
}

/// Take an imported DMA-BUF image from the external queue family for sampling.
fn acquire_from_external(image: Arc<Image>, qfi: u32) -> DependencyInfo {
    color_barrier(
        image,
        ImageLayout::General,
        ImageLayout::ShaderReadOnlyOptimal,
        (PipelineStages::TOP_OF_PIPE, AccessFlags::empty()),
        (
            PipelineStages::FRAGMENT_SHADER,
            AccessFlags::SHADER_SAMPLED_READ,
        ),
        Some(QueueFamilyOwnershipTransfer::ExclusiveFromExternal { dst_index: qfi }),
    )
}

/// Hand a sampled image back to the external queue family so its producer may reuse it.
fn release_to_external(image: Arc<Image>, qfi: u32) -> DependencyInfo {
    color_barrier(
        image,
        ImageLayout::ShaderReadOnlyOptimal,
        ImageLayout::General,
        (
            PipelineStages::FRAGMENT_SHADER,
            AccessFlags::SHADER_SAMPLED_READ,
        ),
        (PipelineStages::BOTTOM_OF_PIPE, AccessFlags::empty()),
        Some(QueueFamilyOwnershipTransfer::ExclusiveToExternal { src_index: qfi }),
    )
}

pub struct ScreenshotData {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub data: Vec<u8>,
}

pub enum RenderMsg {
    Configured,
    Frame,
    CaptureReady,
    // Monotonic presentation timestamp and refresh period, for direct scanout.
    PresentationDone(Option<(u64, u32)>),
    Redraw,
    Quit,
    Screenshot(UnixStream),
}

pub fn spawn(
    dc: DisplayCtx,
    renderer: Renderer,
    backend: Box<dyn CaptureBackend>,
    overlay_handle: OverlayHandle,
    render_rx: mpsc::Receiver<RenderMsg>,
) -> Result<std::thread::JoinHandle<()>> {
    let mon = dc.mon.clone();
    let handle = std::thread::Builder::new()
        .name("render".into())
        .spawn(move || {
            mon.fatal(
                render_loop(dc, renderer, backend, overlay_handle, render_rx)
                    .context("render thread"),
            );
        })?;
    Ok(handle)
}

fn render_loop(
    dc: DisplayCtx,
    mut renderer: Renderer,
    mut backend: Box<dyn CaptureBackend>,
    overlay_handle: OverlayHandle,
    render_rx: mpsc::Receiver<RenderMsg>,
) -> Result<()> {
    let frame_driven = backend.frame_driven();
    let mut configured = false;
    let mut next_draw_ns = 0u64;
    let mut last_draw = Instant::now();
    let mut idle_start = Instant::now();
    // Keep controls, resize and overlays responsive with a static source or no
    // presentation feedback (for example while the window is hidden).
    let maintenance = Duration::from_millis(50);
    loop {
        let msg = if frame_driven && configured {
            let mut timeout = maintenance.saturating_sub(last_draw.elapsed());
            if next_draw_ns != 0 {
                timeout = timeout.min(Duration::from_nanos(
                    next_draw_ns.saturating_sub(crate::utils::clock_monotonic_ns()),
                ));
            }
            match render_rx.recv_timeout(timeout) {
                Ok(msg) => msg,
                Err(mpsc::RecvTimeoutError::Timeout) => RenderMsg::Redraw,
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            }
        } else {
            match render_rx.recv() {
                Ok(msg) => msg,
                Err(mpsc::RecvError) => return Ok(()),
            }
        };
        if let RenderMsg::PresentationDone(timing) = &msg {
            // Leave 5 ms for SDMA, graphics, WSI and compositor scheduling.
            // Until then the capture thread keeps replacing its pending slot.
            // Measured GPU work is about 1.3 ms on the target RADV iGPU.
            const RENDER_AHEAD_NS: u64 = 5_000_000;
            next_draw_ns = match timing {
                Some((presented, refresh))
                    if renderer.scanout_pacing && u64::from(*refresh) > RENDER_AHEAD_NS =>
                {
                    presented + u64::from(*refresh) - RENDER_AHEAD_NS
                }
                _ => 0,
            };
        }
        match msg {
            RenderMsg::Quit => return Ok(()),
            RenderMsg::Frame if frame_driven => continue,
            RenderMsg::PresentationDone(..) if !frame_driven => continue,
            RenderMsg::CaptureReady | RenderMsg::Redraw if !configured => continue,
            RenderMsg::Configured
            | RenderMsg::Frame
            | RenderMsg::PresentationDone(..)
            | RenderMsg::CaptureReady
            | RenderMsg::Redraw => {
                configured = true;
                let capturing = dc.global_state.load().capture;
                // Do not queue another source frame behind the next page flip.
                // Expire the gate after maintenance so missing feedback cannot
                // freeze controls or prevent the window from recovering.
                if frame_driven
                    && capturing
                    && dc.has_pending_presentation()
                    && last_draw.elapsed() < maintenance
                {
                    next_draw_ns = 0;
                    continue;
                }
                if frame_driven && capturing && next_draw_ns > crate::utils::clock_monotonic_ns() {
                    continue;
                }
                next_draw_ns = 0;
                // Drain a pending NVFBC image even while disabled so its slot can
                // notify us again after capture resumes.
                let capture = if capturing || frame_driven {
                    backend.capture()?
                } else {
                    None
                };
                if frame_driven
                    && capturing
                    && capture.is_none()
                    && !matches!(msg, RenderMsg::Configured)
                    && last_draw.elapsed() < maintenance
                {
                    // A deadline is permission to take a frame, not a reason to
                    // resubmit the previous one. If capture is a little late,
                    // wait for its notification instead of queuing a duplicate.
                    continue;
                }
                dc.mon.idle(idle_start.elapsed());
                dc.request_frame();
                if !capturing {
                    renderer.blank()?;
                } else {
                    if capture.is_none() && !frame_driven {
                        dc.mon.skip();
                    }
                    let overlay = overlay_handle.take();
                    renderer.render(capture, overlay)?;
                }
                last_draw = Instant::now();
                idle_start = last_draw;
            }
            RenderMsg::Screenshot(stream) => {
                stream.set_write_timeout(Some(Duration::from_secs(5))).ok();
                let mut stream = std::io::BufWriter::new(stream);
                info!("grabbing screenshot");
                match renderer.screenshot().context("renderer screenshot") {
                    Ok(snap) => {
                        if let Err(e) = stream
                            .write_all(&snap.width.to_le_bytes())
                            .and_then(|_| stream.write_all(&snap.height.to_le_bytes()))
                            .and_then(|_| stream.write_all(&snap.stride.to_le_bytes()))
                            .and_then(|_| stream.write_all(&snap.data))
                            .and_then(|_| stream.flush())
                        {
                            warn!("screenshot write failed: {e:?}");
                        }
                    }
                    Err(e) => warn!("screenshot failed: {e:?}"),
                }
            }
        }
    }
}
