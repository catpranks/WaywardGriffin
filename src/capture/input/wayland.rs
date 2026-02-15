use super::InputBridge;
use crate::plotter::PlotterHandle;
use crate::sizer::SharedSizer;
use crate::utils::wayland_connect;
use anyhow::{Context as _, Result, anyhow};
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::wl_keyboard::{self, WlKeyboard};
use smithay_client_toolkit::reexports::client::protocol::wl_pointer;
use smithay_client_toolkit::reexports::client::protocol::wl_seat::{self, WlSeat};
use smithay_client_toolkit::reexports::client::{
    Connection, Dispatch, QueueHandle, WEnum, event_created_child,
};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::{delegate_registry, registry_handlers};
use std::collections::HashMap;
use std::io::{PipeReader, Read as _, Write as _};
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsFd;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tracing::{info, warn};

use smithay_client_toolkit::reexports::protocols::ext::data_control::v1::client::ext_data_control_device_v1::{self, ExtDataControlDeviceV1};
use smithay_client_toolkit::reexports::protocols::ext::data_control::v1::client::ext_data_control_manager_v1::ExtDataControlManagerV1;
use smithay_client_toolkit::reexports::protocols::ext::data_control::v1::client::ext_data_control_offer_v1::{self, ExtDataControlOfferV1};
use smithay_client_toolkit::reexports::protocols::ext::data_control::v1::client::ext_data_control_source_v1::{self, ExtDataControlSourceV1};
use smithay_client_toolkit::reexports::protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
use smithay_client_toolkit::reexports::protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1;
use smithay_client_toolkit::reexports::protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1;
use smithay_client_toolkit::reexports::protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1;

const TEXT_MIME: &str = "text/plain;charset=utf-8";

struct ClipboardShared {
    selection: Option<(ExtDataControlOfferV1, Vec<String>)>,
    primary: Option<(ExtDataControlOfferV1, Vec<String>)>,
    selection_source: Option<(ExtDataControlSourceV1, Arc<[u8]>)>,
    primary_source: Option<(ExtDataControlSourceV1, Arc<[u8]>)>,
}

pub struct WaylandInput {
    conn: Connection,
    vkbd: ZwpVirtualKeyboardV1,
    vptr: ZwlrVirtualPointerV1,
    sizer: SharedSizer,
    epoch: Instant,
    scroll_h_acc: i32,
    scroll_v_acc: i32,
    qh: QueueHandle<State>,
    data_device: ExtDataControlDeviceV1,
    data_control_mgr: ExtDataControlManagerV1,
    clipboard: Arc<Mutex<ClipboardShared>>,
}

impl WaylandInput {
    pub fn new(display: &str, sizer: SharedSizer, ph: PlotterHandle) -> Result<Self> {
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
        let data_control_mgr: ExtDataControlManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .context("ext_data_control_manager_v1 not available")?;

        let vkbd = vk_mgr.create_virtual_keyboard(&seat, &qh, ());
        let vptr = vp_mgr.create_virtual_pointer(Some(&seat), &qh, ());
        let keyboard = seat.get_keyboard(&qh, ());
        let data_device = data_control_mgr.get_data_device(&seat, &qh, ());

        let clipboard = Arc::new(Mutex::new(ClipboardShared {
            selection: None,
            primary: None,
            selection_source: None,
            primary_source: None,
        }));

        let mut state = State {
            registry_state: RegistryState::new(&globals),
            vkbd: vkbd.clone(),
            _vptr: vptr.clone(),
            _keyboard: keyboard,
            last_keymap: None,
            pending_offers: HashMap::new(),
            clipboard: clipboard.clone(),
            res: None,
        };

        event_queue.roundtrip(&mut state)?;

        std::thread::Builder::new()
            .name("wayland-input".into())
            .spawn(move || {
                loop {
                    match event_queue.blocking_dispatch(&mut state) {
                        Ok(_) => {}
                        Err(e) => {
                            state.res = Some(Err(e.into()));
                        }
                    }
                    if let Some(res) = state.res.take() {
                        ph.fatal(res.context("wayland input dispatch"));
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
            qh,
            data_device,
            data_control_mgr,
            clipboard,
        })
    }

    fn millis(&self) -> u32 {
        self.epoch.elapsed().as_millis() as u32
    }

    fn flush(&self) -> Result<()> {
        self.conn.flush()?;
        Ok(())
    }

    /// Queue receive request while caller holds the clipboard lock.
    fn start_receive(&self, offer: &ExtDataControlOfferV1) -> Option<PipeReader> {
        let (reader, writer) = std::io::pipe().ok()?;
        offer.receive(TEXT_MIME.to_string(), writer.as_fd());
        drop(writer);
        self.conn.flush().ok()?;
        Some(reader)
    }

    fn finish_receive(mut reader: PipeReader) -> Option<String> {
        let mut buf = String::new();
        reader.read_to_string(&mut buf).ok()?;
        Some(buf)
    }

    fn get_offer(&self, primary: bool) -> Option<String> {
        let reader = {
            let shared = self.clipboard.lock().unwrap();
            let slot = if primary {
                &shared.primary
            } else {
                &shared.selection
            };
            let (offer, mimes) = slot.as_ref()?;
            if !mimes.iter().any(|m| m == TEXT_MIME) {
                return None;
            }
            self.start_receive(offer)?
        };
        Self::finish_receive(reader)
    }

    fn set_source(&self, contents: String, primary: bool) {
        let source = self.data_control_mgr.create_data_source(&self.qh, ());
        source.offer(TEXT_MIME.to_string());
        let data: Arc<[u8]> = Arc::from(contents.into_bytes());
        {
            let mut shared = self.clipboard.lock().unwrap();
            let slot = if primary {
                &mut shared.primary_source
            } else {
                &mut shared.selection_source
            };
            if let Some((old, _)) = slot.take() {
                old.destroy();
            }
            *slot = Some((source.clone(), data));
        }
        if primary {
            self.data_device.set_primary_selection(Some(&source));
        } else {
            self.data_device.set_selection(Some(&source));
        }
        let _ = self.conn.flush();
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
            (
                v_abs,
                v120,
                &mut self.scroll_v_acc,
                wl_pointer::Axis::VerticalScroll,
            ),
            (
                h_abs,
                h120,
                &mut self.scroll_h_acc,
                wl_pointer::Axis::HorizontalScroll,
            ),
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
            self.vptr.axis_stop(time, wl_pointer::Axis::VerticalScroll);
        }
        if horizontal {
            self.vptr
                .axis_stop(time, wl_pointer::Axis::HorizontalScroll);
        }
        self.vptr.frame();
        self.flush()
    }

    fn update_modifiers(
        &mut self,
        depressed: u32,
        latched: u32,
        locked: u32,
        group: u32,
    ) -> Result<()> {
        self.vkbd.modifiers(depressed, latched, locked, group);
        self.flush()
    }

    fn get_primary(&mut self) -> Option<String> {
        self.get_offer(true)
    }

    fn set_primary(&mut self, contents: String) {
        self.set_source(contents, true);
    }

    fn get_clipboard(&mut self) -> Option<String> {
        self.get_offer(false)
    }

    fn set_clipboard(&mut self, contents: String) {
        self.set_source(contents, false);
    }
}

struct State {
    registry_state: RegistryState,
    vkbd: ZwpVirtualKeyboardV1,
    _vptr: ZwlrVirtualPointerV1,
    _keyboard: WlKeyboard,
    last_keymap: Option<Vec<u8>>,
    pending_offers: HashMap<ExtDataControlOfferV1, Vec<String>>,
    clipboard: Arc<Mutex<ClipboardShared>>,
    res: Option<Result<()>>,
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

// ext-data-control dispatch impls

impl Dispatch<ExtDataControlManagerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ExtDataControlManagerV1,
        _event: <ExtDataControlManagerV1 as smithay_client_toolkit::reexports::client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _proxy: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_device_v1::Event::DataOffer { id } => {
                state.pending_offers.insert(id, Vec::new());
            }
            ext_data_control_device_v1::Event::Selection { id } => {
                let mut shared = state.clipboard.lock().unwrap();
                if let Some((old, _)) = shared.selection.take() {
                    old.destroy();
                }
                shared.selection = id.and_then(|offer| {
                    state
                        .pending_offers
                        .remove(&offer)
                        .map(|mimes| (offer, mimes))
                });
            }
            ext_data_control_device_v1::Event::PrimarySelection { id } => {
                let mut shared = state.clipboard.lock().unwrap();
                if let Some((old, _)) = shared.primary.take() {
                    old.destroy();
                }
                shared.primary = id.and_then(|offer| {
                    state
                        .pending_offers
                        .remove(&offer)
                        .map(|mimes| (offer, mimes))
                });
            }
            ext_data_control_device_v1::Event::Finished => {
                state.res = Some(Err(anyhow!("data control device finished")));
            }
            _ => {}
        }
    }

    event_created_child!(State, ExtDataControlDeviceV1, [
        0 => (ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event
            && let Some(mimes) = state.pending_offers.get_mut(proxy)
        {
            mimes.push(mime_type);
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_source_v1::Event::Send { mime_type: _, fd } => {
                let shared = state.clipboard.lock().unwrap();
                let find =
                    |slot: &Option<(ExtDataControlSourceV1, Arc<[u8]>)>| -> Option<Arc<[u8]>> {
                        let (s, d) = slot.as_ref()?;
                        (s == proxy).then(|| d.clone())
                    };
                let data = find(&shared.selection_source).or_else(|| find(&shared.primary_source));
                drop(shared);
                if let Some(data) = data {
                    let mut file = std::fs::File::from(fd);
                    let _ = file.write_all(&data);
                }
            }
            ext_data_control_source_v1::Event::Cancelled => {
                let mut shared = state.clipboard.lock().unwrap();
                let find = |slot: &mut Option<(ExtDataControlSourceV1, Arc<[u8]>)>| -> bool {
                    slot.as_ref().is_some_and(|(s, _)| s == proxy) && {
                        slot.take();
                        true
                    }
                };
                let found = find(&mut shared.selection_source) || find(&mut shared.primary_source);
                drop(shared);
                if found {
                    proxy.destroy();
                }
            }
            _ => {}
        }
    }
}
