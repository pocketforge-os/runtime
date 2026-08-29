//! Launcher-side [`pf_ports::SessionPort`] client for a session authority transport.

use pf_ports::{
    Deadline, LaunchRequest, LaunchResult, SessionError, SessionEvent, SessionPoll, SessionPort,
};
use pf_session_authority::{AuthorityApi, AuthorityError, RpcEvent, RpcRequest, RpcResponse};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

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

/// Reconnecting Unix-socket transport. The durable cursor is keyed by `SessionClient`'s
/// stable client id and lives in the authority store, so no launcher-local state file is needed.
pub struct SocketTransport {
    socket: PathBuf,
}

impl SocketTransport {
    pub fn connect(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    fn call(&self, request: &RpcRequest) -> Result<RpcResponse, AuthorityError> {
        let mut stream = UnixStream::connect(&self.socket).map_err(backend)?;
        let body = serde_json::to_vec(request).map_err(backend)?;
        pf_wire::write_frame(&mut stream, &body).map_err(backend)?;
        let body = pf_wire::read_frame(&mut stream).map_err(backend)?;
        let response: RpcResponse = serde_json::from_slice(&body).map_err(backend)?;
        match response {
            RpcResponse::Error { message } => Err(AuthorityError::Backend(message)),
            response => Ok(response),
        }
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket
    }
}

fn backend(error: impl std::fmt::Display) -> AuthorityError {
    AuthorityError::Backend(error.to_string())
}

impl AuthorityApi for SocketTransport {
    fn launch(&mut self, request: LaunchRequest) -> Result<LaunchResult, AuthorityError> {
        match self.call(&RpcRequest::Launch {
            item_id: request.item_id,
        })? {
            RpcResponse::Accepted { session_id } => Ok(LaunchResult::Accepted { session_id }),
            RpcResponse::RejectedBusy => Ok(LaunchResult::RejectedBusy),
            RpcResponse::ItemUnavailable => Ok(LaunchResult::ItemUnavailable),
            _ => Err(AuthorityError::Backend("unexpected launch response".into())),
        }
    }
    fn events_for(&self, client_id: &str) -> Vec<(u64, SessionEvent)> {
        match self.call(&RpcRequest::Events {
            client_id: client_id.to_owned(),
        }) {
            Ok(RpcResponse::Events { events }) => {
                events.into_iter().map(|(s, e)| (s, rpc_event(e))).collect()
            }
            _ => Vec::new(),
        }
    }
    fn acknowledge(&mut self, client_id: &str, sequence: u64) -> Result<(), AuthorityError> {
        match self.call(&RpcRequest::Acknowledge {
            client_id: client_id.to_owned(),
            sequence,
        })? {
            RpcResponse::Ok => Ok(()),
            _ => Err(AuthorityError::Backend(
                "unexpected acknowledge response".into(),
            )),
        }
    }
    fn history(&self) -> Vec<SessionEvent> {
        match self.call(&RpcRequest::History) {
            Ok(RpcResponse::History { events }) => events.into_iter().map(rpc_event).collect(),
            _ => Vec::new(),
        }
    }
}

fn rpc_event(event: RpcEvent) -> SessionEvent {
    match event {
        RpcEvent::Starting => SessionEvent::Observed(pf_ports::ObservedSessionState::Starting),
        RpcEvent::Running => SessionEvent::Observed(pf_ports::ObservedSessionState::Running),
        RpcEvent::ObservationComplete => {
            SessionEvent::Observed(pf_ports::ObservedSessionState::ObservationComplete)
        }
        RpcEvent::Returned { session_id } => {
            SessionEvent::Terminal(pf_ports::TerminalReceipt::Returned { session_id })
        }
        RpcEvent::ForcedClose { session_id } => {
            SessionEvent::Terminal(pf_ports::TerminalReceipt::ForcedClose { session_id })
        }
        RpcEvent::Crash {
            session_id,
            summary,
        } => SessionEvent::Terminal(pf_ports::TerminalReceipt::Crash {
            session_id,
            summary,
        }),
        RpcEvent::RecoveryRequired { session_id, reason } => {
            SessionEvent::RecoveryRequired(pf_ports::RecoveryRequired { session_id, reason })
        }
    }
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
