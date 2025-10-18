use crate::capture::Capture;
use crate::capture::plotter::{self, FrameTimings};
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
use cudarc::driver::sys::{CUDA_MEMCPY2D, CUmemorytype, cuMemcpy2DAsync_v2};
use std::fs::File;
use std::os::fd::IntoRawFd as _;
use std::sync::Arc;
use std::time::{Duration, Instant};
use vulkano::device::Device;
use vulkano::format::Format;
use vulkano::image::sys::RawImage;
use vulkano::image::{Image, ImageCreateInfo, ImageTiling, ImageUsage};
use vulkano::memory::allocator::{MemoryAllocator, MemoryTypeFilter};
use vulkano::memory::{
    DedicatedAllocation, DeviceMemory, ExternalMemoryHandleType, ExternalMemoryHandleTypes,
    MemoryAllocateInfo, ResourceMemory,
};
use vulkano::sync::fence::Fence;

pub struct CudaArray {
    ctx: Arc<CudaContext>,
    array: CUarray,
    marray: CUmipmappedArray,
    ext_mem: cuda::CUexternalMemory,
}

impl CudaArray {
    pub fn new(ctx: Arc<CudaContext>, fd: File, alloc_size: u64, size: (u32, u32)) -> Result<Self> {
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
            ..Default::default()
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

const CAPTURE_WAIT: Duration = Duration::from_millis(2);

impl Capture {
    pub fn capture(&mut self) -> Result<Option<CapturedFrame>> {
        self.ctx.bind_to_thread()?;
        let stream = std::ptr::null_mut();
        self.capturer.bind_thread()?;
        let start = Instant::now();
        // let (dptr, info) = self.capturer.capture_frame(Some(CAPTURE_WAIT))?;
        let (dptr, info) = self.capturer.capture_frame(Some(Duration::default()))?;
        if info.is_new_frame {
            self.ph.frame(plotter::EventType::Capture);
        } else {
            self.ph.drop(plotter::EventType::Capture);
            return Ok(None);
        }
        let capture_time = start.elapsed();
        if self.sizer.load().source_size != info.size {
            self.sizer.rcu(|s| s.with_source_size(info.size));
        }
        if self.global_state.load().cursor_visible != info.cursor_visible {
            self.global_state
                .rcu(|s| s.with_cursor_visible(info.cursor_visible));
        }
        let mut buf = self.bufs.pop_front().unwrap();
        if buf.size != info.size {
            buf = MyBuffer::new(
                self.device.clone(),
                &self.allocator,
                self.ctx.clone(),
                info.size,
            )?;
        }
        if let Some(fence) = buf.fence.take() {
            fence.wait(None)?;
        }
        let wait_time = start.elapsed();
        let (width, height) = info.size;
        let pitch = (width * 4) as usize;
        let copy = CUDA_MEMCPY2D {
            srcMemoryType: CUmemorytype::CU_MEMORYTYPE_DEVICE,
            srcDevice: dptr,
            srcPitch: pitch,
            dstMemoryType: CUmemorytype::CU_MEMORYTYPE_ARRAY,
            dstArray: buf.cumem.array,
            WidthInBytes: pitch,
            Height: height as usize,
            ..Default::default()
        };
        unsafe { cuMemcpy2DAsync_v2(&copy as _, stream) }.result()?;
        unsafe { stream::synchronize(stream) }?;
        let cuda_time = start.elapsed();
        let frame = CapturedFrame {
            buf,
            timings: FrameTimings::new(start, capture_time, wait_time, cuda_time, info),
        };
        Ok(Some(frame))
    }
}

pub struct MyBuffer {
    pub cumem: CudaArray,
    pub image: Arc<Image>,
    pub size: (u32, u32),
    pub fence: Option<Arc<Fence>>,
}

impl MyBuffer {
    pub fn new(
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
            size,
            fence: None,
        })
    }
}

pub struct CapturedFrame {
    pub buf: MyBuffer,
    pub timings: FrameTimings,
}
