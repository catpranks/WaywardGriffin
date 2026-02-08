mod nvcapture;

use self::nvcapture::NvCapture;
use super::{CaptureBackend, CaptureBackendBuilder, CapturedFrame};
use crate::capture::input::InputBridge;
use crate::capture::input::xinput::XInput;
use crate::capture::plotter::{FrameInfo, clock_monotonic_ns};
use anyhow::{Context as _, Result};
use cudarc::driver::result::external_memory::{
    destroy_external_memory, import_external_memory_opaque_fd,
};
use cudarc::driver::result::stream;
use cudarc::driver::safe::CudaContext;
use cudarc::driver::sys::{
    self as cuda, CUDA_ARRAY3D_COLOR_ATTACHMENT, CUDA_ARRAY3D_DESCRIPTOR,
    CUDA_ARRAY3D_SURFACE_LDST, CUDA_EXTERNAL_MEMORY_MIPMAPPED_ARRAY_DESC, CUarray, CUarray_format,
    CUmipmappedArray, cuExternalMemoryGetMappedMipmappedArray, cuMipmappedArrayDestroy,
    cuMipmappedArrayGetLevel,
};
use cudarc::driver::sys::{CUDA_MEMCPY2D, CUctx_flags, CUmemorytype, cuMemcpy2DAsync_v2};
use std::collections::VecDeque;
use std::fs::File;
use std::os::fd::IntoRawFd as _;
use std::sync::Arc;
use std::time::Instant;
use vulkano::device::Device;
use vulkano::format::Format;
use vulkano::image::sys::RawImage;
use vulkano::image::{Image, ImageCreateInfo, ImageTiling, ImageUsage};
use vulkano::memory::allocator::{MemoryAllocator, MemoryTypeFilter, StandardMemoryAllocator};
use vulkano::memory::{
    DedicatedAllocation, DeviceMemory, ExternalMemoryHandleType, ExternalMemoryHandleTypes,
    MemoryAllocateInfo, ResourceMemory,
};
use vulkano::sync::fence::Fence;

pub struct Builder {
    ctx: Arc<CudaContext>,
    capturer: NvCapture,
}

impl Builder {
    pub fn new() -> Result<Self> {
        let ctx = CudaContext::new(0)?;
        ctx.set_flags(CUctx_flags::CU_CTX_SCHED_BLOCKING_SYNC)?;
        ctx.bind_to_thread()?;
        let capturer = NvCapture::new()?;
        capturer.release_thread()?;
        Ok(Self { ctx, capturer })
    }
}

impl CaptureBackendBuilder for Builder {
    fn device_id(&self) -> super::DeviceId {
        super::DeviceId::Uuid(bytemuck::cast(self.ctx.uuid().unwrap().bytes))
    }

    fn build(
        self: Box<Self>,
        device: Arc<Device>,
        allocator: Arc<StandardMemoryAllocator>,
        display: &str,
    ) -> Result<(Box<dyn CaptureBackend>, Box<dyn InputBridge>)> {
        // NVFBC reads the display from the DISPLAY env var; there's no API to pass it explicitly.
        // XInput gets it explicitly below.
        let injector = XInput::new(display)?;

        Ok((
            Box::new(Backend {
                ctx: self.ctx,
                capturer: self.capturer,
                device,
                allocator,
                bufs: VecDeque::new(),
            }),
            Box::new(injector),
        ))
    }
}

pub struct Backend {
    ctx: Arc<CudaContext>,
    capturer: NvCapture,
    device: Arc<Device>,
    allocator: Arc<StandardMemoryAllocator>,
    bufs: VecDeque<PooledBuffer>,
}

impl CaptureBackend for Backend {
    fn capture(&mut self) -> Result<CapturedFrame> {
        self.ctx.bind_to_thread()?;
        let stream = std::ptr::null_mut();
        self.capturer.bind_thread()?;

        let start = Instant::now();
        let (dptr, info) = self.capturer.capture_frame(None)?;
        let capture_mono_ns = clock_monotonic_ns();

        let mut pooled = self.bufs.pop_front();
        let reuse = pooled.as_ref().is_some_and(|p| {
            let ext = p.image.extent();
            (ext[0], ext[1]) == info.size
        });
        if !reuse {
            if let Some(old) = &mut pooled
                && let Some(fence) = old.fence.take()
            {
                fence.wait(None)?;
            }
            pooled = Some(PooledBuffer::new(
                self.device.clone(),
                &self.allocator,
                self.ctx.clone(),
                info.size,
            )?);
        }
        let mut pooled = pooled.unwrap();

        if let Some(fence) = pooled.fence.take() {
            fence.wait(None)?;
        }
        let wait = Instant::now();

        let (width, height) = info.size;
        let pitch = (width * 4) as usize;
        let copy = CUDA_MEMCPY2D {
            srcXInBytes: 0,
            srcY: 0,
            srcMemoryType: CUmemorytype::CU_MEMORYTYPE_DEVICE,
            srcHost: std::ptr::null(),
            srcDevice: dptr,
            srcArray: std::ptr::null_mut(),
            srcPitch: pitch,
            dstXInBytes: 0,
            dstY: 0,
            dstMemoryType: CUmemorytype::CU_MEMORYTYPE_ARRAY,
            dstHost: std::ptr::null_mut(),
            dstDevice: 0,
            dstArray: pooled.cumem.array,
            dstPitch: 0,
            WidthInBytes: pitch,
            Height: height as usize,
        };
        unsafe { cuMemcpy2DAsync_v2(&copy as _, stream) }.result()?;
        unsafe { stream::synchronize(stream) }?;
        let obtain = Instant::now();

        let frame = CapturedFrame {
            image: pooled.image,
            info: FrameInfo {
                start,
                wait,
                obtain,
                commit: None,
                capture_mono_ns,
                present: None,
                cursor_visible: info.cursor_visible,
            },
            handle: Box::new(BufferHandle {
                cumem: pooled.cumem,
            }),
        };
        Ok(frame)
    }

    fn release(&mut self, frame: CapturedFrame, fence: Option<Arc<Fence>>) {
        let handle: BufferHandle = *frame.handle.downcast().expect("invalid handle type");
        assert!(self.bufs.len() < 4, "buffer pool bloated");
        self.bufs.push_back(PooledBuffer {
            cumem: handle.cumem,
            image: frame.image,
            fence,
        });
    }
}

struct BufferHandle {
    cumem: CudaArray,
}

struct CudaArray {
    ctx: Arc<CudaContext>,
    array: CUarray,
    marray: CUmipmappedArray,
    ext_mem: cuda::CUexternalMemory,
}

impl CudaArray {
    fn new(ctx: Arc<CudaContext>, fd: File, alloc_size: u64, size: (u32, u32)) -> Result<Self> {
        ctx.bind_to_thread()?;
        let ext_mem = unsafe { import_external_memory_opaque_fd(fd.into_raw_fd(), alloc_size)? };
        let mut marray: CUmipmappedArray = std::ptr::null_mut();
        let (width, height) = size;
        let desc = CUDA_EXTERNAL_MEMORY_MIPMAPPED_ARRAY_DESC {
            offset: 0,
            numLevels: 1,
            arrayDesc: CUDA_ARRAY3D_DESCRIPTOR {
                Width: width as usize,
                Height: height as usize,
                Depth: 0,
                Format: CUarray_format::CU_AD_FORMAT_UNSIGNED_INT8,
                NumChannels: 4,
                Flags: CUDA_ARRAY3D_COLOR_ATTACHMENT | CUDA_ARRAY3D_SURFACE_LDST,
            },
            reserved: [0; 16],
        };
        unsafe { cuExternalMemoryGetMappedMipmappedArray(&mut marray as _, ext_mem, &desc as _) }
            .result()?;
        let mut array: CUarray = std::ptr::null_mut();
        unsafe { cuMipmappedArrayGetLevel(&mut array as _, marray, 0) }.result()?;
        Ok(Self {
            ctx,
            array,
            marray,
            ext_mem,
        })
    }
}

impl Drop for CudaArray {
    fn drop(&mut self) {
        let _ = self.ctx.bind_to_thread();
        if let Err(e) = unsafe { cuMipmappedArrayDestroy(self.marray) }.result() {
            eprintln!("ERROR: cuMipmappedArrayDestroy failed: {e:?}");
            eprintln!("{:#?}", std::backtrace::Backtrace::capture());
        }
        if let Err(e) = unsafe { destroy_external_memory(self.ext_mem) } {
            eprintln!("ERROR: cuDestroyExternalMemory failed: {e:?}");
        }
    }
}

unsafe impl Send for CudaArray {}
unsafe impl Sync for CudaArray {}

struct PooledBuffer {
    cumem: CudaArray,
    image: Arc<Image>,
    fence: Option<Arc<Fence>>,
}

impl PooledBuffer {
    fn new(
        device: Arc<Device>,
        allocator: &impl MemoryAllocator,
        ctx: Arc<CudaContext>,
        size: (u32, u32),
    ) -> Result<Self> {
        let (width, height) = size;
        let raw_image = RawImage::new(
            device.clone(),
            ImageCreateInfo {
                format: Format::B8G8R8A8_SRGB,
                extent: [width, height, 1],
                usage: ImageUsage::TRANSFER_SRC | ImageUsage::SAMPLED,
                external_memory_handle_types: ExternalMemoryHandleTypes::OPAQUE_FD,
                tiling: ImageTiling::Optimal,
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
                export_handle_types: ExternalMemoryHandleTypes::OPAQUE_FD,
                ..Default::default()
            },
        )?;
        let file = alloc.export_fd(ExternalMemoryHandleType::OpaqueFd)?;
        let cumem = CudaArray::new(ctx, file, req.layout.size(), size)?;
        let image = Arc::new(
            raw_image
                .bind_memory([ResourceMemory::new_dedicated(alloc)])
                .map_err(|(e, _, _)| e)?,
        );
        Ok(Self {
            cumem,
            image,
            fence: None,
        })
    }
}
