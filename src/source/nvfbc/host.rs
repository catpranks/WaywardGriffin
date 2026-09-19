//! CUDA writes pinned system RAM; RADV copies or samples the same pages.
use anyhow::{Context as _, Result, ensure};
use ash::vk;
use cudarc::driver::{safe::CudaContext, sys as cuda};
use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::ptr::NonNull;
use std::sync::Arc;
use tracing::debug;
use vulkano::VulkanObject as _;
use vulkano::buffer::sys::RawBuffer;
use vulkano::buffer::{BufferCreateInfo, BufferUsage, Subbuffer};
use vulkano::command_buffer::allocator::StandardCommandBufferAllocator;
use vulkano::command_buffer::{
    CommandBufferBeginInfo, CommandBufferLevel, CommandBufferUsage, RecordingCommandBuffer,
};
use vulkano::device::{Device, DeviceOwned as _, Queue};
use vulkano::format::Format;
use vulkano::image::sys::RawImage;
use vulkano::image::{Image, ImageAspect, ImageCreateInfo, ImageLayout, ImageTiling, ImageUsage};
use vulkano::memory::{ExternalMemoryHandleTypes, MemoryPropertyFlags};
use vulkano::sync::fence::{Fence, FenceCreateInfo};
use vulkano::sync::{DependencyInfo, ImageMemoryBarrier, PipelineStages};

pub(super) struct Buffer {
    // Drop the image and buffer before their allocation. CapturedFrame/in-flight references
    // keep this whole slot alive until AMD's last read has completed.
    pub image: Arc<Image>,
    pub transfer: Subbuffer<[u8]>,
    memory: HostMemory,
    pub row_pitch: u64,
    offset: usize,
}

impl Buffer {
    pub fn new(queue: Arc<Queue>, ctx: Arc<CudaContext>, size: (u32, u32)) -> Result<Self> {
        let device = queue.device().clone();
        let (width, height) = size;
        let raw_image = RawImage::new(
            device.clone(),
            ImageCreateInfo {
                format: Format::B8G8R8A8_SRGB,
                extent: [width, height, 1],
                usage: ImageUsage::SAMPLED,
                external_memory_handle_types: ExternalMemoryHandleTypes::HOST_ALLOCATION,
                tiling: ImageTiling::Linear,
                initial_layout: ImageLayout::Undefined,
                ..Default::default()
            },
        )?;
        let requirements = raw_image.memory_requirements()[0];
        let image_layout = raw_image.subresource_layout(ImageAspect::Color, 0, 0)?;
        let raw_buffer = RawBuffer::new(
            device.clone(),
            BufferCreateInfo {
                size: requirements.layout.size(),
                usage: BufferUsage::TRANSFER_SRC,
                external_memory_handle_types: ExternalMemoryHandleTypes::HOST_ALLOCATION,
                ..Default::default()
            },
        )?;
        let buffer_requirements = raw_buffer.memory_requirements();
        ensure!(
            !requirements.requires_dedicated_allocation
                && !buffer_requirements.requires_dedicated_allocation,
            "host aliases require shared memory"
        );
        let alignment = device
            .physical_device()
            .properties()
            .min_imported_host_pointer_alignment
            .context("host import alignment")?
            .max(requirements.layout.alignment().into())
            .max(buffer_requirements.layout.alignment().into()) as usize;
        let allocation_size = (requirements
            .layout
            .size()
            .max(buffer_requirements.layout.size()) as usize)
            .next_multiple_of(alignment);
        let pages = PinnedPages::new(ctx, allocation_size, alignment)?;

        let mut host_properties = vk::MemoryHostPointerPropertiesEXT::default();
        unsafe {
            (device
                .fns()
                .ext_external_memory_host
                .get_memory_host_pointer_properties_ext)(
                device.handle(),
                vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT,
                pages.ptr.as_ptr().cast(),
                &mut host_properties,
            )
            .result()?;
        }
        let memory_type_bits = requirements.memory_type_bits
            & buffer_requirements.memory_type_bits
            & host_properties.memory_type_bits;
        let memory_type_index = device
            .physical_device()
            .memory_properties()
            .memory_types
            .iter()
            .enumerate()
            .position(|(index, memory_type)| {
                memory_type_bits & (1 << index) != 0
                    && memory_type.property_flags.contains(
                        MemoryPropertyFlags::HOST_VISIBLE | MemoryPropertyFlags::HOST_COHERENT,
                    )
            })
            .context("coherent host memory type for CUDA/RADV")?
            as u32;

        // Vulkano 0.35 has no host-pointer MemoryImportInfo variant. Keep this
        // allocation raw; assume_bound() lets the rest of rendering use Image.
        let mut import = vk::ImportMemoryHostPointerInfoEXT::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT)
            .host_pointer(pages.ptr.as_ptr().cast());
        let allocate_info = vk::MemoryAllocateInfo::default()
            .allocation_size(allocation_size as u64)
            .memory_type_index(memory_type_index)
            .push_next(&mut import);
        let mut handle = vk::DeviceMemory::null();
        unsafe {
            (device.fns().v1_0.allocate_memory)(
                device.handle(),
                &allocate_info,
                std::ptr::null(),
                &mut handle,
            )
            .result()?;
        }
        let memory = HostMemory {
            device,
            handle,
            pages,
        };
        unsafe {
            (memory.device.fns().v1_0.bind_image_memory)(
                memory.device.handle(),
                raw_image.handle(),
                memory.handle,
                0,
            )
            .result()?;
        }
        ensure!(
            image_layout.row_pitch >= u64::from(width) * 4,
            "host image row pitch"
        );
        debug!(
            width,
            height,
            allocation_size,
            memory_type_index,
            row_pitch = image_layout.row_pitch,
            offset = image_layout.offset,
            "CUDA/RADV host slot"
        );
        unsafe {
            (memory.device.fns().v1_0.bind_buffer_memory)(
                memory.device.handle(),
                raw_buffer.handle(),
                memory.handle,
                0,
            )
            .result()?;
        }
        let transfer: Subbuffer<[u8]> = Arc::new(unsafe { raw_buffer.assume_bound() }).into();
        let transfer = transfer.slice(image_layout.offset..);
        let image = Arc::new(unsafe { raw_image.assume_bound() });
        // External-memory images must start Undefined. Transition once *before*
        // CUDA writes pixels, so a discard-layout transition cannot lose a frame.
        initialize_layout(&queue, image.clone())?;
        Ok(Self {
            image,
            transfer,
            memory,
            row_pitch: image_layout.row_pitch,
            offset: image_layout.offset as usize,
        })
    }

    pub fn host_pointer(&self) -> *mut std::ffi::c_void {
        unsafe { self.memory.pages.ptr.as_ptr().add(self.offset).cast() }
    }
}

struct HostMemory {
    device: Arc<Device>,
    handle: vk::DeviceMemory,
    pages: PinnedPages,
}

impl Drop for HostMemory {
    fn drop(&mut self) {
        unsafe {
            (self.device.fns().v1_0.free_memory)(
                self.device.handle(),
                self.handle,
                std::ptr::null(),
            )
        };
        // Only then may PinnedPages unregister and free the shared allocation.
    }
}

struct PinnedPages {
    ctx: Arc<CudaContext>,
    ptr: NonNull<u8>,
    layout: Layout,
}

impl PinnedPages {
    fn new(ctx: Arc<CudaContext>, size: usize, alignment: usize) -> Result<Self> {
        ctx.bind_to_thread()?;
        let layout = Layout::from_size_align(size, alignment)?;
        let ptr =
            NonNull::new(unsafe { alloc_zeroed(layout) }).context("allocate capture pages")?;
        if let Err(error) =
            unsafe { cuda::cuMemHostRegister_v2(ptr.as_ptr().cast(), size, 0) }.result()
        {
            unsafe { dealloc(ptr.as_ptr(), layout) };
            return Err(error.into());
        }
        Ok(Self { ctx, ptr, layout })
    }
}

impl Drop for PinnedPages {
    fn drop(&mut self) {
        self.ctx.bind_to_thread().unwrap();
        unsafe {
            cuda::cuMemHostUnregister(self.ptr.as_ptr().cast())
                .result()
                .unwrap();
            dealloc(self.ptr.as_ptr(), self.layout);
        }
    }
}

// The capture thread is the sole writer; publication follows CUDA completion.
// Renderer fences and CapturedFrame ownership prevent writes during AMD reads.
unsafe impl Send for PinnedPages {}
unsafe impl Sync for PinnedPages {}

fn initialize_layout(queue: &Arc<Queue>, image: Arc<Image>) -> Result<()> {
    let device = queue.device();
    let allocator = Arc::new(StandardCommandBufferAllocator::new(
        device.clone(),
        Default::default(),
    ));
    let mut cmd = RecordingCommandBuffer::new(
        allocator,
        queue.queue_family_index(),
        CommandBufferLevel::Primary,
        CommandBufferBeginInfo {
            usage: CommandBufferUsage::OneTimeSubmit,
            ..Default::default()
        },
    )?;
    unsafe {
        cmd.pipeline_barrier(&DependencyInfo {
            image_memory_barriers: smallvec::smallvec![ImageMemoryBarrier {
                src_stages: PipelineStages::TOP_OF_PIPE,
                dst_stages: PipelineStages::ALL_COMMANDS,
                subresource_range: image.subresource_range(),
                old_layout: ImageLayout::Undefined,
                new_layout: ImageLayout::General,
                ..ImageMemoryBarrier::image(image)
            }],
            ..Default::default()
        })?;
    }
    let cmd = unsafe { cmd.end() }?;
    let fence = Fence::new(device.clone(), FenceCreateInfo::default())?;
    let commands = [cmd.handle()];
    let submit = vk::SubmitInfo::default().command_buffers(&commands);
    queue.with(|_| unsafe {
        (device.fns().v1_0.queue_submit)(queue.handle(), 1, &submit, fence.handle()).result()
    })?;
    fence.wait(None)?;
    Ok(())
}
