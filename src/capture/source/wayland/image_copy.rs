use super::{Buffer, FrameState, State, fourcc_to_format};
use crate::capture::plotter::FrameInfo;
use anyhow::{Result, anyhow};
use smithay_client_toolkit::reexports::client::{Connection, Dispatch, QueueHandle, WEnum};
use std::time::Instant;
use tracing::info;

// breaks rustfmt import sorting
use smithay_client_toolkit::reexports::protocols::ext::image_capture_source::v1::client::ext_image_capture_source_v1::{self, ExtImageCaptureSourceV1};
use smithay_client_toolkit::reexports::protocols::ext::image_capture_source::v1::client::ext_output_image_capture_source_manager_v1::{self, ExtOutputImageCaptureSourceManagerV1};
use smithay_client_toolkit::reexports::protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1};
use smithay_client_toolkit::reexports::protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::{self, ExtImageCopyCaptureManagerV1};
use smithay_client_toolkit::reexports::protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1};

pub struct ImageCopyState {
    pub session: ExtImageCopyCaptureSessionV1,
    pub _source: ExtImageCaptureSourceV1,
    // Session-level buffer constraints, populated by session events
    pub width: u32,
    pub height: u32,
    pub format: u32,
    // Accumulator for presentation_time (arrives before ready)
    pub capture_mono_ns: u64,
}

impl State {
    pub fn image_copy_issue_capture(&mut self) {
        if let Err(e) = self.image_copy_issue_capture_inner() {
            self.done = Some(Err(e));
        }
    }

    fn image_copy_issue_capture_inner(&mut self) -> Result<()> {
        let ic = self.image_copy.as_ref().unwrap();
        let start = Instant::now();

        let mut buf = self.pool.pop();
        let vk_format = fourcc_to_format(ic.format)?;
        let reuse = buf.as_ref().is_some_and(|b| {
            let ext = b.image.extent();
            b.image.format() == vk_format && (ext[0], ext[1]) == (ic.width, ic.height)
        });
        if !reuse {
            if let Some(old) = &mut buf
                && let Some(fence) = old.fence.take()
            {
                fence.wait(None)?;
            }
            buf = Some(Buffer::new(
                self.device.clone(),
                self.allocator.as_ref(),
                &self.dmabuf_state,
                &self.qh,
                ic.format,
                ic.width,
                ic.height,
            )?);
        }
        let mut buf = buf.unwrap();
        if let Some(fence) = buf.fence.take() {
            fence.wait(None)?;
        }
        let wait = Instant::now();

        let frame = ic.session.create_frame(&self.qh, ());
        frame.attach_buffer(&buf.wl_buffer);
        frame.damage_buffer(0, 0, ic.width as i32, ic.height as i32);
        frame.capture();

        self.frame_state = Some(FrameState::Copying { buf, start, wait });
        Ok(())
    }
}

impl Dispatch<ExtImageCopyCaptureManagerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ExtImageCopyCaptureManagerV1,
        _event: ext_image_copy_capture_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let Some(ic) = &mut state.image_copy else {
            return;
        };
        match event {
            ext_image_copy_capture_session_v1::Event::BufferSize { width, height } => {
                // Constraints are being (re-)sent. Reset format so we accept
                // the next dmabuf_format, and clear stale buffers.
                ic.format = 0;
                ic.width = width;
                ic.height = height;
                state.pool.clear();
            }
            ext_image_copy_capture_session_v1::Event::DmabufFormat { format, modifiers } => {
                info!("format {format:x} modifiers {modifiers:x?}");
                // Take the first format we can map to Vulkan
                if ic.format == 0 && fourcc_to_format(format).is_ok() {
                    ic.format = format;
                }
            }
            ext_image_copy_capture_session_v1::Event::Stopped => {
                state.done = Some(Err(anyhow!("imagecopy session stopped")));
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        frame: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_image_copy_capture_frame_v1::Event::PresentationTime {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
            } => {
                if let Some(ic) = &mut state.image_copy {
                    ic.capture_mono_ns = ((tv_sec_hi as u64) << 32 | tv_sec_lo as u64)
                        * 1_000_000_000
                        + tv_nsec as u64;
                }
            }
            ext_image_copy_capture_frame_v1::Event::Ready => {
                let Some(FrameState::Copying { buf, start, wait }) = state.frame_state.take()
                else {
                    assert!(state.done.is_some());
                    return;
                };
                let obtain = Instant::now();
                let capture_mono_ns = state
                    .image_copy
                    .as_ref()
                    .map(|ic| ic.capture_mono_ns)
                    .unwrap_or(0);
                let info = FrameInfo {
                    start,
                    wait,
                    obtain,
                    commit: None,
                    capture_mono_ns,
                    present: None,
                    cursor_visible: true,
                };
                frame.destroy();
                state.handle_ready(info, buf);
            }
            ext_image_copy_capture_frame_v1::Event::Failed { reason } => {
                let constraint_mismatch = reason
                    == WEnum::Value(
                        ext_image_copy_capture_frame_v1::FailureReason::BufferConstraints,
                    );
                if let Some(FrameState::Copying { buf, .. }) = state.frame_state.take()
                    && !constraint_mismatch
                {
                    state.pool.push(buf);
                }
                if constraint_mismatch {
                    state.pool.clear();
                }
                frame.destroy();
                state.handle_failed();
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCaptureSourceV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ExtImageCaptureSourceV1,
        _event: ext_image_capture_source_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtOutputImageCaptureSourceManagerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ExtOutputImageCaptureSourceManagerV1,
        _event: ext_output_image_capture_source_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}
