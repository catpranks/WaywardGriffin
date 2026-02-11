use super::InputBridge;
use crate::sizer::SharedSizer;
use crate::utils::wayland_connect;
use anyhow::{Context as _, Result};
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::wl_keyboard::{self, WlKeyboard};
use smithay_client_toolkit::reexports::client::protocol::wl_pointer;
use smithay_client_toolkit::reexports::client::protocol::wl_seat::{self, WlSeat};
use smithay_client_toolkit::reexports::client::{Connection, Dispatch, QueueHandle, WEnum};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::{delegate_registry, registry_handlers};
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsFd;
use std::time::Instant;
use tracing::{error, info, warn};

use smithay_client_toolkit::reexports::protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
use smithay_client_toolkit::reexports::protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1;
use smithay_client_toolkit::reexports::protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1;
use smithay_client_toolkit::reexports::protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1;

pub struct WaylandInput {
    conn: Connection,
    vkbd: ZwpVirtualKeyboardV1,
    vptr: ZwlrVirtualPointerV1,
    sizer: SharedSizer,
    epoch: Instant,
    scroll_h_acc: i32,
    scroll_v_acc: i32,
}

impl WaylandInput {
    pub fn new(display: &str, sizer: SharedSizer) -> Result<Self> {
        let conn = wayland_connect(display)?;

        let (globals, mut event_queue) = registry_queue_init::<State>(&conn)?;
        let qh = event_queue.handle();

        let vk_mgr: ZwpVirtualKeyboardManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .context("zwp_virtual_keyboard_manager_v1 not available")?;
        let vp_mgr: ZwlrVirtualPointerManagerV1 = globals
            .bind(&qh, 1..=2, ())
            .context("zwlr_virtual_pointer_manager_v1 not available")?;
        let seat: WlSeat = globals
            .bind(&qh, 1..=1, ())
            .context("wl_seat not available")?;

        let vkbd = vk_mgr.create_virtual_keyboard(&seat, &qh, ());
        let vptr = vp_mgr.create_virtual_pointer(Some(&seat), &qh, ());
        let keyboard = seat.get_keyboard(&qh, ());

        let mut state = State {
            registry_state: RegistryState::new(&globals),
            vkbd: vkbd.clone(),
            _vptr: vptr.clone(),
            _keyboard: keyboard,
            last_keymap: None,
        };

        event_queue.roundtrip(&mut state)?;

        std::thread::Builder::new()
            .name("wayland-input".into())
            .spawn(move || loop {
                match event_queue.blocking_dispatch(&mut state) {
                    Ok(_) => {}
                    Err(e) => {
                        error!("wayland input: {e}");
                        break;
                    }
                }
            })?;

        Ok(WaylandInput {
            conn,
            vkbd,
            vptr,
            sizer,
            epoch: Instant::now(),
            scroll_h_acc: 0,
            scroll_v_acc: 0,
        })
    }

    fn millis(&self) -> u32 {
        self.epoch.elapsed().as_millis() as u32
    }

    fn flush(&self) -> Result<()> {
        self.conn.flush()?;
        Ok(())
    }
}

impl InputBridge for WaylandInput {
    fn mouse_delta(&mut self, x: f64, y: f64) -> Result<()> {
        self.vptr.motion(self.millis(), x, y);
        self.vptr.frame();
        self.flush()
    }

    fn mouse_absolute(&mut self, x: u32, y: u32) -> Result<()> {
        let (sw, sh) = self.sizer.load().source_size;
        self.vptr.motion_absolute(self.millis(), x, y, sw, sh);
        self.vptr.frame();
        self.flush()
    }

    fn mouse_press(&mut self, button: u32) -> Result<()> {
        self.vptr
            .button(self.millis(), button, wl_pointer::ButtonState::Pressed);
        self.vptr.frame();
        self.flush()
    }

    fn mouse_release(&mut self, button: u32) -> Result<()> {
        self.vptr
            .button(self.millis(), button, wl_pointer::ButtonState::Released);
        self.vptr.frame();
        self.flush()
    }

    fn key_press(&mut self, keycode: u32) -> Result<()> {
        self.vkbd.key(self.millis(), keycode, 1);
        self.flush()
    }

    fn key_release(&mut self, keycode: u32) -> Result<()> {
        self.vkbd.key(self.millis(), keycode, 0);
        self.flush()
    }

    fn scroll(&mut self, h_abs: f64, v_abs: f64, h120: i32, v120: i32) -> Result<()> {
        let time = self.millis();
        for (abs, v120, acc, axis) in [
            (v_abs, v120, &mut self.scroll_v_acc, wl_pointer::Axis::VerticalScroll),
            (h_abs, h120, &mut self.scroll_h_acc, wl_pointer::Axis::HorizontalScroll),
        ] {
            if v120 != 0 {
                *acc += v120;
                let discrete = *acc / 120;
                *acc %= 120;
                self.vptr.axis_discrete(time, axis, abs, discrete);
            } else if abs != 0.0 {
                self.vptr.axis(time, axis, abs);
            }
        }
        self.vptr.frame();
        self.flush()
    }

    fn scroll_stop(&mut self, horizontal: bool, vertical: bool) -> Result<()> {
        let time = self.millis();
        if vertical {
            self.vptr
                .axis_stop(time, wl_pointer::Axis::VerticalScroll);
        }
        if horizontal {
            self.vptr
                .axis_stop(time, wl_pointer::Axis::HorizontalScroll);
        }
        self.vptr.frame();
        self.flush()
    }
}

struct State {
    registry_state: RegistryState,
    vkbd: ZwpVirtualKeyboardV1,
    _vptr: ZwlrVirtualPointerV1,
    _keyboard: WlKeyboard,
    last_keymap: Option<Vec<u8>>,
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![];
}

delegate_registry!(State);

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &WlSeat,
        _event: wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &WlKeyboard,
        event: wl_keyboard::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Keymap { format, fd, size } = event
            && let WEnum::Value(wl_keyboard::KeymapFormat::XkbV1) = format
        {
            let file = std::fs::File::from(fd);
            let mut buf = vec![0u8; size as usize];
            if let Err(e) = file.read_exact_at(&mut buf, 0) {
                warn!("failed to read keymap fd: {e}");
                return;
            }
            if state.last_keymap.as_deref() == Some(&buf[..]) {
                return;
            }
            info!("forwarding keymap ({size} bytes) to virtual keyboard");
            state
                .vkbd
                .keymap(wl_keyboard::KeymapFormat::XkbV1 as u32, file.as_fd(), size);
            state.last_keymap = Some(buf);
        }
    }
}

impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpVirtualKeyboardManagerV1,
        _event: <ZwpVirtualKeyboardManagerV1 as smithay_client_toolkit::reexports::client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpVirtualKeyboardV1,
        _event: <ZwpVirtualKeyboardV1 as smithay_client_toolkit::reexports::client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrVirtualPointerManagerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrVirtualPointerManagerV1,
        _event: <ZwlrVirtualPointerManagerV1 as smithay_client_toolkit::reexports::client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrVirtualPointerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrVirtualPointerV1,
        _event: <ZwlrVirtualPointerV1 as smithay_client_toolkit::reexports::client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}
