mod host;
mod nvcapture;

use self::nvcapture::NvCapture;
use super::{CaptureBackendBuilder, CaptureEnv, DeviceId};
use crate::GlobalState;
use crate::GlobalStateExt as _;
use crate::bridge::InputBridge;
use crate::bridge::xinput::XInput;
use crate::monitor::{FrameInfo, MonitorHandle};
use crate::sizer::SharedSizer;
use crate::source::{CaptureBackend, CapturedFrame, ReclaimedBuffer};
use anyhow::Result;
use cudarc::driver::result::stream;
use cudarc::driver::safe::CudaContext;
use cudarc::driver::sys::{CUDA_MEMCPY2D, CUctx_flags, CUmemorytype, cuMemcpy2DAsync_v2};
use host::Buffer;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use vulkano::device::Queue;

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
        Ok(DeviceId::RadvIntegrated)
    }

    fn build(
        self: Box<Self>,
        env: CaptureEnv,
        _sizer: SharedSizer,
    ) -> Result<(Box<dyn CaptureBackend>, Box<dyn InputBridge>)> {
        self.ctx.bind_to_thread()?;
        let capturer = NvCapture::new()?;
        capturer.release_thread()?;
        let bridge = Box::new(XInput::new(&self.display)?);
        let (reclaim_tx, reclaim_rx) = mpsc::channel();
        let mut capturer = Capturer {
            ctx: self.ctx,
            capturer,
            mon: env.mon.clone(),
            global_state: env.global_state.clone(),
            queue: env.queue,
            bufs: VecDeque::new(),
            reclaim_tx,
            reclaim_rx,
        };
        let slot = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let capture_slot = slot.clone();
        let capture_stop = stop.clone();
        let thread = thread::Builder::new()
            .name("nvfbc-capture".into())
            .spawn(move || {
                let result = (|| -> Result<()> {
                    capturer.ctx.bind_to_thread()?;
                    capturer.capturer.bind_thread()?;
                    while !capture_stop.load(Ordering::Relaxed) {
                        if !env.global_state.load().capture {
                            thread::park_timeout(Duration::from_millis(20));
                            continue;
                        }
                        let Some(frame) = capturer.capture()? else {
                            continue;
                        };
                        // One pending frame, never a FIFO of increasingly stale images.
                        // CUDA copy + sync completed before capture() returned, so the next
                        // NVFBC grab cannot overwrite an image still used by Vulkan.
                        let replaced = capture_slot.lock().unwrap().replace(frame);
                        if replaced.is_some() {
                            env.mon.capture_drop();
                        } else if env
                            .render_tx
                            .send(crate::render::RenderMsg::CaptureReady)
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    env.mon.fatal(Err(error.context("NVFBC capture thread")));
                }
            })?;
        Ok((
            Box::new(Backend {
                slot,
                stop,
                thread: Some(thread),
            }),
            bridge,
        ))
    }
}

pub struct Backend {
    slot: Arc<Mutex<Option<CapturedFrame>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl CaptureBackend for Backend {
    fn frame_driven(&self) -> bool {
        true
    }

    fn capture(&mut self) -> Result<Option<CapturedFrame>> {
        Ok(self.slot.lock().unwrap().take())
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

struct Capturer {
    ctx: Arc<CudaContext>,
    capturer: NvCapture,
    mon: MonitorHandle,
    global_state: GlobalState,
    queue: Arc<Queue>,
    bufs: VecDeque<Buffer>,
    reclaim_tx: mpsc::Sender<ReclaimedBuffer>,
    reclaim_rx: mpsc::Receiver<ReclaimedBuffer>,
}

impl Capturer {
    fn capture(&mut self) -> Result<Option<CapturedFrame>> {
        let stream_ptr = std::ptr::null_mut();
        let start = Instant::now();
        let (dptr, info) = self
            .capturer
            .capture_frame(Some(Duration::from_millis(50)))?;
        let wait = Instant::now();
        let grab_mono_ns = crate::utils::clock_monotonic_ns();
        for _ in 0..info.missed_frames {
            self.mon.capture_miss();
        }
        if !info.is_new_frame {
            return Ok(None);
        }
        self.mon.capture();
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
                self.queue.clone(),
                self.ctx.clone(),
                info.size,
            )?);
        }
        let buf = buf.unwrap();

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
            dstMemoryType: CUmemorytype::CU_MEMORYTYPE_HOST,
            dstHost: buf.host_pointer(),
            dstDevice: 0,
            dstArray: std::ptr::null_mut(),
            dstPitch: buf.row_pitch as usize,
            WidthInBytes: pitch,
            Height: height as usize,
        };
        unsafe { cuMemcpy2DAsync_v2(&copy as _, stream_ptr) }.result()?;
        unsafe { stream::synchronize(stream_ptr) }?;
        let obtain = Instant::now();

        if self.global_state.load().cursor_visible != info.cursor_visible {
            self.global_state
                .update(|s| s.cursor_visible = info.cursor_visible);
        }

        let info = FrameInfo {
            start,
            wait,
            obtain,
            latency: crate::stats::FrameLatency {
                grab_mono_ns: Some(grab_mono_ns),
                direct_capture: Some(info.direct_capture),
                ..Default::default()
            },
            commit: None,
            capture_mono_ns,
            present: None,
            cursor_visible: info.cursor_visible,
        };
        Ok(Some(CapturedFrame {
            host_buffer: Some(crate::source::HostBuffer {
                bytes: buf.transfer.clone(),
                row_length: (buf.row_pitch / 4) as u32,
            }),
            info: Some(info),
            image: buf.image.clone(),
            backend_data: Some(Box::new(buf)),
            reclaim_tx: self.reclaim_tx.clone(),
        }))
    }
}

impl From<ReclaimedBuffer> for Buffer {
    fn from(rbuf: ReclaimedBuffer) -> Self {
        *rbuf.backend_data.downcast().unwrap()
    }
}
