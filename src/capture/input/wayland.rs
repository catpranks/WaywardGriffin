use super::InputBridge;
use crate::capture::source::wayland::connect;
use anyhow::{Context as _, Result};
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::wl_seat::{self, WlSeat};
use smithay_client_toolkit::reexports::client::{Connection, Dispatch, QueueHandle};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::{delegate_registry, registry_handlers};

use smithay_client_toolkit::reexports::protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
use smithay_client_toolkit::reexports::protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1;
use smithay_client_toolkit::reexports::protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1;
use smithay_client_toolkit::reexports::protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1;

pub struct WaylandInput;

impl WaylandInput {
    pub fn new(display: &str) -> Result<Self> {
        let conn = connect(display)?;

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

        let mut state = State {
            registry_state: RegistryState::new(&globals),
            _vkbd: vkbd,
            _vptr: vptr,
        };

        event_queue.roundtrip(&mut state)?;

        std::thread::Builder::new()
            .name("wayland-input".into())
            .spawn(move || {
                let _conn = conn;
                loop {
                    match event_queue.blocking_dispatch(&mut state) {
                        Ok(_) => {}
                        Err(e) => {
                            tracing::error!("wayland input: {e}");
                            break;
                        }
                    }
                }
            })?;

        Ok(WaylandInput)
    }
}

impl InputBridge for WaylandInput {
    fn mouse_delta(&mut self, _x: f64, _y: f64) -> Result<()> {
        Ok(())
    }

    fn mouse_absolute(&mut self, _x: i32, _y: i32) -> Result<()> {
        Ok(())
    }

    fn mouse_press(&mut self, _button: u32) -> Result<()> {
        Ok(())
    }

    fn mouse_release(&mut self, _button: u32) -> Result<()> {
        Ok(())
    }

    fn key_press(&mut self, _keycode: u32) -> Result<()> {
        Ok(())
    }

    fn key_release(&mut self, _keycode: u32) -> Result<()> {
        Ok(())
    }

    fn scroll(&mut self, _h: i32, _v: i32) -> Result<()> {
        Ok(())
    }
}

struct State {
    registry_state: RegistryState,
    _vkbd: ZwpVirtualKeyboardV1,
    _vptr: ZwlrVirtualPointerV1,
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
