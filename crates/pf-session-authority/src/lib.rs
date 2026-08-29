//! Independent, transport-neutral foreground session authority.
//!
//! A terminal receipt is truthful only after this observation ladder completes in order:
//! foreground unit inactive, foreground target released, selected shell owner active, and a
//! real shell presentation acknowledged. The core atomically persists the completed receipt
//! before publishing it. Failure at termination or at any ladder rung atomically records
//! [`RecoveryRequired`] instead; an unavailable shell is never asked to render its own failure.

use pf_ports::{
    Clock, LaunchRequest, LaunchResult, MonotonicTime, ObservedSessionState, RecoveryRequired,
    SessionEvent, TerminalReceipt,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthorityError {
    Backend(String),
    Persistence(String),
    CorruptState { path: PathBuf, reason: String },
    InvalidObservation,
}

impl std::fmt::Display for AuthorityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Backend(reason) => write!(f, "backend: {reason}"),
            Self::Persistence(reason) => write!(f, "persistence: {reason}"),
            Self::CorruptState { path, reason } => {
                write!(f, "corrupt state {}: {reason}", path.display())
            }
            Self::InvalidObservation => f.write_str("invalid lifecycle observation"),
        }
    }
}
impl std::error::Error for AuthorityError {}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Receipt {
    Returned,
    ForcedClose,
    Crash { summary: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub session_id: String,
    pub item_id: String,
    pub receipt: Option<Receipt>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Phase {
    Idle,
    Starting {
        session_id: String,
        item_id: String,
        start_invoked: bool,
    },
    Running {
        session_id: String,
    },
    StoppingGracefully {
        session_id: String,
        boot_marker: String,
    },
    ForceStopping {
        session_id: String,
    },
    Restoring {
        session_id: String,
        receipt: Receipt,
        rung: RestorationRung,
    },
    RecoveryRequired {
        session_id: String,
        reason: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum RestorationRung {
    UnitInactive,
    TargetReleased,
    OwnerActive,
    PresentationAcknowledged,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct PublishedEvent {
    sequence: u64,
    event: WireEvent,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum WireEvent {
    ObservedStarting,
    ObservedRunning,
    ObservationComplete,
    Terminal {
        session_id: String,
        receipt: Receipt,
    },
    RecoveryRequired {
        session_id: String,
        reason: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PersistedState {
    pub phase: Phase,
    pub history: VecDeque<HistoryEntry>,
    pending: VecDeque<PublishedEvent>,
    next_sequence: u64,
    next_session: u64,
    safe_return_queue: u64,
    pub safe_return_binding_revision: u64,
    acknowledged: BTreeMap<String, u64>,
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            phase: Phase::Idle,
            history: VecDeque::new(),
            pending: VecDeque::new(),
            next_sequence: 1,
            next_session: 1,
            safe_return_queue: 0,
            safe_return_binding_revision: 0,
            acknowledged: BTreeMap::new(),
        }
    }
}

pub trait StateStore {
    fn load(&self) -> Result<Option<PersistedState>, AuthorityError>;
    fn save(&mut self, state: &PersistedState) -> Result<(), AuthorityError>;
}

#[derive(Clone, Debug, Default)]
pub struct MemoryStore {
    state: Option<PersistedState>,
}
impl MemoryStore {
    pub fn snapshot(&self) -> Option<&PersistedState> {
        self.state.as_ref()
    }
}
impl StateStore for MemoryStore {
    fn load(&self) -> Result<Option<PersistedState>, AuthorityError> {
        Ok(self.state.clone())
    }
    fn save(&mut self, state: &PersistedState) -> Result<(), AuthorityError> {
        self.state = Some(state.clone());
        Ok(())
    }
}

/// Durable JSON state store using fsync + atomic replacement in the destination directory.
pub struct FileStore {
    path: PathBuf,
}
impl FileStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}
impl StateStore for FileStore {
    fn load(&self) -> Result<Option<PersistedState>, AuthorityError> {
        match fs::read(&self.path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes)
                    .map(Some)
                    .map_err(|e| AuthorityError::CorruptState {
                        path: self.path.clone(),
                        reason: e.to_string(),
                    })
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(AuthorityError::Persistence(e.to_string())),
        }
    }
    fn save(&mut self, state: &PersistedState) -> Result<(), AuthorityError> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|e| AuthorityError::Persistence(e.to_string()))?;
        let tmp = self
            .path
            .with_extension(format!("tmp.{}", std::process::id()));
        let bytes =
            serde_json::to_vec(state).map_err(|e| AuthorityError::Persistence(e.to_string()))?;
        let result = (|| -> io::Result<()> {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::rename(&tmp, &self.path)?;
            fs::File::open(parent)?.sync_all()
        })();
        result.map_err(|e| AuthorityError::Persistence(e.to_string()))
    }
}

/// Executes one configured command. Kept injectable so tests never invoke the host service manager.
pub trait CommandExecutor {
    fn execute(&mut self, program: &str, args: &[String]) -> Result<i32, String>;
}

#[derive(Default)]
pub struct ProcessExecutor;
impl CommandExecutor for ProcessExecutor {
    fn execute(&mut self, program: &str, args: &[String]) -> Result<i32, String> {
        Command::new(program)
            .args(args)
            .status()
            .map_err(|e| e.to_string())
            .map(|status| status.code().unwrap_or(1))
    }
}

/// Command templates used by [`CommandSystem`]. Tokens support `{item_id}` and `{session_id}`.
#[derive(Clone, Debug)]
pub struct CommandTemplates {
    pub start_foreground: Vec<String>,
    pub request_graceful_stop: Vec<String>,
    pub enforce_termination: Vec<String>,
    pub activate_selected_owner: Vec<String>,
}

impl Default for CommandTemplates {
    fn default() -> Self {
        Self {
            start_foreground: words("systemctl start pf-foreground@{session_id}.service"),
            request_graceful_stop: words("systemctl stop pf-foreground@{session_id}.service"),
            enforce_termination: words(
                "systemctl kill --kill-who=all pf-foreground@{session_id}.service",
            ),
            activate_selected_owner: words("systemctl start pf-shell-selected.service"),
        }
    }
}

fn words(value: &str) -> Vec<String> {
    value.split_whitespace().map(str::to_owned).collect()
}

impl CommandTemplates {
    pub fn from_strings(start: &str, graceful: &str, terminate: &str, activate: &str) -> Self {
        Self {
            start_foreground: words(start),
            request_graceful_stop: words(graceful),
            enforce_termination: words(terminate),
            activate_selected_owner: words(activate),
        }
    }
}

pub struct CommandSystem<E = ProcessExecutor> {
    templates: CommandTemplates,
    executor: E,
}

impl CommandSystem<ProcessExecutor> {
    pub fn new(templates: CommandTemplates) -> Self {
        Self {
            templates,
            executor: ProcessExecutor,
        }
    }
}

impl<E> CommandSystem<E> {
    pub fn with_executor(templates: CommandTemplates, executor: E) -> Self {
        Self {
            templates,
            executor,
        }
    }
    pub fn into_executor(self) -> E {
        self.executor
    }
}

impl<E: CommandExecutor> CommandSystem<E> {
    fn run(&mut self, template: &[String], item: &str, session: &str) -> Result<i32, String> {
        let expanded: Vec<String> = template
            .iter()
            .map(|token| {
                token
                    .replace("{item_id}", item)
                    .replace("{session_id}", session)
            })
            .collect();
        let (program, args) = expanded
            .split_first()
            .ok_or_else(|| "empty command template".to_owned())?;
        self.executor.execute(program, args)
    }
}

/// Trait-shaped image/service integration. F13 supplies the real systemd implementation.
pub trait SessionSystem {
    fn start_foreground(
        &mut self,
        request: &LaunchRequest,
        session_id: &str,
    ) -> Result<bool, String>;
    fn request_graceful_stop(&mut self, session_id: &str) -> Result<(), String>;
    fn enforce_termination(&mut self, session_id: &str) -> Result<(), String>;
    fn activate_selected_owner(&mut self) -> Result<(), String>;
}

impl<E: CommandExecutor> SessionSystem for CommandSystem<E> {
    fn start_foreground(
        &mut self,
        request: &LaunchRequest,
        session_id: &str,
    ) -> Result<bool, String> {
        let template = self.templates.start_foreground.clone();
        match self.run(&template, &request.item_id, session_id)? {
            0 => Ok(true),
            3 => Ok(false),
            code => Err(format!("start command exited {code}")),
        }
    }
    fn request_graceful_stop(&mut self, session_id: &str) -> Result<(), String> {
        let template = self.templates.request_graceful_stop.clone();
        command_ok(self.run(&template, "", session_id)?)
    }
    fn enforce_termination(&mut self, session_id: &str) -> Result<(), String> {
        let template = self.templates.enforce_termination.clone();
        command_ok(self.run(&template, "", session_id)?)
    }
    fn activate_selected_owner(&mut self) -> Result<(), String> {
        let template = self.templates.activate_selected_owner.clone();
        command_ok(self.run(&template, "", "")?)
    }
}

fn command_ok(code: i32) -> Result<(), String> {
    if code == 0 {
        Ok(())
    } else {
        Err(format!("command exited {code}"))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Observation {
    SessionRunning,
    SessionExitedCleanly,
    SessionCrashed { summary: String },
    UnitInactive,
    TargetReleased,
    SelectedOwnerActive,
    PresentationAcknowledged,
    Failed { rung: FailureRung, reason: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureRung {
    Termination,
    UnitInactive,
    TargetReleased,
    OwnerActivation,
    OwnerActive,
    Presentation,
}

pub trait AuthorityApi {
    fn launch(&mut self, request: LaunchRequest) -> Result<LaunchResult, AuthorityError>;
    fn events_for(&self, client_id: &str) -> Vec<(u64, SessionEvent)>;
    fn try_events_for(&self, client_id: &str) -> Result<Vec<(u64, SessionEvent)>, AuthorityError> {
        Ok(self.events_for(client_id))
    }
    fn acknowledge(&mut self, client_id: &str, sequence: u64) -> Result<(), AuthorityError>;
    fn history(&self) -> Vec<SessionEvent>;
}

/// Versioned session-authority RPC payload carried inside `pf-wire` frames.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum RpcRequest {
    Launch { item_id: String },
    Events { client_id: String },
    Acknowledge { client_id: String, sequence: u64 },
    History,
    Observe { observation: RpcObservation },
    Tick,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RpcObservation {
    SessionRunning,
    SessionExitedCleanly,
    SessionCrashed { summary: String },
    UnitInactive,
    TargetReleased,
    SelectedOwnerActive,
    PresentationAcknowledged,
}

impl From<RpcObservation> for Observation {
    fn from(value: RpcObservation) -> Self {
        match value {
            RpcObservation::SessionRunning => Self::SessionRunning,
            RpcObservation::SessionExitedCleanly => Self::SessionExitedCleanly,
            RpcObservation::SessionCrashed { summary } => Self::SessionCrashed { summary },
            RpcObservation::UnitInactive => Self::UnitInactive,
            RpcObservation::TargetReleased => Self::TargetReleased,
            RpcObservation::SelectedOwnerActive => Self::SelectedOwnerActive,
            RpcObservation::PresentationAcknowledged => Self::PresentationAcknowledged,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum RpcResponse {
    Accepted { session_id: String },
    RejectedBusy,
    ItemUnavailable,
    Events { events: Vec<(u64, RpcEvent)> },
    History { events: Vec<RpcEvent> },
    Ok,
    Error { message: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum RpcEvent {
    Starting,
    Running,
    ObservationComplete,
    Returned { session_id: String },
    ForcedClose { session_id: String },
    Crash { session_id: String, summary: String },
    RecoveryRequired { session_id: String, reason: String },
}

impl From<SessionEvent> for RpcEvent {
    fn from(value: SessionEvent) -> Self {
        match value {
            SessionEvent::Observed(ObservedSessionState::Starting) => Self::Starting,
            SessionEvent::Observed(ObservedSessionState::Running) => Self::Running,
            SessionEvent::Observed(ObservedSessionState::ObservationComplete) => {
                Self::ObservationComplete
            }
            SessionEvent::Observed(ObservedSessionState::Suspended) => Self::Running,
            SessionEvent::Terminal(TerminalReceipt::Returned { session_id }) => {
                Self::Returned { session_id }
            }
            SessionEvent::Terminal(TerminalReceipt::ForcedClose { session_id }) => {
                Self::ForcedClose { session_id }
            }
            SessionEvent::Terminal(TerminalReceipt::Crash {
                session_id,
                summary,
            }) => Self::Crash {
                session_id,
                summary,
            },
            SessionEvent::RecoveryRequired(value) => Self::RecoveryRequired {
                session_id: value.session_id,
                reason: value.reason,
            },
        }
    }
}

pub fn serve_connection<S: StateStore, B: SessionSystem, C: Clock>(
    authority: &mut Authority<S, B, C>,
    stream: &mut impl io::Read,
    writer: &mut impl io::Write,
) -> Result<(), AuthorityError> {
    let body = pf_wire::read_frame(stream).map_err(|e| AuthorityError::Backend(e.to_string()))?;
    let request: RpcRequest =
        serde_json::from_slice(&body).map_err(|e| AuthorityError::Backend(e.to_string()))?;
    let response = handle_rpc(authority, request).unwrap_or_else(|error| RpcResponse::Error {
        message: format!("{error:?}"),
    });
    let body = serde_json::to_vec(&response).map_err(|e| AuthorityError::Backend(e.to_string()))?;
    pf_wire::write_frame(writer, &body).map_err(|e| AuthorityError::Backend(e.to_string()))
}

fn handle_rpc<S: StateStore, B: SessionSystem, C: Clock>(
    authority: &mut Authority<S, B, C>,
    request: RpcRequest,
) -> Result<RpcResponse, AuthorityError> {
    Ok(match request {
        RpcRequest::Launch { item_id } => match authority.launch(LaunchRequest { item_id })? {
            LaunchResult::Accepted { session_id } => RpcResponse::Accepted { session_id },
            LaunchResult::RejectedBusy => RpcResponse::RejectedBusy,
            LaunchResult::ItemUnavailable => RpcResponse::ItemUnavailable,
        },
        RpcRequest::Events { client_id } => RpcResponse::Events {
            events: authority
                .events_for(&client_id)
                .into_iter()
                .map(|(s, e)| (s, e.into()))
                .collect(),
        },
        RpcRequest::Acknowledge {
            client_id,
            sequence,
        } => {
            authority.acknowledge(&client_id, sequence)?;
            RpcResponse::Ok
        }
        RpcRequest::History => RpcResponse::History {
            events: authority.history().into_iter().map(Into::into).collect(),
        },
        RpcRequest::Observe { observation } => {
            authority.observe(observation.into())?;
            RpcResponse::Ok
        }
        RpcRequest::Tick => {
            authority.tick()?;
            RpcResponse::Ok
        }
    })
}

pub struct Authority<S, B, C> {
    store: S,
    system: B,
    clock: C,
    state: PersistedState,
    recent_bound: usize,
    grace: Duration,
    graceful_deadline: Option<MonotonicTime>,
}

impl<S: StateStore, B: SessionSystem, C: Clock> Authority<S, B, C> {
    pub fn open(
        store: S,
        system: B,
        clock: C,
        recent_bound: usize,
        grace: Duration,
    ) -> Result<Self, AuthorityError> {
        let state = store.load()?.unwrap_or_default();
        Ok(Self {
            store,
            system,
            clock,
            state,
            recent_bound: recent_bound.max(1),
            grace,
            graceful_deadline: None,
        })
    }
    pub fn state(&self) -> &PersistedState {
        &self.state
    }
    pub fn into_parts(self) -> (S, B, C) {
        (self.store, self.system, self.clock)
    }
    fn persist(&mut self) -> Result<(), AuthorityError> {
        self.store.save(&self.state)
    }
    fn session_id(&self) -> Option<String> {
        match &self.state.phase {
            Phase::Idle => None,
            Phase::Starting { session_id, .. }
            | Phase::Running { session_id }
            | Phase::StoppingGracefully { session_id, .. }
            | Phase::ForceStopping { session_id }
            | Phase::Restoring { session_id, .. }
            | Phase::RecoveryRequired { session_id, .. } => Some(session_id.clone()),
        }
    }
    fn publish(&mut self, event: WireEvent) {
        let sequence = self.state.next_sequence;
        self.state.next_sequence += 1;
        self.state
            .pending
            .push_back(PublishedEvent { sequence, event });
    }
    fn compact_acknowledged(&mut self) {
        let Some(floor) = self.state.acknowledged.values().copied().min() else {
            return;
        };
        while self
            .state
            .pending
            .front()
            .is_some_and(|event| event.sequence <= floor)
        {
            self.state.pending.pop_front();
        }
    }
    fn recover(&mut self, reason: String) -> Result<(), AuthorityError> {
        let session_id = self
            .session_id()
            .ok_or(AuthorityError::InvalidObservation)?;
        self.state.phase = Phase::RecoveryRequired {
            session_id: session_id.clone(),
            reason: reason.clone(),
        };
        self.publish(WireEvent::RecoveryRequired { session_id, reason });
        self.persist()
    }
    pub fn update_safe_return_binding(&mut self, revision: u64) -> Result<(), AuthorityError> {
        if revision > self.state.safe_return_binding_revision {
            self.state.safe_return_binding_revision = revision;
            self.persist()?;
        }
        Ok(())
    }
    /// Durable protected intake, deliberately separate from any foreground application's input.
    pub fn intake_safe_return(&mut self) -> Result<(), AuthorityError> {
        self.state.safe_return_queue = self.state.safe_return_queue.saturating_add(1);
        self.persist()
    }
    pub fn reconcile(&mut self) -> Result<(), AuthorityError> {
        if matches!(
            self.state.phase,
            Phase::Starting {
                start_invoked: false,
                ..
            }
        ) {
            return self
                .recover("interrupted start intent: no foreground unit was observed".into());
        }
        if let Phase::StoppingGracefully { session_id, .. } = self.state.phase.clone() {
            if self.graceful_deadline.is_none() {
                if let Err(reason) = self.system.enforce_termination(&session_id) {
                    return self.recover(format!("termination: {reason}"));
                }
                self.state.phase = Phase::ForceStopping { session_id };
                return self.persist();
            }
        }
        if self.state.safe_return_queue > 0
            && matches!(
                self.state.phase,
                Phase::Starting { .. } | Phase::Running { .. }
            )
        {
            self.state.safe_return_queue -= 1;
            let id = self.session_id().unwrap();
            match self.system.request_graceful_stop(&id) {
                Ok(()) => {
                    self.graceful_deadline = Some(self.clock.deadline_after(self.grace).0);
                    self.state.phase = Phase::StoppingGracefully {
                        session_id: id,
                        boot_marker: boot_marker(),
                    }
                }
                Err(_) => {
                    if let Err(reason) = self.system.enforce_termination(&id) {
                        return self.recover(format!("termination: {reason}"));
                    }
                    self.state.phase = Phase::ForceStopping { session_id: id };
                }
            }
            self.persist()?;
        }
        Ok(())
    }
    pub fn tick(&mut self) -> Result<(), AuthorityError> {
        self.reconcile()?;
        if let Phase::StoppingGracefully { session_id, .. } = self.state.phase.clone() {
            if self
                .graceful_deadline
                .is_some_and(|deadline| self.clock.now() >= deadline)
            {
                if let Err(reason) = self.system.enforce_termination(&session_id) {
                    return self.recover(format!("termination: {reason}"));
                }
                self.state.phase = Phase::ForceStopping { session_id };
                self.graceful_deadline = None;
                self.persist()?;
            }
        }
        Ok(())
    }
    fn begin_restoration(&mut self, receipt: Receipt) -> Result<(), AuthorityError> {
        let session_id = self
            .session_id()
            .ok_or(AuthorityError::InvalidObservation)?;
        self.state.phase = Phase::Restoring {
            session_id,
            receipt,
            rung: RestorationRung::UnitInactive,
        };
        self.persist()
    }
    fn unit_inactive(&mut self, receipt: Receipt) -> Result<(), AuthorityError> {
        let session_id = self
            .session_id()
            .ok_or(AuthorityError::InvalidObservation)?;
        self.state.phase = Phase::Restoring {
            session_id,
            receipt,
            rung: RestorationRung::TargetReleased,
        };
        self.persist()
    }
    pub fn observe(&mut self, observation: Observation) -> Result<(), AuthorityError> {
        if let Observation::Failed { rung, reason } = observation {
            return self.recover(format!("{rung:?}: {reason}"));
        }
        match (&self.state.phase, observation) {
            (Phase::Starting { .. }, Observation::SessionRunning) => {
                let id = self.session_id().unwrap();
                self.state.phase = Phase::Running { session_id: id };
                self.publish(WireEvent::ObservedRunning);
                self.persist()
            }
            (Phase::Starting { .. } | Phase::Running { .. }, Observation::SessionExitedCleanly) => {
                self.begin_restoration(Receipt::Returned)
            }
            (
                Phase::Starting { .. } | Phase::Running { .. },
                Observation::SessionCrashed { summary },
            ) => self.begin_restoration(Receipt::Crash { summary }),
            (Phase::StoppingGracefully { .. }, Observation::UnitInactive) => {
                self.unit_inactive(Receipt::Returned)
            }
            (Phase::ForceStopping { .. }, Observation::UnitInactive) => {
                self.unit_inactive(Receipt::ForcedClose)
            }
            (
                Phase::Restoring {
                    session_id,
                    receipt,
                    rung: RestorationRung::UnitInactive,
                },
                Observation::UnitInactive,
            ) => {
                self.state.phase = Phase::Restoring {
                    session_id: session_id.clone(),
                    receipt: receipt.clone(),
                    rung: RestorationRung::TargetReleased,
                };
                self.persist()
            }
            (
                Phase::Restoring {
                    session_id,
                    receipt,
                    rung: RestorationRung::TargetReleased,
                },
                Observation::TargetReleased,
            ) => {
                let session_id = session_id.clone();
                let receipt = receipt.clone();
                if let Err(reason) = self.system.activate_selected_owner() {
                    return self.recover(format!("owner activation: {reason}"));
                }
                self.state.phase = Phase::Restoring {
                    session_id,
                    receipt,
                    rung: RestorationRung::OwnerActive,
                };
                self.persist()
            }
            (
                Phase::Restoring {
                    session_id,
                    receipt,
                    rung: RestorationRung::OwnerActive,
                },
                Observation::SelectedOwnerActive,
            ) => {
                self.state.phase = Phase::Restoring {
                    session_id: session_id.clone(),
                    receipt: receipt.clone(),
                    rung: RestorationRung::PresentationAcknowledged,
                };
                self.persist()
            }
            (
                Phase::Restoring {
                    session_id,
                    receipt,
                    rung: RestorationRung::PresentationAcknowledged,
                },
                Observation::PresentationAcknowledged,
            ) => {
                let id = session_id.clone();
                let receipt = receipt.clone();
                if let Some(entry) = self.state.history.iter_mut().find(|e| e.session_id == id) {
                    entry.receipt = Some(receipt.clone());
                }
                self.publish(WireEvent::ObservationComplete);
                self.publish(WireEvent::Terminal {
                    session_id: id,
                    receipt,
                });
                self.state.phase = Phase::Idle;
                self.persist()
            }
            _ => Err(AuthorityError::InvalidObservation),
        }
    }
}

impl<S: StateStore, B: SessionSystem, C: Clock> AuthorityApi for Authority<S, B, C> {
    fn launch(&mut self, request: LaunchRequest) -> Result<LaunchResult, AuthorityError> {
        if !matches!(self.state.phase, Phase::Idle) {
            return Ok(LaunchResult::RejectedBusy);
        }
        let id = format!("session-{}", self.state.next_session);
        let before_intent = self.state.clone();
        self.state.next_session += 1;
        self.state.phase = Phase::Starting {
            session_id: id.clone(),
            item_id: request.item_id.clone(),
            start_invoked: false,
        };
        if let Err(error) = self.persist() {
            self.state = before_intent;
            return Err(error);
        }
        let available = self
            .system
            .start_foreground(&request, &id)
            .map_err(AuthorityError::Backend)?;
        if !available {
            self.state.phase = Phase::Idle;
            self.persist()?;
            return Ok(LaunchResult::ItemUnavailable);
        }
        self.state.phase = Phase::Starting {
            session_id: id.clone(),
            item_id: request.item_id.clone(),
            start_invoked: true,
        };
        self.state.history.push_front(HistoryEntry {
            session_id: id.clone(),
            item_id: request.item_id,
            receipt: None,
        });
        self.state.history.truncate(self.recent_bound);
        self.publish(WireEvent::ObservedStarting);
        self.persist()?;
        Ok(LaunchResult::Accepted { session_id: id })
    }
    fn events_for(&self, client_id: &str) -> Vec<(u64, SessionEvent)> {
        let sequence = self.state.acknowledged.get(client_id).copied().unwrap_or(0);
        self.state
            .pending
            .iter()
            .filter(|e| e.sequence > sequence)
            .map(|e| (e.sequence, wire_to_port(&e.event)))
            .collect()
    }
    fn acknowledge(&mut self, client_id: &str, sequence: u64) -> Result<(), AuthorityError> {
        if sequence >= self.state.next_sequence {
            return Err(AuthorityError::InvalidObservation);
        }
        let before_acknowledgement = self.state.clone();
        let cursor = self
            .state
            .acknowledged
            .entry(client_id.to_owned())
            .or_default();
        *cursor = (*cursor).max(sequence);
        self.compact_acknowledged();
        if let Err(error) = self.persist() {
            self.state = before_acknowledgement;
            return Err(error);
        }
        Ok(())
    }
    fn history(&self) -> Vec<SessionEvent> {
        self.state
            .history
            .iter()
            .filter_map(|h| {
                h.receipt
                    .as_ref()
                    .map(|r| SessionEvent::Terminal(receipt_to_port(r, &h.session_id)))
            })
            .collect()
    }
}

fn boot_marker() -> String {
    fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|value| value.trim().to_owned())
        .unwrap_or_else(|_| "boot-id-unavailable".to_owned())
}

fn receipt_to_port(receipt: &Receipt, id: &str) -> TerminalReceipt {
    match receipt {
        Receipt::Returned => TerminalReceipt::Returned {
            session_id: id.into(),
        },
        Receipt::ForcedClose => TerminalReceipt::ForcedClose {
            session_id: id.into(),
        },
        Receipt::Crash { summary } => TerminalReceipt::Crash {
            session_id: id.into(),
            summary: summary.clone(),
        },
    }
}
fn wire_to_port(event: &WireEvent) -> SessionEvent {
    match event {
        WireEvent::ObservedStarting => SessionEvent::Observed(ObservedSessionState::Starting),
        WireEvent::ObservedRunning => SessionEvent::Observed(ObservedSessionState::Running),
        WireEvent::ObservationComplete => {
            SessionEvent::Observed(ObservedSessionState::ObservationComplete)
        }
        WireEvent::Terminal {
            session_id,
            receipt,
        } => SessionEvent::Terminal(receipt_to_port(receipt, session_id)),
        WireEvent::RecoveryRequired { session_id, reason } => {
            SessionEvent::RecoveryRequired(RecoveryRequired {
                session_id: session_id.clone(),
                reason: reason.clone(),
            })
        }
    }
}

#[cfg(test)]
mod tests;
