mod input;

use crate::GlobalStateExt as _;
use crate::config::Config;
use crate::control::{ControlMsg, ControlReq};
use crate::display::input::InputState;
use crate::monitor::{FrameInfo, MonitorHandle};
use crate::overlay::OverlayHandle;
use crate::render::{RenderMsg, Renderer};
use crate::sizer::SharedSizer;
use crate::source::{BackendType, CaptureEnv, create_backend_builder};
use crate::utils::{clock_monotonic_ns, compose_timestamp};
use crate::{GlobalState, Opts};
use anyhow::{Result, anyhow, bail};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, FrameCallbackData};
use smithay_client_toolkit::dispatch2::Dispatch2;
use smithay_client_toolkit::dmabuf::{DmabufFeedback, DmabufHandler, DmabufState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::channel;
use smithay_client_toolkit::reexports::calloop::generic::Generic;
use smithay_client_toolkit::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay_client_toolkit::reexports::calloop::{
    EventLoop, Interest, LoopHandle, Mode, PostAction, RegistrationToken,
};
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::client::backend::ObjectId;
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::wl_buffer::WlBuffer;
use smithay_client_toolkit::reexports::client::protocol::wl_output::{self, WlOutput};
use smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface;
use smithay_client_toolkit::reexports::client::{Connection, Proxy, QueueHandle};
use smithay_client_toolkit::reexports::protocols::wp::content_type::v1::client::{
    wp_content_type_manager_v1::WpContentTypeManagerV1,
    wp_content_type_v1::{Type as ContentType, WpContentTypeV1},
};
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
use smithay_client_toolkit::{delegate_dispatch2, delegate_registry, registry_handlers};
use std::collections::VecDeque;
use std::os::linux::net::SocketAddrExt as _;
use std::os::unix::net::{SocketAddr, UnixListener};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;
use tracing::{info, warn};

// breaks rustfmt import sorting for some reason
use smithay_client_toolkit::reexports::protocols::wp::fractional_scale::v1::client::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1;
use smithay_client_toolkit::reexports::protocols::wp::fractional_scale::v1::client::wp_fractional_scale_v1::{self, WpFractionalScaleV1};

#[derive(Clone)]
pub struct DisplayCtx {
    pub mon: MonitorHandle,
    pub global_state: GlobalState,
    pub surface: WlSurface,
    pub viewport: WpViewport,
    pub compositor_state: CompositorState,
    presentation: wp_presentation::WpPresentation,
    pending_feedback: Arc<Mutex<VecDeque<(ObjectId, PendingFeedback)>>>,
    presentation_clock: Arc<AtomicU32>,
    frame_pending: Arc<AtomicBool>,
    qh: QueueHandle<App>,
}

struct PendingFeedback {
    info: FrameInfo,
    /// Some(true) for presented, Some(false) for discarded.
    outcome: Option<bool>,
}

impl DisplayCtx {
    pub fn has_pending_presentation(&self) -> bool {
        !self.pending_feedback.lock().unwrap().is_empty()
    }

    pub fn request_frame(&self) {
        if !self.frame_pending.swap(true, Ordering::Relaxed) {
            self.surface
                .frame(&self.qh, FrameCallbackData(self.surface.clone()));
        }
    }

    pub fn request_feedback(&self, info: FrameInfo) -> ObjectId {
        let fb = self.presentation.feedback(&self.surface, &self.qh, AppData);
        let mut deque = self.pending_feedback.lock().unwrap();
        while let Some((_, front)) = deque.front()
            && front.info.start.elapsed() > Duration::from_secs(1)
        {
            deque.pop_front();
            self.mon.feedback_timeout();
        }
        let id = fb.id();
        deque.push_back((
            id.clone(),
            PendingFeedback {
                info,
                outcome: None,
            },
        ));
        id
    }

    pub fn finish_present(&self, id: ObjectId, submit: Duration, present: Duration, returned: u64) {
        self.update_feedback(id, |pending| {
            pending.info.latency.queue_submit = submit;
            pending.info.latency.queue_present = Some(present);
            pending.info.latency.present_return_mono_ns = Some(returned);
        });
    }

    // Feedback can arrive before queue_present returns. Join the two halves without
    // holding a lock during WSI calls or blocking Wayland event dispatch.
    fn update_feedback(&self, id: ObjectId, update: impl FnOnce(&mut PendingFeedback)) {
        let mut deque = self.pending_feedback.lock().unwrap();
        let Some(pos) = deque.iter().position(|(pending_id, _)| *pending_id == id) else {
            return;
        };
        let pending = &mut deque[pos].1;
        update(pending);
        if pending.info.latency.queue_present.is_some() && pending.outcome.is_some() {
            let (_, pending) = deque.remove(pos).unwrap();
            if pending.outcome == Some(true) {
                self.mon.render(pending.info);
            } else {
                self.mon.discard();
            }
        }
    }
}

struct AppData;

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
    _content_type: Option<WpContentTypeV1>,

    // Window State
    size: (u32, u32),
    scale120: u32,
    feedback: Option<DmabufFeedback>,
    first_configure: bool,
    pending_resize: Option<RegistrationToken>,
    res: Option<Result<()>>,
}

impl App {
    fn handle_control(&mut self, msg: ControlMsg, conn: &Connection) {
        info!("control: {msg:?}");
        match msg {
            ControlMsg::SetConfine(v) => self.input.set_confined(&self.dc, conn, v),
            ControlMsg::SetForceRelative(v) => self.input.set_force_relative(&self.dc, v),
            ControlMsg::SetCapture(v) => {
                self.dc.global_state.update(|s| s.capture = v);
            }
            ControlMsg::Ping => {}
            ControlMsg::Quit => self.res = Some(Ok(())),
        }
    }

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
            let _ = self.render_tx.send(RenderMsg::Configured);
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
        self.dc.frame_pending.store(false, Ordering::Relaxed);
        self.dc.mon.callback();
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

impl Dispatch2<WpFractionalScaleV1, App> for AppData {
    fn event(
        &self,
        state: &mut App,
        _proxy: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _conn: &Connection,
        _qh: &QueueHandle<App>,
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

impl Dispatch2<WpViewport, App> for AppData {
    fn event(
        &self,
        _state: &mut App,
        _proxy: &WpViewport,
        _event: wp_viewport::Event,
        _conn: &Connection,
        _qh: &QueueHandle<App>,
    ) {
    }
}

impl Dispatch2<WpViewporter, App> for AppData {
    fn event(
        &self,
        _state: &mut App,
        _proxy: &WpViewporter,
        _event: <WpViewporter as smithay_client_toolkit::reexports::client::Proxy>::Event,
        _conn: &Connection,
        _qh: &QueueHandle<App>,
    ) {
    }
}

impl Dispatch2<WpFractionalScaleManagerV1, App> for AppData {
    fn event(
        &self,
        _state: &mut App,
        _proxy: &WpFractionalScaleManagerV1,
        _event: <WpFractionalScaleManagerV1 as smithay_client_toolkit::reexports::client::Proxy>::Event,
        _conn: &Connection,
        _qh: &QueueHandle<App>,
    ) {
    }
}

impl Dispatch2<wp_presentation::WpPresentation, App> for AppData {
    fn event(
        &self,
        state: &mut App,
        _proxy: &wp_presentation::WpPresentation,
        event: wp_presentation::Event,
        _conn: &Connection,
        _qh: &QueueHandle<App>,
    ) {
        if let wp_presentation::Event::ClockId { clk_id } = event {
            state.dc.presentation_clock.store(clk_id, Ordering::Relaxed);
            info!("presentation clock_id: {clk_id}");
            if clk_id != nix::libc::CLOCK_MONOTONIC as u32 {
                warn!("presentation clock is not CLOCK_MONOTONIC; cross-clock latency unavailable");
            }
        }
    }
}

impl Dispatch2<wp_presentation_feedback::WpPresentationFeedback, App> for AppData {
    fn event(
        &self,
        state: &mut App,
        feedback: &wp_presentation_feedback::WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        _conn: &Connection,
        _qh: &QueueHandle<App>,
    ) {
        match event {
            wp_presentation_feedback::Event::Presented {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
                flags,
                refresh,
                ..
            } => {
                let received = clock_monotonic_ns();
                let clock_matches = state.dc.presentation_clock.load(Ordering::Relaxed)
                    == nix::libc::CLOCK_MONOTONIC as u32;
                state.dc.update_feedback(feedback.id(), |pending| {
                    pending.outcome = Some(true);
                    pending.info.latency.feedback_mono_ns = Some(received);
                    pending.info.latency.presentation_flags = Some(flags.into());
                    pending.info.latency.refresh_ns = (refresh != 0).then_some(refresh);
                    if clock_matches {
                        pending
                            .info
                            .set_present(compose_timestamp(tv_sec_hi, tv_sec_lo, tv_nsec));
                    }
                });
                let _ = state.render_tx.send(RenderMsg::PresentationDone(
                    (clock_matches && u32::from(flags) & 8 != 0 && refresh != 0)
                        .then(|| (compose_timestamp(tv_sec_hi, tv_sec_lo, tv_nsec), refresh)),
                ));
            }
            wp_presentation_feedback::Event::Discarded => {
                state
                    .dc
                    .update_feedback(feedback.id(), |pending| pending.outcome = Some(false));
                let _ = state.render_tx.send(RenderMsg::PresentationDone(None));
            }
            _ => {}
        }
    }
}

impl Dispatch2<WpContentTypeManagerV1, App> for AppData {
    fn event(
        &self,
        _: &mut App,
        _: &WpContentTypeManagerV1,
        _: <WpContentTypeManagerV1 as Proxy>::Event,
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
    }
}

impl Dispatch2<WpContentTypeV1, App> for AppData {
    fn event(
        &self,
        _: &mut App,
        _: &WpContentTypeV1,
        _: <WpContentTypeV1 as Proxy>::Event,
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
    }
}

delegate_registry!(App);
delegate_dispatch2!(App);

pub fn run(
    opts: Opts,
    global_state: GlobalState,
    mon: MonitorHandle,
    sizer: SharedSizer,
    config: Config,
    control_rx: channel::Channel<ControlReq>,
) -> Result<()> {
    let mut event_loop: EventLoop<App> = EventLoop::try_new()?;
    let loop_handle = event_loop.handle();
    let conn = Connection::connect_to_env()?;

    let (globals, mut event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();

    let xdg_shell = XdgShell::bind(&globals, &qh)?;
    let compositor_state = CompositorState::bind(&globals, &qh)?;
    let surface = compositor_state.create_surface(&qh);
    let wp_viewporter: SimpleGlobal<WpViewporter, 1> =
        SimpleGlobal::from_bound(globals.bind(&qh, 1..=1, AppData)?);
    let viewport = wp_viewporter.get()?.get_viewport(&surface, &qh, AppData);

    let window = xdg_shell.create_window(surface.clone(), WindowDecorations::RequestServer, &qh);
    window.set_title("WaywardGriffin");
    window.set_app_id("waygriff");
    // Hyprland's direct_scanout=2 policy requires game content. Only opt in
    // for the AMD presentation path; foreign Nvidia WSI buffers cannot scan out.
    let content_type = if opts.capture_opts.backend == BackendType::Nvfbc {
        let manager: WpContentTypeManagerV1 = globals.bind(&qh, 1..=1, AppData)?;
        let content = manager.get_surface_content_type(&surface, &qh, AppData);
        content.set_content_type(ContentType::Game);
        manager.destroy();
        Some(content)
    } else {
        None
    };
    window.commit();

    let mut wp_frac_mgr: Option<SimpleGlobal<WpFractionalScaleManagerV1, 1>> = None;
    let mut wp_frac = None;
    if let Ok(proxy) = globals.bind(&qh, 1..=1, AppData) {
        let mgr: SimpleGlobal<WpFractionalScaleManagerV1, 1> = SimpleGlobal::from_bound(proxy);
        wp_frac = Some(mgr.get()?.get_fractional_scale(&surface, &qh, AppData));
        wp_frac_mgr = Some(mgr);
    }

    let presentation: wp_presentation::WpPresentation = globals.bind(&qh, 1..=1, AppData)?;
    let (render_tx, render_rx) = mpsc::channel();
    let dc = DisplayCtx {
        mon: mon.clone(),
        global_state: global_state.clone(),
        surface: surface.clone(),
        viewport,
        compositor_state,
        presentation,
        pending_feedback: Arc::new(Mutex::new(VecDeque::new())),
        presentation_clock: Arc::new(AtomicU32::new(u32::MAX)),
        frame_pending: Arc::new(AtomicBool::new(false)),
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
        mon.clone(),
        sizer.clone(),
    )?;

    let env = CaptureEnv {
        render_tx: render_tx.clone(),
        mon,
        global_state,
        device: renderer.device.clone(),
        queue: renderer.queue.clone(),
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

    let render_thread =
        crate::render::spawn(dc.clone(), renderer, backend, overlay_handle, render_rx)?;

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
        _content_type: content_type,

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

    let control_conn = conn.clone();
    loop_handle
        .insert_source(control_rx, move |event, _, app| {
            if let channel::Event::Msg(req) = event {
                app.handle_control(req.msg, &control_conn);
                let _ = req.done.send(());
            }
        })
        .map_err(|e| anyhow!("{}", e.error))?;

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
        if app.res.is_some() {
            let res = app.res.take().unwrap();
            let _ = app.render_tx.send(RenderMsg::Quit);
            drop(app);
            // vulkan swapchain borrows our connection
            let _ = render_thread.join();
            return res;
        }
    }
}
