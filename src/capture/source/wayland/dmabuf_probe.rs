use crate::utils::wayland_connect;
use anyhow::{Context as _, Result};
use smithay_client_toolkit::dmabuf::{DmabufFeedback, DmabufHandler, DmabufState};
use smithay_client_toolkit::reexports::client::QueueHandle;
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::wl_buffer::WlBuffer;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::{delegate_dmabuf, delegate_registry, registry_handlers};

// breaks rustfmt import sorting for some reason
use smithay_client_toolkit::reexports::protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1;
use smithay_client_toolkit::reexports::protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1;

pub fn query_dmabuf_feedback(display: &str) -> Result<DmabufFeedback> {
    let conn = wayland_connect(display)?;

    let (globals, mut event_queue) = registry_queue_init::<State>(&conn)?;
    let qh = event_queue.handle();

    let mut state = State {
        registry_state: RegistryState::new(&globals),
        dmabuf_state: DmabufState::new(&globals, &qh),
        feedback: None,
    };

    state.dmabuf_state.get_default_feedback(&qh)?;
    event_queue.roundtrip(&mut state)?;

    state
        .feedback
        .context("Compositor did not provide dmabuf feedback")
}

struct State {
    registry_state: RegistryState,
    dmabuf_state: DmabufState,
    feedback: Option<DmabufFeedback>,
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![];
}

impl DmabufHandler for State {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_feedback(
        &mut self,
        _conn: &smithay_client_toolkit::reexports::client::Connection,
        _qh: &QueueHandle<Self>,
        _proxy: &ZwpLinuxDmabufFeedbackV1,
        feedback: DmabufFeedback,
    ) {
        self.feedback = Some(feedback);
    }

    fn created(
        &mut self,
        _conn: &smithay_client_toolkit::reexports::client::Connection,
        _qh: &QueueHandle<Self>,
        _params: &ZwpLinuxBufferParamsV1,
        _buffer: WlBuffer,
    ) {
    }

    fn failed(
        &mut self,
        _conn: &smithay_client_toolkit::reexports::client::Connection,
        _qh: &QueueHandle<Self>,
        _params: &ZwpLinuxBufferParamsV1,
    ) {
    }

    fn released(
        &mut self,
        _conn: &smithay_client_toolkit::reexports::client::Connection,
        _qh: &QueueHandle<Self>,
        _buffer: &WlBuffer,
    ) {
    }
}

delegate_registry!(State);
delegate_dmabuf!(State);
