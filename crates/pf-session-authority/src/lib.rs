//! Independent, transport-neutral foreground session authority.
//!
//! A terminal receipt is truthful only after this observation ladder completes in order:
//! foreground unit inactive, foreground target released, selected shell owner active, and a
//! real shell presentation acknowledged. The core atomically persists the completed receipt
//! before publishing it. Failure at termination or at any ladder rung atomically records
//! [`RecoveryRequired`] instead; an unavailable shell is never asked to render its own failure.
//!
//! Session history uses user-facing wall-clock time. An NTP step during a session can skew one
//! entry, so consumers must clamp negative durations to zero. A duration is derivable only when
//! both timestamps are present: an absent stamp means "unknown", never a zero-length session.
//! Aggregation is deliberately a consumer concern.
//!
//! `pf-session-authorityd` uses device systemd command templates by default. Desktop and
//! simulator runs can select its `desktop-sim` command preset, which represents running
//! sessions with marker files under the daemon state directory while exercising the same
//! [`CommandTemplates`] substitution and execution path as the device commands.
//!
//! The authority never waits on a client to make progress: [`run_service_loop`] drives
//! [`Authority::tick`] at least once per tick interval between RPCs, so crash/exit detection,
//! the restoration ladder and its deadlines advance while no shell is connected. The
//! presentation-acknowledgement rung has a deadline ([`DEFAULT_PRESENTATION_TIMEOUT`]); on expiry
//! the authority records `RecoveryRequired` with reason `presentation_not_acknowledged` and keeps
//! the owed receipt, so a late acknowledgement still completes the ladder truthfully. That is the
//! only recovery a client can complete; every other `RecoveryRequired` reason stays terminal.

use pf_app_manifest::{validate_app_id, ReasonCode, Resolver};
use pf_ports::{
    Clock, LaunchRequest, LaunchResult, MonotonicTime, ObservedSessionState, RecoveryRequired,
    SessionEvent, TerminalReceipt,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io;
use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

/// Default deadline for the restored shell to acknowledge its first presentation.
pub const DEFAULT_PRESENTATION_TIMEOUT: Duration = Duration::from_secs(10);

/// Default cadence of the daemon's self-driven reconcile/deadline tick.
pub const DEFAULT_TICK_INTERVAL: Duration = Duration::from_secs(1);

/// Bounds for the daemon's connection I/O, which runs off the authority loop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConnectionLimits {
    /// Read timeout for one complete request frame, and write timeout for the response.
    pub io_timeout: Duration,
    /// Longest a connection waits for the authority loop's response.
    pub response_timeout: Duration,
    /// Connections served concurrently; further connections are closed immediately.
    pub max_connections: usize,
}

pub const DEFAULT_CONNECTION_LIMITS: ConnectionLimits = ConnectionLimits {
    io_timeout: Duration::from_secs(5),
    response_timeout: Duration::from_secs(30),
    max_connections: 16,
};

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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum EndPrecision {
    Observed,
    Approximate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct EndStamp {
    pub at: SystemTime,
    pub precision: EndPrecision,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub session_id: String,
    pub item_id: String,
    pub receipt: Option<Receipt>,
    #[serde(default)]
    pub started_at: Option<SystemTime>,
    #[serde(default)]
    pub ended_at: Option<EndStamp>,
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
        #[serde(default)]
        item_id: String,
    },
    StoppingGracefully {
        session_id: String,
        #[serde(default)]
        item_id: String,
        boot_marker: String,
    },
    ForceStopping {
        session_id: String,
        #[serde(default)]
        item_id: String,
    },
    Restoring {
        session_id: String,
        #[serde(default)]
        item_id: String,
        receipt: Receipt,
        rung: RestorationRung,
    },
    RecoveryRequired {
        session_id: String,
        #[serde(default)]
        item_id: String,
        reason: String,
        /// Receipt still owed to history. Present only when the presentation-acknowledgement
        /// deadline expired: a late acknowledgement then completes the ladder with it. Absent for
        /// every other (terminal) recovery reason.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pending_receipt: Option<Receipt>,
    },
}

impl Phase {
    /// Stable snake_case phase name used in diagnostics.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Starting { .. } => "starting",
            Self::Running { .. } => "running",
            Self::StoppingGracefully { .. } => "stopping_gracefully",
            Self::ForceStopping { .. } => "force_stopping",
            Self::Restoring { .. } => "restoring",
            Self::RecoveryRequired { .. } => "recovery_required",
        }
    }
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

    fn query(&mut self, program: &str, args: &[String]) -> Result<CommandOutput, String> {
        self.execute(program, args).map(|code| CommandOutput {
            code,
            stdout: String::new(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandOutput {
    pub code: i32,
    pub stdout: String,
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

    fn query(&mut self, program: &str, args: &[String]) -> Result<CommandOutput, String> {
        let output = Command::new(program)
            .args(args)
            .output()
            .map_err(|error| error.to_string())?;
        Ok(CommandOutput {
            code: output.status.code().unwrap_or(1),
            stdout: String::from_utf8(output.stdout).map_err(|error| error.to_string())?,
        })
    }
}

/// Command templates used by [`CommandSystem`]. Tokens support `{item_id}` and `{session_id}`.
#[derive(Clone, Debug)]
pub struct CommandTemplates {
    pub start_foreground: Vec<String>,
    pub request_graceful_stop: Vec<String>,
    pub enforce_termination: Vec<String>,
    pub activate_selected_owner: Vec<String>,
    systemd_reconciliation: bool,
}

impl Default for CommandTemplates {
    fn default() -> Self {
        Self {
            start_foreground: words("systemctl start pf-app@{item_id}.service"),
            request_graceful_stop: words("systemctl stop pf-app@{item_id}.service"),
            enforce_termination: words("systemctl kill --kill-who=all pf-app@{item_id}.service"),
            activate_selected_owner: words("systemctl start pf-shell-selected.service"),
            systemd_reconciliation: true,
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
            systemd_reconciliation: false,
        }
    }

    /// Builds the dependency-free shell templates used by the daemon's `desktop-sim` preset.
    ///
    /// Starting a session creates `sessions/{session_id}.running`, graceful and forced stops
    /// remove it idempotently, and activating the selected owner creates `shell-selected`.
    pub fn desktop_sim(state_dir: &Path) -> Self {
        let state_dir = state_dir.as_os_str().to_string_lossy().into_owned();
        Self {
            start_foreground: vec![
                "sh".into(),
                "-c".into(),
                "mkdir -p \"$1/sessions\" && touch \"$1/sessions/$2.running\"".into(),
                "pf-session-authorityd".into(),
                state_dir.clone(),
                "{session_id}".into(),
            ],
            request_graceful_stop: desktop_sim_remove_template(&state_dir),
            enforce_termination: desktop_sim_remove_template(&state_dir),
            activate_selected_owner: vec![
                "sh".into(),
                "-c".into(),
                "touch \"$1/shell-selected\"".into(),
                "pf-session-authorityd".into(),
                state_dir,
            ],
            systemd_reconciliation: false,
        }
    }
}

fn desktop_sim_remove_template(state_dir: &str) -> Vec<String> {
    vec![
        "sh".into(),
        "-c".into(),
        "rm -f \"$1/sessions/$2.running\"".into(),
        "pf-session-authorityd".into(),
        state_dir.into(),
        "{session_id}".into(),
    ]
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

    fn query(&mut self, args: &[&str]) -> Result<CommandOutput, String> {
        self.executor.query(
            "systemctl",
            &args
                .iter()
                .map(|value| (*value).to_owned())
                .collect::<Vec<_>>(),
        )
    }

    fn property(&mut self, unit: &str, property: &str) -> Result<String, String> {
        let output = self.query(&["show", "--property", property, "--value", unit])?;
        if output.code != 0 {
            return Err(format!(
                "systemctl show {property} {unit} exited {}",
                output.code
            ));
        }
        Ok(output.stdout.trim().to_owned())
    }

    fn is_active(&mut self, unit: &str) -> Result<bool, String> {
        match self.query(&["is-active", "--quiet", unit])?.code {
            0 => Ok(true),
            3 => Ok(false),
            code => Err(format!("systemctl is-active {unit} exited {code}")),
        }
    }
}

/// Trait-shaped image/service integration. F13 supplies the real systemd implementation.
pub trait SessionSystem {
    fn start_foreground(&mut self, request: &LaunchRequest, session_id: &str)
        -> Result<(), String>;
    fn request_graceful_stop(&mut self, item_id: &str, session_id: &str) -> Result<(), String>;
    fn enforce_termination(&mut self, item_id: &str, session_id: &str) -> Result<(), String>;
    fn activate_selected_owner(&mut self) -> Result<(), String>;
    fn lifecycle(&mut self, _item_id: &str) -> Result<Option<SystemLifecycle>, String> {
        Ok(None)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AppUnitState {
    Active,
    InactiveSuccess,
    InactiveFailure { summary: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SystemLifecycle {
    pub app: AppUnitState,
    pub foreground_target_active: bool,
    pub selected_owner_active: bool,
}

impl<E: CommandExecutor> SessionSystem for CommandSystem<E> {
    fn start_foreground(
        &mut self,
        request: &LaunchRequest,
        session_id: &str,
    ) -> Result<(), String> {
        let template = self.templates.start_foreground.clone();
        command_ok(self.run(&template, &request.item_id, session_id)?)
    }
    fn request_graceful_stop(&mut self, item_id: &str, session_id: &str) -> Result<(), String> {
        let template = self.templates.request_graceful_stop.clone();
        command_ok(self.run(&template, item_id, session_id)?)
    }
    fn enforce_termination(&mut self, item_id: &str, session_id: &str) -> Result<(), String> {
        let template = self.templates.enforce_termination.clone();
        command_ok(self.run(&template, item_id, session_id)?)
    }
    fn activate_selected_owner(&mut self) -> Result<(), String> {
        let template = self.templates.activate_selected_owner.clone();
        command_ok(self.run(&template, "", "")?)
    }

    fn lifecycle(&mut self, item_id: &str) -> Result<Option<SystemLifecycle>, String> {
        if !self.templates.systemd_reconciliation {
            return Ok(None);
        }
        let unit = format!("pf-app@{item_id}.service");
        let active_state = self.property(&unit, "ActiveState")?;
        let app = match active_state.as_str() {
            "active" | "activating" | "reloading" | "deactivating" => AppUnitState::Active,
            "inactive" | "failed" => {
                let result = self.property(&unit, "Result")?;
                if active_state == "inactive" && (result.is_empty() || result == "success") {
                    AppUnitState::InactiveSuccess
                } else {
                    AppUnitState::InactiveFailure {
                        summary: format!("systemd result: {result}"),
                    }
                }
            }
            state => return Err(format!("unknown ActiveState {state:?} for {unit}")),
        };
        Ok(Some(SystemLifecycle {
            app,
            foreground_target_active: self.is_active("pocketforge-foreground.target")?,
            selected_owner_active: self.is_active("pf-shell-selected.service")?,
        }))
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

impl FailureRung {
    const fn reason_code(self) -> ReasonCode {
        match self {
            Self::Termination | Self::UnitInactive => ReasonCode::AppExitFailed,
            Self::TargetReleased => ReasonCode::TargetNotReleased,
            Self::OwnerActivation | Self::OwnerActive => ReasonCode::OwnerNotActive,
            Self::Presentation => ReasonCode::PresentationNotAcknowledged,
        }
    }
}

pub trait AuthorityApi {
    fn launch(&mut self, request: LaunchRequest) -> Result<LaunchResult, AuthorityError>;
    fn events_for(&self, client_id: &str) -> Vec<(u64, SessionEvent)>;
    fn try_events_for(&self, client_id: &str) -> Result<Vec<(u64, SessionEvent)>, AuthorityError> {
        Ok(self.events_for(client_id))
    }
    fn acknowledge(&mut self, client_id: &str, sequence: u64) -> Result<(), AuthorityError>;
    fn history(&self) -> Vec<SessionEvent>;
    fn history_entries(&self) -> Vec<HistoryEntry> {
        Vec::new()
    }
}

/// Versioned session-authority RPC payload carried inside `pf-wire` frames.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum RpcRequest {
    Launch { item_id: String },
    SafeReturn,
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
    History { entries: Vec<HistoryEntry> },
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

/// Serves one request synchronously on the caller's thread (in-process tests and simulators).
/// It blocks on the peer's framing, so the daemon never calls it from the authority loop; see
/// [`spawn_rpc_acceptor`].
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

/// Runs one complete, decoded request against the authority. Never touches a socket.
pub fn dispatch_rpc<S: StateStore, B: SessionSystem, C: Clock>(
    authority: &mut Authority<S, B, C>,
    request: RpcRequest,
) -> RpcResponse {
    handle_rpc(authority, request).unwrap_or_else(|error| RpcResponse::Error {
        message: format!("{error:?}"),
    })
}

/// A complete request read and decoded by a connection thread, with its response channel.
pub struct PendingRpc {
    pub request: RpcRequest,
    reply: mpsc::Sender<RpcResponse>,
}

impl PendingRpc {
    /// Hands the response back to the connection thread; a vanished client is not an error.
    pub fn respond(self, response: RpcResponse) {
        let _ = self.reply.send(response);
    }
}

/// Accepts connections on a helper thread and serves each on its own bounded thread.
///
/// Framing never runs on the authority loop: a connection thread reads and decodes one request
/// under `io_timeout`, forwards only the complete request, waits at most `response_timeout` for
/// the reply and writes it under `io_timeout`. A silent, partial or non-reading client therefore
/// costs one bounded connection thread, never an authority tick. An accept error is forwarded and
/// ends the acceptor.
pub fn spawn_rpc_acceptor(
    listener: UnixListener,
    requests: mpsc::Sender<io::Result<PendingRpc>>,
    limits: ConnectionLimits,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let active = Arc::new(AtomicUsize::new(0));
        for connection in listener.incoming() {
            let stream = match connection {
                Ok(stream) => stream,
                Err(error) => {
                    let _ = requests.send(Err(error));
                    return;
                }
            };
            if active.fetch_add(1, Ordering::AcqRel) >= limits.max_connections {
                active.fetch_sub(1, Ordering::AcqRel);
                eprintln!(
                    "pf-session-authorityd: connection_refused reason=too_many_connections limit={}",
                    limits.max_connections
                );
                continue;
            }
            let (requests, active) = (requests.clone(), active.clone());
            thread::spawn(move || {
                if let Err(error) = serve_rpc_connection(stream, &requests, limits) {
                    eprintln!("pf-session-authorityd: connection error: {error:?}");
                }
                active.fetch_sub(1, Ordering::AcqRel);
            });
        }
    })
}

fn serve_rpc_connection(
    mut stream: UnixStream,
    requests: &mpsc::Sender<io::Result<PendingRpc>>,
    limits: ConnectionLimits,
) -> Result<(), AuthorityError> {
    let backend = |error: &dyn std::fmt::Display| AuthorityError::Backend(error.to_string());
    stream
        .set_read_timeout(Some(limits.io_timeout))
        .and_then(|()| stream.set_write_timeout(Some(limits.io_timeout)))
        .map_err(|e| backend(&e))?;
    let body = pf_wire::read_frame(&mut stream).map_err(|e| backend(&e))?;
    let response = match serde_json::from_slice::<RpcRequest>(&body) {
        Ok(request) => {
            let (reply, response) = mpsc::channel();
            if requests.send(Ok(PendingRpc { request, reply })).is_err() {
                return Err(AuthorityError::Backend("authority loop stopped".into()));
            }
            response
                .recv_timeout(limits.response_timeout)
                .map_err(|e| backend(&e))?
        }
        Err(error) => return Err(backend(&error)),
    };
    let body = serde_json::to_vec(&response).map_err(|e| backend(&e))?;
    pf_wire::write_frame(&mut stream, &body).map_err(|e| backend(&e))
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
        RpcRequest::SafeReturn => {
            if matches!(
                authority.state.phase,
                Phase::Starting { .. } | Phase::Running { .. }
            ) {
                authority.intake_safe_return()?;
                authority.reconcile()?;
            }
            RpcResponse::Ok
        }
        RpcRequest::Events { client_id } => RpcResponse::Events {
            events: {
                authority.reconcile()?;
                authority
                    .events_for(&client_id)
                    .into_iter()
                    .map(|(s, e)| (s, e.into()))
                    .collect()
            },
        },
        RpcRequest::Acknowledge {
            client_id,
            sequence,
        } => {
            authority.acknowledge(&client_id, sequence)?;
            RpcResponse::Ok
        }
        RpcRequest::History => RpcResponse::History {
            entries: authority.history_entries(),
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

/// Receives one complete diagnostic line (no trailing newline).
pub type LogSink = Box<dyn FnMut(&str) + Send>;

fn stderr_log_sink() -> LogSink {
    Box::new(|line: &str| eprintln!("{line}"))
}

pub struct Authority<S, B, C> {
    store: S,
    system: B,
    clock: C,
    resolver: Resolver,
    state: PersistedState,
    recent_bound: usize,
    grace: Duration,
    graceful_deadline: Option<MonotonicTime>,
    presentation_timeout: Duration,
    presentation_deadline: Option<MonotonicTime>,
    log: LogSink,
    now_fn: fn() -> SystemTime,
}

impl<S: StateStore, B: SessionSystem, C: Clock> Authority<S, B, C> {
    pub fn open(
        store: S,
        system: B,
        clock: C,
        recent_bound: usize,
        grace: Duration,
    ) -> Result<Self, AuthorityError> {
        Self::open_with_resolver_and_now_fn(
            store,
            system,
            clock,
            recent_bound,
            grace,
            Resolver::fixed(),
            SystemTime::now,
        )
    }

    pub fn open_with_now_fn(
        store: S,
        system: B,
        clock: C,
        recent_bound: usize,
        grace: Duration,
        now_fn: fn() -> SystemTime,
    ) -> Result<Self, AuthorityError> {
        Self::open_with_resolver_and_now_fn(
            store,
            system,
            clock,
            recent_bound,
            grace,
            Resolver::fixed(),
            now_fn,
        )
    }

    /// Open with injected filesystem roots for hermetic tests.
    pub fn open_with_resolver(
        store: S,
        system: B,
        clock: C,
        recent_bound: usize,
        grace: Duration,
        resolver: Resolver,
    ) -> Result<Self, AuthorityError> {
        Self::open_with_resolver_and_now_fn(
            store,
            system,
            clock,
            recent_bound,
            grace,
            resolver,
            SystemTime::now,
        )
    }

    pub fn open_with_resolver_and_now_fn(
        store: S,
        system: B,
        clock: C,
        recent_bound: usize,
        grace: Duration,
        resolver: Resolver,
        now_fn: fn() -> SystemTime,
    ) -> Result<Self, AuthorityError> {
        let state = store.load()?.unwrap_or_default();
        Ok(Self {
            store,
            system,
            clock,
            resolver,
            state,
            recent_bound: recent_bound.max(1),
            grace,
            graceful_deadline: None,
            presentation_timeout: DEFAULT_PRESENTATION_TIMEOUT,
            presentation_deadline: None,
            log: stderr_log_sink(),
            now_fn,
        })
    }
    /// Overrides [`DEFAULT_PRESENTATION_TIMEOUT`], the deadline for the restored shell to
    /// acknowledge presentation before the authority records `presentation_not_acknowledged`.
    pub fn with_presentation_timeout(mut self, timeout: Duration) -> Self {
        self.presentation_timeout = timeout;
        self
    }
    /// Replaces the default stderr diagnostic sink (tests capture lines with this).
    pub fn with_log_sink(mut self, sink: impl FnMut(&str) + Send + 'static) -> Self {
        self.log = Box::new(sink);
        self
    }
    fn log(&mut self, line: &str) {
        (self.log)(line);
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
            | Phase::Running { session_id, .. }
            | Phase::StoppingGracefully { session_id, .. }
            | Phase::ForceStopping { session_id, .. }
            | Phase::Restoring { session_id, .. }
            | Phase::RecoveryRequired { session_id, .. } => Some(session_id.clone()),
        }
    }
    fn item_id(&self) -> Option<String> {
        match &self.state.phase {
            Phase::Idle => None,
            Phase::Starting { item_id, .. }
            | Phase::Running { item_id, .. }
            | Phase::StoppingGracefully { item_id, .. }
            | Phase::ForceStopping { item_id, .. }
            | Phase::Restoring { item_id, .. }
            | Phase::RecoveryRequired { item_id, .. } => Some(item_id.clone()),
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
        self.recover_owing(reason, None)
    }
    fn recover_owing(
        &mut self,
        reason: String,
        pending_receipt: Option<Receipt>,
    ) -> Result<(), AuthorityError> {
        let session_id = self
            .session_id()
            .ok_or(AuthorityError::InvalidObservation)?;
        let item_id = self.item_id().unwrap_or_default();
        self.presentation_deadline = None;
        self.state.phase = Phase::RecoveryRequired {
            session_id: session_id.clone(),
            item_id,
            reason: reason.clone(),
            pending_receipt,
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
        if let Some(item_id) = self.item_id() {
            if !matches!(self.state.phase, Phase::RecoveryRequired { .. })
                && validate_app_id(&item_id).is_err()
            {
                return self.recover(format!(
                    "{}: invalid persisted item id",
                    ReasonCode::SystemdStateUnknown.as_str()
                ));
            }
        }
        if let Phase::Starting {
            session_id,
            item_id,
            start_invoked: false,
        } = self.state.phase.clone()
        {
            let snapshot = match self.system.lifecycle(&item_id) {
                Ok(Some(snapshot)) => snapshot,
                Ok(None) => {
                    return self.recover(format!(
                        "{}: interrupted start intent was not observable",
                        ReasonCode::SystemdStateUnknown.as_str()
                    ));
                }
                Err(reason) => {
                    return self.recover(format!(
                        "{}: {reason}",
                        ReasonCode::SystemdStateUnknown.as_str()
                    ));
                }
            };
            if !matches!(snapshot.app, AppUnitState::Active) {
                return self.recover(format!(
                    "{}: interrupted start intent has no active app unit",
                    ReasonCode::SystemdStartFailed.as_str()
                ));
            }
            self.state.phase = Phase::Starting {
                session_id: session_id.clone(),
                item_id: item_id.clone(),
                start_invoked: true,
            };
            if !self
                .state
                .history
                .iter()
                .any(|entry| entry.session_id == session_id)
            {
                self.state.history.push_front(HistoryEntry {
                    session_id,
                    item_id,
                    receipt: None,
                    started_at: None,
                    ended_at: None,
                });
                self.state.history.truncate(self.recent_bound);
                self.publish(WireEvent::ObservedStarting);
            }
            self.persist()?;
            self.observe(Observation::SessionRunning)?;
        }
        if let Phase::StoppingGracefully {
            session_id,
            item_id,
            ..
        } = self.state.phase.clone()
        {
            if self.graceful_deadline.is_none() {
                if let Err(reason) = self.system.enforce_termination(&item_id, &session_id) {
                    return self
                        .recover(format!("{}: {reason}", ReasonCode::AppExitFailed.as_str()));
                }
                self.state.phase = Phase::ForceStopping {
                    session_id,
                    item_id,
                };
                self.persist()?;
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
            let item_id = self.item_id().unwrap();
            match self.system.request_graceful_stop(&item_id, &id) {
                Ok(()) => {
                    self.graceful_deadline = Some(self.clock.deadline_after(self.grace).0);
                    self.state.phase = Phase::StoppingGracefully {
                        session_id: id,
                        item_id,
                        boot_marker: boot_marker(),
                    }
                }
                Err(_) => {
                    if let Err(reason) = self.system.enforce_termination(&item_id, &id) {
                        return self
                            .recover(format!("{}: {reason}", ReasonCode::AppExitFailed.as_str()));
                    }
                    self.state.phase = Phase::ForceStopping {
                        session_id: id,
                        item_id,
                    };
                }
            }
            self.persist()?;
        }
        self.reconcile_systemd()
    }
    pub fn tick(&mut self) -> Result<(), AuthorityError> {
        self.reconcile()?;
        if let Phase::StoppingGracefully {
            session_id,
            item_id,
            ..
        } = self.state.phase.clone()
        {
            if self
                .graceful_deadline
                .is_some_and(|deadline| self.clock.now() >= deadline)
            {
                if let Err(reason) = self.system.enforce_termination(&item_id, &session_id) {
                    return self
                        .recover(format!("{}: {reason}", ReasonCode::AppExitFailed.as_str()));
                }
                self.state.phase = Phase::ForceStopping {
                    session_id,
                    item_id,
                };
                self.graceful_deadline = None;
                self.persist()?;
                // reconcile() already drove systemd observations to a fixed point; only a phase
                // change here needs another pass. This keeps the self-driven 1 s tick at one
                // systemd snapshot while an app runs.
                self.reconcile_systemd()?;
            }
        }
        self.enforce_presentation_deadline()
    }
    /// Expires the presentation-acknowledgement rung. A deadline lost to a daemon restart is
    /// re-armed for a full timeout rather than failing immediately: unlike an overdue graceful
    /// stop, an early expiry here would only record a spurious recovery.
    fn enforce_presentation_deadline(&mut self) -> Result<(), AuthorityError> {
        let Phase::Restoring {
            item_id,
            receipt,
            rung: RestorationRung::PresentationAcknowledged,
            ..
        } = &self.state.phase
        else {
            self.presentation_deadline = None;
            return Ok(());
        };
        let (item_id, receipt) = (item_id.clone(), receipt.clone());
        let now = self.clock.now();
        let timeout = self.presentation_timeout;
        let deadline = *self
            .presentation_deadline
            .get_or_insert_with(|| now.saturating_add(timeout));
        if now < deadline {
            return Ok(());
        }
        let code = ReasonCode::PresentationNotAcknowledged.as_str();
        let detail = format!(
            "presentation not acknowledged within {} ms",
            timeout.as_millis()
        );
        self.log(&lifecycle_failure_line(code, &item_id, &detail));
        self.recover_owing(format!("{code}: {detail}"), Some(receipt))
    }
    fn reconcile_systemd(&mut self) -> Result<(), AuthorityError> {
        for _ in 0..8 {
            let Some(item_id) = self.item_id() else {
                return Ok(());
            };
            if matches!(self.state.phase, Phase::RecoveryRequired { .. }) {
                return Ok(());
            }
            let snapshot = match self.system.lifecycle(&item_id) {
                Ok(Some(snapshot)) => snapshot,
                Ok(None) => return Ok(()),
                Err(reason) => {
                    return self.recover(format!(
                        "{}: {reason}",
                        ReasonCode::SystemdStateUnknown.as_str()
                    ));
                }
            };
            let observation = match (&self.state.phase, snapshot.app) {
                (
                    Phase::Starting {
                        start_invoked: true,
                        ..
                    },
                    AppUnitState::Active,
                ) => Some(Observation::SessionRunning),
                (
                    Phase::Starting {
                        start_invoked: true,
                        ..
                    }
                    | Phase::Running { .. },
                    AppUnitState::InactiveSuccess,
                ) => Some(Observation::SessionExitedCleanly),
                (
                    Phase::Starting {
                        start_invoked: true,
                        ..
                    }
                    | Phase::Running { .. },
                    AppUnitState::InactiveFailure { summary },
                ) => Some(Observation::SessionCrashed { summary }),
                (
                    Phase::StoppingGracefully { .. }
                    | Phase::ForceStopping { .. }
                    | Phase::Restoring {
                        rung: RestorationRung::UnitInactive,
                        ..
                    },
                    AppUnitState::InactiveSuccess | AppUnitState::InactiveFailure { .. },
                ) => Some(Observation::UnitInactive),
                (
                    Phase::Restoring {
                        rung: RestorationRung::TargetReleased,
                        ..
                    },
                    _,
                ) if !snapshot.foreground_target_active => Some(Observation::TargetReleased),
                (
                    Phase::Restoring {
                        rung: RestorationRung::OwnerActive,
                        ..
                    },
                    _,
                ) if snapshot.selected_owner_active => Some(Observation::SelectedOwnerActive),
                _ => None,
            };
            let Some(observation) = observation else {
                return Ok(());
            };
            self.observe(observation)?;
        }
        Ok(())
    }
    fn begin_restoration(&mut self, receipt: Receipt) -> Result<(), AuthorityError> {
        let session_id = self
            .session_id()
            .ok_or(AuthorityError::InvalidObservation)?;
        let item_id = self.item_id().ok_or(AuthorityError::InvalidObservation)?;
        self.state.phase = Phase::Restoring {
            session_id,
            item_id,
            receipt,
            rung: RestorationRung::UnitInactive,
        };
        self.persist()
    }
    fn unit_inactive(&mut self, receipt: Receipt) -> Result<(), AuthorityError> {
        let session_id = self
            .session_id()
            .ok_or(AuthorityError::InvalidObservation)?;
        let item_id = self.item_id().ok_or(AuthorityError::InvalidObservation)?;
        if let Some(entry) = self
            .state
            .history
            .iter_mut()
            .find(|e| e.session_id == session_id)
        {
            entry.ended_at = Some(EndStamp {
                at: (self.now_fn)(),
                precision: EndPrecision::Observed,
            });
        }
        self.state.phase = Phase::Restoring {
            session_id,
            item_id,
            receipt,
            rung: RestorationRung::TargetReleased,
        };
        self.persist()
    }
    pub fn observe(&mut self, observation: Observation) -> Result<(), AuthorityError> {
        if let Observation::Failed { rung, reason } = observation {
            return self.recover(format!("{}: {reason}", rung.reason_code().as_str()));
        }
        match (&self.state.phase, observation) {
            (Phase::Starting { .. }, Observation::SessionRunning) => {
                let id = self.session_id().unwrap();
                let item_id = self.item_id().unwrap();
                if let Some(entry) = self.state.history.iter_mut().find(|e| e.session_id == id) {
                    entry.started_at = Some((self.now_fn)());
                }
                self.state.phase = Phase::Running {
                    session_id: id,
                    item_id,
                };
                self.publish(WireEvent::ObservedRunning);
                self.persist()
            }
            (Phase::Starting { .. } | Phase::Running { .. }, Observation::SessionExitedCleanly) => {
                let id = self.session_id().unwrap();
                if let Some(entry) = self.state.history.iter_mut().find(|e| e.session_id == id) {
                    entry.ended_at = Some(EndStamp {
                        at: (self.now_fn)(),
                        precision: EndPrecision::Observed,
                    });
                }
                self.begin_restoration(Receipt::Returned)
            }
            (
                Phase::Starting { .. } | Phase::Running { .. },
                Observation::SessionCrashed { summary },
            ) => {
                let id = self.session_id().unwrap();
                if let Some(entry) = self.state.history.iter_mut().find(|e| e.session_id == id) {
                    entry.ended_at = Some(EndStamp {
                        at: (self.now_fn)(),
                        precision: EndPrecision::Approximate,
                    });
                }
                self.begin_restoration(Receipt::Crash { summary })
            }
            (Phase::StoppingGracefully { .. }, Observation::UnitInactive) => {
                self.unit_inactive(Receipt::Returned)
            }
            (Phase::ForceStopping { .. }, Observation::UnitInactive) => {
                self.unit_inactive(Receipt::ForcedClose)
            }
            (
                Phase::Restoring {
                    session_id,
                    item_id,
                    receipt,
                    rung: RestorationRung::UnitInactive,
                },
                Observation::UnitInactive,
            ) => {
                self.state.phase = Phase::Restoring {
                    session_id: session_id.clone(),
                    item_id: item_id.clone(),
                    receipt: receipt.clone(),
                    rung: RestorationRung::TargetReleased,
                };
                self.persist()
            }
            (
                Phase::Restoring {
                    session_id,
                    item_id,
                    receipt,
                    rung: RestorationRung::TargetReleased,
                },
                Observation::TargetReleased,
            ) => {
                let session_id = session_id.clone();
                let item_id = item_id.clone();
                let receipt = receipt.clone();
                if let Err(reason) = self.system.activate_selected_owner() {
                    return self
                        .recover(format!("{}: {reason}", ReasonCode::OwnerNotActive.as_str()));
                }
                self.state.phase = Phase::Restoring {
                    session_id,
                    item_id,
                    receipt,
                    rung: RestorationRung::OwnerActive,
                };
                self.persist()
            }
            (
                Phase::Restoring {
                    session_id,
                    item_id,
                    receipt,
                    rung: RestorationRung::OwnerActive,
                },
                Observation::SelectedOwnerActive,
            ) => {
                self.state.phase = Phase::Restoring {
                    session_id: session_id.clone(),
                    item_id: item_id.clone(),
                    receipt: receipt.clone(),
                    rung: RestorationRung::PresentationAcknowledged,
                };
                self.presentation_deadline =
                    Some(self.clock.deadline_after(self.presentation_timeout).0);
                self.persist()
            }
            (
                Phase::Restoring {
                    session_id,
                    receipt,
                    rung: RestorationRung::PresentationAcknowledged,
                    ..
                },
                Observation::PresentationAcknowledged,
            ) => {
                let (id, receipt) = (session_id.clone(), receipt.clone());
                self.complete_restoration(id, receipt)
            }
            // A late acknowledgement after the presentation deadline expired: the only
            // self-healing exit from RecoveryRequired. The owed receipt becomes history now.
            (
                Phase::RecoveryRequired {
                    session_id,
                    reason,
                    pending_receipt: Some(receipt),
                    ..
                },
                Observation::PresentationAcknowledged,
            ) if is_presentation_timeout(reason) => {
                let (id, receipt) = (session_id.clone(), receipt.clone());
                self.complete_restoration(id, receipt)
            }
            _ => Err(AuthorityError::InvalidObservation),
        }
    }
    fn complete_restoration(&mut self, id: String, receipt: Receipt) -> Result<(), AuthorityError> {
        if let Some(entry) = self.state.history.iter_mut().find(|e| e.session_id == id) {
            entry.receipt = Some(receipt.clone());
        }
        self.publish(WireEvent::ObservationComplete);
        self.publish(WireEvent::Terminal {
            session_id: id,
            receipt,
        });
        self.presentation_deadline = None;
        self.state.phase = Phase::Idle;
        self.persist()
    }
    /// Pending observed Starting/Running events with a sequence below this bound belong to a
    /// session that has already ended and are never delivered. While a session is Starting or
    /// Running, every earlier session ended at its last Terminal/RecoveryRequired event; in any
    /// other phase every published Starting/Running belongs to an ended session. Filtering at
    /// delivery leaves `pending`, sequences, cursors and compaction untouched (cursor-safe) and
    /// also covers state persisted by an older daemon.
    fn ended_session_observation_bound(&self) -> u64 {
        if !matches!(
            self.state.phase,
            Phase::Starting { .. } | Phase::Running { .. }
        ) {
            return u64::MAX;
        }
        self.state
            .pending
            .iter()
            .rev()
            .find(|e| {
                matches!(
                    e.event,
                    WireEvent::Terminal { .. } | WireEvent::RecoveryRequired { .. }
                )
            })
            .map_or(0, |e| e.sequence)
    }
}

fn is_presentation_timeout(reason: &str) -> bool {
    reason
        .strip_prefix(ReasonCode::PresentationNotAcknowledged.as_str())
        .is_some_and(|rest| rest.starts_with(':'))
}

/// Runs the daemon's single-threaded service loop until `connections` disconnects.
///
/// Each received connection is handed to `serve`; between connections the loop calls
/// [`Authority::tick`] at least once per `tick_interval`, so the authority reconciles systemd and
/// enforces its deadlines with no client RPC. Waiting uses a channel timeout, never a spin; a tick
/// that overruns the interval is followed by a full idle interval. A tick error is logged once
/// until it changes or clears. A connection-source error or a `serve` error ends the loop.
pub fn run_service_loop<S, B, C, T>(
    authority: &mut Authority<S, B, C>,
    connections: &mpsc::Receiver<io::Result<T>>,
    tick_interval: Duration,
    mut serve: impl FnMut(&mut Authority<S, B, C>, T) -> io::Result<()>,
) -> io::Result<()>
where
    S: StateStore,
    B: SessionSystem,
    C: Clock,
{
    let tick_interval = tick_interval.max(Duration::from_millis(1));
    let mut next_tick = Instant::now() + tick_interval;
    let mut last_error: Option<String> = None;
    loop {
        let wait = next_tick.saturating_duration_since(Instant::now());
        match connections.recv_timeout(wait) {
            Ok(Ok(connection)) => serve(authority, connection)?,
            Ok(Err(error)) => return Err(error),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
        let started = Instant::now();
        if started < next_tick {
            continue;
        }
        match authority.tick() {
            Ok(()) => last_error = None,
            Err(error) => {
                let message = format!("{error:?}");
                if last_error.as_deref() != Some(message.as_str()) {
                    authority.log(&format!(
                        "pf-session-authorityd: tick_error detail={}",
                        json_string(&message)
                    ));
                }
                last_error = Some(message);
            }
        }
        next_tick = started + tick_interval;
        let finished = Instant::now();
        if next_tick <= finished {
            next_tick = finished + tick_interval;
        }
    }
}

impl<S: StateStore, B: SessionSystem, C: Clock> AuthorityApi for Authority<S, B, C> {
    fn launch(&mut self, request: LaunchRequest) -> Result<LaunchResult, AuthorityError> {
        if !matches!(self.state.phase, Phase::Idle) {
            let line = launch_busy_line(&request.item_id, &self.state.phase);
            self.log(&line);
            return Ok(LaunchResult::RejectedBusy);
        }
        if let Err(error) = self.resolver.resolve(&request.item_id) {
            let line = launch_refusal_line(error.reason.as_str(), &request.item_id);
            self.log(&line);
            return Ok(LaunchResult::ItemUnavailable);
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
        let start_result = self.system.start_foreground(&request, &id);
        self.state.phase = Phase::Starting {
            session_id: id.clone(),
            item_id: request.item_id.clone(),
            start_invoked: true,
        };
        self.state.history.push_front(HistoryEntry {
            session_id: id.clone(),
            item_id: request.item_id,
            receipt: None,
            started_at: None,
            ended_at: None,
        });
        self.state.history.truncate(self.recent_bound);
        self.publish(WireEvent::ObservedStarting);
        self.persist()?;
        match start_result {
            Ok(()) => self.observe(Observation::SessionRunning)?,
            Err(reason) => {
                let line = lifecycle_failure_line(
                    ReasonCode::SystemdStartFailed.as_str(),
                    self.item_id().as_deref().unwrap_or_default(),
                    &reason,
                );
                self.log(&line);
                self.observe(Observation::SessionCrashed {
                    summary: format!("{}: {reason}", ReasonCode::SystemdStartFailed.as_str()),
                })?;
            }
        }
        Ok(LaunchResult::Accepted { session_id: id })
    }
    fn events_for(&self, client_id: &str) -> Vec<(u64, SessionEvent)> {
        let sequence = self.state.acknowledged.get(client_id).copied().unwrap_or(0);
        let ended_bound = self.ended_session_observation_bound();
        self.state
            .pending
            .iter()
            .filter(|e| e.sequence > sequence)
            .filter(|e| {
                !(matches!(
                    e.event,
                    WireEvent::ObservedStarting | WireEvent::ObservedRunning
                ) && e.sequence < ended_bound)
            })
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
    fn history_entries(&self) -> Vec<HistoryEntry> {
        self.state.history.iter().cloned().collect()
    }
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("string serialization cannot fail")
}

fn launch_refusal_line(reason: &str, item_id: &str) -> String {
    format!(
        "pf-session-authorityd: launch_refused reason={reason} item_id={}",
        json_string(item_id)
    )
}

fn launch_busy_line(item_id: &str, phase: &Phase) -> String {
    format!(
        "{} phase={}",
        launch_refusal_line("busy", item_id),
        phase.name()
    )
}

fn lifecycle_failure_line(reason: &str, item_id: &str, detail: &str) -> String {
    format!(
        "pf-session-authorityd: lifecycle_failure reason={reason} item_id={} detail={}",
        json_string(item_id),
        json_string(detail)
    )
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
