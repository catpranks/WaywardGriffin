use super::{Buffer, FrameState, State};
use crate::plotter::FrameInfo;
use crate::utils::{compose_timestamp, fourcc_to_vk_format};
use anyhow::{Context as _, Result, bail};
use drm_fourcc::DrmFourcc;
use smithay_client_toolkit::reexports::client::{Connection, Dispatch, QueueHandle};
use std::time::Instant;

// breaks rustfmt import sorting
use smithay_client_toolkit::reexports::protocols_wlr::screencopy::v1::client::zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1};
use smithay_client_toolkit::reexports::protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1};

impl State {
    pub fn screencopy_issue_capture(&mut self) {
        self.frame_state = Some(FrameState::Requested {
            start: Instant::now(),
        });
        let output = self.output.as_ref().unwrap();
        self.screencopy_manager.as_ref().unwrap().capture_output(
            /* overlay_cursor */ 1,
            output,
            &self.qh,
            (),
        );
    }

    fn handle_buffer_done(&mut self, frame: &ZwlrScreencopyFrameV1) -> Result<()> {
        let Some(FrameState::Described {
            start,
            format,
            width,
            height,
        }) = self.frame_state.take()
        else {
            bail!("BufferDone without prior LinuxDmabuf");
        };
        let vk_format =
            fourcc_to_vk_format(DrmFourcc::try_from(format).context("unknown fourcc")?)?;

        self.drain_reclaimed();
        let mut buf = None;
        while let Some(pooled) = self.pool.pop() {
            let ext = pooled.image.extent();
            if pooled.image.format() == vk_format && (ext[0], ext[1]) == (width, height) {
                buf = Some(pooled);
                break;
            }
        }
        if buf.is_none() {
            buf = Some(Buffer::new(
                self.device.clone(),
                self.allocator.as_ref(),
                &self.dmabuf_state,
                &self.qh,
                format,
                vec![0],
                width,
                height,
            )?);
        }
        let buf = buf.unwrap();
        let wait = Instant::now();

        frame.copy(&buf.wl_buffer);
        self.frame_state = Some(FrameState::Copying { buf, start, wait });
        Ok(())
    }
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrScreencopyManagerV1,
        _event: zwlr_screencopy_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        frame: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::LinuxDmabuf {
                format,
                width,
                height,
            } => {
                let start = match state.frame_state {
                    Some(FrameState::Requested { start } | FrameState::Described { start, .. }) => {
                        start
                    }
                    _ => return,
                };
                state.frame_state = Some(FrameState::Described {
                    start,
                    format,
                    width,
                    height,
                });
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => {
                if let Err(e) = state.handle_buffer_done(frame) {
                    state.done = Some(Err(e));
                }
            }
            zwlr_screencopy_frame_v1::Event::Ready {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
            } => {
                let Some(FrameState::Copying { buf, start, wait }) = state.frame_state.take()
                else {
                    assert!(state.done.is_some());
                    return;
                };
                let obtain = Instant::now();
                let capture_mono_ns = compose_timestamp(tv_sec_hi, tv_sec_lo, tv_nsec);
                let info = FrameInfo {
                    start,
                    wait,
                    obtain,
                    commit: None,
                    capture_mono_ns,
                    present: None,
                    cursor_visible: false,
                };
                frame.destroy();
                state.handle_ready(info, buf);
            }
            zwlr_screencopy_frame_v1::Event::Failed => {
                if let Some(FrameState::Copying { buf, .. }) = state.frame_state.take() {
                    state.pool.push(buf);
                }
                frame.destroy();
                state.handle_failed();
            }
            _ => {}
        }
    }
}
