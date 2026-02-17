pub mod input;
pub mod source;

use crate::capture::source::{CaptureBackend, CapturedFrame, DeviceId};
use crate::display::DisplayCtx;
use crate::overlay::{OverlayFrame, OverlayHandle, OverlayState};
use crate::sizer::{SharedSizer, Sizer};
use anyhow::{Context as _, Result, bail};
use clap::Args;
use smallvec::smallvec;
use smithay_client_toolkit::compositor::Region;
use smithay_client_toolkit::reexports::client::{Connection, Proxy as _};
use source::BackendType;
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
use vulkano::pipeline::graphics::viewport::{Viewport, ViewportState};
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
struct TexturedPushConstants {
    opaque: u32,
}

#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct BorderPushConstants {
    time: f32,
    content_width: f32,
    content_height: f32,
}

#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
struct ScreenshotPushConstants {
    width: u32,
    height: u32,
}

mod vs {
    vulkano_shaders::shader! {
        ty: "vertex",
        src: r"
            #version 450

            layout(location = 0) out vec2 tex_coords;

            void main() {
                float x = float((gl_VertexIndex & 1) << 2) - 1.0;
                float y = float((gl_VertexIndex & 2) << 1) - 1.0;

                gl_Position = vec4(x, y, 0.0, 1.0);
                tex_coords = gl_Position.xy * 0.5 + 0.5;
            }
        ",
    }
}

mod fs_textured {
    vulkano_shaders::shader! {
        ty: "fragment",
        src: r"
            #version 450

            layout(location = 0) in vec2 tex_coords;
            layout(location = 0) out vec4 f_color;

            layout(set = 0, binding = 0) uniform sampler s;
            layout(set = 0, binding = 1) uniform texture2D tex;

            layout(push_constant) uniform PushConstants {
                uint opaque;
            } pc;

            void main() {
                f_color = texture(sampler2D(tex, s), tex_coords);
                if (pc.opaque != 0u) {
                    f_color.a = 1.0;
                }
            }
        ",
    }
}

mod cs_screenshot {
    vulkano_shaders::shader! {
        ty: "compute",
        src: r"
            #version 450

            layout(local_size_x = 16, local_size_y = 16) in;

            layout(set = 0, binding = 0) uniform sampler s;
            layout(set = 0, binding = 1) uniform texture2D tex;

            layout(set = 0, binding = 2) buffer OutputBuf {
                uint data[];
            } out_buf;

            layout(push_constant) uniform PushConstants {
                uint width;
                uint height;
            } pc;

            vec3 linear_to_srgb(vec3 c) {
                vec3 lo = c * 12.92;
                vec3 hi = 1.055 * pow(c, vec3(1.0 / 2.4)) - 0.055;
                return mix(lo, hi, greaterThan(c, vec3(0.0031308)));
            }

            void main() {
                uint x = gl_GlobalInvocationID.x;
                uint y = gl_GlobalInvocationID.y;
                if (x >= pc.width || y >= pc.height) return;

                vec2 uv = (vec2(x, y) + 0.5) / vec2(pc.width, pc.height);
                vec4 color = texture(sampler2D(tex, s), uv);
                color.rgb = linear_to_srgb(color.rgb);
                uvec4 c = clamp(uvec4(color * 255.0 + 0.5), uvec4(0), uvec4(255));
                out_buf.data[y * pc.width + x] = c.b | (c.g << 8u) | (c.r << 16u) | (255u << 24u);
            }
        ",
    }
}

mod fs_border {
    vulkano_shaders::shader! {
        ty: "fragment",
        src: r"
            #version 450

            layout(location = 0) in vec2 tex_coords;
            layout(location = 0) out vec4 f_color;

            layout(push_constant) uniform PushConstants {
                float time;
                float content_width;
                float content_height;
            } pc;

            void main() {
                float sine_val = (sin(pc.time * 2.0) + 1.0) / 2.0;
                float border_width_px = 15.0 + sine_val * 10.0;
                float alpha = (5.0 + sine_val * 150.0) / 255.0;

                float px = tex_coords.x * pc.content_width;
                float py = tex_coords.y * pc.content_height;

                float edge_dist = min(min(px, pc.content_width - px),
                                      min(py, pc.content_height - py));

                if (edge_dist < border_width_px) {
                    f_color = vec4(0.0, 0.78, 0.78, alpha);
                } else {
                    f_color = vec4(0.0);
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
    frames: Vec<Arc<CapturedFrame>>,
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
    queue: Arc<Queue>,
    pub device: Arc<Device>,
    _surface: Arc<Surface>,
    start_time: Instant,
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

        let physical_device = match device_id {
            DeviceId::Uuid(uuid) => instance
                .enumerate_physical_devices()?
                .find(|p| p.properties().device_uuid == Some(uuid))
                .context("no physical device with matching UUID")?,
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
                    ..Default::default()
                },
                ..Default::default()
            },
        )?;
        let queue = queues.next().unwrap();
        let allocator = Arc::new(StandardMemoryAllocator::new_default(device.clone()));

        let (swapchain, swapchain_images) = {
            let surface_capabilities =
                physical_device.surface_capabilities(&surface, Default::default())?;

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
                    dynamic_state: [DynamicState::Viewport].into_iter().collect(),
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
                    acquire,
                    present,
                    fence,
                    last_command_buffer: None,
                    frames: vec![],
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
            device,
            _surface: surface,
            start_time: Instant::now(),
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
        let info = new.as_mut().and_then(|f| f.info.take());
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
        ifl.fence.wait(None)?;
        ifl.frames.clear();
        if let Some(ref old) = old_frame {
            ifl.frames.push(old.clone());
        }
        ifl.frames.push(current_frame);
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
            // Release old capture image to external (if replaced)
            if let Some(ref old) = old_frame {
                cmd.pipeline_barrier(&DependencyInfo {
                    image_memory_barriers: smallvec![ImageMemoryBarrier {
                        old_layout: ImageLayout::ShaderReadOnlyOptimal,
                        new_layout: ImageLayout::General,
                        src_stages: PipelineStages::FRAGMENT_SHADER,
                        src_access: AccessFlags::SHADER_SAMPLED_READ,
                        dst_stages: PipelineStages::BOTTOM_OF_PIPE,
                        dst_access: AccessFlags::empty(),
                        queue_family_ownership_transfer: Some(
                            QueueFamilyOwnershipTransfer::ExclusiveToExternal { src_index: qfi },
                        ),
                        subresource_range: ImageSubresourceRange {
                            aspects: ImageAspects::COLOR,
                            mip_levels: 0..1,
                            array_layers: 0..1,
                        },
                        ..ImageMemoryBarrier::image(old.image.clone())
                    }],
                    ..Default::default()
                })?;
            }
            // Release old overlay image to external (if replaced)
            if let Some(ref old) = old_overlay {
                cmd.pipeline_barrier(&DependencyInfo {
                    image_memory_barriers: smallvec![ImageMemoryBarrier {
                        old_layout: ImageLayout::ShaderReadOnlyOptimal,
                        new_layout: ImageLayout::General,
                        src_stages: PipelineStages::FRAGMENT_SHADER,
                        src_access: AccessFlags::SHADER_SAMPLED_READ,
                        dst_stages: PipelineStages::BOTTOM_OF_PIPE,
                        dst_access: AccessFlags::empty(),
                        queue_family_ownership_transfer: Some(
                            QueueFamilyOwnershipTransfer::ExclusiveToExternal { src_index: qfi },
                        ),
                        subresource_range: ImageSubresourceRange {
                            aspects: ImageAspects::COLOR,
                            mip_levels: 0..1,
                            array_layers: 0..1,
                        },
                        ..ImageMemoryBarrier::image(old.image.clone())
                    }],
                    ..Default::default()
                })?;
            }
            // Acquire new capture image from external
            if has_new_frame {
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
                        ..ImageMemoryBarrier::image(image.clone())
                    }],
                    ..Default::default()
                })?;
            }
            // Acquire new overlay image from external
            if has_new_overlay && let Some(ref overlay) = self.current_overlay {
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
                        ..ImageMemoryBarrier::image(overlay.image.clone())
                    }],
                    ..Default::default()
                })?;
            }
            // Transition swapchain for rendering
            cmd.pipeline_barrier(&DependencyInfo {
                image_memory_barriers: smallvec![ImageMemoryBarrier {
                    old_layout: ImageLayout::Undefined,
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

            // Draw 1: Base image (opaque)
            cmd.bind_descriptor_sets(
                PipelineBindPoint::Graphics,
                self.textured_pipeline.layout(),
                0,
                &[DescriptorSet::new(
                    self.descriptor_set_allocator.clone(),
                    self.textured_pipeline.layout().set_layouts()[0].clone(),
                    [
                        WriteDescriptorSet::sampler(0, self.sampler.clone()),
                        WriteDescriptorSet::image_view(1, ImageView::new_default(image.clone())?),
                    ],
                    [],
                )?
                .as_raw()],
                &[],
            )?;
            cmd.push_constants(
                self.textured_pipeline.layout(),
                0,
                &TexturedPushConstants { opaque: 1 },
            )?;
            cmd.draw(3, 1, 0, 0)?;

            // Draw 2: Overlay (alpha-blended)
            if let Some(ref overlay) = self.current_overlay {
                cmd.bind_descriptor_sets(
                    PipelineBindPoint::Graphics,
                    self.textured_pipeline.layout(),
                    0,
                    &[DescriptorSet::new(
                        self.descriptor_set_allocator.clone(),
                        self.textured_pipeline.layout().set_layouts()[0].clone(),
                        [
                            WriteDescriptorSet::sampler(0, self.sampler.clone()),
                            WriteDescriptorSet::image_view(
                                1,
                                ImageView::new_default(overlay.image.clone())?,
                            ),
                        ],
                        [],
                    )?
                    .as_raw()],
                    &[],
                )?;
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
                cmd.draw(3, 1, 0, 0)?;
            }

            cmd.end_render_pass(&Default::default())?;

            // Transition swapchain back to present
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
        }
        let command_buffer = Arc::new(unsafe { cmd.end() }?);

        let command_buffer_handle = vec![command_buffer.handle()];

        ifl.last_command_buffer = Some(command_buffer as Arc<dyn Send + Sync>);

        let mut wait_semaphores = vec![ifl.acquire.handle()];
        let mut wait_stages = vec![ash::vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT];
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

        if let Some(mut info) = info {
            info.mark_commit();
            self.dc.request_feedback(info);
        }

        let present_suboptimal = self.queue.with(|mut guard| unsafe {
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
                WriteDescriptorSet::image_view(1, ImageView::new_default(image)?),
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

pub struct ScreenshotData {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub data: Vec<u8>,
}

pub enum RenderMsg {
    Frame,
    Screenshot(UnixStream),
}

pub fn spawn(
    dc: DisplayCtx,
    renderer: Renderer,
    backend: Box<dyn CaptureBackend>,
    overlay_handle: OverlayHandle,
    render_rx: mpsc::Receiver<RenderMsg>,
) -> Result<()> {
    let ph = dc.ph.clone();
    std::thread::Builder::new()
        .name("render".into())
        .spawn(move || {
            ph.fatal(
                render_loop(dc, renderer, backend, overlay_handle, render_rx)
                    .context("render thread"),
            );
        })?;
    Ok(())
}

fn render_loop(
    dc: DisplayCtx,
    mut renderer: Renderer,
    mut backend: Box<dyn CaptureBackend>,
    overlay_handle: OverlayHandle,
    render_rx: mpsc::Receiver<RenderMsg>,
) -> Result<()> {
    loop {
        let msg = match render_rx.recv() {
            Ok(msg) => msg,
            Err(mpsc::RecvError) => return Ok(()),
        };
        match msg {
            RenderMsg::Frame => {
                dc.request_frame();
                if !dc.global_state.load().capture {
                    renderer.blank()?;
                    continue;
                }
                let capture = backend.capture()?;
                if capture.is_none() {
                    dc.ph.skip();
                }
                let overlay = overlay_handle.take();
                renderer.render(capture, overlay)?;
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
