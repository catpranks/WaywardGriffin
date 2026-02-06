pub mod input;
pub mod oneshot;
pub mod plotter;
pub mod source;
pub mod vulkan;

use crate::GlobalState;
use crate::capture::input::InputInjector;
use crate::capture::source::{BackendType, CaptureBackend, CaptureBackendBuilder};
use crate::capture::plotter::{FrameInfo, PlotterHandle};
use crate::sizer::{SharedSizer, Sizer};
use anyhow::{Context as _, Result};
use clap::Args;
use smallvec::smallvec;
use smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface;
use smithay_client_toolkit::reexports::client::{Connection, Proxy as _};
use std::num::NonZero;
use std::sync::{Arc, mpsc};
use std::time::Instant;
use tracing::info;
use vulkano::command_buffer::allocator::StandardCommandBufferAllocator;
use vulkano::command_buffer::{
    AutoCommandBufferBuilder, ClearColorImageInfo, CommandBufferBeginInfo, CommandBufferLevel,
    CommandBufferSubmitInfo, CommandBufferUsage, RecordingCommandBuffer, RenderPassBeginInfo,
    SemaphoreSubmitInfo, SubmitInfo, SubpassBeginInfo,
};
use vulkano::descriptor_set::allocator::StandardDescriptorSetAllocator;
use vulkano::descriptor_set::{DescriptorSet, WriteDescriptorSet};
use vulkano::device::{
    Device, DeviceCreateInfo, DeviceExtensions, DeviceFeatures, Queue, QueueCreateInfo, QueueFlags,
};
use vulkano::format::Format;
use vulkano::image::sampler::{Filter, Sampler, SamplerAddressMode, SamplerCreateInfo};
use vulkano::image::view::ImageView;
use vulkano::image::{ImageAspects, ImageLayout, ImageSubresourceRange, ImageUsage};
use vulkano::memory::allocator::StandardMemoryAllocator;
use vulkano::pipeline::graphics::GraphicsPipelineCreateInfo;
use vulkano::pipeline::graphics::color_blend::{
    AttachmentBlend, ColorBlendAttachmentState, ColorBlendState,
};
use vulkano::pipeline::graphics::input_assembly::{InputAssemblyState, PrimitiveTopology};
use vulkano::pipeline::graphics::multisample::MultisampleState;
use vulkano::pipeline::graphics::rasterization::RasterizationState;
use vulkano::pipeline::graphics::vertex_input::VertexInputState;
use vulkano::pipeline::graphics::viewport::{Viewport, ViewportState};
use vulkano::pipeline::layout::PipelineDescriptorSetLayoutCreateInfo;
use vulkano::pipeline::{
    DynamicState, GraphicsPipeline, Pipeline, PipelineBindPoint, PipelineLayout,
    PipelineShaderStageCreateInfo,
};
use vulkano::render_pass::{Framebuffer, FramebufferCreateInfo, RenderPass, Subpass};
use vulkano::swapchain::{
    AcquireNextImageInfo, AcquiredImage, ColorSpace, CompositeAlpha, PresentInfo, PresentMode,
    SemaphorePresentInfo, Surface, Swapchain, SwapchainCreateInfo, SwapchainPresentInfo,
};
use vulkano::sync::fence::{Fence, FenceCreateFlags, FenceCreateInfo};
use vulkano::sync::semaphore::{Semaphore, SemaphoreCreateInfo};
use vulkano::sync::{
    AccessFlags, DependencyInfo, ImageMemoryBarrier, PipelineStages, QueueFamilyOwnershipTransfer,
};
use vulkano::{VulkanObject as _, single_pass_renderpass};

#[derive(Debug, Clone, Args)]
pub struct CaptureOpts {
    /// The display to capture
    #[arg(long)]
    pub display: String,

    /// Capture backend to use
    #[arg(long, value_enum)]
    pub backend: BackendType,
}

#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct BorderPushConstants {
    time: f32,
    show_border: u32,
    content_width: f32,
    content_height: f32,
}

mod vs {
    vulkano_shaders::shader! {
        ty: "vertex",
        src: r"
            #version 450

            layout(location = 0) out vec2 tex_coords;

            void main() {
                // A fullscreen triangle without buffers.
                // The coordinates are hard-coded to cover the entire screen.
                // Mapping gl_VertexIndex to triangle coordinates
                float x = float((gl_VertexIndex & 1) << 2) - 1.0;
                float y = float((gl_VertexIndex & 2) << 1) - 1.0;

                gl_Position = vec4(x, y, 0.0, 1.0);
                tex_coords = gl_Position.xy * 0.5 + 0.5;
            }
        ",
    }
}

mod fs {
    vulkano_shaders::shader! {
        ty: "fragment",
        src: r"
            #version 450

            layout(location = 0) in vec2 tex_coords;
            layout(location = 0) out vec4 f_color;

            layout(set = 0, binding = 0) uniform sampler s;
            layout(set = 0, binding = 1) uniform texture2D tex;

            layout(push_constant) uniform PushConstants {
                float time;
                uint show_border;
                float content_width;
                float content_height;
            } pc;

            void main() {
                f_color = texture(sampler2D(tex, s), tex_coords);
                f_color.a = 1.0;

                if (pc.show_border != 0u) {
                    float sine_val = (sin(pc.time * 2.0) + 1.0) / 2.0;
                    float border_width_px = 15.0 + sine_val * 10.0;
                    float alpha = (5.0 + sine_val * 150.0) / 255.0;

                    // Convert to pixel coords
                    float px = tex_coords.x * pc.content_width;
                    float py = tex_coords.y * pc.content_height;

                    float edge_dist = min(min(px, pc.content_width - px),
                                          min(py, pc.content_height - py));

                    if (edge_dist < border_width_px) {
                        vec3 teal = vec3(0.0, 0.78, 0.78);
                        f_color.rgb = mix(f_color.rgb, teal, alpha);
                    }
                }
            }
        ",
    }
}

struct InFlight {
    acquire: Arc<Semaphore>,
    present: Arc<Semaphore>,
    fence: Arc<Fence>,
    last_command_buffer: Option<Arc<dyn Send + Sync>>,
}

pub enum CaptureCommand {
    Resize(Sizer),
    Frame(Sizer),
    ForceRender(Sizer),
}

#[derive(Clone)]
pub struct CaptureHandle {
    cmd: mpsc::Sender<CaptureCommand>,
}

impl CaptureHandle {
    pub fn resize(&self, sizer: &Sizer) {
        let _ = self.cmd.send(CaptureCommand::Resize(sizer.clone()));
    }

    pub fn frame(&self, sizer: &Sizer) {
        let _ = self.cmd.send(CaptureCommand::Frame(sizer.clone()));
    }

    pub fn force_render(&self, sizer: &Sizer) {
        let _ = self.cmd.send(CaptureCommand::ForceRender(sizer.clone()));
    }
}

struct PresentWaitRequest {
    swapchain: Arc<Swapchain>,
    present_id: NonZero<u64>,
    info: FrameInfo,
}

pub struct Capture {
    pub handle: CaptureHandle,
    rx_cmd: mpsc::Receiver<CaptureCommand>,
    ph: PlotterHandle,
    global_state: GlobalState,

    sizer: SharedSizer,
    wl_surface: WlSurface,

    frame_idx: usize,
    present_counter: u64,
    present_wait_tx: mpsc::Sender<PresentWaitRequest>,

    backend: Box<dyn CaptureBackend>,

    in_flight: Vec<InFlight>,
    images: Vec<Arc<Framebuffer>>,
    pipeline: Arc<GraphicsPipeline>,
    sampler: Arc<Sampler>,
    render_pass: Arc<RenderPass>,
    swapchain: Arc<Swapchain>,

    command_buffer_allocator: Arc<StandardCommandBufferAllocator>,
    descriptor_set_allocator: Arc<StandardDescriptorSetAllocator>,

    queue: Arc<Queue>,
    device: Arc<Device>,
    _surface: Arc<Surface>,
    start_time: Instant,
}

impl Capture {
    pub fn new(
        ph: PlotterHandle,
        global_state: GlobalState,
        backend_builder: Box<dyn CaptureBackendBuilder>,
        conn: &Connection,
        wl_surface: &WlSurface,
        sizer: SharedSizer,
        opts: &CaptureOpts,
    ) -> Result<(Self, Box<dyn InputInjector>)> {
        let (instance, physical_device) =
            vulkan::create_instance_and_select_device(backend_builder.as_ref())?;

        let surface = unsafe {
            Surface::from_wayland(
                instance.clone(),
                conn.backend().display_ptr() as _,
                wl_surface.id().as_ptr() as _,
                None,
            )?
        };

        let device_extensions = DeviceExtensions {
            khr_swapchain: true,
            khr_external_memory: true,
            khr_external_memory_fd: true,
            ext_external_memory_dma_buf: true,
            khr_external_semaphore_fd: true,
            khr_timeline_semaphore: true,
            khr_present_id: true,
            khr_present_wait: true,
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
        // eprintln!(
        //     "Using device: {} (type: {:?})",
        //     physical_device.properties().device_name,
        //     physical_device.properties().device_type
        // );

        let (device, mut queues) = Device::new(
            physical_device.clone(),
            DeviceCreateInfo {
                enabled_extensions: device_extensions,
                queue_create_infos: vec![QueueCreateInfo {
                    queue_family_index,
                    ..Default::default()
                }],
                enabled_features: DeviceFeatures {
                    timeline_semaphore: true,
                    present_id: true,
                    present_wait: true,
                    ..Default::default()
                },
                ..Default::default()
            },
        )?;

        let queue = queues.next().unwrap();

        let (swapchain, _images) = {
            let surface_capabilities =
                physical_device.surface_capabilities(&surface, Default::default())?;
            // let (image_format, _) = physical_device
            //     .surface_formats(&surface, Default::default())?[0];

            Swapchain::new(
                device.clone(),
                surface.clone(),
                SwapchainCreateInfo {
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
                    load_op: Clear,
                    store_op: Store,
                },
            },
            pass: {
                color: [color],
                depth_stencil: {},
            },
        )?;
        let vs = vs::load(device.clone())?.entry_point("main").unwrap();
        let fs = fs::load(device.clone())?.entry_point("main").unwrap();
        let stages = [
            PipelineShaderStageCreateInfo::new(vs),
            PipelineShaderStageCreateInfo::new(fs),
        ];
        let layout = PipelineLayout::new(
            device.clone(),
            PipelineDescriptorSetLayoutCreateInfo::from_stages(&stages)
                .into_pipeline_layout_create_info(device.clone())
                .unwrap(),
        )
        .unwrap();
        let subpass = Subpass::from(render_pass.clone(), 0).unwrap();

        let pipeline = GraphicsPipeline::new(
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
                dynamic_state: [DynamicState::Viewport].into_iter().collect(),
                subpass: Some(subpass.into()),
                ..GraphicsPipelineCreateInfo::layout(layout)
            },
        )?;
        let allocator = Arc::new(StandardMemoryAllocator::new_default(device.clone()));
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

        let (backend, injector) =
            backend_builder.build(device.clone(), allocator.clone(), ph.clone(), &opts.display)?;

        let in_flight = (0..3)
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
                    acquire,
                    present,
                    fence,
                    last_command_buffer: None,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let (tx_cmd, rx_cmd) = mpsc::channel();
        let handle = CaptureHandle { cmd: tx_cmd };

        let (present_wait_tx, present_wait_rx) = mpsc::channel::<PresentWaitRequest>();
        let wait_ph = ph.clone();
        std::thread::spawn(move || {
            for req in present_wait_rx {
                match req.swapchain.wait_for_present(req.present_id, None) {
                    Ok(_) => {
                        let mut info = req.info;
                        info.mark_present();
                        wait_ph.render(info);
                    }
                    Err(e) => {
                        wait_ph.log(format!("wait_for_present error: {e:?}"));
                    }
                }
            }
        });

        let this = Self {
            handle,
            rx_cmd,
            ph,
            global_state,
            sizer,
            wl_surface: wl_surface.clone(),
            frame_idx: 0,
            present_counter: 0,
            present_wait_tx,
            backend,
            in_flight,
            images: vec![],
            pipeline,
            sampler,
            render_pass,
            swapchain,
            command_buffer_allocator,
            descriptor_set_allocator,
            queue,
            device,
            _surface: surface,
            start_time: Instant::now(),
        };
        Ok((this, injector))
    }

    pub fn run(&mut self) {
        let ph = self.ph.clone();
        ph.fatal(self.run_internal().context("capture thread"));
    }

    fn run_internal(&mut self) -> Result<()> {
        while let Ok(cmd) = self.rx_cmd.recv() {
            match cmd {
                CaptureCommand::Resize(sizer) => self.resize(&sizer)?,
                CaptureCommand::Frame(sizer) => self.render(&sizer)?,
                CaptureCommand::ForceRender(sizer) => self.force_render(&sizer)?,
            }
        }
        Ok(())
    }

    pub fn resize(&mut self, sizer: &Sizer) -> Result<()> {
        let (r_w, r_h) = sizer.render_size;
        let (swapchain, images) = self.swapchain.recreate(SwapchainCreateInfo {
            image_extent: [r_w, r_h],
            ..self.swapchain.create_info()
        })?;

        self.swapchain = swapchain;
        self.images = images
            .into_iter()
            .map(|image| {
                let view = ImageView::new_default(image)?;
                let fb = Framebuffer::new(
                    self.render_pass.clone(),
                    FramebufferCreateInfo {
                        attachments: vec![view],
                        ..Default::default()
                    },
                )?;
                Ok(fb)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(())
    }

    pub fn force_render(&mut self, sizer: &Sizer) -> Result<()> {
        let ifli = self.frame_idx % self.in_flight.len();
        let ifl = &mut self.in_flight[ifli];
        ifl.fence.wait(None)?;

        let AcquiredImage {
            image_index,
            is_suboptimal,
        } = unsafe {
            self.swapchain.acquire_next_image(&AcquireNextImageInfo {
                semaphore: Some(ifl.acquire.clone()),
                ..Default::default()
            })
        }?;
        if is_suboptimal {
            info!("suboptimal, resizing");
            self.resize(sizer)?;
            self.wl_surface.commit();
            return Ok(());
        }

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

        let _pres = self.queue.with(|mut guard| unsafe {
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
        Ok(())
    }

    pub fn render(&mut self, sizer: &Sizer) -> Result<()> {
        if !self.global_state.load().capture {
            return self.force_render(sizer);
        }
        let Some(frame) = self.backend.capture()? else {
            self.ph.skip();
            self.wl_surface.commit();
            return Ok(());
        };

        // Update sizer/global_state from frame info
        let frame_size = frame.image.extent();
        let frame_size = (frame_size[0], frame_size[1]);
        if self.sizer.load().source_size != frame_size {
            self.sizer.rcu(|s| s.with_source_size(frame_size));
        }
        if self.global_state.load().cursor_visible != frame.info.cursor_visible {
            self.global_state
                .rcu(|s| s.with_cursor_visible(frame.info.cursor_visible));
        }

        let source_image = frame.image.clone();

        let ifli = self.frame_idx % self.in_flight.len();
        let ifl = &mut self.in_flight[ifli];
        ifl.fence.wait(None)?;

        let AcquiredImage {
            image_index,
            is_suboptimal,
        } = unsafe {
            self.swapchain.acquire_next_image(&AcquireNextImageInfo {
                semaphore: Some(ifl.acquire.clone()),
                ..Default::default()
            })
        }?;
        if is_suboptimal {
            info!("suboptimal, resizing");
            self.backend.release(frame, None);
            self.resize(sizer)?;
            self.ph.skip();
            self.wl_surface.commit();
            return Ok(());
        };

        unsafe { ifl.fence.reset() }?;

        let fb = self.images[image_index as usize].clone();
        let swapchain_image = self.images[image_index as usize].attachments()[0]
            .image()
            .clone();
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
            cmd.pipeline_barrier(&DependencyInfo {
                image_memory_barriers: smallvec![ImageMemoryBarrier {
                    old_layout: ImageLayout::General,
                    new_layout: ImageLayout::ShaderReadOnlyOptimal,
                    src_stages: PipelineStages::TOP_OF_PIPE,
                    src_access: AccessFlags::empty(),
                    dst_stages: PipelineStages::FRAGMENT_SHADER,
                    dst_access: AccessFlags::SHADER_SAMPLED_READ,
                    queue_family_ownership_transfer: Some(
                        QueueFamilyOwnershipTransfer::ExclusiveFromExternal { dst_index: qfi },
                    ),
                    subresource_range: ImageSubresourceRange {
                        aspects: ImageAspects::COLOR,
                        mip_levels: 0..1,
                        array_layers: 0..1,
                    },
                    ..ImageMemoryBarrier::image(source_image.clone())
                }],
                ..Default::default()
            })?;
            cmd.pipeline_barrier(&DependencyInfo {
                image_memory_barriers: smallvec![ImageMemoryBarrier {
                    old_layout: ImageLayout::PresentSrc,
                    new_layout: ImageLayout::ColorAttachmentOptimal,
                    src_stages: PipelineStages::TOP_OF_PIPE,
                    src_access: AccessFlags::empty(),
                    dst_stages: PipelineStages::COLOR_ATTACHMENT_OUTPUT,
                    dst_access: AccessFlags::COLOR_ATTACHMENT_WRITE,
                    subresource_range: ImageSubresourceRange {
                        aspects: ImageAspects::COLOR,
                        mip_levels: 0..1,
                        array_layers: 0..1,
                    },
                    ..ImageMemoryBarrier::image(swapchain_image.clone())
                }],
                ..Default::default()
            })?;
            cmd.begin_render_pass(
                &RenderPassBeginInfo {
                    clear_values: vec![Some([0.0, 0.0, 0.0, 0.0].into())],
                    ..RenderPassBeginInfo::framebuffer(fb.clone())
                },
                &SubpassBeginInfo::default(),
            )?;
            cmd.bind_pipeline_graphics(&self.pipeline)?;
            let content = sizer.render_sizing.content;
            cmd.set_viewport(
                0,
                &[Viewport {
                    offset: [content.x as f32, content.y as f32],
                    extent: [content.width as f32, content.height as f32],
                    depth_range: 0.0..=1.0,
                }],
            )?;
            cmd.bind_descriptor_sets(
                PipelineBindPoint::Graphics,
                self.pipeline.layout(),
                0,
                &[DescriptorSet::new(
                    self.descriptor_set_allocator.clone(),
                    self.pipeline.layout().set_layouts()[0].clone(),
                    [
                        WriteDescriptorSet::sampler(0, self.sampler.clone()),
                        WriteDescriptorSet::image_view(
                            1,
                            ImageView::new_default(source_image.clone())?,
                        ),
                    ],
                    [],
                )?
                .as_raw()],
                &[],
            )?;
            let push_constants = BorderPushConstants {
                time: self.start_time.elapsed().as_secs_f32(),
                show_border: if self.global_state.load().confine {
                    0
                } else {
                    1
                },
                content_width: content.width as f32,
                content_height: content.height as f32,
            };
            cmd.push_constants(self.pipeline.layout(), 0, &push_constants)?;
            // Full-screen triangle via gl_VertexIndex in the VS
            cmd.draw(3, 1, 0, 0)?;
            cmd.end_render_pass(&Default::default())?;
            cmd.pipeline_barrier(&DependencyInfo {
                image_memory_barriers: smallvec![ImageMemoryBarrier {
                    old_layout: ImageLayout::ColorAttachmentOptimal,
                    new_layout: ImageLayout::PresentSrc,
                    src_stages: PipelineStages::COLOR_ATTACHMENT_OUTPUT,
                    src_access: AccessFlags::COLOR_ATTACHMENT_WRITE,
                    dst_stages: PipelineStages::BOTTOM_OF_PIPE,
                    dst_access: AccessFlags::empty(),
                    subresource_range: ImageSubresourceRange {
                        aspects: ImageAspects::COLOR,
                        mip_levels: 0..1,
                        array_layers: 0..1,
                    },
                    ..ImageMemoryBarrier::image(swapchain_image)
                }],
                ..Default::default()
            })?;
            cmd.pipeline_barrier(&DependencyInfo {
                image_memory_barriers: smallvec![ImageMemoryBarrier {
                    old_layout: ImageLayout::ShaderReadOnlyOptimal,
                    new_layout: ImageLayout::General,
                    src_stages: PipelineStages::TOP_OF_PIPE,
                    src_access: AccessFlags::empty(),
                    dst_stages: PipelineStages::TOP_OF_PIPE,
                    dst_access: AccessFlags::empty(),
                    queue_family_ownership_transfer: Some(
                        QueueFamilyOwnershipTransfer::ExclusiveToExternal { src_index: qfi },
                    ),
                    subresource_range: ImageSubresourceRange {
                        aspects: ImageAspects::COLOR,
                        mip_levels: 0..1,
                        array_layers: 0..1,
                    },
                    ..ImageMemoryBarrier::image(source_image.clone())
                }],
                ..Default::default()
            })?;
        }
        let command_buffer = Arc::new(unsafe { cmd.end() }?);

        let command_buffer_handle = vec![command_buffer.handle()];

        // Keep alive
        ifl.last_command_buffer = Some(command_buffer as Arc<dyn Send + Sync>);

        // Build submit info
        let render_semaphore = [ifl.acquire.handle()];
        let present_semaphore = [ifl.present.handle()];
        let submit_info = ash::vk::SubmitInfo::default()
            .command_buffers(&command_buffer_handle)
            .wait_semaphores(&render_semaphore)
            .wait_dst_stage_mask(&[ash::vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT])
            .signal_semaphores(&present_semaphore);

        self.present_counter += 1;
        let present_id = NonZero::new(self.present_counter).unwrap();

        let _pres = self.queue.with(|mut guard| unsafe {
            (self.device.fns().v1_0.queue_submit)(
                self.queue.handle(),
                1,
                &submit_info as *const _,
                ifl.fence.handle(),
            )
            .result()?;

            guard
                .present(&PresentInfo {
                    wait_semaphores: vec![SemaphorePresentInfo::new(ifl.present.clone())],
                    swapchain_infos: vec![SwapchainPresentInfo {
                        present_id: Some(present_id),
                        ..SwapchainPresentInfo::swapchain_image_index(
                            self.swapchain.clone(),
                            image_index,
                        )
                    }],
                    ..Default::default()
                })?
                .next()
                .unwrap()
                .context("present")
        })?;

        let mut info = frame.info.clone();
        self.backend.release(frame, Some(ifl.fence.clone()));

        info.mark_commit();
        let _ = self.present_wait_tx.send(PresentWaitRequest {
            swapchain: self.swapchain.clone(),
            present_id,
            info,
        });
        self.frame_idx += 1;
        Ok(())
    }
}
