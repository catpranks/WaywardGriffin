use crate::capture::input::InputBridge;
use copypasta::ClipboardProvider as _;
use copypasta::wayland_clipboard::{Clipboard as WaylandClipboard, Primary as WaylandPrimary};
use smithay_client_toolkit::compositor::Region;
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
use smithay_client_toolkit::{
    delegate_keyboard, delegate_pointer, delegate_pointer_constraints, delegate_relative_pointer,
    delegate_seat, delegate_simple,
};
use tracing::info;

// breaks rustfmt import sorting for some reason
use smithay_client_toolkit::reexports::protocols::wp::keyboard_shortcuts_inhibit::zv1::client::zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1;
use smithay_client_toolkit::reexports::protocols::wp::keyboard_shortcuts_inhibit::zv1::client::zwp_keyboard_shortcuts_inhibitor_v1::ZwpKeyboardShortcutsInhibitorV1;
use smithay_client_toolkit::reexports::protocols::wp::pointer_constraints::zv1::client::zwp_confined_pointer_v1::ZwpConfinedPointerV1;
use smithay_client_toolkit::reexports::protocols::wp::pointer_constraints::zv1::client::zwp_locked_pointer_v1::ZwpLockedPointerV1;
use smithay_client_toolkit::reexports::protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_v1::ZwpRelativePointerV1;
use super::App;

pub struct InputState {
    // Wayland State
    pub seat_state: SeatState,
    pub relative_pointer_state: RelativePointerState,
    pub pointer_constraints_state: PointerConstraintsState,
    pub shortcuts_inhibit_manager: SimpleGlobal<ZwpKeyboardShortcutsInhibitManagerV1, 1>,

    // Wayland Objects
    pub cursor_surface: WlSurface,
    pub keyboard: Option<WlKeyboard>,
    pub pointer: Option<ThemedPointer>,
    pub relative_pointer: Option<ZwpRelativePointerV1>,
    pub confined_pointer: Option<ZwpConfinedPointerV1>,
    pub shortcuts_inhibitor: Option<ZwpKeyboardShortcutsInhibitorV1>,

    // Internal State
    pub modifiers: Modifiers,
    pub pointer_serial: u32,
    pub confinement_region: Option<Region>,
    pub confined: bool,
    pub force_relative: bool,
    pub cursor_over_surface: bool,
    pub keyboard_focus: bool,

    // Clipboards
    pub wl_primary: WaylandPrimary,
    pub wl_clipboard: WaylandClipboard,

    // Source bridge (input injection + clipboard)
    pub bridge: Box<dyn InputBridge>,
}

impl App {
    fn set_confined(&mut self, conn: &Connection, confined: bool) {
        if self.input.confined == confined {
            return;
        }
        self.input.confined = confined;

        if self.input.confined {
            if let Ok(c) = self.input.wl_primary.get_contents() {
                self.input.bridge.set_primary(c);
            }
            if let Ok(c) = self.input.wl_clipboard.get_contents() {
                self.input.bridge.set_clipboard(c);
            }
        } else {
            if let Some(c) = self.input.bridge.get_primary() {
                let _ = self.input.wl_primary.set_contents(c);
            }
            if let Some(c) = self.input.bridge.get_clipboard() {
                let _ = self.input.wl_clipboard.set_contents(c);
            }
        }

        self.input_update_confine();
        self.input_update_shortcut_inhibitor();
        self.dc
            .global_state
            .rcu(|s| s.with_confine(self.input.confined));
        self.input_update_cursor(conn);
    }

    fn input_update_shortcut_inhibitor(&mut self) {
        if self.input.confined {
            if self.input.shortcuts_inhibitor.is_some() {
                return;
            }
            if let Some(seat) = self.input.seat_state.seats().next() {
                self.input.shortcuts_inhibitor = Some(
                    self.input
                        .shortcuts_inhibit_manager
                        .get()
                        .unwrap()
                        .inhibit_shortcuts(&self.dc.surface, &seat, &self.dc.qh, ()),
                );
            }
        } else if let Some(inhibitor) = self.input.shortcuts_inhibitor.take() {
            inhibitor.destroy();
        }
    }

    fn input_update_cursor(&mut self, conn: &Connection) {
        if let Some(pointer) = self.input.pointer.as_mut() {
            if !self.input.confined || !self.input.cursor_over_surface {
                pointer
                    .set_cursor(conn, CursorIcon::Crosshair)
                    .expect("Failed to set cursor");
            } else {
                pointer
                    .pointer()
                    .set_cursor(self.input.pointer_serial, None, 0, 0);
            }
        }
    }

    pub fn input_update_confine(&mut self) -> Option<()> {
        if !self.input.confined {
            if let Some(p) = self.input.confined_pointer.take() {
                p.destroy();
            }
            return None;
        }
        let pointer = self.input.pointer.as_ref()?;
        let region = self.input.confinement_region.as_ref()?;
        if let Some(p) = self.input.confined_pointer.as_mut() {
            p.set_region(Some(region.wl_region()));
        } else {
            self.input.confined_pointer = self.input.confined.then_some(
                self.input
                    .pointer_constraints_state
                    .confine_pointer(
                        &self.dc.surface,
                        pointer.pointer(),
                        Some(region.wl_region()),
                        zwp_pointer_constraints_v1::Lifetime::Persistent,
                        &self.dc.qh,
                    )
                    .unwrap(),
            );
        }
        None
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
        // info!("relative {} {}", event.delta.0, event.delta.1);
        if self.dc.global_state.load().cursor_visible && !self.input.force_relative {
            return;
        }
        if !self.input.confined {
            return;
        }
        let sizer = self.dc.sizer.load();
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
                    // info!("Pointer entered @{:?}", event.position);
                    self.input.pointer_serial = serial;
                    self.input.cursor_over_surface = true;
                    self.input_update_cursor(conn);
                }
                Leave { .. } => {
                    // info!("Pointer left");
                    self.input.cursor_over_surface = false;
                    self.input_update_cursor(conn);
                }
                Motion { .. } => {
                    if let Some((sx, sy)) = self
                        .dc
                        .sizer
                        .load()
                        .window_to_source((event.position.0 as u32, event.position.1 as u32))
                        && self.dc.global_state.load().cursor_visible
                        && !self.input.force_relative
                        && self.input.confined
                    {
                        self.input
                            .bridge
                            .mouse_absolute(sx as i32, sy as i32)
                            .unwrap();
                    }
                    // info!("motion {event:#?}");
                }
                Press { button, .. } => {
                    // info!("Press {:x} @ {:?}", button, event.position);
                    // info!("button {button}");
                    if self.input.confined {
                        self.input.bridge.mouse_press(button).unwrap();
                    }
                }
                Release { button, .. } => {
                    // info!("Release {:x} @ {:?}", button, event.position);
                    if button == BTN_LEFT && !self.input.confined {
                        self.set_confined(conn, true);
                    } else if self.input.confined {
                        self.input.bridge.mouse_release(button).unwrap();
                    }
                }
                Axis {
                    horizontal,
                    vertical,
                    ..
                } => {
                    // info!("Scroll H:{horizontal:?}, V:{vertical:?}");
                    if self.input.confined {
                        self.input
                            .bridge
                            .scroll(horizontal.value120, vertical.value120)
                            .unwrap();
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
        surface: &WlSurface,
        _: u32,
        _: &[u32],
        _keysyms: &[Keysym],
    ) {
        if self.dc.surface != *surface {
            return;
        }
        // info!("Keyboard focus on window with pressed syms: {keysyms:?}");
        self.input.keyboard_focus = true;
    }
    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &WlSurface,
        _: u32,
    ) {
        if self.dc.surface != *surface {
            info!("not my surface");
            return;
        }
        // info!("Release keyboard focus on window");
        self.input.keyboard_focus = false;
    }
    fn press_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        // info!("Key press: {event:?}");
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
        // info!("Key repeat: {event:?}");
    }

    fn release_key(
        &mut self,
        conn: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        // info!("Key release: {event:?} R: {:?}", Keysym::R);
        if event.keysym == Keysym::Super_L {
            return;
        }
        if self.input.modifiers.logo {
            match event.keysym {
                Keysym::Escape => {
                    self.set_confined(conn, !self.input.confined);
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
        _raw_modifiers: RawModifiers,
        _layout: u32,
    ) {
        // info!("Update modifiers: {modifiers:?}");
        self.input.modifiers = modifiers;
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
        // info!("seat cap {cap:?}");
        if cap == Capability::Pointer && self.input.pointer.is_none() {
            let themed_pointer = self
                .input
                .seat_state
                .get_pointer_with_theme(
                    qh,
                    &seat,
                    self.shm_state.wl_shm(),
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
            self.input_update_confine();
        }
        if cap == Capability::Keyboard && self.input.keyboard.is_none() {
            // info!("Set keyboard capability");
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
            // info!("Unset keyboard capability");
            self.input.keyboard.take().unwrap().release();
        }
        if cap == Capability::Pointer && self.input.pointer.is_some() {
            // info!("Unset pointer capability");
            self.input.pointer.take().unwrap().pointer().release();
            self.input.relative_pointer.take().unwrap().destroy();
            if let Some(confined) = self.input.confined_pointer.take() {
                confined.destroy();
            }
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}
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
                state.set_confined(conn, false);
            }
            _ => {}
        }
    }
}

impl AsMut<SimpleGlobal<ZwpKeyboardShortcutsInhibitManagerV1, 1>> for App {
    fn as_mut(&mut self) -> &mut SimpleGlobal<ZwpKeyboardShortcutsInhibitManagerV1, 1> {
        &mut self.input.shortcuts_inhibit_manager
    }
}

delegate_seat!(App);
delegate_pointer!(App);
delegate_relative_pointer!(App);
delegate_keyboard!(App);
delegate_pointer_constraints!(App);
delegate_simple!(App, ZwpKeyboardShortcutsInhibitManagerV1, 1);
