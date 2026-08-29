//! Launcher-side [`pf_ports::SessionPort`] client for a session authority transport.

use pf_ports::{
    Deadline, LaunchRequest, LaunchResult, SessionError, SessionEvent, SessionPoll, SessionPort,
};
use pf_session_authority::{AuthorityApi, AuthorityError};

pub struct SessionClient<T> {
    transport: T,
    client_id: String,
    delivered_sequence: Option<u64>,
    history: Vec<SessionEvent>,
}
impl<T> SessionClient<T> {
    pub fn new(client_id: impl Into<String>, transport: T) -> Self {
        Self {
            transport,
            client_id: client_id.into(),
            delivered_sequence: None,
            history: Vec::new(),
        }
    }
    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }
    pub fn into_inner(self) -> T {
        self.transport
    }
}

impl<T: AuthorityApi> SessionClient<T> {
    /// Durably acknowledge the last event returned to this client identity.
    pub fn acknowledge_last(&mut self) -> Result<(), SessionError> {
        if let Some(sequence) = self.delivered_sequence.take() {
            self.transport
                .acknowledge(&self.client_id, sequence)
                .map_err(map_error)?;
        }
        Ok(())
    }
}

fn map_error(_: AuthorityError) -> SessionError {
    SessionError::BackendUnavailable
}
impl<T: AuthorityApi> SessionPort for SessionClient<T> {
    fn launch(&mut self, request: LaunchRequest) -> Result<LaunchResult, SessionError> {
        self.transport.launch(request).map_err(map_error)
    }
    fn next_event(&mut self, _deadline: Deadline) -> Result<SessionPoll, SessionError> {
        let Some((sequence, event)) = self
            .transport
            .events_for(&self.client_id)
            .into_iter()
            .next()
        else {
            return Ok(SessionPoll::Idle);
        };
        self.delivered_sequence = Some(sequence);
        self.history.push(event.clone());
        Ok(SessionPoll::Event(event))
    }
    fn history(&self) -> &[SessionEvent] {
        &self.history
    }
}
