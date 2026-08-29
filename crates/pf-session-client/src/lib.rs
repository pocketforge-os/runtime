//! Launcher-side [`pf_ports::SessionPort`] client for a session authority transport.

use pf_ports::{
    Deadline, LaunchRequest, LaunchResult, SessionError, SessionEvent, SessionPoll, SessionPort,
};
use pf_session_authority::{AuthorityApi, AuthorityError};

pub struct SessionClient<T> {
    transport: T,
    sequence: u64,
    history: Vec<SessionEvent>,
}
impl<T> SessionClient<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            sequence: 0,
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
            .events_after(self.sequence)
            .into_iter()
            .next()
        else {
            return Ok(SessionPoll::Idle);
        };
        self.sequence = sequence;
        self.history.push(event.clone());
        Ok(SessionPoll::Event(event))
    }
    fn history(&self) -> &[SessionEvent] {
        &self.history
    }
}
