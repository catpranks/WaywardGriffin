use super::{CaptureBackend, CaptureBackendBuilder};
use crate::capture::input::InputInjector;
use crate::capture::plotter::PlotterHandle;
use anyhow::{Context as _, Result, bail};
use smithay_client_toolkit::dmabuf::{DmabufFeedback, DmabufHandler, DmabufState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::wl_buffer::WlBuffer;
use smithay_client_toolkit::reexports::client::protocol::wl_output::WlOutput;
use smithay_client_toolkit::reexports::client::{Connection, Dispatch, EventQueue, QueueHandle};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::{
    delegate_dmabuf, delegate_output, delegate_registry, registry_handlers,
};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use vulkano::device::Device;
use vulkano::memory::allocator::StandardMemoryAllocator;

// breaks rustfmt import sorting for some reason
use smithay_client_toolkit::reexports::protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1;
use smithay_client_toolkit::reexports::protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1;
use smithay_client_toolkit::reexports::protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1};

struct State {
    registry_state: RegistryState,
    output_state: OutputState,
    dmabuf_state: DmabufState,
    screencopy_manager: ZwlrScreencopyManagerV1,
    feedback: Option<DmabufFeedback>,
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
}

impl DmabufHandler for State {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_feedback(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _proxy: &ZwpLinuxDmabufFeedbackV1,
        feedback: DmabufFeedback,
    ) {
        self.feedback = Some(feedback);
    }

    fn created(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _params: &ZwpLinuxBufferParamsV1,
        _buffer: WlBuffer,
    ) {
    }

    fn failed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _params: &ZwpLinuxBufferParamsV1,
    ) {
    }

    fn released(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _buffer: &WlBuffer) {}
}

impl Dispatch<ZwlrScreencopyManagerV1, ()> for State {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrScreencopyManagerV1,
        _event: zwlr_screencopy_manager_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

delegate_registry!(State);
delegate_output!(State);
delegate_dmabuf!(State);

pub struct Builder {
    conn: Connection,
    event_queue: EventQueue<State>,
    state: State,
}

impl Builder {
    pub fn new(display: &str) -> Result<Self> {
        let stream = UnixStream::connect(display)
            .with_context(|| format!("Failed to connect to Wayland socket: {display}"))?;
        let conn = Connection::from_socket(stream)
            .context("Failed to create Wayland connection from socket")?;

        let (globals, mut event_queue) = registry_queue_init::<State>(&conn)?;
        let qh = event_queue.handle();

        let screencopy_manager: ZwlrScreencopyManagerV1 = globals
            .bind(&qh, 3..=3, ())
            .context("zwlr_screencopy_manager_v1 v3 not available")?;

        let mut state = State {
            registry_state: RegistryState::new(&globals),
            output_state: OutputState::new(&globals, &qh),
            dmabuf_state: DmabufState::new(&globals, &qh),
            screencopy_manager,
            feedback: None,
        };

        event_queue.roundtrip(&mut state)?;

        state.dmabuf_state.get_default_feedback(&qh)?;
        event_queue.roundtrip(&mut state)?;

        if state.feedback.is_none() {
            bail!("Compositor did not provide dmabuf feedback");
        }

        Ok(Self {
            conn,
            event_queue,
            state,
        })
    }
}

impl CaptureBackendBuilder for Builder {
    fn device_id(&self) -> super::DeviceId {
        let dev = self.state.feedback.as_ref().unwrap().main_device();
        super::DeviceId::DevMajorMinor(
            nix::sys::stat::major(dev),
            nix::sys::stat::minor(dev),
        )
    }

    fn build(
        self: Box<Self>,
        _device: Arc<Device>,
        _allocator: Arc<StandardMemoryAllocator>,
        _ph: PlotterHandle,
        _display: &str,
    ) -> Result<(Box<dyn CaptureBackend>, Box<dyn InputInjector>)> {
        todo!("wlr-screencopy backend build")
    }
}
