use crate::GlobalState;
use crate::capture::backend::InputInjector;
use crate::capture::plotter::PlotterHandle;
use crate::sizer::SharedSizer;
use anyhow::{Context as _, Result};
use copypasta::wayland_clipboard::{
    self, Clipboard as WaylandClipboard, Primary as WaylandPrimary,
};
use copypasta::x11_clipboard::{
    Clipboard as X11Clipboard, Primary as X11Primary, X11ClipboardContext,
};
use smithay_client_toolkit::compositor::{Region, SurfaceData};
use smithay_client_toolkit::reexports::calloop::EventLoop;
use smithay_client_toolkit::reexports::calloop::channel::{Channel, Event, Sender};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::client::globals::GlobalList;
use smithay_client_toolkit::reexports::client::protocol::wl_keyboard::{self, WlKeyboard};
use smithay_client_toolkit::reexports::client::protocol::wl_pointer::WlPointer;
use smithay_client_toolkit::reexports::client::protocol::wl_seat::WlSeat;
use smithay_client_toolkit::reexports::client::protocol::wl_shm::WlShm;
use smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface;
use smithay_client_toolkit::reexports::client::{Connection, Dispatch, Proxy, QueueHandle};
use smithay_client_toolkit::reexports::protocols::wp::keyboard_shortcuts_inhibit::zv1::client::zwp_keyboard_shortcuts_inhibitor_v1;
use smithay_client_toolkit::reexports::protocols::wp::pointer_constraints::zv1::client::zwp_pointer_constraints_v1;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState, SimpleGlobal};
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
    delegate_keyboard, delegate_pointer, delegate_pointer_constraints, delegate_registry,
    delegate_relative_pointer, delegate_seat, delegate_simple, registry_handlers,
};
use std::sync::Arc;
use tracing::{error, info};

// breaks rustfmt import sorting for some reason
use smithay_client_toolkit::reexports::protocols::wp::keyboard_shortcuts_inhibit::zv1::client::zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1;
use smithay_client_toolkit::reexports::protocols::wp::keyboard_shortcuts_inhibit::zv1::client::zwp_keyboard_shortcuts_inhibitor_v1::ZwpKeyboardShortcutsInhibitorV1;
use smithay_client_toolkit::reexports::protocols::wp::pointer_constraints::zv1::client::zwp_confined_pointer_v1::ZwpConfinedPointerV1;
use smithay_client_toolkit::reexports::protocols::wp::pointer_constraints::zv1::client::zwp_locked_pointer_v1::ZwpLockedPointerV1;
use smithay_client_toolkit::reexports::protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_v1::ZwpRelativePointerV1;

pub enum InputThreadCommand {
    UpdateConfinement { region: Region },
}

pub type InputThreadHandle = Sender<InputThreadCommand>;

pub struct InputThreadInit {
    pub conn: Connection,
    pub globals: Arc<GlobalList>,
    pub surface: WlSurface,
    pub cursor_surface: WlSurface,
    pub wl_shm: WlShm,
    pub sizer: SharedSizer,
    pub global_state: GlobalState,
    pub ph: PlotterHandle,
    pub rx_input: Channel<InputThreadCommand>,
    pub confined: bool,
    pub injector: Box<dyn InputInjector>,
}

struct InputState {
    // Handles & Shared State
    qh: QueueHandle<Self>,
    global_state: GlobalState,
    sizer: SharedSizer,
    injector: Box<dyn InputInjector>,
    _ph: PlotterHandle,

    // Wayland State
    registry_state: RegistryState,
    seat_state: SeatState,
    relative_pointer_state: RelativePointerState,
    pointer_constraints_state: PointerConstraintsState,
    shortcuts_inhibit_manager: SimpleGlobal<ZwpKeyboardShortcutsInhibitManagerV1, 1>,

    // Wayland Objects
    surface: WlSurface,
    cursor_surface: WlSurface,
    wl_shm: WlShm,
    keyboard: Option<WlKeyboard>,
    pointer: Option<ThemedPointer>,
    relative_pointer: Option<ZwpRelativePointerV1>,
    confined_pointer: Option<ZwpConfinedPointerV1>,
    shortcuts_inhibitor: Option<ZwpKeyboardShortcutsInhibitorV1>,

    // Internal State
    modifiers: Modifiers,
    pointer_serial: u32,
    confinement_region: Option<Region>,
    confined: bool,
    force_relative: bool,
    cursor_over_surface: bool,
    keyboard_focus: bool,

    // Clipboards
    wl_primary: WaylandPrimary,
    wl_clipboard: WaylandClipboard,
    x11_primary: X11ClipboardContext<X11Primary>,
    x11_clipboard: X11ClipboardContext<X11Clipboard>,
}

fn sync_clipboards<P1, P2>(p1: &mut P1, p2: &mut P2)
where
    P1: copypasta::ClipboardProvider,
    P2: copypasta::ClipboardProvider,
{
    if let Ok(c) = p1.get_contents()
        && let Err(e) = p2.set_contents(c)
    {
        error!("failed to set clipboard: {e}");
    }
}

impl InputState {
    fn set_confined(&mut self, conn: &Connection, confined: bool) {
        if self.confined == confined {
            return;
        }
        self.confined = confined;

        if self.confined {
            sync_clipboards(&mut self.wl_primary, &mut self.x11_primary);
            sync_clipboards(&mut self.wl_clipboard, &mut self.x11_clipboard);
        } else {
            sync_clipboards(&mut self.x11_primary, &mut self.wl_primary);
            sync_clipboards(&mut self.x11_clipboard, &mut self.wl_clipboard);
        }

        self.update_confine();
        self.update_shortcut_inhibitor();
        self.global_state.rcu(|s| s.with_confine(self.confined));
        self.update_cursor(conn);
    }

    fn update_shortcut_inhibitor(&mut self) {
        if self.confined {
            if self.shortcuts_inhibitor.is_some() {
                return;
            }
            if let Some(seat) = self.seat_state.seats().next() {
                self.shortcuts_inhibitor = Some(
                    self.shortcuts_inhibit_manager
                        .get()
                        .unwrap()
                        .inhibit_shortcuts(&self.surface, &seat, &self.qh, ()),
                );
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

    fn update_confine(&mut self) -> Option<()> {
        if !self.confined {
            if let Some(p) = self.confined_pointer.take() {
                p.destroy();
            }
            return None;
        }
        let pointer = self.pointer.as_ref()?;
        let region = self.confinement_region.as_ref()?;
        if let Some(p) = self.confined_pointer.as_mut() {
            p.set_region(Some(region.wl_region()));
        } else {
            self.confined_pointer = self.confined.then_some(
                self.pointer_constraints_state
                    .confine_pointer(
                        &self.surface,
                        pointer.pointer(),
                        Some(region.wl_region()),
                        zwp_pointer_constraints_v1::Lifetime::Persistent,
                        &self.qh,
                    )
                    .unwrap(),
            );
        }
        None
    }
}

impl PointerConstraintsHandler for InputState {
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

impl RelativePointerHandler for InputState {
    fn relative_pointer_motion(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _relative_pointer: &ZwpRelativePointerV1,
        _pointer: &WlPointer,
        event: RelativeMotionEvent,
    ) {
        // info!("relative {} {}", event.delta.0, event.delta.1);
        if self.global_state.load().cursor_visible && !self.force_relative {
            return;
        }
        if !self.confined {
            return;
        }
        let sizer = self.sizer.load();
        let (x, y) = sizer.window_to_source_delta(event.delta);
        self.injector.mouse_delta(x, y).unwrap();
    }
}

impl PointerHandler for InputState {
    fn pointer_frame(
        &mut self,
        conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &WlPointer,
        events: &[PointerEvent],
    ) {
        use PointerEventKind::*;
        for event in events {
            if event.surface != self.surface {
                continue;
            }
            match event.kind {
                Enter { serial, .. } => {
                    // info!("Pointer entered @{:?}", event.position);
                    self.pointer_serial = serial;
                    self.cursor_over_surface = true;
                    self.update_cursor(conn);
                }
                Leave { .. } => {
                    // info!("Pointer left");
                    self.cursor_over_surface = false;
                    self.update_cursor(conn);
                }
                Motion { .. } => {
                    if let Some((sx, sy)) = self
                        .sizer
                        .load()
                        .window_to_source((event.position.0 as u32, event.position.1 as u32))
                        && self.global_state.load().cursor_visible
                        && !self.force_relative
                        && self.confined
                    {
                        self.injector.mouse_absolute(sx as i32, sy as i32).unwrap();
                    }
                    // info!("motion {event:#?}");
                }
                Press { button, .. } => {
                    // info!("Press {:x} @ {:?}", button, event.position);
                    // info!("button {button}");
                    if self.confined {
                        self.injector.mouse_press(button).unwrap();
                    }
                }
                Release { button, .. } => {
                    // info!("Release {:x} @ {:?}", button, event.position);
                    if button == BTN_LEFT && !self.confined {
                        self.set_confined(conn, true);
                    } else if self.confined {
                        self.injector.mouse_release(button).unwrap();
                    }
                }
                Axis {
                    horizontal,
                    vertical,
                    ..
                } => {
                    // info!("Scroll H:{horizontal:?}, V:{vertical:?}");
                    if self.confined {
                        self.injector
                            .scroll(horizontal.value120, vertical.value120)
                            .unwrap();
                    }
                }
            }
        }
    }
}

impl KeyboardHandler for InputState {
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
        if self.surface != *surface {
            return;
        }
        // info!("Keyboard focus on window with pressed syms: {keysyms:?}");
        self.keyboard_focus = true;
    }
    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &WlSurface,
        _: u32,
    ) {
        if self.surface != *surface {
            info!("not my surface");
            return;
        }
        // info!("Release keyboard focus on window");
        self.keyboard_focus = false;
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
        if self.modifiers.logo {
            return;
        }
        if self.confined {
            self.injector.key_press(event.raw_code).unwrap();
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
        if self.modifiers.logo {
            match event.keysym {
                Keysym::Escape => {
                    self.set_confined(conn, !self.confined);
                }
                Keysym::r => {
                    self.force_relative = !self.force_relative;
                    self.global_state
                        .rcu(|s| s.with_force_relative(self.force_relative));
                }
                Keysym::c => {
                    self.global_state.rcu(|s| s.with_capture(!s.capture));
                }
                _ => {}
            }
            return;
        }
        if self.confined {
            self.injector.key_release(event.raw_code).unwrap();
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
        self.modifiers = modifiers;
    }
}

impl SeatHandler for InputState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
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
        if cap == Capability::Pointer && self.pointer.is_none() {
            let themed_pointer = self
                .seat_state
                .get_pointer_with_theme(
                    qh,
                    &seat,
                    &self.wl_shm,
                    self.cursor_surface.clone(),
                    ThemeSpec::default(),
                )
                .expect("Failed to create themed pointer");

            self.relative_pointer = Some(
                self.relative_pointer_state
                    .get_relative_pointer(themed_pointer.pointer(), qh)
                    .expect("Failed to create relative pointer"),
            );
            self.pointer = Some(themed_pointer);
            self.update_confine();
        }
        if cap == Capability::Keyboard && self.keyboard.is_none() {
            // info!("Set keyboard capability");
            let keyboard = self
                .seat_state
                .get_keyboard(qh, &seat, None)
                .expect("Failed to create keyboard");
            self.keyboard = Some(keyboard);
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: WlSeat,
        cap: Capability,
    ) {
        if cap == Capability::Keyboard && self.keyboard.is_some() {
            // info!("Unset keyboard capability");
            self.keyboard.take().unwrap().release();
        }
        if cap == Capability::Pointer && self.pointer.is_some() {
            // info!("Unset pointer capability");
            self.pointer.take().unwrap().pointer().release();
            self.relative_pointer.take().unwrap().destroy();
            if let Some(confined) = self.confined_pointer.take() {
                confined.destroy();
            }
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}
}

impl ProvidesRegistryState for InputState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![SeatState];
}

impl Dispatch<ZwpKeyboardShortcutsInhibitorV1, ()> for InputState {
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

impl AsMut<SimpleGlobal<ZwpKeyboardShortcutsInhibitManagerV1, 1>> for InputState {
    fn as_mut(&mut self) -> &mut SimpleGlobal<ZwpKeyboardShortcutsInhibitManagerV1, 1> {
        &mut self.shortcuts_inhibit_manager
    }
}

impl Dispatch<WlSurface, SurfaceData> for InputState {
    fn event(
        _state: &mut Self,
        _proxy: &WlSurface,
        _event: <WlSurface as smithay_client_toolkit::reexports::client::Proxy>::Event,
        _data: &SurfaceData,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
    }
}

delegate_seat!(InputState);
delegate_pointer!(InputState);
delegate_relative_pointer!(InputState);
delegate_keyboard!(InputState);
delegate_pointer_constraints!(InputState);
delegate_registry!(InputState);
delegate_simple!(InputState, ZwpKeyboardShortcutsInhibitManagerV1, 1);

fn run_internal(init: InputThreadInit) -> Result<()> {
    let mut event_loop: EventLoop<InputState> = EventLoop::try_new()?;
    let loop_handle = event_loop.handle();

    let conn = init.conn;
    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();
    let (wl_primary, wl_clipboard) = unsafe {
        wayland_clipboard::create_clipboards_from_external(conn.display().id().as_ptr() as *mut _)
    };
    let x11_primary = X11ClipboardContext::<X11Primary>::new().unwrap();
    let x11_clipboard = X11ClipboardContext::<X11Clipboard>::new().unwrap();

    let mut state = InputState {
        // Handles & Shared State
        qh: qh.clone(),
        global_state: init.global_state,
        sizer: init.sizer,
        injector: init.injector,
        _ph: init.ph,

        // Wayland State
        registry_state: RegistryState::new(&init.globals),
        seat_state: SeatState::new(&init.globals, &qh),
        relative_pointer_state: RelativePointerState::bind(&init.globals, &qh),
        pointer_constraints_state: PointerConstraintsState::bind(&init.globals, &qh),
        shortcuts_inhibit_manager: SimpleGlobal::bind(&init.globals, &qh)?,

        // Wayland Objects
        surface: init.surface,
        cursor_surface: init.cursor_surface,
        wl_shm: init.wl_shm,
        keyboard: None,
        pointer: None,
        relative_pointer: None,
        confined_pointer: None,
        shortcuts_inhibitor: None,

        // Internal State
        modifiers: Modifiers::default(),
        pointer_serial: 0,
        confinement_region: None,
        confined: init.confined,
        force_relative: false,
        cursor_over_surface: false,
        keyboard_focus: false,

        // Clipboards
        wl_primary,
        wl_clipboard,
        x11_primary,
        x11_clipboard,
    };

    event_queue
        .roundtrip(&mut state)
        .context("Failed to discover seats")?;

    loop_handle
        .insert_source(init.rx_input, |event, _, state| {
            if let Event::Msg(cmd) = event {
                match cmd {
                    InputThreadCommand::UpdateConfinement { region } => {
                        state.confinement_region = Some(region);
                        state.update_confine();
                    }
                }
            }
        })
        .unwrap();

    WaylandSource::new(conn, event_queue)
        .insert(loop_handle)
        .unwrap();

    loop {
        event_loop.dispatch(None, &mut state)?;
    }
}

pub fn run(init: InputThreadInit) {
    let ph = init.ph.clone();
    ph.fatal(run_internal(init).context("input thread"));
}
