mod nvcapture;

use self::nvcapture::NvCapture;
use super::{CaptureBackend, CaptureBackendBuilder, CapturedFrame};
use crate::capture::plotter::{FrameInfo, PlotterHandle};
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
use std::time::{Duration, Instant};
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

const NUM_BUFFERS: usize = 3;

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
        Ok(Self { ctx, capturer })
    }
}

impl CaptureBackendBuilder for Builder {
    fn device_uuid(&self) -> Option<[u8; 16]> {
        self.ctx.uuid().ok().map(|u| bytemuck::cast(u.bytes))
    }

    fn build(
        self: Box<Self>,
        device: Arc<Device>,
        allocator: Arc<StandardMemoryAllocator>,
        ph: PlotterHandle,
    ) -> Result<Box<dyn CaptureBackend>> {
        let (_, info) = self.capturer.capture_frame(Some(Duration::ZERO))?;
        self.capturer.release_thread()?;

        let bufs = (0..NUM_BUFFERS)
            .map(|_| PooledBuffer::new(device.clone(), &allocator, self.ctx.clone(), info.size))
            .collect::<Result<VecDeque<_>>>()?;

        Ok(Box::new(Backend {
            ctx: self.ctx,
            capturer: self.capturer,
            device,
            allocator,
            bufs,
            ph,
        }))
    }
}

pub struct Backend {
    ctx: Arc<CudaContext>,
    capturer: NvCapture,
    device: Arc<Device>,
    allocator: Arc<StandardMemoryAllocator>,
    bufs: VecDeque<PooledBuffer>,
    ph: PlotterHandle,
}

impl CaptureBackend for Backend {
    fn capture(&mut self) -> Result<Option<CapturedFrame>> {
        self.ctx.bind_to_thread()?;
        let stream = std::ptr::null_mut();
        self.capturer.bind_thread()?;

        let start = Instant::now();
        let (dptr, info) = self.capturer.capture_frame(Some(Duration::default()))?;

        if info.is_new_frame {
            self.ph.capture();
        } else {
            self.ph.capture_miss();
        }

        let mut pooled = self.bufs.pop_front().unwrap();
        let buf_extent = pooled.image.extent();
        let buf_size = (buf_extent[0], buf_extent[1]);
        if buf_size != info.size {
            // Wait for GPU to finish with old buffer before dropping
            if let Some(fence) = pooled.fence.take() {
                fence.wait(None)?;
            }
            pooled = PooledBuffer::new(
                self.device.clone(),
                &self.allocator,
                self.ctx.clone(),
                info.size,
            )?;
        }

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
                cursor_visible: info.cursor_visible,
            },
            handle: Box::new(BufferHandle { cumem: pooled.cumem }),
        };
        Ok(Some(frame))
    }

    fn release(&mut self, frame: CapturedFrame, fence: Option<Arc<Fence>>) {
        let handle: BufferHandle = *frame.handle.downcast().expect("invalid handle type");
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
