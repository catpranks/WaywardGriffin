use std::os::linux::net::SocketAddrExt as _;
use std::os::unix::net::{SocketAddr, UnixDatagram};

use crate::capture::input::InputBridge;
use crate::sizer::SharedSizer;
use anyhow::{Result, anyhow};
use copypasta::ClipboardProvider as _;
use copypasta::wayland_clipboard::{
    self, Clipboard as WaylandClipboard, Primary as WaylandPrimary,
};
use smithay_client_toolkit::compositor::Region;
use smithay_client_toolkit::reexports::calloop::generic::Generic;
use smithay_client_toolkit::reexports::calloop::{Interest, LoopHandle, Mode, PostAction};
use smithay_client_toolkit::reexports::client::globals::GlobalList;
use smithay_client_toolkit::reexports::client::protocol::wl_keyboard::{self, WlKeyboard};
use smithay_client_toolkit::reexports::client::protocol::wl_pointer::WlPointer;
use smithay_client_toolkit::reexports::client::protocol::wl_seat::WlSeat;
use smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface;
use smithay_client_toolkit::reexports::client::{Connection, Dispatch, Proxy, QueueHandle};
use smithay_client_toolkit::reexports::protocols::wp::keyboard_shortcuts_inhibit::zv1::client::zwp_keyboard_shortcuts_inhibitor_v1;
use smithay_client_toolkit::reexports::protocols::wp::pointer_constraints::zv1::client::zwp_pointer_constraints_v1;
use smithay_client_toolkit::registry::SimpleGlobal;
use smithay_client_toolkit::seat::keyboard::{
    KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers,
};
use smithay_client_toolkit::seat::pointer::{
    BTN_LEFT, CursorIcon, PointerEvent, PointerEventKind, PointerHandler, ThemeSpec, ThemedPointer,
};
use smithay_client_toolkit::seat::pointer_constraints::{
    PointerConstraintsHandler, PointerConstraintsState,
};
use smithay_client_toolkit::seat::relative_pointer::{
    RelativeMotionEvent, RelativePointerHandler, RelativePointerState,
};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_keyboard, delegate_pointer, delegate_pointer_constraints, delegate_relative_pointer,
    delegate_seat, delegate_shm, delegate_simple,
};
use tracing::info;

use super::{App, DisplayCtx};

// breaks rustfmt import sorting for some reason
use smithay_client_toolkit::reexports::protocols::wp::keyboard_shortcuts_inhibit::zv1::client::zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1;
use smithay_client_toolkit::reexports::protocols::wp::keyboard_shortcuts_inhibit::zv1::client::zwp_keyboard_shortcuts_inhibitor_v1::ZwpKeyboardShortcutsInhibitorV1;
use smithay_client_toolkit::reexports::protocols::wp::pointer_constraints::zv1::client::zwp_confined_pointer_v1::ZwpConfinedPointerV1;
use smithay_client_toolkit::reexports::protocols::wp::pointer_constraints::zv1::client::zwp_locked_pointer_v1::ZwpLockedPointerV1;
use smithay_client_toolkit::reexports::protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_v1::ZwpRelativePointerV1;

pub struct InputState {
    // Wayland globals (input-specific)
    shm_state: Shm,
    seat_state: SeatState,
    relative_pointer_state: RelativePointerState,
    pointer_constraints_state: PointerConstraintsState,
    shortcuts_inhibit_manager: Option<SimpleGlobal<ZwpKeyboardShortcutsInhibitManagerV1, 1>>,

    // Wayland objects
    cursor_surface: WlSurface,
    keyboard: Option<WlKeyboard>,
    pointer: Option<ThemedPointer>,
    relative_pointer: Option<ZwpRelativePointerV1>,
    confined_pointer: Option<ZwpConfinedPointerV1>,
    shortcuts_inhibitor: Option<ZwpKeyboardShortcutsInhibitorV1>,

    // Internal state
    modifiers: Modifiers,
    pointer_serial: u32,
    confinement_region: Option<Region>,
    confined: bool,
    force_relative: bool,
    cursor_over_surface: bool,

    // Clipboards
    wl_primary: WaylandPrimary,
    wl_clipboard: WaylandClipboard,

    // Source bridge (input injection + clipboard)
    bridge: Box<dyn InputBridge>,
}

impl InputState {
    fn set_confined(&mut self, dc: &DisplayCtx, conn: &Connection, confined: bool) {
        if self.confined == confined {
            return;
        }
        self.confined = confined;

        if self.confined {
            if let Ok(c) = self.wl_primary.get_contents() {
                self.bridge.set_primary(c);
            }
            if let Ok(c) = self.wl_clipboard.get_contents() {
                self.bridge.set_clipboard(c);
            }
        } else {
            if let Some(c) = self.bridge.get_primary() {
                let _ = self.wl_primary.set_contents(c);
            }
            if let Some(c) = self.bridge.get_clipboard() {
                let _ = self.wl_clipboard.set_contents(c);
            }
        }

        self.update_confine(dc);
        self.update_shortcut_inhibitor(dc);
        dc.global_state.rcu(|s| s.with_confine(self.confined));
        self.update_cursor(conn);
    }

    fn update_shortcut_inhibitor(&mut self, dc: &DisplayCtx) {
        let Some(ref manager) = self.shortcuts_inhibit_manager else {
            return;
        };
        if self.confined {
            if self.shortcuts_inhibitor.is_some() {
                return;
            }
            if let Some(seat) = self.seat_state.seats().next() {
                self.shortcuts_inhibitor = Some(manager.get().unwrap().inhibit_shortcuts(
                    &dc.surface,
                    &seat,
                    &dc.qh,
                    (),
                ));
            }
        } else if let Some(inhibitor) = self.shortcuts_inhibitor.take() {
            inhibitor.destroy();
        }
    }

    fn update_cursor(&mut self, conn: &Connection) {
        if let Some(pointer) = self.pointer.as_mut() {
            if !self.confined || !self.cursor_over_surface {
                pointer
                    .set_cursor(conn, CursorIcon::Crosshair)
                    .expect("Failed to set cursor");
            } else {
                pointer
                    .pointer()
                    .set_cursor(self.pointer_serial, None, 0, 0);
            }
        }
    }

    fn update_confine(&mut self, dc: &DisplayCtx) {
        if !self.confined {
            if let Some(p) = self.confined_pointer.take() {
                p.destroy();
            }
            return;
        }
        let Some(pointer) = self.pointer.as_ref() else {
            return;
        };
        let Some(region) = self.confinement_region.as_ref() else {
            return;
        };
        if let Some(p) = self.confined_pointer.as_mut() {
            p.set_region(Some(region.wl_region()));
        } else {
            self.confined_pointer = Some(
                self.pointer_constraints_state
                    .confine_pointer(
                        &dc.surface,
                        pointer.pointer(),
                        Some(region.wl_region()),
                        zwp_pointer_constraints_v1::Lifetime::Persistent,
                        &dc.qh,
                    )
                    .unwrap(),
            );
        }
    }

    pub fn handle_resize(&mut self, dc: &DisplayCtx, sizer: &SharedSizer) {
        let sizer = sizer.load();
        if !sizer.ready() {
            return;
        }
        let rect = sizer.window_sizing.content;
        let region = Region::new(&dc.compositor_state).expect("Failed to create region");
        region.add(
            rect.x as i32,
            rect.y as i32,
            rect.width as i32,
            rect.height as i32,
        );
        self.confinement_region = Some(region);
        self.update_confine(dc);
    }

    pub fn new(
        conn: &Connection,
        globals: &GlobalList,
        dc: &DisplayCtx,
        bridge: Box<dyn InputBridge>,
        confined: bool,
        loop_handle: &LoopHandle<'static, App>,
    ) -> Result<InputState> {
        let (wl_primary, wl_clipboard) = unsafe {
            wayland_clipboard::create_clipboards_from_external(
                conn.display().id().as_ptr() as *mut _
            )
        };
        let qh = &dc.qh;

        let shm_state = Shm::bind(globals, qh)?;
        let seat_state = SeatState::new(globals, qh);
        let relative_pointer_state = RelativePointerState::bind(globals, qh);
        let pointer_constraints_state = PointerConstraintsState::bind(globals, qh);
        let shortcuts_inhibit_manager = match SimpleGlobal::bind(globals, qh) {
            Ok(v) => Some(v),
            Err(_) => {
                info!(
                    "zwp_keyboard_shortcuts_inhibit_manager_v1 not available, grab won't inhibit compositor shortcuts"
                );
                None
            }
        };
        let cursor_surface = dc.compositor_state.create_surface(qh);

        // Input socket for receiving pointer deltas from external clients
        // Protocol: 12-byte LE datagrams { type: u32, f32, f32 }
        //   type 0 (pointer_motion): { 0u32, dx: f32, dy: f32 }
        let addr = SocketAddr::from_abstract_name("waygriff-0.input")?;
        let sock = UnixDatagram::bind_addr(&addr)?;
        sock.set_nonblocking(true)?;
        info!("input socket: @waygriff-0.input");

        loop_handle
            .insert_source(
                Generic::new(sock, Interest::READ, Mode::Level),
                |_readiness, sock, app: &mut App| {
                    let mut buf = [0u8; 12];
                    while let Ok(n) = sock.recv(&mut buf) {
                        if n != 12 {
                            continue;
                        }
                        let msg_type = u32::from_le_bytes(buf[0..4].try_into().unwrap());
                        #[allow(clippy::single_match)]
                        match msg_type {
                            0 => {
                                let dx = f32::from_le_bytes(buf[4..8].try_into().unwrap());
                                let dy = f32::from_le_bytes(buf[8..12].try_into().unwrap());
                                app.input.bridge.mouse_delta(dx as f64, dy as f64).unwrap();
                            }
                            _ => {}
                        }
                    }
                    Ok(PostAction::Continue)
                },
            )
            .map_err(|e| anyhow!("{}", e.error))?;

        Ok(InputState {
            shm_state,
            seat_state,
            relative_pointer_state,
            pointer_constraints_state,
            shortcuts_inhibit_manager,

            cursor_surface,
            keyboard: None,
            pointer: None,
            relative_pointer: None,
            confined_pointer: None,
            shortcuts_inhibitor: None,

            modifiers: Modifiers::default(),
            pointer_serial: 0,
            confinement_region: None,
            confined,
            force_relative: false,
            cursor_over_surface: false,

            wl_primary,
            wl_clipboard,
            bridge,
        })
    }
}

impl PointerConstraintsHandler for App {
    fn confined(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _confined_pointer: &ZwpConfinedPointerV1,
        _surface: &WlSurface,
        _pointer: &WlPointer,
    ) {
    }

    fn unconfined(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _confined_pointer: &ZwpConfinedPointerV1,
        _surface: &WlSurface,
        _pointer: &WlPointer,
    ) {
    }

    fn locked(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _locked_pointer: &ZwpLockedPointerV1,
        _surface: &WlSurface,
        _pointer: &WlPointer,
    ) {
    }

    fn unlocked(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _locked_pointer: &ZwpLockedPointerV1,
        _surface: &WlSurface,
        _pointer: &WlPointer,
    ) {
    }
}

impl RelativePointerHandler for App {
    fn relative_pointer_motion(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _relative_pointer: &ZwpRelativePointerV1,
        _pointer: &WlPointer,
        event: RelativeMotionEvent,
    ) {
        if self.dc.global_state.load().cursor_visible && !self.input.force_relative {
            return;
        }
        if !self.input.confined {
            return;
        }
        let sizer = self.sizer.load();
        let (x, y) = sizer.window_to_source_delta(event.delta);
        self.input.bridge.mouse_delta(x, y).unwrap();
    }
}

impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &WlPointer,
        events: &[PointerEvent],
    ) {
        use PointerEventKind::*;
        for event in events {
            if event.surface != self.dc.surface {
                continue;
            }
            match event.kind {
                Enter { serial, .. } => {
                    self.input.pointer_serial = serial;
                    self.input.cursor_over_surface = true;
                    self.input.update_cursor(conn);
                }
                Leave { .. } => {
                    self.input.cursor_over_surface = false;
                    self.input.update_cursor(conn);
                }
                Motion { .. } => {
                    if let Some((sx, sy)) = self
                        .sizer
                        .load()
                        .window_to_source((event.position.0 as u32, event.position.1 as u32))
                        && self.dc.global_state.load().cursor_visible
                        && !self.input.force_relative
                        && self.input.confined
                    {
                        self.input.bridge.mouse_absolute(sx, sy).unwrap();
                    }
                }
                Press { button, .. } => {
                    if self.input.confined {
                        self.input.bridge.mouse_press(button).unwrap();
                    }
                }
                Release { button, .. } => {
                    if button == BTN_LEFT && !self.input.confined {
                        self.input.set_confined(&self.dc, conn, true);
                    } else if self.input.confined {
                        self.input.bridge.mouse_release(button).unwrap();
                    }
                }
                Axis {
                    horizontal,
                    vertical,
                    ..
                } => {
                    if self.input.confined {
                        self.input
                            .bridge
                            .scroll(
                                horizontal.absolute,
                                vertical.absolute,
                                horizontal.value120,
                                vertical.value120,
                            )
                            .unwrap();
                        if horizontal.stop || vertical.stop {
                            self.input
                                .bridge
                                .scroll_stop(horizontal.stop, vertical.stop)
                                .unwrap();
                        }
                    }
                }
            }
        }
    }
}

impl KeyboardHandler for App {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
    }
    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &WlSurface,
        _: u32,
    ) {
    }
    fn press_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        if event.keysym == Keysym::Super_L {
            return;
        }
        if self.input.modifiers.logo {
            return;
        }
        if self.input.confined {
            self.input.bridge.key_press(event.raw_code).unwrap();
        }
    }

    fn repeat_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _event: KeyEvent,
    ) {
    }

    fn release_key(
        &mut self,
        conn: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        if event.keysym == Keysym::Super_L {
            return;
        }
        if self.input.modifiers.logo {
            match event.keysym {
                Keysym::Escape => {
                    let confined = !self.input.confined;
                    self.input.set_confined(&self.dc, conn, confined);
                }
                Keysym::r => {
                    self.input.force_relative = !self.input.force_relative;
                    self.dc
                        .global_state
                        .rcu(|s| s.with_force_relative(self.input.force_relative));
                }
                Keysym::c => {
                    self.dc.global_state.rcu(|s| s.with_capture(!s.capture));
                }
                _ => {}
            }
            return;
        }
        if self.input.confined {
            self.input.bridge.key_release(event.raw_code).unwrap();
        }
    }
    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _serial: u32,
        modifiers: Modifiers,
        raw_modifiers: RawModifiers,
        layout: u32,
    ) {
        self.input.modifiers = modifiers;
        if self.input.confined {
            const MOD4_MASK: u32 = 1 << 6;
            self.input
                .bridge
                .update_modifiers(
                    raw_modifiers.depressed & !MOD4_MASK,
                    raw_modifiers.latched & !MOD4_MASK,
                    raw_modifiers.locked & !MOD4_MASK,
                    layout,
                )
                .unwrap();
        }
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.input.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: WlSeat,
        cap: Capability,
    ) {
        if cap == Capability::Pointer && self.input.pointer.is_none() {
            let themed_pointer = self
                .input
                .seat_state
                .get_pointer_with_theme(
                    qh,
                    &seat,
                    self.input.shm_state.wl_shm(),
                    self.input.cursor_surface.clone(),
                    ThemeSpec::default(),
                )
                .expect("Failed to create themed pointer");

            self.input.relative_pointer = Some(
                self.input
                    .relative_pointer_state
                    .get_relative_pointer(themed_pointer.pointer(), qh)
                    .expect("Failed to create relative pointer"),
            );
            self.input.pointer = Some(themed_pointer);
            self.input.update_confine(&self.dc);
        }
        if cap == Capability::Keyboard && self.input.keyboard.is_none() {
            let keyboard = self
                .input
                .seat_state
                .get_keyboard(qh, &seat, None)
                .expect("Failed to create keyboard");
            self.input.keyboard = Some(keyboard);
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: WlSeat,
        cap: Capability,
    ) {
        if cap == Capability::Keyboard && self.input.keyboard.is_some() {
            self.input.keyboard.take().unwrap().release();
        }
        if cap == Capability::Pointer && self.input.pointer.is_some() {
            self.input.pointer.take().unwrap().pointer().release();
            self.input.relative_pointer.take().unwrap().destroy();
            if let Some(confined) = self.input.confined_pointer.take() {
                confined.destroy();
            }
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.input.shm_state
    }
}

impl Dispatch<ZwpKeyboardShortcutsInhibitorV1, ()> for App {
    fn event(
        state: &mut Self,
        _proxy: &ZwpKeyboardShortcutsInhibitorV1,
        event: <ZwpKeyboardShortcutsInhibitorV1 as Proxy>::Event,
        _data: &(),
        conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        match event {
            zwp_keyboard_shortcuts_inhibitor_v1::Event::Active => {}
            zwp_keyboard_shortcuts_inhibitor_v1::Event::Inactive => {
                state.input.set_confined(&state.dc, conn, false);
            }
            _ => {}
        }
    }
}

impl AsMut<SimpleGlobal<ZwpKeyboardShortcutsInhibitManagerV1, 1>> for App {
    fn as_mut(&mut self) -> &mut SimpleGlobal<ZwpKeyboardShortcutsInhibitManagerV1, 1> {
        self.input.shortcuts_inhibit_manager.as_mut().unwrap()
    }
}

delegate_seat!(App);
delegate_pointer!(App);
delegate_relative_pointer!(App);
delegate_keyboard!(App);
delegate_pointer_constraints!(App);
delegate_shm!(App);
delegate_simple!(App, ZwpKeyboardShortcutsInhibitManagerV1, 1);
