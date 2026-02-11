mod nvcapture;

use self::nvcapture::NvCapture;
use super::{CaptureBackend, CaptureEnv, DeviceId, SpawnResult};
use crate::capture::input::xinput::XInput;
use crate::capture::plotter::FrameInfo;
use crate::utils::clock_monotonic_ns;
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
use vulkano::memory::allocator::{MemoryAllocator, MemoryTypeFilter};
use vulkano::memory::{
    DedicatedAllocation, DeviceMemory, ExternalMemoryHandleType, ExternalMemoryHandleTypes,
    MemoryAllocateInfo, ResourceMemory,
};
use vulkano::sync::fence::Fence;

pub struct Backend {
    ctx: Arc<CudaContext>,
    capturer: NvCapture,
    display: String,
}

impl Backend {
    pub fn new(display: &str) -> Result<Self> {
        let ctx = CudaContext::new(0)?;
        ctx.set_flags(CUctx_flags::CU_CTX_SCHED_BLOCKING_SYNC)?;
        let capturer = NvCapture::new()?;
        capturer.release_thread()?;
        Ok(Self {
            ctx,
            capturer,
            display: display.to_owned(),
        })
    }
}

impl CaptureBackend for Backend {
    fn device_id(&self) -> DeviceId {
        DeviceId::Uuid(bytemuck::cast(self.ctx.uuid().unwrap().bytes))
    }

    fn spawn(self: Box<Self>, env: CaptureEnv) -> Result<SpawnResult> {
        let bridgde = Box::new(XInput::new(&self.display)?);
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("capture-nvfbc".into())
            .spawn({
                let ph = env.ph.clone();
                move || {
                    ph.fatal(
                        run(self.ctx, self.capturer, env, rx).context("capture thread (nvfbc)"),
                    );
                }
            })
            .unwrap();
        Ok(SpawnResult {
            bridge: bridgde,
            wake: Box::new(move || {
                let _ = tx.send(());
            }),
        })
    }
}

fn drain(rx: &mpsc::Receiver<()>) -> bool {
    let mut got = false;
    while rx.try_recv().is_ok() {
        got = true;
    }
    got
}

fn run(
    ctx: Arc<CudaContext>,
    capturer: NvCapture,
    env: CaptureEnv,
    rx: mpsc::Receiver<()>,
) -> Result<()> {
    let CaptureEnv {
        mut renderer,
        ph,
        global_state,
        sizer: _,
        device,
        allocator,
        backend: _,
    } = env;
    ctx.bind_to_thread()?;
    capturer.bind_thread()?;
    let mut bufs: VecDeque<Buffer> = VecDeque::new();

    // First frame: wait for wakeup, render blank
    rx.recv()?;
    renderer.blank()?;
    let mut last_render = Instant::now();

    loop {
        if !global_state.load().capture {
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            }
            drain(&rx);
            renderer.blank()?;
            last_render = Instant::now();
            continue;
        }

        let stream_ptr = std::ptr::null_mut();
        capturer.bind_thread()?;

        let start = Instant::now();
        let (dptr, info) = capturer.capture_frame(None)?;
        let capture_mono_ns = clock_monotonic_ns();

        let mut pooled = bufs.pop_front();
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
            pooled = Some(Buffer::new(
                device.clone(),
                &allocator,
                ctx.clone(),
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
        unsafe { cuMemcpy2DAsync_v2(&copy as _, stream_ptr) }.result()?;
        unsafe { stream::synchronize(stream_ptr) }?;
        let obtain = Instant::now();

        ph.capture();

        if global_state.load().cursor_visible != info.cursor_visible {
            global_state.rcu(|s| s.with_cursor_visible(info.cursor_visible));
        }

        let woke = drain(&rx);
        if woke || last_render.elapsed() >= Duration::from_secs(1) {
            let frame_info = FrameInfo {
                start,
                wait,
                obtain,
                commit: None,
                capture_mono_ns,
                present: None,
                cursor_visible: info.cursor_visible,
                safety: !woke,
            };
            let fence = renderer.render(pooled.image.clone(), frame_info)?;
            pooled.fence = Some(fence);
            last_render = Instant::now();
        } else {
            ph.capture_miss();
        }

        assert!(bufs.len() < 4, "buffer pool bloated");
        bufs.push_back(pooled);
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
    fence: Option<Arc<Fence>>,
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
