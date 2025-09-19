mod input;
mod render;
mod xinput;

use crate::capture::plotter::{self, PlotterHandle};
use crate::capture::{Capture, CaptureHandle};
use crate::display::input::{InputThreadHandle, InputThreadInit};
use crate::sizer::{SharedSizer, Sizer};
use crate::{GlobalState, Opts};
use anyhow::{Context as _, Result, anyhow, bail};
use arc_swap::ArcSwap;
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, Region};
use smithay_client_toolkit::dmabuf::{DmabufFeedback, DmabufHandler, DmabufState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::channel::channel;
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay_client_toolkit::reexports::calloop::{EventLoop, LoopHandle, RegistrationToken};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::wl_buffer::WlBuffer;
use smithay_client_toolkit::reexports::client::protocol::wl_output::{self, WlOutput};
use smithay_client_toolkit::reexports::client::protocol::wl_subsurface::WlSubsurface;
use smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface;
use smithay_client_toolkit::reexports::client::{Connection, Dispatch, QueueHandle};
use smithay_client_toolkit::reexports::protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_feedback_v1,
};
use smithay_client_toolkit::reexports::protocols::wp::viewporter::client::wp_viewport::{
    self, WpViewport,
};
use smithay_client_toolkit::reexports::protocols::wp::viewporter::client::wp_viewporter::WpViewporter;
use smithay_client_toolkit::registry::SimpleGlobal;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::xdg::XdgShell;
use smithay_client_toolkit::shell::xdg::window::{
    Window, WindowConfigure, WindowDecorations, WindowHandler,
};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::subcompositor::SubcompositorState;
use smithay_client_toolkit::{
    delegate_compositor, delegate_dmabuf, delegate_output, delegate_registry, delegate_shm,
    delegate_simple, delegate_subcompositor, delegate_xdg_shell, delegate_xdg_window,
    registry_handlers,
};
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

// breaks rustfmt import sorting for some reason
use smithay_client_toolkit::reexports::protocols::wp::fractional_scale::v1::client::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1;
use smithay_client_toolkit::reexports::protocols::wp::fractional_scale::v1::client::wp_fractional_scale_v1::{self, WpFractionalScaleV1};

struct App {
    // Handles
    loop_handle: LoopHandle<'static, App>,
    input_handle: InputThreadHandle,
    ph: PlotterHandle,

    // Wayland State
    registry_state: RegistryState,
    output_state: OutputState,
    compositor_state: CompositorState,
    _subcompositor_state: SubcompositorState,
    dmabuf_state: DmabufState,
    shm_state: Shm,
    _xdg_shell: XdgShell,
    wp_viewporter: SimpleGlobal<WpViewporter, 1>,
    wp_frac_mgr: Option<SimpleGlobal<WpFractionalScaleManagerV1, 1>>,

    // Wayland Objects
    surface: WlSurface,
    _ui_surface: WlSurface,
    _subsurface: WlSubsurface,
    viewport: WpViewport,
    _wp_frac: Option<WpFractionalScaleV1>,

    // Application Logic
    ch: CaptureHandle,
    sizer: SharedSizer,
    _global_state: GlobalState,

    // Window & Render State
    renderer: render::Renderer,
    size: (u32, u32),
    scale120: u32,
    last_commited_sizer: Option<Sizer>,
    feedback: Option<DmabufFeedback>,

    // Event Loop & Control Flow
    res: Option<Result<()>>,
    first_configure: bool,
    pending_resize: Option<RegistrationToken>,
}

impl App {
    fn content_region(&self) -> Option<Region> {
        let region = Region::new(&self.compositor_state).expect("Failed to create region");
        let rect = self.last_commited_sizer.as_ref()?.window_sizing.content;
        region.add(
            rect.x as i32,
            rect.y as i32,
            rect.width as i32,
            rect.height as i32,
        );
        Some(region)
    }

    fn handle_resize(&mut self) {
        let sizer = self.sizer.load();
        if !sizer.ready() || (sizer.window_size == self.size && sizer.scale120 == self.scale120) {
            return;
        }
        self.sizer
            .rcu(|s| s.with_window_size(self.size, self.scale120));
        self.renderer.resize(&self.sizer.load());
    }

    fn sched_resize(&mut self) {
        if let Some(pending) = self.pending_resize.take() {
            self.loop_handle.remove(pending);
        }
        let timer = Timer::from_duration(Duration::from_millis(100));
        let pending = self
            .loop_handle
            .insert_source(timer, move |_event, _meta, app| {
                app.handle_resize();
                app.pending_resize = None;
                TimeoutAction::Drop
            })
            .unwrap();
        self.pending_resize = Some(pending);
    }

    fn draw_capture(&mut self) {
        let sizer = (**self.sizer.load()).clone();
        if self
            .last_commited_sizer
            .as_ref()
            .is_none_or(|s| *s != sizer)
        {
            self.last_commited_sizer = Some(sizer.clone());
            let (w_w, w_h) = sizer.window_size;
            let (r_w, r_h) = sizer.render_size;
            self.ch.resize(&sizer);
            let region = self.content_region().unwrap();
            self.viewport.set_source(0.0, 0.0, r_w as f64, r_h as f64);
            self.viewport.set_destination(w_w as i32, w_h as i32);
            self.surface.set_opaque_region(Some(region.wl_region()));
            self.surface.set_input_region(Some(region.wl_region()));
            self.input_handle
                .send(input::InputThreadCommand::UpdateConfinement { region })
                .unwrap();
        }

        if let Err(e) = self.renderer.render() {
            self.res = Some(Err(e).context("ui render"));
            return;
        }

        self.ch.frame(&sizer);
    }
}

impl AsMut<SimpleGlobal<WpFractionalScaleManagerV1, 1>> for App {
    fn as_mut(&mut self) -> &mut SimpleGlobal<WpFractionalScaleManagerV1, 1> {
        self.wp_frac_mgr.as_mut().unwrap()
    }
}

impl WindowHandler for App {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        self.res = Some(Ok(()));
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _window: &Window,
        configure: WindowConfigure,
        _serial: u32,
    ) {
        // info!("configure {:?}", configure.new_size);
        let width = configure.new_size.0.map(|v| v.get()).unwrap_or(1280);
        let height = configure.new_size.1.map(|v| v.get()).unwrap_or(720);
        self.size = (width, height);

        if self.first_configure {
            self.first_configure = false;
            self.handle_resize();

            self.surface.frame(qh, self.surface.clone());
            if let Err(e) = self.renderer.render() {
                self.res = Some(Err(e).context("first UI render"));
            }
            self.ch.force_render();
            info!("render forced");
        } else {
            self.sched_resize();
        }
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _new_factor: i32,
    ) {
        // info!("scale factor display: {_new_factor}");
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _new_transform: wl_output::Transform,
    ) {
        // info!("scale transform display: {_new_transform:?}");
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _time: u32,
    ) {
        // info!("frame");
        self.ph.frame(plotter::EventType::Present);
        self.surface.frame(qh, self.surface.clone());
        self.draw_capture();
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _output: &wl_output::WlOutput,
    ) {
        // info!("enter, display");
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _output: &wl_output::WlOutput,
    ) {
        // info!("leave, display");
    }
}

impl DmabufHandler for App {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_feedback(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _proxy: &zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1,
        feedback: DmabufFeedback,
    ) {
        // eprintln!("dmabuf feedback:");
        // eprintln!("  device: {:x}", feedback.main_device());
        // for fmt in feedback.format_table() {
        //     if fmt.modifier == 0 {
        //         eprintln!(
        //             "  fmt {} fourcc {:?} mod {}",
        //             fmt.format,
        //             DrmFourcc::try_from(fmt.format).map(|v| v.to_string()),
        //             fmt.modifier
        //         );
        //     }
        // }
        self.feedback = Some(feedback);
    }

    fn created(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _params: &zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
        _buffer: WlBuffer,
    ) {
        unreachable!()
    }

    fn failed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        params: &zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
    ) {
        self.res = Some(Err(anyhow!("dmabuf failed: {params:#?}")));
    }

    fn released(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _buffer: &WlBuffer) {
        eprintln!("release!");
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

impl Dispatch<WpFractionalScaleV1, ()> for App {
    fn event(
        state: &mut Self,
        _proxy: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // info!("fractional {event:?}");
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            state.scale120 = scale;
            state.sched_resize();
        }
    }
}

impl AsMut<SimpleGlobal<WpViewporter, 1>> for App {
    fn as_mut(&mut self) -> &mut SimpleGlobal<WpViewporter, 1> {
        &mut self.wp_viewporter
    }
}

impl Dispatch<WpViewport, ()> for App {
    fn event(
        _state: &mut Self,
        _proxy: &WpViewport,
        _event: wp_viewport::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm_state
    }
}

delegate_xdg_window!(App);
delegate_xdg_shell!(App);
delegate_compositor!(App);
delegate_subcompositor!(App);
delegate_dmabuf!(App);
delegate_output!(App);
delegate_registry!(App);
delegate_shm!(App);
delegate_simple!(App, WpFractionalScaleManagerV1, 1);
delegate_simple!(App, WpViewporter, 1);

fn run_internal(opts: Opts, global_state: GlobalState, ph: PlotterHandle) -> Result<()> {
    let mut event_loop: EventLoop<App> = EventLoop::try_new()?;
    let loop_handle = event_loop.handle();
    let conn = Connection::connect_to_env()?;

    let (globals, mut event_queue) = registry_queue_init(&conn)?;
    let globals = Arc::new(globals);
    let qh = event_queue.handle();

    let (tx_input, rx_input) = channel();
    let xdg_shell = XdgShell::bind(&globals, &qh)?;
    let compositor_state = CompositorState::bind(&globals, &qh)?;
    let subcompositor_state =
        SubcompositorState::bind(compositor_state.wl_compositor().clone(), &globals, &qh)?;
    let shm_state = Shm::bind(&globals, &qh)?;
    let wl_shm = shm_state.wl_shm().clone();
    let surface = compositor_state.create_surface(&qh);
    let (subsurface, ui_surface) = subcompositor_state.create_subsurface(surface.clone(), &qh);
    let empty_region = Region::new(&compositor_state)?;
    ui_surface.set_input_region(Some(empty_region.wl_region()));
    subsurface.set_position(0, 0);
    subsurface.place_above(&surface);
    let wp_viewporter = SimpleGlobal::<WpViewporter, 1>::bind(&globals, &qh)?;
    let viewport = wp_viewporter.get()?.get_viewport(&surface, &qh, ());

    let window = xdg_shell.create_window(surface.clone(), WindowDecorations::RequestServer, &qh);
    window.set_title("WaywardGriffin");
    window.set_app_id("waygriff");
    window.commit();
    let sizer = Arc::new(ArcSwap::from_pointee(Sizer::default()));

    let cursor_surface = compositor_state.create_surface(&qh);
    let init = InputThreadInit {
        conn: conn.clone(),
        globals: globals.clone(),
        surface: surface.clone(),
        cursor_surface,
        wl_shm,
        sizer: sizer.clone(),
        global_state: global_state.clone(),
        ph: ph.clone(),
        rx_input,
        confined: opts.confine,
    };
    std::thread::spawn(move || input::run(init));

    let mut wp_frac_mgr = None;
    let mut wp_frac = None;
    if let Ok(mgr) = SimpleGlobal::<WpFractionalScaleManagerV1, 1>::bind(&globals, &qh) {
        wp_frac = Some(mgr.get()?.get_fractional_scale(&surface, &qh, ()));
        wp_frac_mgr = Some(mgr);
    }

    let mut capture = Capture::new(
        ph.clone(),
        global_state.clone(),
        &conn,
        &surface,
        sizer.clone(),
    )?;
    let ch = capture.handle.clone();
    std::thread::spawn(move || capture.run());
    ch.resize(&sizer.load());
    let renderer = render::Renderer::new(&conn, &ui_surface, global_state.clone())?;

    let mut app = App {
        // Handles
        loop_handle: loop_handle.clone(),
        input_handle: tx_input,
        ph,

        // Wayland State
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        compositor_state,
        _subcompositor_state: subcompositor_state,
        dmabuf_state: DmabufState::new(&globals, &qh),
        shm_state,
        _xdg_shell: xdg_shell,
        wp_viewporter,
        wp_frac_mgr,

        // Wayland Objects
        surface: surface.clone(),
        _ui_surface: ui_surface,
        _subsurface: subsurface,
        viewport,
        _wp_frac: wp_frac,

        // Application Logic
        ch,
        sizer,
        _global_state: global_state,

        // Window & Render State
        renderer,
        size: (0, 0),
        scale120: 120,
        last_commited_sizer: None,
        feedback: None,

        // Event Loop & Control Flow
        res: None,
        first_configure: true,
        pending_resize: None,
    };

    event_queue.roundtrip(&mut app)?;
    if let Some(4..) = app.dmabuf_state.version() {
        app.dmabuf_state.get_surface_feedback(&surface, &qh)?;
    } else {
        bail!("zwp_linux_dmabuf_v1 v4 is required.");
    }
    event_queue.roundtrip(&mut app)?;
    if app.feedback.is_none() {
        bail!("Compositor did not provide dmabuf feedback");
    }

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

pub fn run(opts: Opts, global_state: GlobalState, ph: PlotterHandle) {
    ph.fatal(run_internal(opts, global_state, ph.clone()).context("display thread"));
}
