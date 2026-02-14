mod nvcapture;

use self::nvcapture::NvCapture;
use super::{CaptureBackendBuilder, CaptureEnv, DeviceId};
use crate::GlobalState;
use crate::capture::input::InputBridge;
use crate::capture::input::xinput::XInput;
use crate::plotter::{FrameInfo, PlotterHandle};
use crate::capture::source::{CaptureBackend, CapturedFrame, ReclaimedBuffer};
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
use std::sync::mpsc;
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

pub struct Builder {
    ctx: Arc<CudaContext>,
    display: String,
}

impl Builder {
    pub fn new(display: &str) -> Result<Self> {
        let ctx = CudaContext::new(0)?;
        ctx.set_flags(CUctx_flags::CU_CTX_SCHED_BLOCKING_SYNC)?;
        Ok(Self {
            ctx,
            display: display.to_owned(),
        })
    }
}

impl CaptureBackendBuilder for Builder {
    fn device_id(&self) -> Result<DeviceId> {
        self.ctx.bind_to_thread()?;
        Ok(DeviceId::Uuid(bytemuck::cast(self.ctx.uuid()?.bytes)))
    }

    fn build(
        self: Box<Self>,
        env: CaptureEnv,
    ) -> Result<(Box<dyn CaptureBackend>, Box<dyn InputBridge>)> {
        self.ctx.bind_to_thread()?;
        let capturer = NvCapture::new()?;
        let bridge = Box::new(XInput::new(&self.display)?);
        let (reclaim_tx, reclaim_rx) = mpsc::channel();
        Ok((
            Box::new(Backend {
                ctx: self.ctx,
                capturer,
                ph: env.ph,
                global_state: env.global_state,
                device: env.device,
                allocator: env.allocator,
                bufs: VecDeque::new(),
                reclaim_tx,
                reclaim_rx,
            }),
            bridge,
        ))
    }
}

pub struct Backend {
    ctx: Arc<CudaContext>,
    capturer: NvCapture,
    ph: PlotterHandle,
    global_state: GlobalState,
    device: Arc<Device>,
    allocator: Arc<StandardMemoryAllocator>,
    bufs: VecDeque<Buffer>,
    reclaim_tx: mpsc::Sender<ReclaimedBuffer>,
    reclaim_rx: mpsc::Receiver<ReclaimedBuffer>,
}

impl CaptureBackend for Backend {
    fn capture(&mut self) -> Result<Option<CapturedFrame>> {
        let stream_ptr = std::ptr::null_mut();
        let start = Instant::now();
        let (dptr, info) = self.capturer.capture_frame(Some(Duration::ZERO))?;
        for _ in 0..info.missed_frames {
            self.ph.capture_miss();
        }
        if !info.is_new_frame {
            return Ok(None);
        }
        self.ph.capture();
        // let capture_mono_ns = clock_monotonic_ns();
        let capture_mono_ns = info.timestamp_us * 1000;

        while let Ok(rbuf) = self.reclaim_rx.try_recv() {
            self.bufs.push_back(rbuf.into());
        }

        let mut buf = None;
        while let Some(pooled) = self.bufs.pop_front() {
            let ext = pooled.image.extent();
            if (ext[0], ext[1]) == info.size {
                buf = Some(pooled);
                break;
            }
        }
        if buf.is_none() {
            buf = Some(Buffer::new(
                self.device.clone(),
                &self.allocator,
                self.ctx.clone(),
                info.size,
            )?);
        }
        let buf = buf.unwrap();

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
            dstArray: buf.cumem.array,
            dstPitch: 0,
            WidthInBytes: pitch,
            Height: height as usize,
        };
        unsafe { cuMemcpy2DAsync_v2(&copy as _, stream_ptr) }.result()?;
        unsafe { stream::synchronize(stream_ptr) }?;
        let obtain = Instant::now();

        if self.global_state.load().cursor_visible != info.cursor_visible {
            self.global_state
                .rcu(|s| s.with_cursor_visible(info.cursor_visible));
        }

        let info = FrameInfo {
            start,
            wait,
            obtain,
            commit: None,
            capture_mono_ns,
            present: None,
            cursor_visible: info.cursor_visible,
        };
        Ok(Some(CapturedFrame {
            info: Some(info),
            image: buf.image,
            backend_data: Some(Box::new(buf.cumem)),
            reclaim_tx: self.reclaim_tx.clone(),
        }))
    }
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

struct Buffer {
    cumem: CudaArray,
    image: Arc<Image>,
}

impl Buffer {
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
            device,
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
        Ok(Self { cumem, image })
    }
}

impl From<ReclaimedBuffer> for Buffer {
    fn from(rbuf: ReclaimedBuffer) -> Self {
        Self {
            cumem: *rbuf.backend_data.downcast().unwrap(),
            image: rbuf.image,
        }
    }
}
