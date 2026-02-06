use super::CaptureOpts;
use super::plotter::PlotterHandle;
use super::source::create_backend_builder;
use super::vulkan::create_instance_and_select_device;
use anyhow::{Context as _, Result};
use std::sync::Arc;
use vulkano::buffer::{Buffer, BufferCreateInfo, BufferUsage};
use vulkano::command_buffer::allocator::StandardCommandBufferAllocator;
use vulkano::command_buffer::{
    AutoCommandBufferBuilder, BlitImageInfo, CommandBufferUsage, CopyImageToBufferInfo,
    PrimaryCommandBufferAbstract as _,
};
use vulkano::device::{Device, DeviceCreateInfo, DeviceExtensions, QueueCreateInfo, QueueFlags};
use vulkano::format::Format;
use vulkano::image::{Image, ImageCreateInfo, ImageType, ImageUsage};
use vulkano::memory::allocator::{AllocationCreateInfo, MemoryTypeFilter, StandardMemoryAllocator};
use vulkano::sync::GpuFuture as _;

pub fn run(opts: &CaptureOpts) -> Result<()> {
    let ph = PlotterHandle::dummy();

    // Build backend (CUDA/NVFBC init happens here)
    let backend_builder = create_backend_builder(opts)?;

    let (_instance, physical_device) =
        create_instance_and_select_device(backend_builder.as_ref())?;

    let queue_family_index = physical_device
        .queue_family_properties()
        .iter()
        .position(|q| q.queue_flags.intersects(QueueFlags::GRAPHICS))
        .map(|i| i as u32)
        .context("no graphics queue family")?;

    let (device, mut queues) = Device::new(
        physical_device,
        DeviceCreateInfo {
            enabled_extensions: DeviceExtensions {
                khr_external_memory: true,
                khr_external_memory_fd: true,
                ext_external_memory_dma_buf: true,
                ..DeviceExtensions::empty()
            },
            queue_create_infos: vec![QueueCreateInfo {
                queue_family_index,
                ..Default::default()
            }],
            ..Default::default()
        },
    )?;
    let queue = queues.next().unwrap();

    let allocator = Arc::new(StandardMemoryAllocator::new_default(device.clone()));
    let cb_allocator = Arc::new(StandardCommandBufferAllocator::new(
        device.clone(),
        Default::default(),
    ));

    // Build backend and capture one frame
    let (mut backend, _injector) =
        backend_builder.build(device.clone(), allocator.clone(), ph, &opts.display)?;
    let frame = backend.capture()?.context("no frame captured")?;

    let extent = frame.image.extent();
    let width = extent[0];
    let height = extent[1];
    let size = (width * height * 4) as u64;

    // Intermediate RGBA image for format conversion
    let rgba_image = Image::new(
        allocator.clone(),
        ImageCreateInfo {
            image_type: ImageType::Dim2d,
            format: Format::R8G8B8A8_SRGB,
            extent,
            usage: ImageUsage::TRANSFER_SRC | ImageUsage::TRANSFER_DST,
            ..Default::default()
        },
        AllocationCreateInfo::default(),
    )?;

    // Staging buffer for readback
    let staging = Buffer::new_slice::<u8>(
        allocator,
        BufferCreateInfo {
            usage: BufferUsage::TRANSFER_DST,
            ..Default::default()
        },
        AllocationCreateInfo {
            memory_type_filter: MemoryTypeFilter::HOST_RANDOM_ACCESS,
            ..Default::default()
        },
        size,
    )?;

    // Blit source -> RGBA, then copy to buffer
    let mut cmd = AutoCommandBufferBuilder::primary(
        cb_allocator,
        queue.queue_family_index(),
        CommandBufferUsage::OneTimeSubmit,
    )?;

    cmd.blit_image(BlitImageInfo::images(
        frame.image.clone(),
        rgba_image.clone(),
    ))?
    .copy_image_to_buffer(CopyImageToBufferInfo::image_buffer(
        rgba_image,
        staging.clone(),
    ))?;

    let command_buffer = cmd.build()?;

    // Submit and wait
    command_buffer
        .execute(queue)?
        .then_signal_fence_and_flush()?
        .wait(None)?;

    // Read pixels and fix alpha
    let mut rgba = staging.read()?.to_vec();
    for chunk in rgba.chunks_exact_mut(4) {
        chunk[3] = 255;
    }

    let img =
        image::RgbaImage::from_raw(width, height, rgba).context("invalid image dimensions")?;
    let dyn_img = image::DynamicImage::ImageRgba8(img);

    viuer::print(&dyn_img, &Default::default())?;

    Ok(())
}
