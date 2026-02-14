use super::{Buffer, FrameState, State};
use crate::plotter::FrameInfo;
use crate::utils::compose_timestamp;
use anyhow::{Context as _, Result, anyhow};
use drm_fourcc::DrmFourcc;
use smithay_client_toolkit::reexports::client::{Connection, Dispatch, QueueHandle, WEnum};
use std::time::Instant;
use tracing::info;

// breaks rustfmt import sorting
use smithay_client_toolkit::reexports::protocols::ext::image_capture_source::v1::client::ext_image_capture_source_v1::{self, ExtImageCaptureSourceV1};
use smithay_client_toolkit::reexports::protocols::ext::image_capture_source::v1::client::ext_output_image_capture_source_manager_v1::{self, ExtOutputImageCaptureSourceManagerV1};
use smithay_client_toolkit::reexports::protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1};
use smithay_client_toolkit::reexports::protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_manager_v1::{self, ExtImageCopyCaptureManagerV1};
use smithay_client_toolkit::reexports::protocols::ext::image_copy_capture::v1::client::ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1};

struct DmabufFormatEntry {
    format: u32,
    modifiers: Vec<u64>,
}

#[derive(Default)]
struct ImageCopyCaps {
    width: u32,
    height: u32,
    formats: Vec<DmabufFormatEntry>,
}

pub struct ImageCopyState {
    session: ExtImageCopyCaptureSessionV1,
    _source: ExtImageCaptureSourceV1,
    caps: ImageCopyCaps,
    pending_caps: ImageCopyCaps,
    capture_mono_ns: u64,
}

impl ImageCopyState {
    pub fn new(session: ExtImageCopyCaptureSessionV1, source: ExtImageCaptureSourceV1) -> Self {
        Self {
            session,
            _source: source,
            caps: Default::default(),
            pending_caps: Default::default(),
            capture_mono_ns: 0,
        }
    }
}

impl State {
    pub fn image_copy_issue_capture(&mut self) {
        if let Err(e) = self.image_copy_issue_capture_inner() {
            self.done = Some(Err(e));
        }
    }

    fn image_copy_issue_capture_inner(&mut self) -> Result<()> {
        self.drain_reclaimed();

        let ic = self.image_copy.as_ref().unwrap();
        let c = &ic.caps;
        let width = c.width;
        let height = c.height;

        let start = Instant::now();

        let mut buf = None;
        while let Some(pooled) = self.pool.pop() {
            let ext = pooled.image.extent();
            if (ext[0], ext[1]) == (width, height) {
                buf = Some(pooled);
                break;
            }
        }
        if buf.is_none() {
            // Caps may not be available yet if the session Done event hasn't
            // arrived. Return without error; the Done handler will issue the
            // capture.
            if c.formats.is_empty() {
                return Ok(());
            }
            let entry = c
                .formats
                .iter()
                .find(|e| {
                    DrmFourcc::try_from(e.format)
                        .ok()
                        .and_then(|f| crate::utils::fourcc_to_vk_format(f).ok())
                        .is_some()
                })
                .context("no usable format in imagecopy caps")?;
            let fourcc = entry.format;
            let modifiers = entry.modifiers.clone();

            buf = Some(Buffer::new(
                self.device.clone(),
                self.allocator.as_ref(),
                &self.dmabuf_state,
                &self.qh,
                fourcc,
                modifiers,
                width,
                height,
            )?);
        }
        let buf = buf.unwrap();
        let wait = Instant::now();

        let ic = self.image_copy.as_ref().unwrap();
        let frame = ic.session.create_frame(&self.qh, ());
        frame.attach_buffer(&buf.wl_buffer);
        frame.damage_buffer(0, 0, width as i32, height as i32);
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
                ic.pending_caps.width = width;
                ic.pending_caps.height = height;
            }
            ext_image_copy_capture_session_v1::Event::DmabufDevice { device } => {
                let dev = u64::from_ne_bytes(device[..8].try_into().unwrap());
                info!("imagecopy dmabuf device {dev:#x}");
            }
            ext_image_copy_capture_session_v1::Event::DmabufFormat { format, modifiers } => {
                let parsed_modifiers = modifiers
                    .chunks_exact(8)
                    .map(|chunk| u64::from_le_bytes(chunk.try_into().unwrap()))
                    .collect();
                info!(
                    "{format:x} {:?} {parsed_modifiers:x?}",
                    DrmFourcc::try_from(format)
                );
                ic.pending_caps.formats.push(DmabufFormatEntry {
                    format,
                    modifiers: parsed_modifiers,
                });
            }
            ext_image_copy_capture_session_v1::Event::Done => {
                let caps = std::mem::take(&mut ic.pending_caps);
                ic.caps.width = caps.width;
                ic.caps.height = caps.height;
                if !caps.formats.is_empty() {
                    ic.caps.formats = caps.formats;
                }
                state.pool.clear();

                // If a capture was deferred because caps weren't ready yet,
                // issue it now.
                if state.capturing() && state.frame_state.is_none() {
                    state.image_copy_issue_capture();
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
                    ic.capture_mono_ns = compose_timestamp(tv_sec_hi, tv_sec_lo, tv_nsec);
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
                    // TODO: start using create_pointer_cursor_session
                    cursor_visible: false,
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
