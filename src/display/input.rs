use crate::GlobalState;
use crate::capture::input::InputBridge;
use crate::plotter::PlotterHandle;
use crate::sizer::SharedSizer;
use anyhow::{Context as _, Result, anyhow};
use copypasta::ClipboardProvider as _;
use copypasta::wayland_clipboard::{
    self, Clipboard as WaylandClipboard, Primary as WaylandPrimary,
};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, Region};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::EventLoop;
use smithay_client_toolkit::reexports::calloop::channel::{self as calloop_channel, Channel};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::client::protocol::wl_keyboard::{self, WlKeyboard};
use smithay_client_toolkit::reexports::client::protocol::wl_output::{self, WlOutput};
use smithay_client_toolkit::reexports::client::protocol::wl_pointer::WlPointer;
use smithay_client_toolkit::reexports::client::protocol::wl_seat::WlSeat;
use smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface;
use smithay_client_toolkit::reexports::client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
};
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
    delegate_compositor, delegate_keyboard, delegate_output, delegate_pointer,
    delegate_pointer_constraints, delegate_relative_pointer, delegate_seat, delegate_shm,
    delegate_simple,
};
use std::sync::Arc;
use tracing::info;

// breaks rustfmt import sorting for some reason
use smithay_client_toolkit::reexports::protocols::wp::keyboard_shortcuts_inhibit::zv1::client::zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1;
use smithay_client_toolkit::reexports::protocols::wp::keyboard_shortcuts_inhibit::zv1::client::zwp_keyboard_shortcuts_inhibitor_v1::ZwpKeyboardShortcutsInhibitorV1;
use smithay_client_toolkit::reexports::protocols::wp::pointer_constraints::zv1::client::zwp_confined_pointer_v1::ZwpConfinedPointerV1;
use smithay_client_toolkit::reexports::protocols::wp::pointer_constraints::zv1::client::zwp_locked_pointer_v1::ZwpLockedPointerV1;
use smithay_client_toolkit::reexports::protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_v1::ZwpRelativePointerV1;

pub enum InputMsg {
    Resize,
}

pub struct InputApp {
    qh: QueueHandle<InputApp>,

    // Shared state
    surface: WlSurface,
    global_state: GlobalState,
    sizer: SharedSizer,

    // Wayland globals (own bindings)
    compositor_state: CompositorState,
    output_state: OutputState,
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
    res: Option<Result<()>>,
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

impl InputApp {
    fn set_confined(&mut self, conn: &Connection, confined: bool) {
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

        self.update_confine();
        self.update_shortcut_inhibitor();
        self.global_state.rcu(|s| s.with_confine(self.confined));
        self.update_cursor(conn);
    }

    fn update_shortcut_inhibitor(&mut self) {
        let Some(ref manager) = self.shortcuts_inhibit_manager else {
            return;
        };
        if self.confined {
            if self.shortcuts_inhibitor.is_some() {
                return;
            }
            if let Some(seat) = self.seat_state.seats().next() {
                self.shortcuts_inhibitor = Some(manager.get().unwrap().inhibit_shortcuts(
                    &self.surface,
                    &seat,
                    &self.qh,
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

    fn handle_resize(&mut self) {
        let sizer = self.sizer.load();
        if !sizer.ready() {
            return;
        }
        let rect = sizer.window_sizing.content;
        let region = Region::new(&self.compositor_state).expect("Failed to create region");
        region.add(
            rect.x as i32,
            rect.y as i32,
            rect.width as i32,
            rect.height as i32,
        );
        self.confinement_region = Some(region);
        self.update_confine();
    }
}

// CompositorHandler is required for delegate_compositor! but we don't care about
// surface events on the input queue — we only need CompositorState for Region creation.
impl CompositorHandler for InputApp {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _time: u32,
    ) {
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _output: &WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _output: &WlOutput,
    ) {
    }
}

impl OutputHandler for InputApp {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
}

impl PointerConstraintsHandler for InputApp {
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

impl RelativePointerHandler for InputApp {
    fn relative_pointer_motion(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _relative_pointer: &ZwpRelativePointerV1,
        _pointer: &WlPointer,
        event: RelativeMotionEvent,
    ) {
        if self.global_state.load().cursor_visible && !self.force_relative {
            return;
        }
        if !self.confined {
            return;
        }
        let sizer = self.sizer.load();
        let (x, y) = sizer.window_to_source_delta(event.delta);
        self.bridge.mouse_delta(x, y).unwrap();
    }
}

impl PointerHandler for InputApp {
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
                    self.pointer_serial = serial;
                    self.cursor_over_surface = true;
                    self.update_cursor(conn);
                }
                Leave { .. } => {
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
                        self.bridge.mouse_absolute(sx, sy).unwrap();
                    }
                }
                Press { button, .. } => {
                    if self.confined {
                        self.bridge.mouse_press(button).unwrap();
                    }
                }
                Release { button, .. } => {
                    if button == BTN_LEFT && !self.confined {
                        self.set_confined(conn, true);
                    } else if self.confined {
                        self.bridge.mouse_release(button).unwrap();
                    }
                }
                Axis {
                    horizontal,
                    vertical,
                    ..
                } => {
                    if self.confined {
                        self.bridge
                            .scroll(
                                horizontal.absolute,
                                vertical.absolute,
                                horizontal.value120,
                                vertical.value120,
                            )
                            .unwrap();
                        if horizontal.stop || vertical.stop {
                            self.bridge
                                .scroll_stop(horizontal.stop, vertical.stop)
                                .unwrap();
                        }
                    }
                }
            }
        }
    }
}

impl KeyboardHandler for InputApp {
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
        if self.modifiers.logo {
            return;
        }
        if self.confined {
            self.bridge.key_press(event.raw_code).unwrap();
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
            self.bridge.key_release(event.raw_code).unwrap();
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
        self.modifiers = modifiers;
        if self.confined {
            const MOD4_MASK: u32 = 1 << 6;
            self.bridge
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

impl SeatHandler for InputApp {
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
        if cap == Capability::Pointer && self.pointer.is_none() {
            let themed_pointer = self
                .seat_state
                .get_pointer_with_theme(
                    qh,
                    &seat,
                    self.shm_state.wl_shm(),
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
            self.keyboard.take().unwrap().release();
        }
        if cap == Capability::Pointer && self.pointer.is_some() {
            self.pointer.take().unwrap().pointer().release();
            self.relative_pointer.take().unwrap().destroy();
            if let Some(confined) = self.confined_pointer.take() {
                confined.destroy();
            }
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}
}

impl ShmHandler for InputApp {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm_state
    }
}

impl Dispatch<ZwpKeyboardShortcutsInhibitorV1, ()> for InputApp {
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

impl AsMut<SimpleGlobal<ZwpKeyboardShortcutsInhibitManagerV1, 1>> for InputApp {
    fn as_mut(&mut self) -> &mut SimpleGlobal<ZwpKeyboardShortcutsInhibitManagerV1, 1> {
        self.shortcuts_inhibit_manager.as_mut().unwrap()
    }
}

delegate_compositor!(InputApp);
delegate_output!(InputApp);
delegate_seat!(InputApp);
delegate_pointer!(InputApp);
delegate_relative_pointer!(InputApp);
delegate_keyboard!(InputApp);
delegate_pointer_constraints!(InputApp);
delegate_shm!(InputApp);
delegate_simple!(InputApp, ZwpKeyboardShortcutsInhibitManagerV1, 1);

#[allow(clippy::too_many_arguments)]
pub fn spawn(
    conn: Connection,
    globals: Arc<smithay_client_toolkit::reexports::client::globals::GlobalList>,
    surface: WlSurface,
    global_state: GlobalState,
    sizer: SharedSizer,
    bridge: Box<dyn InputBridge>,
    confined: bool,
    resize_rx: Channel<InputMsg>,
    ph: PlotterHandle,
) -> Result<()> {
    let (wl_primary, wl_clipboard) = unsafe {
        wayland_clipboard::create_clipboards_from_external(conn.display().id().as_ptr() as *mut _)
    };
    let mut event_queue: EventQueue<InputApp> = conn.new_event_queue();
    let qh = event_queue.handle();

    let compositor_state = CompositorState::bind(&globals, &qh)?;
    let output_state = OutputState::new(&globals, &qh);
    let shm_state = Shm::bind(&globals, &qh)?;
    let seat_state = SeatState::new(&globals, &qh);
    let relative_pointer_state = RelativePointerState::bind(&globals, &qh);
    let pointer_constraints_state = PointerConstraintsState::bind(&globals, &qh);
    let shortcuts_inhibit_manager = match SimpleGlobal::bind(&globals, &qh) {
        Ok(v) => Some(v),
        Err(_) => {
            info!(
                "zwp_keyboard_shortcuts_inhibit_manager_v1 not available, grab won't inhibit compositor shortcuts"
            );
            None
        }
    };
    let cursor_surface = compositor_state.create_surface(&qh);

    let mut app = InputApp {
        qh: qh.clone(),

        surface,
        global_state,
        sizer,

        compositor_state,
        output_state,
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

        res: None,
        modifiers: Modifiers::default(),
        pointer_serial: 0,
        confinement_region: None,
        confined,
        force_relative: false,
        cursor_over_surface: false,

        wl_primary,
        wl_clipboard,
        bridge,
    };

    // Discover seats
    event_queue.roundtrip(&mut app)?;

    std::thread::Builder::new()
        .name("input".into())
        .spawn(move || {
            ph.fatal(run(conn, event_queue, app, resize_rx).context("input thread"));
        })?;

    Ok(())
}

fn run(
    conn: Connection,
    event_queue: EventQueue<InputApp>,
    mut app: InputApp,
    resize_rx: Channel<InputMsg>,
) -> Result<()> {
    let mut event_loop: EventLoop<InputApp> = EventLoop::try_new()?;
    let loop_handle = event_loop.handle();

    loop_handle
        .insert_source(resize_rx, |event, _, app| match event {
            calloop_channel::Event::Msg(InputMsg::Resize) => app.handle_resize(),
            calloop_channel::Event::Closed => {
                app.res = Some(Ok(()));
            }
        })
        .map_err(|e| anyhow!("{}", e.error))?;

    WaylandSource::new(conn, event_queue)
        .insert(loop_handle)
        .unwrap();

    loop {
        event_loop.dispatch(None, &mut app)?;
        if let Some(res) = app.res {
            return res;
        }
    }
}
