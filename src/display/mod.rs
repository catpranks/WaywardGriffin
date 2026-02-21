mod input;

use crate::capture::source::{CaptureEnv, create_backend_builder};
use crate::capture::{RenderMsg, Renderer};
use crate::config::Config;
use crate::display::input::InputState;
use crate::overlay::OverlayHandle;
use crate::plotter::{FrameInfo, PlotterHandle};
use crate::sizer::SharedSizer;
use crate::utils::clock_monotonic_ns;
use crate::{GlobalState, Opts};
use anyhow::{Result, anyhow, bail};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::dmabuf::{DmabufFeedback, DmabufHandler, DmabufState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::generic::Generic;
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay_client_toolkit::reexports::calloop::{
    EventLoop, Interest, LoopHandle, Mode, PostAction, RegistrationToken,
};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::client::backend::ObjectId;
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::wl_buffer::WlBuffer;
use smithay_client_toolkit::reexports::client::protocol::wl_callback;
use smithay_client_toolkit::reexports::client::protocol::wl_output::{self, WlOutput};
use smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface;
use smithay_client_toolkit::reexports::client::{Connection, Dispatch, Proxy, QueueHandle};
use smithay_client_toolkit::reexports::protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_feedback_v1,
};
use smithay_client_toolkit::reexports::protocols::wp::presentation_time::client::{
    wp_presentation, wp_presentation_feedback,
};
use smithay_client_toolkit::reexports::protocols::wp::viewporter::client::wp_viewport::{
    self, WpViewport,
};
use smithay_client_toolkit::reexports::protocols::wp::viewporter::client::wp_viewporter::WpViewporter;
use smithay_client_toolkit::registry::SimpleGlobal;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::SeatState;
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::xdg::XdgShell;
use smithay_client_toolkit::shell::xdg::window::{
    Window, WindowConfigure, WindowDecorations, WindowHandler,
};
use smithay_client_toolkit::{
    delegate_compositor, delegate_dmabuf, delegate_output, delegate_registry, delegate_simple,
    delegate_xdg_shell, delegate_xdg_window, registry_handlers,
};
use std::collections::VecDeque;
use std::os::linux::net::SocketAddrExt as _;
use std::os::unix::net::{SocketAddr, UnixListener};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;
use tracing::{info, warn};

// breaks rustfmt import sorting for some reason
use smithay_client_toolkit::reexports::protocols::wp::fractional_scale::v1::client::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1;
use smithay_client_toolkit::reexports::protocols::wp::fractional_scale::v1::client::wp_fractional_scale_v1::{self, WpFractionalScaleV1};

#[derive(Clone)]
pub struct DisplayCtx {
    pub ph: PlotterHandle,
    pub global_state: GlobalState,
    pub surface: WlSurface,
    pub viewport: WpViewport,
    pub compositor_state: CompositorState,
    presentation: wp_presentation::WpPresentation,
    pending_feedback: Arc<Mutex<VecDeque<(ObjectId, FrameInfo)>>>,
    qh: QueueHandle<App>,
}

impl DisplayCtx {
    pub fn request_frame(&self) {
        self.surface.frame(&self.qh, self.surface.clone());
    }

    pub fn request_feedback(&self, info: FrameInfo) {
        let fb = self.presentation.feedback(&self.surface, &self.qh, ());
        let mut deque = self.pending_feedback.lock().unwrap();
        while let Some((_, front)) = deque.front()
            && front.start.elapsed() > Duration::from_secs(1)
        {
            deque.pop_front();
            info!("presentation feedback timed out");
        }
        deque.push_back((fb.id(), info));
    }
}

pub struct App {
    loop_handle: LoopHandle<'static, App>,
    sizer: SharedSizer,
    dc: DisplayCtx,
    render_tx: mpsc::Sender<RenderMsg>,
    pub input: InputState,

    // Wayland State
    registry_state: RegistryState,
    output_state: OutputState,
    dmabuf_state: DmabufState,
    _xdg_shell: XdgShell,
    wp_viewporter: SimpleGlobal<WpViewporter, 1>,
    wp_frac_mgr: Option<SimpleGlobal<WpFractionalScaleManagerV1, 1>>,
    _wp_frac: Option<WpFractionalScaleV1>,

    // Window State
    size: (u32, u32),
    scale120: u32,
    feedback: Option<DmabufFeedback>,
    first_configure: bool,
    pending_resize: Option<RegistrationToken>,
    res: Option<Result<()>>,
}

impl App {
    fn handle_resize(&mut self) {
        let sizer = self.sizer.load();
        if !sizer.ready() || (sizer.window_size == self.size && sizer.scale120 == self.scale120) {
            return;
        }
        self.sizer
            .rcu(|s| s.with_window_size(self.size, self.scale120));

        self.input.handle_resize(&self.dc, &self.sizer);
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
        _qh: &QueueHandle<Self>,
        _window: &Window,
        configure: WindowConfigure,
        _serial: u32,
    ) {
        let width = configure.new_size.0.map(|v| v.get()).unwrap_or(1280);
        let height = configure.new_size.1.map(|v| v.get()).unwrap_or(720);
        self.size = (width, height);

        if self.first_configure {
            self.first_configure = false;
            self.handle_resize();
            let _ = self.render_tx.send(RenderMsg::Frame);
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
        let _ = self.render_tx.send(RenderMsg::Frame);
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
    registry_handlers![OutputState, SeatState];
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

impl Dispatch<wl_callback::WlCallback, ()> for App {
    fn event(
        _state: &mut Self,
        _proxy: &wl_callback::WlCallback,
        _event: wl_callback::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wp_presentation::WpPresentation, ()> for App {
    fn event(
        _state: &mut Self,
        _proxy: &wp_presentation::WpPresentation,
        event: wp_presentation::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let wp_presentation::Event::ClockId { clk_id } = event {
            info!("clock_id: {clk_id}");
        }
    }
}

impl Dispatch<wp_presentation_feedback::WpPresentationFeedback, ()> for App {
    fn event(
        state: &mut Self,
        feedback: &wp_presentation_feedback::WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wp_presentation_feedback::Event::Presented { .. } => {
                let now = clock_monotonic_ns();
                let mut deque = state.dc.pending_feedback.lock().unwrap();
                if let Some(pos) = deque.iter().position(|(id, _)| *id == feedback.id()) {
                    let (_, mut info) = deque.remove(pos).unwrap();
                    info.set_present(now);
                    state.dc.ph.render(info);
                }
            }
            wp_presentation_feedback::Event::Discarded => {
                let mut deque = state.dc.pending_feedback.lock().unwrap();
                if let Some(pos) = deque.iter().position(|(id, _)| *id == feedback.id()) {
                    deque.remove(pos);
                }
            }
            _ => {}
        }
    }
}

delegate_xdg_window!(App);
delegate_xdg_shell!(App);
delegate_compositor!(App);
delegate_dmabuf!(App);
delegate_output!(App);
delegate_registry!(App);
delegate_simple!(App, WpFractionalScaleManagerV1, 1);
delegate_simple!(App, WpViewporter, 1);

pub fn run(
    opts: Opts,
    global_state: GlobalState,
    ph: PlotterHandle,
    sizer: SharedSizer,
    config: Config,
) -> Result<()> {
    let mut event_loop: EventLoop<App> = EventLoop::try_new()?;
    let loop_handle = event_loop.handle();
    let conn = Connection::connect_to_env()?;

    let (globals, mut event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();

    let xdg_shell = XdgShell::bind(&globals, &qh)?;
    let compositor_state = CompositorState::bind(&globals, &qh)?;
    let surface = compositor_state.create_surface(&qh);
    let wp_viewporter = SimpleGlobal::<WpViewporter, 1>::bind(&globals, &qh)?;
    let viewport = wp_viewporter.get()?.get_viewport(&surface, &qh, ());

    let window = xdg_shell.create_window(surface.clone(), WindowDecorations::RequestServer, &qh);
    window.set_title("WaywardGriffin");
    window.set_app_id("waygriff");
    window.commit();

    let mut wp_frac_mgr = None;
    let mut wp_frac = None;
    if let Ok(mgr) = SimpleGlobal::<WpFractionalScaleManagerV1, 1>::bind(&globals, &qh) {
        wp_frac = Some(mgr.get()?.get_fractional_scale(&surface, &qh, ()));
        wp_frac_mgr = Some(mgr);
    }

    let presentation: wp_presentation::WpPresentation = globals.bind(&qh, 1..=1, ())?;
    let dc = DisplayCtx {
        ph: ph.clone(),
        global_state: global_state.clone(),
        surface: surface.clone(),
        viewport,
        compositor_state,
        presentation,
        pending_feedback: Arc::new(Mutex::new(VecDeque::new())),
        qh: qh.clone(),
    };
    let backend_builder = create_backend_builder(&opts.capture_opts)?;
    let renderer = Renderer::new(
        &conn,
        backend_builder.device_id()?,
        dc.clone(),
        sizer.clone(),
    )?;
    let overlay_handle = OverlayHandle::new(
        renderer.device.clone(),
        renderer.allocator.clone(),
        ph.clone(),
        sizer.clone(),
    )?;

    let env = CaptureEnv {
        ph,
        global_state,
        device: renderer.device.clone(),
        allocator: renderer.allocator.clone(),
    };
    let (backend, bridge) = backend_builder.build(env, sizer.clone())?;

    let input = InputState::new(
        &conn,
        &globals,
        &dc,
        bridge,
        opts.confine,
        &loop_handle,
        config,
    )?;

    let (render_tx, render_rx) = mpsc::channel();
    crate::capture::spawn(dc.clone(), renderer, backend, overlay_handle, render_rx)?;

    let mut app = App {
        loop_handle: loop_handle.clone(),
        sizer,
        dc,
        render_tx,
        input,

        // Wayland State
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        dmabuf_state: DmabufState::new(&globals, &qh),
        _xdg_shell: xdg_shell,
        wp_viewporter,
        wp_frac_mgr,
        _wp_frac: wp_frac,

        // Window State
        size: (0, 0),
        scale120: 120,
        feedback: None,
        first_configure: true,
        pending_resize: None,
        res: None,
    };

    // Roundtrip for dmabuf feedback
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
        .insert(loop_handle.clone())
        .unwrap();

    let snap_addr = SocketAddr::from_abstract_name("waygriff-0.snap")?;
    let listener = UnixListener::bind_addr(&snap_addr)?;
    listener.set_nonblocking(true)?;
    info!("screenshot socket: @waygriff-0.snap");
    loop_handle.insert_source(
        Generic::new(listener, Interest::READ, Mode::Level),
        |_readiness, listener, app| {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = app.render_tx.send(RenderMsg::Screenshot(stream));
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => warn!("screenshot accept failed: {e}"),
            }
            Ok(PostAction::Continue)
        },
    )?;

    loop {
        event_loop.dispatch(None, &mut app)?;
        if let Some(res) = app.res {
            return res;
        }
    }
}
