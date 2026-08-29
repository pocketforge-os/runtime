//! Launcher-side [`pf_ports::SessionPort`] client for a session authority transport.

use pf_ports::{
    Deadline, LaunchRequest, LaunchResult, SessionError, SessionEvent, SessionPoll, SessionPort,
};
use pf_session_authority::{
    AuthorityApi, AuthorityError, HistoryEntry, RpcEvent, RpcRequest, RpcResponse,
};
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

    /// Fetch durable session history, including wall-clock playtime stamps.
    pub fn history_entries(&self) -> Result<Vec<HistoryEntry>, AuthorityError> {
        match self.call(&RpcRequest::History)? {
            RpcResponse::History { entries } => Ok(entries),
            _ => Err(AuthorityError::Backend(
                "unexpected history response".into(),
            )),
        }
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
        self.try_events_for(client_id).unwrap_or_default()
    }
    fn try_events_for(&self, client_id: &str) -> Result<Vec<(u64, SessionEvent)>, AuthorityError> {
        match self.call(&RpcRequest::Events {
            client_id: client_id.to_owned(),
        })? {
            RpcResponse::Events { events } => {
                Ok(events.into_iter().map(|(s, e)| (s, rpc_event(e))).collect())
            }
            _ => Err(AuthorityError::Backend("unexpected events response".into())),
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
        self.history_entries()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|entry| {
                entry
                    .receipt
                    .map(|receipt| receipt_event(entry.session_id, receipt))
            })
            .collect()
    }
    fn history_entries(&self) -> Vec<HistoryEntry> {
        SocketTransport::history_entries(self).unwrap_or_default()
    }
}

fn receipt_event(session_id: String, receipt: pf_session_authority::Receipt) -> SessionEvent {
    match receipt {
        pf_session_authority::Receipt::Returned => {
            SessionEvent::Terminal(pf_ports::TerminalReceipt::Returned { session_id })
        }
        pf_session_authority::Receipt::ForcedClose => {
            SessionEvent::Terminal(pf_ports::TerminalReceipt::ForcedClose { session_id })
        }
        pf_session_authority::Receipt::Crash { summary } => {
            SessionEvent::Terminal(pf_ports::TerminalReceipt::Crash {
                session_id,
                summary,
            })
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
            .try_events_for(&self.client_id)
            .map_err(map_error)?
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
