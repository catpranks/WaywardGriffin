use anyhow::{Result, bail};
use cudarc::driver::sys::CUdeviceptr;
use std::os::raw::c_int;
use std::time::Duration;

enum NvCaptureHandle {}

type NvFbcStatus = c_int;
type NvFbcBool = c_int;

// From NvFBC.h
const NVFBC_SUCCESS: NvFbcStatus = 0;
const NVFBC_TRUE: NvFbcBool = 1;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct RawFrameGrabInfo {
    dw_width: u32,
    dw_height: u32,
    dw_byte_size: u32,
    dw_current_frame: u32,
    b_is_new_frame: NvFbcBool,
    ul_timestamp_us: u64,
    dw_missed_frames: u32,
    b_required_post_processing: NvFbcBool,
    b_direct_capture: NvFbcBool,
    b_cursor_visible: NvFbcBool,
    b_cursor_composited: NvFbcBool,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct NvfbcFrameInfo {
    pub size: (u32, u32),
    pub byte_size: u32,
    current_frame: u32,
    is_new_frame: bool,
    timestamp_us: u64,
    missed_frames: u32,
    required_post_processing: bool,
    direct_capture: bool,
    pub cursor_visible: bool,
    cursor_composited: bool,
}

impl From<RawFrameGrabInfo> for NvfbcFrameInfo {
    fn from(raw: RawFrameGrabInfo) -> Self {
        Self {
            size: (raw.dw_width, raw.dw_height),
            byte_size: raw.dw_byte_size,
            current_frame: raw.dw_current_frame,
            is_new_frame: raw.b_is_new_frame == NVFBC_TRUE,
            timestamp_us: raw.ul_timestamp_us,
            missed_frames: raw.dw_missed_frames,
            required_post_processing: raw.b_required_post_processing == NVFBC_TRUE,
            direct_capture: raw.b_direct_capture == NVFBC_TRUE,
            cursor_visible: raw.b_cursor_visible == NVFBC_TRUE,
            cursor_composited: raw.b_cursor_composited == NVFBC_TRUE,
        }
    }
}

unsafe extern "C" {
    fn nvcapture_init(handle: *mut *mut NvCaptureHandle) -> NvFbcStatus;
    fn nvcapture_capture(
        handle: *mut NvCaptureHandle,
        dptr: *mut CUdeviceptr,
        info: *mut RawFrameGrabInfo,
        timeout_ms: u32,
    ) -> NvFbcStatus;
    fn nvcapture_destroy(handle: *mut NvCaptureHandle) -> NvFbcStatus;
    fn nvcapture_bind_thread(handle: *mut NvCaptureHandle) -> NvFbcStatus;
    fn nvcapture_release_thread(handle: *mut NvCaptureHandle) -> NvFbcStatus;
}

pub struct NvCapture {
    handle: *mut NvCaptureHandle,
}

impl NvCapture {
    pub fn new() -> Result<Self> {
        let mut handle: *mut NvCaptureHandle = std::ptr::null_mut();
        let status = unsafe { nvcapture_init(&mut handle) };
        if status != NVFBC_SUCCESS {
            bail!("NVFBC code: {}", status)
        }
        Ok(NvCapture { handle })
    }

    pub fn release_thread(&self) -> Result<()> {
        let status = unsafe { nvcapture_release_thread(self.handle) };
        if status != NVFBC_SUCCESS {
            bail!("NVFBC code: {}", status)
        }
        Ok(())
    }

    pub fn bind_thread(&self) -> Result<()> {
        let status = unsafe { nvcapture_bind_thread(self.handle) };
        if status != NVFBC_SUCCESS {
            bail!("NVFBC code: {}", status)
        }
        Ok(())
    }

    pub fn capture_frame(
        &self,
        timeout: Option<Duration>,
    ) -> Result<(CUdeviceptr, NvfbcFrameInfo)> {
        let timeout_ms = timeout.map(|d| d.as_millis() as u32).unwrap_or(1000);
        let mut dptr: CUdeviceptr = 0;
        let mut info = RawFrameGrabInfo::default();
        let status = unsafe { nvcapture_capture(self.handle, &mut dptr, &mut info, timeout_ms) };
        if status != NVFBC_SUCCESS {
            bail!("NVFBC code: {}", status)
        }
        let info: NvfbcFrameInfo = info.into();
        let (width, height) = info.size;
        if info.byte_size != width * height * 4 {
            bail!(
                "Unexpected frame byte size: {} (expected {}x{}x4)",
                info.byte_size,
                width,
                height,
            );
        }
        Ok((dptr, info))
    }
}

impl Drop for NvCapture {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                nvcapture_destroy(self.handle);
            }
        }
    }
}

unsafe impl Send for NvCapture {}
unsafe impl Sync for NvCapture {}
