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
//! the restoration ladder and its deadlines advance while no shell is connected. The loop also
//! wakes at armed deadlines themselves ([`Authority::time_until_next_deadline`]), so a graceful
//! stop escalates at its deadline instead of up to a tick later. The
//! presentation-acknowledgement rung has a deadline ([`DEFAULT_PRESENTATION_TIMEOUT`]); on expiry
//! the authority records `RecoveryRequired` with reason `presentation_not_acknowledged` and keeps
//! the owed receipt, so a late client acknowledgement or the configured Gamescope home becoming
//! presented still completes the ladder truthfully. Presentation timeout is the only
//! self-healing `RecoveryRequired` reason; every other reason stays terminal.
//!
//! Closes are the authority's own escalation ladder, and contract v2 of the RPC publishes its
//! progress for the system menu (every-menu spec §9.6): `StoppingGracefully` when a graceful stop
//! is accepted, `ForceStopping` (with its [`ForceStopReason`]) when the authority escalates by
//! itself, and `StopFailed` when even the forced kill fails. `StopFailed` is durable and
//! retryable — a further `SafeReturn` runs the whole close sequence again — because the menu row
//! offers exactly that retry. Contract v1 clients never see these events; see
//! `docs/SESSION-AUTHORITY-CONTRACT.md`. Right before every close-path escalation the optional
//! [`EscalationHook`] fires synchronously so the root hang sampler can take its sample; with no
//! hook attached the escalation path adds no observable delay.

//! # URL handoff (tsp-ght0z)
//!
//! Apps never reach the authority. The input epic's SDK front forwards `OpenUrl` and
//! `ReturnToCaller` over a private socket ([`spawn_front_acceptor`]), authenticated at accept by
//! the front's uid and systemd unit, and every request carries the caller's process handle over
//! `SCM_RIGHTS`. The authority re-derives the app id from that handle with the shared
//! `pf-peer-identity` crate and compares it with the front's claim; nothing in the payload is
//! trusted for identity. The trusted shell may also `OpenUrl` on its own socket.
//!
//! A successful `OpenUrl` opens the depth-1 [`ReturnSlot`]: the caller keeps running behind the
//! handler, and `ReturnToCaller` restores it. The slot never stacks. A caller reaped by the
//! memory policy answers `caller_gone`, a normal outcome. The URL is data in the
//! [`UrlHandoff`]; no command template ever expands it. The foreground switch itself belongs to
//! the trusted session path (G2.2–G2.4); [`CommandSystem`] refuses it until that lands.

use pf_app_manifest::{parse_capability_requirement, validate_app_id, ReasonCode, Resolver};
use pf_ports::{
    Clock, LaunchRequest, LaunchResult, MonotonicTime, ObservedSessionState, RecoveryRequired,
    SessionEvent, TerminalReceipt,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::io;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

pub mod orientation;
pub mod orientation_source;

/// Default deadline for the restored shell to acknowledge its first presentation.
pub const DEFAULT_PRESENTATION_TIMEOUT: Duration = Duration::from_secs(10);
const GAMESCOPE_SESSION_TARGET: &str = "pocketforge-gamescope-session.target";
const GAMESCOPE_RESIDENT_UNIT: &str = "pocketforge-gamescope-resident.service";

/// Version of the private SDK-front wire ([`FrontEnvelope`]).
pub const FRONT_WIRE_VERSION: u32 = 1;
/// Manifest capability an app must declare to open URLs (owner decision B).
pub const OPEN_URL_CAPABILITY: &str = "open_url";
/// `OpenUrl` flag reserved for OAuth auth sessions (tsp-o9fo8); refused in v1.
pub const OPEN_URL_FLAG_AUTH_SESSION: u32 = 1 << 0;
/// `OpenUrl` flag for captive-portal sign-in (tsp-7s0ti); accepted only from the shell socket.
pub const OPEN_URL_FLAG_CAPTIVE_PORTAL: u32 = 1 << 1;
/// Longest URL the authority accepts, in bytes.
pub const MAX_URL_BYTES: usize = 8 * 1024;
/// `OpenUrl` requests allowed per caller within [`URL_RATE_WINDOW`].
pub const URL_RATE_LIMIT: usize = 3;
pub const URL_RATE_WINDOW: Duration = Duration::from_secs(10);
/// Rate-limit key for the shell as a URL caller.
pub const SHELL_CALLER_KEY: &str = "shell";
const FOREGROUND_HANDOFF_UNAVAILABLE: &str =
    "foreground handoff needs the trusted session path (G2.2 tsp-op5a.440.3, G2.3 tsp-op5a.440.4, G2.4 tsp-op5a.440.5)";

/// Default cadence of the daemon's self-driven reconcile/deadline tick.
pub const DEFAULT_TICK_INTERVAL: Duration = Duration::from_secs(1);

/// Default graceful-stop window before the authority escalates by itself.
///
/// Budget: grace (1500 ms) plus the root hang sampler's own bound (≤300 ms, `cr-collect`)
/// must stay inside the app unit's `TimeoutStopSec=2 s` SIGKILL backstop, so escalation is
/// the authority's decision and the backstop stays the last resort.
pub const DEFAULT_GRACE_PERIOD: Duration = Duration::from_millis(1500);

/// Baseline session RPC contract: launch/safe-return/events/acknowledge/history/observe/tick
/// with the original event vocabulary. Requests without a `contract` field mean v1.
pub const SESSION_RPC_CONTRACT_V1: u32 = 1;

/// Adds the close-progress events the system menu renders (every-menu spec §9.6):
/// `stopping_gracefully`, `force_stopping` and `stop_failed`. Purely additive: the v1 stream
/// and its persisted sequences are unchanged.
pub const SESSION_RPC_CONTRACT_V2: u32 = 2;

/// Newest contract this authority serves. A request for a newer one is refused explicitly.
pub const SESSION_RPC_CONTRACT_CURRENT: u32 = SESSION_RPC_CONTRACT_V2;

const fn default_rpc_contract() -> u32 {
    SESSION_RPC_CONTRACT_V1
}

const fn is_v1_rpc_contract(contract: &u32) -> bool {
    *contract == SESSION_RPC_CONTRACT_V1
}

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
    /// How ownership was restored after this app ended. Omitted for the legacy fb0 path so
    /// pre-Gamescope receipts remain byte-for-byte stable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_app_owner: Option<PostAppOwnerEvidence>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerTier {
    Gamescope,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PostAppOwner {
    SessionIdle,
    HomeClient,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PostAppOwnerEvidence {
    pub owner_tier: OwnerTier,
    pub post_app_owner: PostAppOwner,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home_unit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ack_signal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ack_latency_ms: Option<u64>,
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
    /// A close path could not kill the app (`systemctl kill` itself failed). Durable and
    /// retryable: a further `SafeReturn` runs the full close sequence again, and the session
    /// only ends through the ordinary observation ladder. `reason` uses the same
    /// `{code}: {detail}` shape as [`Phase::RecoveryRequired`].
    StopFailed {
        session_id: String,
        #[serde(default)]
        item_id: String,
        reason: String,
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
            Self::StopFailed { .. } => "stop_failed",
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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum ResidentMonitorRung {
    Resident,
    TargetReleased,
    OwnerActive,
    PresentationAcknowledged,
    RecoveryRequired,
}

/// A completed Gamescope restoration remains observable without holding the app launch slot.
/// Its receipt belongs to the app-less fallback transition and is consumed when presentation is
/// acknowledged; app history has already received its own terminal receipt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct ResidentMonitor {
    item_id: String,
    receipt: Receipt,
    rung: ResidentMonitorRung,
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
    // Contract v2 close-progress events (every-menu spec §9.6). Durable like every other
    // published event; never delivered to a v1 client.
    StoppingGracefully {
        session_id: String,
    },
    ForceStopping {
        session_id: String,
        reason: ForceStopReason,
    },
    StopFailed {
        session_id: String,
        reason: String,
    },
}

/// Why the authority escalated to its forced kill. The menu shows `StopTimeout` as
/// "‹App› didn't stop in time" (§9.6).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForceStopReason {
    /// The app did not stop inside the graceful window.
    StopTimeout,
    /// The graceful-stop request itself failed, so the kill was immediate.
    StopRequestFailed,
}

impl ForceStopReason {
    /// Stable snake_case name used in diagnostics and on the wire.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StopTimeout => "stop_timeout",
            Self::StopRequestFailed => "stop_request_failed",
        }
    }
}

/// Who asked for a URL handoff and therefore owns the return.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotCaller {
    /// The trusted shell: returning means Home.
    Shell,
    /// The foreground app, which keeps running behind the handler.
    App { session_id: String, item_id: String },
}

/// The depth-1 return slot. Exactly one handoff can be outstanding; it never stacks.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ReturnSlot {
    pub caller: SlotCaller,
    pub handler_session_id: String,
    pub handler_item_id: String,
    /// The app caller ended (memory policy, crash or exit) while the handler was in front. The
    /// handler was promoted to the foreground session; `ReturnToCaller` answers `caller_gone`.
    #[serde(default)]
    pub caller_gone: bool,
}

/// A foreground-changing URL operation that was made durable before its external side effect.
/// Reconciliation may replay it, so [`SessionSystem`] implementations must be idempotent for the
/// session ids carried by the handoff or slot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum UrlTransition {
    Opening { handoff: UrlHandoff },
    Restoring { slot: ReturnSlot },
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_slot: Option<ReturnSlot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    url_transition: Option<UrlTransition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resident_monitor: Option<ResidentMonitor>,
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
            return_slot: None,
            url_transition: None,
            resident_monitor: None,
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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionOwnerConfig {
    home_client_unit: Option<String>,
    home_client_presentation: Option<PathBuf>,
}

impl SessionOwnerConfig {
    pub fn new(
        home_client_unit: String,
        home_client_presentation: PathBuf,
    ) -> Result<Self, String> {
        if !valid_service_unit(&home_client_unit) {
            return Err("home client must be a valid .service unit".to_owned());
        }
        if !home_client_presentation.is_absolute()
            || !home_client_presentation.starts_with("/run/pocketforge/session")
            || home_client_presentation
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(
                "home client presentation signal must be below /run/pocketforge/session".to_owned(),
            );
        }
        Ok(Self {
            home_client_unit: Some(home_client_unit),
            home_client_presentation: Some(home_client_presentation),
        })
    }
}

fn valid_service_unit(unit: &str) -> bool {
    unit.ends_with(".service")
        && !unit.is_empty()
        && unit
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'@'))
}

impl Default for CommandTemplates {
    fn default() -> Self {
        Self {
            start_foreground: words("systemctl start pf-app@{item_id}.service"),
            // --no-block: a blocking `systemctl stop` only returns after the unit's own
            // TimeoutStopSec SIGKILL, which made the authority's escalation unreachable and
            // burned the whole app window. The non-blocking job leaves the window to
            // DEFAULT_GRACE_PERIOD, and the unit's TimeoutStopSec stays the last-resort backstop.
            request_graceful_stop: words("systemctl stop --no-block pf-app@{item_id}.service"),
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
    session_owner: SessionOwnerConfig,
}

impl CommandSystem<ProcessExecutor> {
    pub fn new(templates: CommandTemplates) -> Self {
        Self {
            templates,
            executor: ProcessExecutor,
            session_owner: SessionOwnerConfig::default(),
        }
    }

    pub fn new_with_session_owner(
        templates: CommandTemplates,
        session_owner: SessionOwnerConfig,
    ) -> Self {
        Self {
            templates,
            executor: ProcessExecutor,
            session_owner,
        }
    }
}

impl<E> CommandSystem<E> {
    pub fn with_executor(templates: CommandTemplates, executor: E) -> Self {
        Self {
            templates,
            executor,
            session_owner: SessionOwnerConfig::default(),
        }
    }
    pub fn with_executor_and_session_owner(
        templates: CommandTemplates,
        executor: E,
        session_owner: SessionOwnerConfig,
    ) -> Self {
        Self {
            templates,
            executor,
            session_owner,
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

    fn gamescope_target_state(&mut self) -> Result<GamescopeTargetState, String> {
        let unit = GAMESCOPE_SESSION_TARGET;
        let output = self.query(&[
            "show",
            "--property",
            "ActiveState",
            "--property",
            "Job",
            "--property",
            "BoundBy",
            unit,
        ])?;
        if output.code != 0 {
            return Err(format!(
                "systemctl show ActiveState/Job {unit} exited {}",
                output.code
            ));
        }
        let mut active_state = None;
        let mut job = None;
        let mut bound_by = None;
        for line in output.stdout.lines() {
            let (property, value) = line
                .split_once('=')
                .ok_or_else(|| format!("malformed systemctl property for {unit}: {line:?}"))?;
            match property {
                "ActiveState" if active_state.is_none() => active_state = Some(value.to_owned()),
                "Job" if job.is_none() => {
                    job = Some((!value.is_empty()).then(|| value.to_owned()));
                }
                "BoundBy" if bound_by.is_none() => {
                    bound_by = Some(value.split_whitespace().map(str::to_owned).collect());
                }
                "ActiveState" | "Job" | "BoundBy" => {
                    return Err(format!(
                        "duplicate systemctl property {property} for {unit}"
                    ));
                }
                _ => {
                    return Err(format!(
                        "unexpected systemctl property {property} for {unit}"
                    ))
                }
            }
        }
        let active_state =
            active_state.ok_or_else(|| format!("systemctl omitted ActiveState for {unit}"))?;
        let job = job.ok_or_else(|| format!("systemctl omitted Job for {unit}"))?;
        let bound_by = bound_by.ok_or_else(|| format!("systemctl omitted BoundBy for {unit}"))?;
        Ok(GamescopeTargetState {
            active_state,
            job,
            bound_by,
        })
    }

    fn resident_unit_state(&mut self) -> Result<Option<String>, String> {
        let unit = GAMESCOPE_RESIDENT_UNIT;
        let output = self.query(&[
            "show",
            "--property",
            "LoadState",
            "--property",
            "ActiveState",
            unit,
        ])?;
        if output.code != 0 {
            return Err(format!(
                "systemctl show LoadState/ActiveState {unit} exited {}",
                output.code
            ));
        }
        let mut load_state = None;
        let mut active_state = None;
        for line in output.stdout.lines() {
            let (property, value) = line
                .split_once('=')
                .ok_or_else(|| format!("malformed systemctl property for {unit}: {line:?}"))?;
            match property {
                "LoadState" if load_state.is_none() => load_state = Some(value.to_owned()),
                "ActiveState" if active_state.is_none() => active_state = Some(value.to_owned()),
                "LoadState" | "ActiveState" => {
                    return Err(format!(
                        "duplicate systemctl property {property} for {unit}"
                    ));
                }
                _ => {
                    return Err(format!(
                        "unexpected systemctl property {property} for {unit}"
                    ));
                }
            }
        }
        let load_state =
            load_state.ok_or_else(|| format!("systemctl omitted LoadState for {unit}"))?;
        let active_state =
            active_state.ok_or_else(|| format!("systemctl omitted ActiveState for {unit}"))?;
        match load_state.as_str() {
            "not-found" => Ok(None),
            "loaded" => Ok(Some(active_state)),
            state => Err(format!("resident unit {unit} has LoadState {state:?}")),
        }
    }

    fn is_active(&mut self, unit: &str) -> Result<bool, String> {
        match self.query(&["is-active", "--quiet", unit])?.code {
            0 => Ok(true),
            3 => Ok(false),
            code => Err(format!("systemctl is-active {unit} exited {code}")),
        }
    }
}

/// One URL handoff for the session system: the URL is data here, never an argv token.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UrlHandoff {
    pub caller: SlotCaller,
    pub handler_item_id: String,
    pub handler_session_id: String,
    pub url: String,
    pub flags: u32,
}

/// How the handler took the URL.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UrlHandoffOutcome {
    /// The handler was started as a new session with the URL.
    Launched,
    /// A running handler took the URL (a new tab) and was brought to the front.
    Delivered,
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
    fn gamescope_session_lifecycle(&mut self) -> Result<Option<GamescopeSessionLifecycle>, String> {
        Ok(None)
    }
    fn activate_home_client(&mut self, _unit: &str) -> Result<(), String> {
        Err("Gamescope home client is not configured".to_owned())
    }
    /// Start the handler (or deliver to a running one) with the URL as data and switch the
    /// foreground to it, leaving an app caller running behind it. Calls with the same handler
    /// session id must be idempotent because a durable intent is replayed after an ambiguous
    /// crash boundary. The default refuses: the switch belongs to the trusted session path
    /// (G2.2–G2.4).
    fn open_url(&mut self, _handoff: &UrlHandoff) -> Result<UrlHandoffOutcome, String> {
        Err(FOREGROUND_HANDOFF_UNAVAILABLE.to_owned())
    }
    /// Bring the slot's app caller back to the front; the handler keeps running behind it. Calls
    /// for the same slot must be idempotent because a durable intent is replayed after an
    /// ambiguous crash boundary.
    fn restore_caller(&mut self, _slot: &ReturnSlot) -> Result<(), String> {
        Err(FOREGROUND_HANDOFF_UNAVAILABLE.to_owned())
    }
}

/// Resolves the default handler (a pf-app item id) for a URL scheme. The user's choice lives in
/// pf-prefsd (owner decision D; read API in tsp-mv7zn).
pub trait UrlHandlerResolver {
    fn handler_for(&mut self, scheme: &str) -> Result<Option<String>, String>;
}

/// No handler installed: every `OpenUrl` answers `no_handler` (drives the QR fallback).
#[derive(Clone, Copy, Debug, Default)]
pub struct NoUrlHandler;
impl UrlHandlerResolver for NoUrlHandler {
    fn handler_for(&mut self, _scheme: &str) -> Result<Option<String>, String> {
        Ok(None)
    }
}

/// One fixed handler item for http and https (the daemon's `--url-handler`).
#[derive(Clone, Debug)]
pub struct FixedUrlHandler(pub String);
impl UrlHandlerResolver for FixedUrlHandler {
    fn handler_for(&mut self, _scheme: &str) -> Result<Option<String>, String> {
        Ok(Some(self.0.clone()))
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

fn selected_owner_release_ready(
    foreground_target_active: bool,
    gamescope: &Option<GamescopeSessionLifecycle>,
) -> bool {
    gamescope.is_none() && !foreground_target_active
}

#[derive(Debug, Eq, PartialEq)]
struct GamescopeTargetState {
    active_state: String,
    job: Option<String>,
    bound_by: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GamescopeSessionLifecycle {
    /// The target has a unit job or is deactivating, so ownership is not stable yet.
    Transitioning,
    /// The target has no independent resident pin and must be allowed to become inactive.
    Transient {
        diagnostic: Option<String>,
    },
    SessionIdle,
    HomeClient {
        unit: String,
        active: bool,
        presenting: bool,
        ack_signal: String,
    },
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

    fn gamescope_session_lifecycle(&mut self) -> Result<Option<GamescopeSessionLifecycle>, String> {
        let target = GAMESCOPE_SESSION_TARGET;
        let target_state = self.gamescope_target_state()?;
        if target_state.job.is_some() || target_state.active_state == "deactivating" {
            return Ok(Some(GamescopeSessionLifecycle::Transitioning));
        }
        match target_state.active_state.as_str() {
            "active" => {}
            "inactive" | "failed" => return Ok(None),
            state => {
                return Err(format!(
                    "Gamescope session target {target} has ActiveState {state:?} without a unit job"
                ));
            }
        }
        let resident_state = self.resident_unit_state()?;
        let resident_active = matches!(resident_state.as_deref(), Some("active" | "activating"));
        let resident_bound = target_state
            .bound_by
            .iter()
            .any(|unit| unit == GAMESCOPE_RESIDENT_UNIT);
        if !resident_active || !resident_bound {
            let diagnostic = (resident_active && !resident_bound).then(|| {
                format!(
                    "gamescope_residency_misconfigured reason=active_without_bound_by unit={GAMESCOPE_RESIDENT_UNIT} target={GAMESCOPE_SESSION_TARGET}"
                )
            });
            return Ok(Some(GamescopeSessionLifecycle::Transient { diagnostic }));
        }
        let lifecycle = match (
            self.session_owner.home_client_unit.clone(),
            self.session_owner.home_client_presentation.clone(),
        ) {
            (Some(unit), Some(signal)) => match self.property(&unit, "LoadState")?.as_str() {
                "not-found" => GamescopeSessionLifecycle::SessionIdle,
                "loaded" => {
                    let active = self.is_active(&unit)?;
                    let presenting = if active {
                        match fs::read_to_string(&signal) {
                            Ok(value) => value == format!("{unit}\n"),
                            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
                            Err(error) => {
                                return Err(format!(
                                    "read Gamescope presentation signal {}: {error}",
                                    signal.display()
                                ));
                            }
                        }
                    } else {
                        false
                    };
                    GamescopeSessionLifecycle::HomeClient {
                        unit,
                        active,
                        presenting,
                        ack_signal: signal.display().to_string(),
                    }
                }
                state => return Err(format!("home client {unit} has LoadState {state:?}")),
            },
            _ => GamescopeSessionLifecycle::SessionIdle,
        };

        // `systemctl show` is D-Bus-backed but each unit is observed separately. Re-read both
        // ends of the residency edge after collecting any home-client evidence; a target or
        // resident transition between the first and second samples must not yield a terminal
        // Gamescope owner observation assembled from two systemd epochs.
        let resident_after = self.resident_unit_state()?;
        let target_after = self.gamescope_target_state()?;
        if resident_after != resident_state || target_after != target_state {
            return Ok(Some(GamescopeSessionLifecycle::Transitioning));
        }
        Ok(Some(lifecycle))
    }

    fn activate_home_client(&mut self, unit: &str) -> Result<(), String> {
        if self.session_owner.home_client_unit.as_deref() != Some(unit) {
            return Err("requested Gamescope home client is not configured".to_owned());
        }
        let args = ["start".to_owned(), unit.to_owned()];
        command_ok(self.executor.execute("systemctl", &args)?)
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
    GamescopeSessionIdle,
    GamescopeHomeAvailable { unit: String, ack_signal: String },
    GamescopeHomeActive { unit: String, ack_signal: String },
    GamescopeHomePresented { unit: String, ack_signal: String },
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
    Launch {
        item_id: String,
    },
    SafeReturn,
    Events {
        client_id: String,
        /// Highest contract the client speaks. Absent means v1 (byte-identical requests for
        /// old clients and old servers); a value above [`SESSION_RPC_CONTRACT_CURRENT`] is
        /// refused with [`RpcResponse::Error`].
        #[serde(
            default = "default_rpc_contract",
            skip_serializing_if = "is_v1_rpc_contract"
        )]
        contract: u32,
    },
    Acknowledge {
        client_id: String,
        sequence: u64,
    },
    History,
    Observe {
        observation: RpcObservation,
    },
    Tick,
    /// Read or long-poll the complete compositor orientation snapshot.
    Orientation {
        version: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after_serial: Option<u64>,
        #[serde(default)]
        wait_ms: u64,
    },
    /// Open `url` in the default handler. On the front socket `app_id` is the front's claim,
    /// re-verified against the forwarded process handle; on the shell socket it must be absent.
    OpenUrl {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        app_id: Option<String>,
        url: String,
        #[serde(default)]
        flags: u32,
    },
    /// Front socket only: the handler returns to the slot's caller. `url` is reserved for auth
    /// sessions (tsp-o9fo8) and refused in v1.
    ReturnToCaller {
        app_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
    },
}

/// The private front socket's request: a wire version plus one [`RpcRequest`], of which only
/// `open_url` and `return_to_caller` are accepted there.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FrontEnvelope {
    pub version: u32,
    #[serde(flatten)]
    pub request: RpcRequest,
}

/// Where a decoded request came from. Identity is never read from the payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RpcOrigin {
    /// The shell's own socket (hidden from apps by the image).
    Shell,
    /// The root input broker on the shell socket. Its role is restricted to `SafeReturn`.
    Broker,
    /// The compositor has read-only access to the orientation method.
    Compositor,
    /// The SDK front's private socket, with the app id re-derived from the forwarded process
    /// handle, or the reason no app id could be derived.
    Front { caller: Result<String, String> },
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
    Accepted {
        session_id: String,
    },
    RejectedBusy,
    ItemUnavailable,
    Events {
        events: Vec<(u64, RpcEvent)>,
        /// The contract the events were selected for. Omitted on the wire for v1 so the
        /// response stays byte-identical for pre-negotiation clients.
        #[serde(
            default = "default_rpc_contract",
            skip_serializing_if = "is_v1_rpc_contract"
        )]
        contract: u32,
    },
    History {
        entries: Vec<HistoryEntry>,
    },
    Orientation {
        version: u32,
        state: orientation::OrientationState,
    },
    Ok,
    Error {
        message: String,
    },
    /// `OpenUrl`: the handler was started as this session.
    Launched {
        session_id: String,
    },
    /// `OpenUrl`: a running handler took the URL as a new tab.
    Delivered,
    /// `OpenUrl`: no handler is installed (apps fall back, e.g. to a QR code).
    NoHandler,
    /// `OpenUrl`/`ReturnToCaller`: the URL is not an allowed http(s) URL.
    InvalidUrl,
    /// Refused: identity, capability, flags or not the right caller. The reason is logged.
    Denied,
    /// The slot is occupied, or the foreground is not in a state that can hand off.
    Busy,
    /// More than [`URL_RATE_LIMIT`] requests within [`URL_RATE_WINDOW`].
    RateLimited,
    /// `ReturnToCaller`: the caller is in front again.
    Restored,
    /// `ReturnToCaller`: the caller is no longer running (a normal outcome).
    CallerGone,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum RpcEvent {
    Starting,
    Running,
    ObservationComplete,
    Returned {
        session_id: String,
    },
    ForcedClose {
        session_id: String,
    },
    Crash {
        session_id: String,
        summary: String,
    },
    RecoveryRequired {
        session_id: String,
        reason: String,
    },
    /// Contract v2 close-progress events (every-menu spec §9.6). `ForcedClose` is the
    /// "Returned with escalated flag" of the spec: the menu closes and toasts
    /// "‹App› was force-closed".
    StoppingGracefully {
        session_id: String,
    },
    ForceStopping {
        session_id: String,
        reason: ForceStopReason,
    },
    StopFailed {
        session_id: String,
        reason: String,
    },
}

/// True for events that exist only in contract v2 and must never reach a v1 client.
const fn is_close_progress(event: &WireEvent) -> bool {
    matches!(
        event,
        WireEvent::StoppingGracefully { .. }
            | WireEvent::ForceStopping { .. }
            | WireEvent::StopFailed { .. }
    )
}

fn wire_to_rpc(event: &WireEvent) -> RpcEvent {
    match event {
        WireEvent::ObservedStarting => RpcEvent::Starting,
        WireEvent::ObservedRunning => RpcEvent::Running,
        WireEvent::ObservationComplete => RpcEvent::ObservationComplete,
        WireEvent::Terminal {
            session_id,
            receipt,
        } => match receipt {
            Receipt::Returned => RpcEvent::Returned {
                session_id: session_id.clone(),
            },
            Receipt::ForcedClose => RpcEvent::ForcedClose {
                session_id: session_id.clone(),
            },
            Receipt::Crash { summary } => RpcEvent::Crash {
                session_id: session_id.clone(),
                summary: summary.clone(),
            },
        },
        WireEvent::RecoveryRequired { session_id, reason } => RpcEvent::RecoveryRequired {
            session_id: session_id.clone(),
            reason: reason.clone(),
        },
        WireEvent::StoppingGracefully { session_id } => RpcEvent::StoppingGracefully {
            session_id: session_id.clone(),
        },
        WireEvent::ForceStopping { session_id, reason } => RpcEvent::ForceStopping {
            session_id: session_id.clone(),
            reason: *reason,
        },
        WireEvent::StopFailed { session_id, reason } => RpcEvent::StopFailed {
            session_id: session_id.clone(),
            reason: reason.clone(),
        },
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
    let response = dispatch_rpc(authority, request);
    let body = serde_json::to_vec(&response).map_err(|e| AuthorityError::Backend(e.to_string()))?;
    pf_wire::write_frame(writer, &body).map_err(|e| AuthorityError::Backend(e.to_string()))
}

/// Runs one complete, decoded shell-socket request against the authority. Never touches a socket.
pub fn dispatch_rpc<S: StateStore, B: SessionSystem, C: Clock>(
    authority: &mut Authority<S, B, C>,
    request: RpcRequest,
) -> RpcResponse {
    dispatch_rpc_from(authority, RpcOrigin::Shell, request)
}

/// Runs one decoded request of known origin against the authority.
pub fn dispatch_rpc_from<S: StateStore, B: SessionSystem, C: Clock>(
    authority: &mut Authority<S, B, C>,
    origin: RpcOrigin,
    request: RpcRequest,
) -> RpcResponse {
    handle_rpc_from(authority, origin, request).unwrap_or_else(|error| RpcResponse::Error {
        message: format!("{error:?}"),
    })
}

/// A complete request read and decoded by a connection thread, with its response channel.
pub struct PendingRpc {
    pub request: RpcRequest,
    reply: mpsc::Sender<RpcResponse>,
    origin: RpcOrigin,
}

impl PendingRpc {
    /// Hands the response back to the connection thread; a vanished client is not an error.
    pub fn respond(self, response: RpcResponse) {
        let _ = self.reply.send(response);
    }
    pub fn origin(&self) -> &RpcOrigin {
        &self.origin
    }
    /// Dispatches with the connection's verified origin and answers it.
    pub fn dispatch<S: StateStore, B: SessionSystem, C: Clock>(
        self,
        authority: &mut Authority<S, B, C>,
    ) {
        let response = dispatch_rpc_from(authority, self.origin.clone(), self.request.clone());
        self.respond(response);
    }
}

/// How the private front socket authenticates its peer and derives the caller's app id. Both
/// are injectable so hermetic tests can stand in for the SDK front and app units; production uses
/// [`FrontPolicy::kernel`], the shared `pf-peer-identity` implementation.
#[derive(Clone)]
pub struct FrontPolicy {
    authenticate_peer: Arc<PeerAuthenticator>,
    derive_caller: Arc<CallerDeriver>,
}

/// Accepts or refuses an accepted front connection.
pub type PeerAuthenticator = dyn Fn(&UnixStream) -> io::Result<()> + Send + Sync;
/// Derives the caller's app id from the forwarded process handle, or the refusal reason.
pub type CallerDeriver = dyn Fn(OwnedFd) -> Result<String, String> + Send + Sync;

/// How the shell socket authenticates and classifies its two legitimate peers. The resolver is
/// injectable for hermetic tests; production uses the shared `pf-peer-identity` implementation.
#[derive(Clone)]
pub struct ShellPolicy {
    shell_uid: u32,
    shell_unit: String,
    broker_unit: String,
    compositor_unit: Option<String>,
    resolve_peer: Arc<ShellPeerResolver>,
}

type ShellPeerResolver =
    dyn Fn(&UnixStream) -> io::Result<pf_peer_identity::PeerServiceIdentity> + Send + Sync;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShellPeerRole {
    Shell,
    Broker,
    Compositor,
}

impl ShellPolicy {
    pub fn kernel(shell_uid: u32, shell_unit: String, broker_unit: String) -> io::Result<Self> {
        Self::custom(
            shell_uid,
            shell_unit,
            broker_unit,
            pf_peer_identity::peer_service_identity,
        )
    }

    pub fn kernel_with_compositor(
        shell_uid: u32,
        shell_unit: String,
        broker_unit: String,
        compositor_unit: String,
    ) -> io::Result<Self> {
        Self::custom_with_compositor(
            shell_uid,
            shell_unit,
            broker_unit,
            Some(compositor_unit),
            pf_peer_identity::peer_service_identity,
        )
    }

    pub fn custom(
        shell_uid: u32,
        shell_unit: String,
        broker_unit: String,
        resolve_peer: impl Fn(&UnixStream) -> io::Result<pf_peer_identity::PeerServiceIdentity>
            + Send
            + Sync
            + 'static,
    ) -> io::Result<Self> {
        Self::custom_with_compositor(shell_uid, shell_unit, broker_unit, None, resolve_peer)
    }

    pub fn custom_with_compositor(
        shell_uid: u32,
        shell_unit: String,
        broker_unit: String,
        compositor_unit: Option<String>,
        resolve_peer: impl Fn(&UnixStream) -> io::Result<pf_peer_identity::PeerServiceIdentity>
            + Send
            + Sync
            + 'static,
    ) -> io::Result<Self> {
        if shell_uid == 0 && shell_unit == broker_unit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("shell uid=0 unit={shell_unit} overlaps the root broker identity"),
            ));
        }
        if compositor_unit.as_deref() == Some(shell_unit.as_str()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("compositor unit={shell_unit} overlaps the full-authority shell identity"),
            ));
        }
        if shell_uid == 0 && compositor_unit.as_deref() == Some(broker_unit.as_str()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("compositor unit={broker_unit} overlaps the root broker identity"),
            ));
        }
        Ok(Self {
            shell_uid,
            shell_unit,
            broker_unit,
            compositor_unit,
            resolve_peer: Arc::new(resolve_peer),
        })
    }

    fn authenticate_peer(&self, stream: &UnixStream) -> io::Result<ShellPeerRole> {
        let identity = (self.resolve_peer)(stream)?;
        if identity.cred.uid == self.shell_uid && identity.unit == self.shell_unit {
            return Ok(ShellPeerRole::Shell);
        }
        if identity.cred.uid == 0 && identity.unit == self.broker_unit {
            return Ok(ShellPeerRole::Broker);
        }
        if identity.cred.uid == self.shell_uid
            && self.compositor_unit.as_deref() == Some(identity.unit.as_str())
        {
            return Ok(ShellPeerRole::Compositor);
        }
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "refused peer pid={} uid={} unit={} (expected uid={} unit={} or uid=0 unit={})",
                identity.cred.pid,
                identity.cred.uid,
                identity.unit,
                self.shell_uid,
                self.shell_unit,
                self.broker_unit
            ),
        ))
    }
}

impl FrontPolicy {
    /// The SDK front is exactly `front_unit` running as `front_uid`; callers are the
    /// `pf-app@<id>.service` unit of the forwarded process handle.
    pub fn kernel(front_uid: u32, front_unit: String) -> Self {
        Self {
            authenticate_peer: Arc::new(move |stream| {
                pf_peer_identity::verify_peer_service(stream, front_uid, &front_unit)
            }),
            derive_caller: Arc::new(|fd| {
                pf_peer_identity::ReceivedProcess::from_fd(fd)
                    .and_then(|process| process.app_id())
                    .map_err(|error| error.to_string())
            }),
        }
    }

    pub fn custom(
        authenticate_peer: impl Fn(&UnixStream) -> io::Result<()> + Send + Sync + 'static,
        derive_caller: impl Fn(OwnedFd) -> Result<String, String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            authenticate_peer: Arc::new(authenticate_peer),
            derive_caller: Arc::new(derive_caller),
        }
    }
}

/// Sends one front request: the framed envelope and the caller's process handle in a single
/// `sendmsg`, as the SDK front does.
pub fn send_front_request(
    stream: &UnixStream,
    envelope: &FrontEnvelope,
    process_handle: Option<std::os::fd::BorrowedFd<'_>>,
) -> io::Result<()> {
    let body = serde_json::to_vec(envelope).map_err(io::Error::other)?;
    if body.len() > pf_wire::MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame too large",
        ));
    }
    let mut frame = (body.len() as u32).to_be_bytes().to_vec();
    frame.extend_from_slice(&body);
    pf_peer_identity::scm::send_with_fd(stream.as_raw_fd(), &frame, process_handle)
}

/// Reads one `pf-wire` frame whose first bytes may carry one `SCM_RIGHTS` descriptor.
fn read_frame_with_fd(stream: &mut UnixStream) -> io::Result<(Vec<u8>, Option<OwnedFd>)> {
    let mut buffer = vec![0u8; 4 + pf_wire::MAX_FRAME];
    let (mut filled, fd) = pf_peer_identity::scm::recv_with_fd(stream.as_raw_fd(), &mut buffer)?;
    if filled == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "peer closed before a request",
        ));
    }
    while filled < 4 {
        let read = stream.read(&mut buffer[filled..4])?;
        if read == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        filled += read;
    }
    let len = u32::from_be_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]) as usize;
    if len > pf_wire::MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame too large: {len}"),
        ));
    }
    let end = 4 + len;
    if filled < end {
        stream.read_exact(&mut buffer[filled..end])?;
    }
    buffer.truncate(end);
    buffer.drain(..4);
    Ok((buffer, fd))
}

/// Accepts the SDK front on its private socket. Each connection is authenticated by
/// `policy` before a byte is read; a non-front peer is closed without a frame and logged. One
/// request per connection, carrying the caller's process handle, decoded under `io_timeout` and
/// forwarded to the authority loop with its verified origin.
pub fn spawn_front_acceptor(
    listener: UnixListener,
    requests: mpsc::Sender<io::Result<PendingRpc>>,
    limits: ConnectionLimits,
    policy: FrontPolicy,
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
                    "pf-session-authorityd: front_refused reason=too_many_connections limit={}",
                    limits.max_connections
                );
                continue;
            }
            let (requests, active, policy) = (requests.clone(), active.clone(), policy.clone());
            thread::spawn(move || {
                if let Err(error) = serve_front_connection(stream, &requests, limits, &policy) {
                    eprintln!("pf-session-authorityd: front connection error: {error:?}");
                }
                active.fetch_sub(1, Ordering::AcqRel);
            });
        }
    })
}

fn serve_front_connection(
    mut stream: UnixStream,
    requests: &mpsc::Sender<io::Result<PendingRpc>>,
    limits: ConnectionLimits,
    policy: &FrontPolicy,
) -> Result<(), AuthorityError> {
    let backend = |error: &dyn std::fmt::Display| AuthorityError::Backend(error.to_string());
    if let Err(error) = (policy.authenticate_peer)(&stream) {
        eprintln!(
            "pf-session-authorityd: front_refused reason=peer_identity detail={}",
            json_string(&error.to_string())
        );
        return Ok(());
    }
    stream
        .set_read_timeout(Some(limits.io_timeout))
        .and_then(|()| stream.set_write_timeout(Some(limits.io_timeout)))
        .map_err(|e| backend(&e))?;
    let (body, handle) = read_frame_with_fd(&mut stream).map_err(|e| backend(&e))?;
    let response = match serde_json::from_slice::<FrontEnvelope>(&body) {
        Ok(envelope) if envelope.version != FRONT_WIRE_VERSION => RpcResponse::Error {
            message: format!(
                "unsupported front wire version {} (expected {FRONT_WIRE_VERSION})",
                envelope.version
            ),
        },
        Ok(FrontEnvelope {
            request: request @ (RpcRequest::OpenUrl { .. } | RpcRequest::ReturnToCaller { .. }),
            ..
        }) => {
            let caller = match handle {
                Some(fd) => (policy.derive_caller)(fd),
                None => Err("missing_process_handle".to_owned()),
            };
            let (reply, response) = mpsc::channel();
            let pending = PendingRpc {
                request,
                reply,
                origin: RpcOrigin::Front { caller },
            };
            if requests.send(Ok(pending)).is_err() {
                return Err(AuthorityError::Backend("authority loop stopped".into()));
            }
            response
                .recv_timeout(limits.response_timeout)
                .map_err(|e| backend(&e))?
        }
        Ok(_) => RpcResponse::Error {
            message: "verb not accepted on the front socket".into(),
        },
        Err(error) => return Err(backend(&error)),
    };
    let body = serde_json::to_vec(&response).map_err(|e| backend(&e))?;
    pf_wire::write_frame(&mut stream, &body).map_err(|e| backend(&e))
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
    policy: ShellPolicy,
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
            let (requests, active, policy) = (requests.clone(), active.clone(), policy.clone());
            thread::spawn(move || {
                if let Err(error) = serve_rpc_connection(stream, &requests, limits, &policy) {
                    eprintln!("pf-session-authorityd: connection error: {error:?}");
                }
                active.fetch_sub(1, Ordering::AcqRel);
            });
        }
    })
}

/// Orientation-aware acceptor. Orientation waits use [`orientation::OrientationWatch`]'s
/// independent permit bound and never enter the lifecycle request channel; lifecycle RPCs retain
/// the existing `max_connections` bound even when all orientation permits are occupied.
pub fn spawn_rpc_acceptor_with_orientation(
    listener: UnixListener,
    requests: mpsc::Sender<io::Result<PendingRpc>>,
    limits: ConnectionLimits,
    policy: ShellPolicy,
    watch: Arc<orientation::OrientationWatch>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let lifecycle_active = Arc::new(AtomicUsize::new(0));
        let connections_active = Arc::new(AtomicUsize::new(0));
        let total_bound = limits.max_connections.saturating_add(watch.max_waiters());
        for connection in listener.incoming() {
            let stream = match connection {
                Ok(stream) => stream,
                Err(error) => {
                    let _ = requests.send(Err(error));
                    return;
                }
            };
            if connections_active.fetch_add(1, Ordering::AcqRel) >= total_bound {
                connections_active.fetch_sub(1, Ordering::AcqRel);
                continue;
            }
            let (requests, lifecycle_active, connections_active, policy, watch) = (
                requests.clone(),
                Arc::clone(&lifecycle_active),
                Arc::clone(&connections_active),
                policy.clone(),
                Arc::clone(&watch),
            );
            thread::spawn(move || {
                let _connection_permit = CounterPermit(&connections_active);
                if let Err(error) = serve_orientation_aware_connection(
                    stream,
                    &requests,
                    limits,
                    &policy,
                    &watch,
                    &lifecycle_active,
                ) {
                    eprintln!("pf-session-authorityd: connection error: {error:?}");
                }
            });
        }
    })
}

fn serve_orientation_aware_connection(
    mut stream: UnixStream,
    requests: &mpsc::Sender<io::Result<PendingRpc>>,
    limits: ConnectionLimits,
    policy: &ShellPolicy,
    watch: &orientation::OrientationWatch,
    lifecycle_active: &AtomicUsize,
) -> Result<(), AuthorityError> {
    let backend = |error: &dyn std::fmt::Display| AuthorityError::Backend(error.to_string());
    let origin = match policy.authenticate_peer(&stream) {
        Ok(ShellPeerRole::Shell) => RpcOrigin::Shell,
        Ok(ShellPeerRole::Broker) => RpcOrigin::Broker,
        Ok(ShellPeerRole::Compositor) => RpcOrigin::Compositor,
        Err(error) => {
            eprintln!(
                "pf-session-authorityd: shell_refused reason=peer_identity detail={}",
                json_string(&error.to_string())
            );
            return Ok(());
        }
    };
    stream
        .set_read_timeout(Some(limits.io_timeout))
        .and_then(|()| stream.set_write_timeout(Some(limits.io_timeout)))
        .map_err(|e| backend(&e))?;
    let body = pf_wire::read_frame(&mut stream).map_err(|e| backend(&e))?;
    let request: RpcRequest = serde_json::from_slice(&body).map_err(|e| backend(&e))?;
    let response = if let RpcRequest::Orientation {
        version,
        after_serial,
        wait_ms,
    } = request
    {
        if version != 1 {
            RpcResponse::Error {
                message: format!("unsupported orientation rpc version {version}"),
            }
        } else {
            let current = watch.snapshot();
            let state = if after_serial == Some(current.serial) && wait_ms > 0 {
                match watch.wait_after(current.serial, Duration::from_millis(wait_ms)) {
                    Ok(state) => state,
                    Err(orientation::WaitError::TooManyWaiters) => {
                        let response = RpcResponse::Error {
                            message: "too many orientation waiters".into(),
                        };
                        let body = serde_json::to_vec(&response).map_err(|e| backend(&e))?;
                        return pf_wire::write_frame(&mut stream, &body).map_err(|e| backend(&e));
                    }
                    Err(orientation::WaitError::Poisoned) => {
                        return Err(AuthorityError::Backend("orientation watch poisoned".into()))
                    }
                }
            } else {
                current
            };
            RpcResponse::Orientation { version: 1, state }
        }
    } else {
        if origin == RpcOrigin::Compositor {
            RpcResponse::Denied
        } else {
            if lifecycle_active.fetch_add(1, Ordering::AcqRel) >= limits.max_connections {
                lifecycle_active.fetch_sub(1, Ordering::AcqRel);
                return Ok(());
            }
            let _permit = CounterPermit(lifecycle_active);
            let (reply, response) = mpsc::channel();
            requests
                .send(Ok(PendingRpc {
                    request,
                    reply,
                    origin,
                }))
                .map_err(|_| AuthorityError::Backend("authority loop stopped".into()))?;
            response
                .recv_timeout(limits.response_timeout)
                .map_err(|e| backend(&e))?
        }
    };
    let body = serde_json::to_vec(&response).map_err(|e| backend(&e))?;
    pf_wire::write_frame(&mut stream, &body).map_err(|e| backend(&e))
}

struct CounterPermit<'a>(&'a AtomicUsize);
impl Drop for CounterPermit<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn serve_rpc_connection(
    mut stream: UnixStream,
    requests: &mpsc::Sender<io::Result<PendingRpc>>,
    limits: ConnectionLimits,
    policy: &ShellPolicy,
) -> Result<(), AuthorityError> {
    let backend = |error: &dyn std::fmt::Display| AuthorityError::Backend(error.to_string());
    let origin = match policy.authenticate_peer(&stream) {
        Ok(ShellPeerRole::Shell) => RpcOrigin::Shell,
        Ok(ShellPeerRole::Broker) => RpcOrigin::Broker,
        Ok(ShellPeerRole::Compositor) => RpcOrigin::Compositor,
        Err(error) => {
            eprintln!(
                "pf-session-authorityd: shell_refused reason=peer_identity detail={}",
                json_string(&error.to_string())
            );
            return Ok(());
        }
    };
    stream
        .set_read_timeout(Some(limits.io_timeout))
        .and_then(|()| stream.set_write_timeout(Some(limits.io_timeout)))
        .map_err(|e| backend(&e))?;
    let body = pf_wire::read_frame(&mut stream).map_err(|e| backend(&e))?;
    let response = match serde_json::from_slice::<RpcRequest>(&body) {
        Ok(request) => {
            let (reply, response) = mpsc::channel();
            let pending = PendingRpc {
                request,
                reply,
                origin,
            };
            if requests.send(Ok(pending)).is_err() {
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

/// Shell-socket request handling (the existing tests' entry point).
#[cfg(test)]
fn handle_rpc<S: StateStore, B: SessionSystem, C: Clock>(
    authority: &mut Authority<S, B, C>,
    request: RpcRequest,
) -> Result<RpcResponse, AuthorityError> {
    handle_rpc_from(authority, RpcOrigin::Shell, request)
}

fn handle_rpc_from<S: StateStore, B: SessionSystem, C: Clock>(
    authority: &mut Authority<S, B, C>,
    origin: RpcOrigin,
    request: RpcRequest,
) -> Result<RpcResponse, AuthorityError> {
    if origin == RpcOrigin::Broker
        && !matches!(
            &request,
            RpcRequest::SafeReturn | RpcRequest::Orientation { .. }
        )
    {
        authority.log(&format!(
            "pf-session-authorityd: shell_refused reason=peer_scope role=broker method={}",
            rpc_method(&request)
        ));
        return Ok(RpcResponse::Denied);
    }
    if origin == RpcOrigin::Compositor && !matches!(&request, RpcRequest::Orientation { .. }) {
        authority.log(&format!(
            "pf-session-authorityd: shell_refused reason=peer_scope role=compositor method={}",
            rpc_method(&request)
        ));
        return Ok(RpcResponse::Denied);
    }
    Ok(match request {
        RpcRequest::OpenUrl { app_id, url, flags } => {
            let caller = match (origin, app_id) {
                (RpcOrigin::Shell, None) => UrlCaller::Shell,
                (RpcOrigin::Shell, Some(claimed)) => {
                    authority.log(&url_refusal_line(
                        "open_url",
                        "claim_without_identity",
                        &claimed,
                        None,
                    ));
                    return Ok(RpcResponse::Denied);
                }
                (RpcOrigin::Front { caller }, claimed) => {
                    match authority.verify_front_caller("open_url", caller, claimed) {
                        Some(app_id) => UrlCaller::App(app_id),
                        None => return Ok(RpcResponse::Denied),
                    }
                }
                (RpcOrigin::Broker | RpcOrigin::Compositor, _) => {
                    unreachable!("restricted requests are filtered above")
                }
            };
            authority.open_url(caller, &url, flags)?
        }
        RpcRequest::ReturnToCaller { app_id, url } => match origin {
            RpcOrigin::Shell => {
                authority.log(&url_refusal_line(
                    "return_to_caller",
                    "front_only",
                    &app_id,
                    None,
                ));
                RpcResponse::Denied
            }
            RpcOrigin::Front { caller } => {
                match authority.verify_front_caller("return_to_caller", caller, Some(app_id)) {
                    Some(app_id) => authority.return_to_caller(&app_id, url.as_deref())?,
                    None => RpcResponse::Denied,
                }
            }
            RpcOrigin::Broker | RpcOrigin::Compositor => {
                unreachable!("restricted requests are filtered above")
            }
        },
        RpcRequest::Launch { item_id } => match authority.launch(LaunchRequest { item_id })? {
            LaunchResult::Accepted { session_id } => RpcResponse::Accepted { session_id },
            LaunchResult::RejectedBusy => RpcResponse::RejectedBusy,
            LaunchResult::ItemUnavailable => RpcResponse::ItemUnavailable,
        },
        RpcRequest::SafeReturn => {
            if matches!(
                authority.state.phase,
                Phase::Starting { .. } | Phase::Running { .. } | Phase::StopFailed { .. }
            ) {
                authority.intake_safe_return()?;
                authority.reconcile()?;
            }
            RpcResponse::Ok
        }
        RpcRequest::Events {
            client_id,
            contract,
        } => {
            if contract > SESSION_RPC_CONTRACT_CURRENT {
                return Ok(RpcResponse::Error {
                    message: format!("unsupported session rpc contract {contract}"),
                });
            }
            authority.reconcile()?;
            RpcResponse::Events {
                events: authority.rpc_events_for(&client_id, contract),
                contract,
            }
        }
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
        RpcRequest::Orientation { version, .. } => {
            if version != 1 {
                RpcResponse::Error {
                    message: format!("unsupported orientation rpc version {version}"),
                }
            } else {
                RpcResponse::Orientation {
                    version: 1,
                    state: authority.orientation.watch().snapshot(),
                }
            }
        }
    })
}

fn rpc_method(request: &RpcRequest) -> &'static str {
    match request {
        RpcRequest::Launch { .. } => "launch",
        RpcRequest::SafeReturn => "safe_return",
        RpcRequest::Events { .. } => "events",
        RpcRequest::Acknowledge { .. } => "acknowledge",
        RpcRequest::History => "history",
        RpcRequest::Observe { .. } => "observe",
        RpcRequest::Tick => "tick",
        RpcRequest::Orientation { .. } => "orientation",
        RpcRequest::OpenUrl { .. } => "open_url",
        RpcRequest::ReturnToCaller { .. } => "return_to_caller",
    }
}

/// Receives one complete diagnostic line (no trailing newline).
pub type LogSink = Box<dyn FnMut(&str) + Send>;

fn stderr_log_sink() -> LogSink {
    Box::new(|line: &str| eprintln!("{line}"))
}

/// Everything a sampler needs to capture the hang it is about to escalate out of.
pub struct EscalationContext<'a> {
    pub session_id: &'a str,
    pub item_id: &'a str,
    pub reason: ForceStopReason,
}

/// Fires synchronously immediately before every close-path `enforce_termination`, so the root
/// hang sampler (`cr-collect`) can seize, unwind and detach inside its own budget. The
/// authority adds no delay of its own: with no hook attached the escalation is a single
/// branch. The hook owns its time bound; the close is never delayed past it by the authority.
pub type EscalationHook = Box<dyn FnMut(&EscalationContext) + Send>;

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
    home_ack_started: Option<MonotonicTime>,
    log: LogSink,
    escalation: Option<EscalationHook>,
    now_fn: fn() -> SystemTime,
    url_handler: Box<dyn UrlHandlerResolver + Send>,
    /// Monotonic `OpenUrl` timestamps per caller, oldest first (memory only).
    url_rate: BTreeMap<String, VecDeque<MonotonicTime>>,
    orientation: Arc<orientation::OrientationController>,
    orientation_active_app: orientation::ActiveApp,
}

/// The verified caller of an `OpenUrl`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UrlCaller {
    Shell,
    App(String),
}

/// Why a URL was refused as `invalid_url`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UrlRejection {
    TooLong,
    ControlOrNonAscii,
    NoScheme,
    SchemeNotAllowed,
    NoAuthority,
    EmptyHost,
}

impl UrlRejection {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TooLong => "too_long",
            Self::ControlOrNonAscii => "control_or_non_ascii",
            Self::NoScheme => "no_scheme",
            Self::SchemeNotAllowed => "scheme_not_allowed",
            Self::NoAuthority => "no_authority",
            Self::EmptyHost => "empty_host",
        }
    }
}

/// The v1 URL policy: http or https (any case), an authority with a host, printable ASCII only,
/// at most [`MAX_URL_BYTES`]. Returns the lowercase scheme. `file:`, `data:`, `javascript:` and
/// every other scheme are refused.
pub fn validate_url(url: &str) -> Result<&'static str, UrlRejection> {
    if url.len() > MAX_URL_BYTES {
        return Err(UrlRejection::TooLong);
    }
    if url.bytes().any(|byte| !(0x21..0x7f).contains(&byte)) {
        return Err(UrlRejection::ControlOrNonAscii);
    }
    let (scheme, rest) = url.split_once(':').ok_or(UrlRejection::NoScheme)?;
    let scheme = if scheme.eq_ignore_ascii_case("http") {
        "http"
    } else if scheme.eq_ignore_ascii_case("https") {
        "https"
    } else {
        return Err(UrlRejection::SchemeNotAllowed);
    };
    let authority = rest.strip_prefix("//").ok_or(UrlRejection::NoAuthority)?;
    let authority = authority.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    if !valid_host_port(host_port) {
        return Err(UrlRejection::EmptyHost);
    }
    Ok(scheme)
}

fn valid_host_port(host_port: &str) -> bool {
    if let Some(bracketed) = host_port.strip_prefix('[') {
        let Some((host, rest)) = bracketed.split_once(']') else {
            return false;
        };
        return host.parse::<std::net::Ipv6Addr>().is_ok() && valid_port_suffix(rest);
    }
    if host_port.contains(['[', ']']) {
        return false;
    }
    let (host, port) = match host_port.split_once(':') {
        Some((host, port)) if !port.contains(':') => (host, Some(port)),
        Some(_) => return false,
        None => (host_port, None),
    };
    !host.is_empty() && valid_reg_name(host) && port.is_none_or(valid_port)
}

fn valid_port_suffix(suffix: &str) -> bool {
    if suffix.is_empty() {
        return true;
    }
    let Some(port) = suffix.strip_prefix(':') else {
        return false;
    };
    valid_port(port)
}

fn valid_port(port: &str) -> bool {
    !port.is_empty()
        && port.bytes().all(|byte| byte.is_ascii_digit())
        && port.parse::<u16>().is_ok()
}

fn valid_reg_name(host: &str) -> bool {
    let bytes = host.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return false;
            }
            index += 3;
            continue;
        }
        if !(byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'.'
                    | b'_'
                    | b'~'
                    | b'!'
                    | b'$'
                    | b'&'
                    | b'\''
                    | b'('
                    | b')'
                    | b'*'
                    | b'+'
                    | b','
                    | b';'
                    | b'='
            ))
        {
            return false;
        }
        index += 1;
    }
    true
}

fn declares_open_url(manifest: &pf_app_manifest::Manifest) -> bool {
    manifest
        .app
        .capabilities
        .iter()
        .any(|capability| parse_capability_requirement(capability).base == OPEN_URL_CAPABILITY)
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
        mut store: S,
        system: B,
        clock: C,
        recent_bound: usize,
        grace: Duration,
        resolver: Resolver,
        now_fn: fn() -> SystemTime,
    ) -> Result<Self, AuthorityError> {
        let state = match store.load()? {
            Some(state) => state,
            None => {
                let state = PersistedState::default();
                store.save(&state)?;
                state
            }
        };
        let orientation_active_app = Arc::new(Mutex::new(match &state.phase {
            Phase::Idle => None,
            Phase::Starting { item_id, .. }
            | Phase::Running { item_id, .. }
            | Phase::StoppingGracefully { item_id, .. }
            | Phase::ForceStopping { item_id, .. }
            | Phase::Restoring { item_id, .. }
            | Phase::RecoveryRequired { item_id, .. }
            | Phase::StopFailed { item_id, .. } => Some(item_id.clone()),
        }));
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
            home_ack_started: None,
            log: stderr_log_sink(),
            escalation: None,
            now_fn,
            url_handler: Box::new(NoUrlHandler),
            url_rate: BTreeMap::new(),
            orientation: Arc::new(orientation::OrientationController::new(
                orientation::OrientationInputs::default(),
                DEFAULT_CONNECTION_LIMITS.max_connections,
            )),
            orientation_active_app,
        })
    }
    /// Replaces the default [`NoUrlHandler`] resolver.
    pub fn with_url_handler(mut self, handler: impl UrlHandlerResolver + Send + 'static) -> Self {
        self.url_handler = Box::new(handler);
        self
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
    /// Attaches the sampler hook that fires immediately before every close-path escalation.
    pub fn with_escalation_hook(
        mut self,
        hook: impl FnMut(&EscalationContext) + Send + 'static,
    ) -> Self {
        self.escalation = Some(Box::new(hook));
        self
    }
    fn log(&mut self, line: &str) {
        (self.log)(line);
    }
    pub fn state(&self) -> &PersistedState {
        &self.state
    }
    pub fn orientation_watch(&self) -> Arc<orientation::OrientationWatch> {
        self.orientation.watch()
    }
    pub fn orientation_controller(&self) -> Arc<orientation::OrientationController> {
        Arc::clone(&self.orientation)
    }
    pub fn orientation_active_app(&self) -> orientation::ActiveApp {
        Arc::clone(&self.orientation_active_app)
    }
    pub fn update_orientation(&self, inputs: orientation::OrientationInputs) {
        self.orientation.update(inputs);
    }
    pub fn into_parts(self) -> (S, B, C) {
        (self.store, self.system, self.clock)
    }
    fn persist(&mut self) -> Result<(), AuthorityError> {
        self.store.save(&self.state)?;
        let active_app = self.item_id();
        *self
            .orientation_active_app
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = active_app;
        Ok(())
    }
    fn session_id(&self) -> Option<String> {
        match &self.state.phase {
            Phase::Idle => None,
            Phase::Starting { session_id, .. }
            | Phase::Running { session_id, .. }
            | Phase::StoppingGracefully { session_id, .. }
            | Phase::ForceStopping { session_id, .. }
            | Phase::Restoring { session_id, .. }
            | Phase::RecoveryRequired { session_id, .. }
            | Phase::StopFailed { session_id, .. } => Some(session_id.clone()),
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
            | Phase::RecoveryRequired { item_id, .. }
            | Phase::StopFailed { item_id, .. } => Some(item_id.clone()),
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
    /// Runs the sampler hook (if any) and then the forced kill. Every close-path escalation
    /// goes through here so the hook can never be bypassed; with no hook attached this is one
    /// branch and the kill itself.
    fn escalate(
        &mut self,
        item_id: &str,
        session_id: &str,
        reason: ForceStopReason,
    ) -> Result<(), String> {
        if let Some(hook) = self.escalation.as_mut() {
            hook(&EscalationContext {
                session_id,
                item_id,
                reason,
            });
        }
        self.system.enforce_termination(item_id, session_id)
    }
    /// Records a kill failure on a close path as the durable, retryable `StopFailed` phase
    /// (§9.6 "Stop failed") instead of a terminal recovery: the menu row keeps offering the
    /// retry, and a further `SafeReturn` runs the whole close sequence again.
    fn stop_failed(&mut self, detail: String) -> Result<(), AuthorityError> {
        let session_id = self
            .session_id()
            .ok_or(AuthorityError::InvalidObservation)?;
        let item_id = self.item_id().unwrap_or_default();
        self.graceful_deadline = None;
        let reason = format!("{}: {detail}", ReasonCode::AppExitFailed.as_str());
        self.log(&lifecycle_failure_line(
            ReasonCode::AppExitFailed.as_str(),
            &item_id,
            &detail,
        ));
        self.state.phase = Phase::StopFailed {
            session_id: session_id.clone(),
            item_id,
            reason: reason.clone(),
        };
        self.publish(WireEvent::StopFailed { session_id, reason });
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
        self.reconcile_url_transition()?;
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
                    post_app_owner: None,
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
                // The monotonic deadline was lost to a daemon restart. Read the unit before
                // acting: if the app already stopped while the daemon was down (by itself or
                // by the unit's own TimeoutStopSec backstop), the close completes as Returned
                // through reconcile_systemd() below. Killing a dead unit would publish a
                // wrong ForcedClose — and sample a hang that is already gone.
                let still_running = match self.system.lifecycle(&item_id) {
                    Ok(Some(snapshot)) => matches!(snapshot.app, AppUnitState::Active),
                    // No lifecycle source (fakes, the desktop-sim preset): a lost deadline
                    // stays immediately due, as before.
                    Ok(None) => true,
                    // Unknown unit state: never kill blind; reconcile_systemd() below records
                    // the typed systemd_state_unknown recovery.
                    Err(_) => false,
                };
                if still_running {
                    if let Err(reason) =
                        self.escalate(&item_id, &session_id, ForceStopReason::StopTimeout)
                    {
                        return self.stop_failed(reason);
                    }
                    self.publish(WireEvent::ForceStopping {
                        session_id: session_id.clone(),
                        reason: ForceStopReason::StopTimeout,
                    });
                    self.state.phase = Phase::ForceStopping {
                        session_id,
                        item_id,
                    };
                    self.persist()?;
                }
            }
        }
        self.reconcile_handler()?;
        if self.state.safe_return_queue > 0
            && matches!(
                self.state.phase,
                Phase::Starting { .. } | Phase::Running { .. } | Phase::StopFailed { .. }
            )
        {
            self.state.safe_return_queue -= 1;
            self.release_slot_for_safe_return()?;
            if matches!(self.state.phase, Phase::RecoveryRequired { .. }) {
                return Ok(());
            }
            let id = self.session_id().unwrap();
            let item_id = self.item_id().unwrap();
            match self.system.request_graceful_stop(&item_id, &id) {
                Ok(()) => {
                    self.graceful_deadline = Some(self.clock.deadline_after(self.grace).0);
                    self.publish(WireEvent::StoppingGracefully {
                        session_id: id.clone(),
                    });
                    self.state.phase = Phase::StoppingGracefully {
                        session_id: id,
                        item_id,
                        boot_marker: boot_marker(),
                    }
                }
                Err(_) => {
                    if let Err(reason) =
                        self.escalate(&item_id, &id, ForceStopReason::StopRequestFailed)
                    {
                        return self.stop_failed(reason);
                    }
                    self.publish(WireEvent::ForceStopping {
                        session_id: id.clone(),
                        reason: ForceStopReason::StopRequestFailed,
                    });
                    self.state.phase = Phase::ForceStopping {
                        session_id: id,
                        item_id,
                    };
                }
            }
            self.persist()?;
        }
        self.reconcile_systemd()?;
        self.reconcile_resident_monitor()
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
                if let Err(reason) =
                    self.escalate(&item_id, &session_id, ForceStopReason::StopTimeout)
                {
                    return self.stop_failed(reason);
                }
                self.publish(WireEvent::ForceStopping {
                    session_id: session_id.clone(),
                    reason: ForceStopReason::StopTimeout,
                });
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
        if matches!(self.state.phase, Phase::Idle)
            && self.state.resident_monitor.as_ref().is_some_and(|monitor| {
                monitor.rung == ResidentMonitorRung::PresentationAcknowledged
            })
        {
            let now = self.clock.now();
            let timeout = self.presentation_timeout;
            let deadline = *self
                .presentation_deadline
                .get_or_insert_with(|| now.saturating_add(timeout));
            if now >= deadline {
                let item_id = self
                    .state
                    .resident_monitor
                    .as_ref()
                    .map(|monitor| monitor.item_id.clone())
                    .unwrap_or_default();
                let code = ReasonCode::PresentationNotAcknowledged.as_str();
                let detail = format!(
                    "app-less restoration presentation not acknowledged within {} ms",
                    timeout.as_millis()
                );
                self.log(&lifecycle_failure_line(code, &item_id, &detail));
                self.state.resident_monitor.as_mut().unwrap().rung =
                    ResidentMonitorRung::RecoveryRequired;
                self.presentation_deadline = None;
                self.persist()?;
            }
            return Ok(());
        }
        let Phase::Restoring {
            item_id,
            receipt,
            rung,
            ..
        } = &self.state.phase
        else {
            self.presentation_deadline = None;
            return Ok(());
        };
        let awaiting_gamescope_home =
            *rung == RestorationRung::OwnerActive && self.home_ack_started.is_some();
        if *rung != RestorationRung::PresentationAcknowledged && !awaiting_gamescope_home {
            self.presentation_deadline = None;
            return Ok(());
        }
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
                return self.reconcile_late_gamescope_home();
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
            let gamescope = match self.system.gamescope_session_lifecycle() {
                Ok(snapshot) => snapshot,
                Err(reason) => {
                    return self.recover(format!(
                        "{}: {reason}",
                        ReasonCode::SystemdStateUnknown.as_str()
                    ));
                }
            };
            if matches!(
                self.state.phase,
                Phase::Restoring {
                    rung: RestorationRung::OwnerActive,
                    ..
                }
            ) && matches!(
                gamescope,
                Some(GamescopeSessionLifecycle::HomeClient { .. })
            ) && self.home_ack_started.is_none()
            {
                self.home_ack_started = Some(self.clock.now());
                self.presentation_deadline =
                    Some(self.clock.deadline_after(self.presentation_timeout).0);
            }
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
                    | Phase::StopFailed { .. }
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
                ) => match gamescope.clone() {
                    Some(GamescopeSessionLifecycle::Transitioning) => None,
                    Some(GamescopeSessionLifecycle::Transient { diagnostic }) => {
                        if let Some(diagnostic) = diagnostic {
                            self.log(&diagnostic);
                        }
                        None
                    }
                    Some(GamescopeSessionLifecycle::SessionIdle) => {
                        Some(Observation::GamescopeSessionIdle)
                    }
                    Some(GamescopeSessionLifecycle::HomeClient {
                        unit, ack_signal, ..
                    }) => Some(Observation::GamescopeHomeAvailable { unit, ack_signal }),
                    None if selected_owner_release_ready(
                        snapshot.foreground_target_active,
                        &gamescope,
                    ) =>
                    {
                        Some(Observation::TargetReleased)
                    }
                    None => None,
                },
                (
                    Phase::Restoring {
                        rung: RestorationRung::OwnerActive,
                        ..
                    },
                    _,
                ) => match gamescope.clone() {
                    Some(GamescopeSessionLifecycle::HomeClient {
                        unit,
                        active: true,
                        ack_signal,
                        ..
                    }) => Some(Observation::GamescopeHomeActive { unit, ack_signal }),
                    Some(_) => None,
                    None if snapshot.selected_owner_active => {
                        Some(Observation::SelectedOwnerActive)
                    }
                    None => None,
                },
                (
                    Phase::Restoring {
                        rung: RestorationRung::PresentationAcknowledged,
                        ..
                    },
                    _,
                ) => match gamescope {
                    Some(GamescopeSessionLifecycle::HomeClient {
                        unit,
                        presenting: true,
                        ack_signal,
                        ..
                    }) => Some(Observation::GamescopeHomePresented { unit, ack_signal }),
                    _ => None,
                },
                _ => None,
            };
            let Some(observation) = observation else {
                return Ok(());
            };
            self.observe(observation)?;
        }
        Ok(())
    }

    /// Re-samples a completed resident restoration on every service-loop tick. This is a
    /// post-terminal convergence path, not a read/commit lease: observing release is latched so
    /// a later equal-looking resident endpoint cannot erase an intervening transition.
    fn reconcile_resident_monitor(&mut self) -> Result<(), AuthorityError> {
        let Some(monitor) = self.state.resident_monitor.clone() else {
            return Ok(());
        };
        match monitor.rung {
            ResidentMonitorRung::Resident | ResidentMonitorRung::TargetReleased => {
                let gamescope = self
                    .system
                    .gamescope_session_lifecycle()
                    .map_err(AuthorityError::Backend)?;
                match gamescope {
                    Some(GamescopeSessionLifecycle::Transitioning)
                    | Some(GamescopeSessionLifecycle::Transient { .. }) => {
                        if monitor.rung != ResidentMonitorRung::TargetReleased {
                            self.state.resident_monitor.as_mut().unwrap().rung =
                                ResidentMonitorRung::TargetReleased;
                            self.persist()?;
                        }
                        Ok(())
                    }
                    None => {
                        if monitor.rung != ResidentMonitorRung::TargetReleased {
                            self.state.resident_monitor.as_mut().unwrap().rung =
                                ResidentMonitorRung::TargetReleased;
                            self.persist()?;
                        }
                        let snapshot = self
                            .system
                            .lifecycle(&monitor.item_id)
                            .map_err(AuthorityError::Backend)?;
                        if snapshot.as_ref().is_some_and(|snapshot| {
                            selected_owner_release_ready(
                                snapshot.foreground_target_active,
                                &gamescope,
                            )
                        }) {
                            self.begin_resident_monitor_fallback()
                        } else {
                            Ok(())
                        }
                    }
                    Some(_) => Ok(()),
                }
            }
            ResidentMonitorRung::OwnerActive => {
                let selected_owner_active = self
                    .system
                    .lifecycle(&monitor.item_id)
                    .map_err(AuthorityError::Backend)?
                    .is_some_and(|snapshot| snapshot.selected_owner_active);
                if selected_owner_active {
                    self.state.resident_monitor.as_mut().unwrap().rung =
                        ResidentMonitorRung::PresentationAcknowledged;
                    self.presentation_deadline =
                        Some(self.clock.deadline_after(self.presentation_timeout).0);
                    self.persist()?;
                }
                Ok(())
            }
            ResidentMonitorRung::PresentationAcknowledged
            | ResidentMonitorRung::RecoveryRequired => Ok(()),
        }
    }

    fn begin_resident_monitor_fallback(&mut self) -> Result<(), AuthorityError> {
        match self.state.phase.clone() {
            Phase::Idle => {
                self.system
                    .activate_selected_owner()
                    .map_err(AuthorityError::Backend)?;
                self.state.resident_monitor.as_mut().unwrap().rung =
                    ResidentMonitorRung::OwnerActive;
                self.persist()
            }
            Phase::Restoring {
                session_id,
                item_id,
                receipt,
                ..
            } => {
                self.system
                    .activate_selected_owner()
                    .map_err(AuthorityError::Backend)?;
                self.set_post_app_owner_for_fallback(&session_id);
                self.home_ack_started = None;
                self.presentation_deadline = None;
                self.state.resident_monitor = None;
                self.state.phase = Phase::Restoring {
                    session_id,
                    item_id,
                    receipt,
                    rung: RestorationRung::OwnerActive,
                };
                self.persist()
            }
            Phase::RecoveryRequired {
                session_id,
                item_id,
                reason,
                pending_receipt: Some(receipt),
            } if is_presentation_timeout(&reason) => {
                self.system
                    .activate_selected_owner()
                    .map_err(AuthorityError::Backend)?;
                self.set_post_app_owner_for_fallback(&session_id);
                self.home_ack_started = None;
                self.presentation_deadline = None;
                self.state.resident_monitor = None;
                self.state.phase = Phase::Restoring {
                    session_id,
                    item_id,
                    receipt,
                    rung: RestorationRung::OwnerActive,
                };
                self.persist()
            }
            _ => Ok(()),
        }
    }

    fn set_post_app_owner_for_fallback(&mut self, session_id: &str) {
        if let Some(entry) = self
            .state
            .history
            .iter_mut()
            .find(|entry| entry.session_id == session_id)
        {
            entry.post_app_owner = None;
        }
    }

    fn reconcile_late_gamescope_home(&mut self) -> Result<(), AuthorityError> {
        let Phase::RecoveryRequired {
            session_id,
            reason,
            pending_receipt: Some(_),
            ..
        } = &self.state.phase
        else {
            return Ok(());
        };
        if !is_presentation_timeout(reason) {
            return Ok(());
        }
        let session_id = session_id.clone();
        let Some(evidence) = self
            .state
            .history
            .iter()
            .find(|entry| entry.session_id == session_id)
            .and_then(|entry| entry.post_app_owner.as_ref())
        else {
            // The legacy fb0 presentation path has no Gamescope owner evidence and remains
            // byte-for-byte unchanged: only its explicit client acknowledgement can recover.
            return Ok(());
        };
        if evidence.owner_tier != OwnerTier::Gamescope
            || evidence.post_app_owner != PostAppOwner::HomeClient
        {
            return Ok(());
        }
        let (Some(expected_unit), Some(expected_signal)) =
            (evidence.home_unit.clone(), evidence.ack_signal.clone())
        else {
            return Ok(());
        };
        let gamescope = self
            .system
            .gamescope_session_lifecycle()
            .map_err(AuthorityError::Backend)?;
        match gamescope {
            Some(GamescopeSessionLifecycle::HomeClient {
                unit,
                active: true,
                presenting: true,
                ack_signal,
            }) if unit == expected_unit && ack_signal == expected_signal => {
                self.observe(Observation::GamescopeHomePresented { unit, ack_signal })
            }
            _ => Ok(()),
        }
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
        if self.promote_handler_if_caller_ended(&observation)? {
            return Ok(());
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
            // The kill failed, but the unit is gone now (systemd's own backstop or the app
            // itself): the close the user asked for did happen, as a forced close.
            (Phase::StopFailed { .. }, Observation::UnitInactive) => {
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
                Observation::GamescopeSessionIdle,
            ) => {
                let (session_id, item_id, receipt) =
                    (session_id.clone(), item_id.clone(), receipt.clone());
                self.set_post_app_owner(
                    &session_id,
                    PostAppOwnerEvidence {
                        owner_tier: OwnerTier::Gamescope,
                        post_app_owner: PostAppOwner::SessionIdle,
                        home_unit: None,
                        ack_signal: None,
                        ack_latency_ms: None,
                    },
                );
                self.arm_resident_monitor(&item_id);
                self.complete_restoration(session_id, receipt)
            }
            (
                Phase::Restoring {
                    session_id,
                    item_id,
                    receipt,
                    rung: RestorationRung::TargetReleased,
                },
                Observation::GamescopeHomeAvailable { unit, ack_signal },
            ) => {
                let session_id = session_id.clone();
                let item_id = item_id.clone();
                let receipt = receipt.clone();
                if let Err(reason) = self.system.activate_home_client(&unit) {
                    return self
                        .recover(format!("{}: {reason}", ReasonCode::OwnerNotActive.as_str()));
                }
                self.set_post_app_owner(
                    &session_id,
                    PostAppOwnerEvidence {
                        owner_tier: OwnerTier::Gamescope,
                        post_app_owner: PostAppOwner::HomeClient,
                        home_unit: Some(unit),
                        ack_signal: Some(ack_signal),
                        ack_latency_ms: None,
                    },
                );
                self.arm_resident_monitor(&item_id);
                self.home_ack_started = Some(self.clock.now());
                self.presentation_deadline =
                    Some(self.clock.deadline_after(self.presentation_timeout).0);
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
                    rung: RestorationRung::TargetReleased,
                },
                Observation::TargetReleased,
            ) => {
                let session_id = session_id.clone();
                let item_id = item_id.clone();
                let receipt = receipt.clone();
                if let Some(entry) = self
                    .state
                    .history
                    .iter_mut()
                    .find(|entry| entry.session_id == session_id)
                {
                    entry.post_app_owner = None;
                }
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
                Observation::GamescopeHomeActive { unit, ack_signal },
            ) => {
                let session_id = session_id.clone();
                let item_id = item_id.clone();
                let receipt = receipt.clone();
                self.refresh_home_evidence(&session_id, &unit, &ack_signal, None);
                if self.home_ack_started.is_none() {
                    self.home_ack_started = Some(self.clock.now());
                }
                self.state.phase = Phase::Restoring {
                    session_id,
                    item_id,
                    receipt,
                    rung: RestorationRung::PresentationAcknowledged,
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
                if self.home_ack_started.take().is_some() {
                    self.presentation_deadline = None;
                    if let Some(entry) = self
                        .state
                        .history
                        .iter_mut()
                        .find(|entry| entry.session_id == session_id.as_str())
                    {
                        entry.post_app_owner = None;
                    }
                }
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
            (
                Phase::Restoring {
                    session_id,
                    item_id,
                    receipt,
                    rung: RestorationRung::PresentationAcknowledged,
                },
                Observation::GamescopeHomePresented { unit, ack_signal },
            ) => {
                let (id, receipt) = (session_id.clone(), receipt.clone());
                let item_id = item_id.clone();
                let latency_ms = self.home_ack_started.map(|started| {
                    self.clock
                        .now()
                        .as_nanos()
                        .saturating_sub(started.as_nanos())
                        / 1_000_000
                });
                self.refresh_home_evidence(&id, &unit, &ack_signal, latency_ms);
                if self.state.resident_monitor.is_none() {
                    self.arm_resident_monitor(&item_id);
                }
                self.complete_restoration(id, receipt)
            }
            // A late client acknowledgement after the presentation deadline expired. The owed
            // receipt becomes history now; the system-driven Gamescope form is handled below.
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
            (
                Phase::RecoveryRequired {
                    session_id,
                    item_id,
                    reason,
                    pending_receipt: Some(receipt),
                },
                Observation::GamescopeHomePresented { unit, ack_signal },
            ) if is_presentation_timeout(reason) => {
                let (id, receipt) = (session_id.clone(), receipt.clone());
                let item_id = item_id.clone();
                if !self.recorded_gamescope_home_matches(&id, &unit, &ack_signal) {
                    return Err(AuthorityError::InvalidObservation);
                }
                let latency_ms = self.home_ack_started.map(|started| {
                    self.clock
                        .now()
                        .as_nanos()
                        .saturating_sub(started.as_nanos())
                        / 1_000_000
                });
                self.refresh_home_evidence(&id, &unit, &ack_signal, latency_ms);
                if self.state.resident_monitor.is_none() {
                    self.arm_resident_monitor(&item_id);
                }
                self.complete_restoration(id, receipt)
            }
            (Phase::Idle, Observation::PresentationAcknowledged)
                if self.state.resident_monitor.as_ref().is_some_and(|monitor| {
                    matches!(
                        monitor.rung,
                        ResidentMonitorRung::PresentationAcknowledged
                            | ResidentMonitorRung::RecoveryRequired
                    )
                }) =>
            {
                self.presentation_deadline = None;
                self.state.resident_monitor = None;
                self.persist()
            }
            _ => Err(AuthorityError::InvalidObservation),
        }
    }

    fn recorded_gamescope_home_matches(
        &self,
        session_id: &str,
        unit: &str,
        ack_signal: &str,
    ) -> bool {
        self.state
            .history
            .iter()
            .find(|entry| entry.session_id == session_id)
            .and_then(|entry| entry.post_app_owner.as_ref())
            .is_some_and(|evidence| {
                evidence.owner_tier == OwnerTier::Gamescope
                    && evidence.post_app_owner == PostAppOwner::HomeClient
                    && evidence.home_unit.as_deref() == Some(unit)
                    && evidence.ack_signal.as_deref() == Some(ack_signal)
            })
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
        self.home_ack_started = None;
        self.state.phase = Phase::Idle;
        self.state.return_slot = None;
        self.persist()
    }

    fn arm_resident_monitor(&mut self, item_id: &str) {
        self.state.resident_monitor = Some(ResidentMonitor {
            item_id: item_id.to_owned(),
            receipt: Receipt::Returned,
            rung: ResidentMonitorRung::Resident,
        });
    }

    fn set_post_app_owner(&mut self, session_id: &str, evidence: PostAppOwnerEvidence) {
        if let Some(entry) = self
            .state
            .history
            .iter_mut()
            .find(|entry| entry.session_id == session_id)
        {
            entry.post_app_owner = Some(evidence);
        }
    }

    fn refresh_home_evidence(
        &mut self,
        session_id: &str,
        unit: &str,
        ack_signal: &str,
        latency_ms: Option<u64>,
    ) {
        self.set_post_app_owner(
            session_id,
            PostAppOwnerEvidence {
                owner_tier: OwnerTier::Gamescope,
                post_app_owner: PostAppOwner::HomeClient,
                home_unit: Some(unit.to_owned()),
                ack_signal: Some(ack_signal.to_owned()),
                ack_latency_ms: latency_ms,
            },
        );
    }
    /// Stamps a history entry's end and receipt without publishing a Terminal event: used for
    /// sessions that end behind or beside the foreground session (the slot's caller or handler),
    /// where the shell is NOT back in front.
    fn close_history_entry(&mut self, session_id: &str, receipt: Receipt, precision: EndPrecision) {
        let at = (self.now_fn)();
        if let Some(entry) = self
            .state
            .history
            .iter_mut()
            .find(|entry| entry.session_id == session_id)
        {
            entry.ended_at = Some(EndStamp { at, precision });
            entry.receipt = Some(receipt);
        }
    }
    fn rate_limited(&mut self, key: &str) -> bool {
        let now = self.clock.now();
        let stamps = self.url_rate.entry(key.to_owned()).or_default();
        while stamps
            .front()
            .is_some_and(|stamp| stamp.saturating_add(URL_RATE_WINDOW) <= now)
        {
            stamps.pop_front();
        }
        if stamps.len() >= URL_RATE_LIMIT {
            return true;
        }
        stamps.push_back(now);
        false
    }
    fn begin_url_transition(&mut self, transition: UrlTransition) -> Result<(), AuthorityError> {
        let before = self.state.clone();
        self.state.url_transition = Some(transition);
        if let Err(error) = self.persist() {
            self.state = before;
            return Err(error);
        }
        Ok(())
    }
    fn finish_url_handoff(&mut self, handoff: &UrlHandoff) -> Result<(), AuthorityError> {
        let durable_intent = self.state.clone();
        if matches!(handoff.caller, SlotCaller::App { .. })
            && !self
                .state
                .history
                .iter()
                .any(|entry| entry.session_id == handoff.handler_session_id)
        {
            self.state.history.push_front(HistoryEntry {
                session_id: handoff.handler_session_id.clone(),
                item_id: handoff.handler_item_id.clone(),
                receipt: None,
                started_at: Some((self.now_fn)()),
                ended_at: None,
                post_app_owner: None,
            });
            self.state.history.truncate(self.recent_bound);
        }
        let slot = ReturnSlot {
            caller: handoff.caller.clone(),
            handler_session_id: handoff.handler_session_id.clone(),
            handler_item_id: handoff.handler_item_id.clone(),
            caller_gone: false,
        };
        self.state.return_slot = Some(slot.clone());
        self.state.url_transition = None;
        if let Err(error) = self.persist() {
            self.state = durable_intent;
            return Err(error);
        }
        let line = url_handoff_line("opened", &slot);
        self.log(&line);
        Ok(())
    }
    fn begin_restore(&mut self, slot: &ReturnSlot) -> Result<(), AuthorityError> {
        self.begin_url_transition(UrlTransition::Restoring { slot: slot.clone() })
    }
    fn finish_restore(&mut self) -> Result<(), AuthorityError> {
        let durable_intent = self.state.clone();
        self.state.return_slot = None;
        self.state.url_transition = None;
        if let Err(error) = self.persist() {
            self.state = durable_intent;
            return Err(error);
        }
        Ok(())
    }
    fn reconcile_url_transition(&mut self) -> Result<(), AuthorityError> {
        let Some(transition) = self.state.url_transition.clone() else {
            return Ok(());
        };
        match transition {
            UrlTransition::Opening { handoff } => {
                if let Err(reason) = self.system.open_url(&handoff) {
                    return Err(self.handoff_failure(&handoff.handler_item_id, &reason));
                }
                self.finish_url_handoff(&handoff)
            }
            UrlTransition::Restoring { slot } => {
                if let Err(reason) = self.system.restore_caller(&slot) {
                    let line = lifecycle_failure_line(
                        ReasonCode::OwnerNotActive.as_str(),
                        &slot.handler_item_id,
                        &format!("caller restore replay failed: {reason}"),
                    );
                    self.log(&line);
                    return Err(AuthorityError::Backend(reason));
                }
                self.finish_restore()
            }
        }
    }
    /// Re-verifies the front's claim against the kernel-derived app id. Any mismatch or missing
    /// identity is refused and logged; the derived id, never the claim, is what the authority
    /// acts on.
    fn verify_front_caller(
        &mut self,
        verb: &str,
        derived: Result<String, String>,
        claimed: Option<String>,
    ) -> Option<String> {
        match (derived, claimed) {
            (Err(reason), claimed) => {
                let line = url_refusal_line(
                    verb,
                    "identity_unverified",
                    claimed.as_deref().unwrap_or("-"),
                    Some(&reason),
                );
                self.log(&line);
                None
            }
            (Ok(derived), None) => {
                let line = url_refusal_line(verb, "missing_claim", &derived, None);
                self.log(&line);
                None
            }
            (Ok(derived), Some(claimed)) if claimed != derived => {
                let line = url_refusal_line(
                    verb,
                    "identity_mismatch",
                    &claimed,
                    Some(&format!("derived={derived}")),
                );
                self.log(&line);
                None
            }
            (Ok(derived), Some(_)) => Some(derived),
        }
    }
    fn refuse_url(&mut self, verb: &str, reason: &str, caller: &str, detail: Option<&str>) {
        let line = url_refusal_line(verb, reason, caller, detail);
        self.log(&line);
    }
    /// `OpenUrl` for a verified caller. See the crate docs for the check order.
    pub fn open_url(
        &mut self,
        caller: UrlCaller,
        url: &str,
        flags: u32,
    ) -> Result<RpcResponse, AuthorityError> {
        const VERB: &str = "open_url";
        let caller_key = match &caller {
            UrlCaller::Shell => SHELL_CALLER_KEY.to_owned(),
            UrlCaller::App(app_id) => app_id.clone(),
        };
        if self.rate_limited(&caller_key) {
            self.refuse_url(VERB, "rate_limited", &caller_key, None);
            return Ok(RpcResponse::RateLimited);
        }
        if flags & OPEN_URL_FLAG_AUTH_SESSION != 0 {
            self.refuse_url(VERB, "flag_reserved", &caller_key, Some("auth_session"));
            return Ok(RpcResponse::Denied);
        }
        if flags & OPEN_URL_FLAG_CAPTIVE_PORTAL != 0 && matches!(caller, UrlCaller::App(_)) {
            self.refuse_url(VERB, "flag_shell_only", &caller_key, Some("captive_portal"));
            return Ok(RpcResponse::Denied);
        }
        if flags & !(OPEN_URL_FLAG_AUTH_SESSION | OPEN_URL_FLAG_CAPTIVE_PORTAL) != 0 {
            self.refuse_url(
                VERB,
                "flag_unknown",
                &caller_key,
                Some(&format!("{flags:#x}")),
            );
            return Ok(RpcResponse::Denied);
        }
        let scheme = match validate_url(url) {
            Ok(scheme) => scheme,
            Err(rejection) => {
                self.refuse_url(VERB, "invalid_url", &caller_key, Some(rejection.as_str()));
                return Ok(RpcResponse::InvalidUrl);
            }
        };
        if let UrlCaller::App(app_id) = &caller {
            match self.resolver.resolve(app_id) {
                Ok(resolved) if declares_open_url(&resolved.manifest) => {}
                Ok(_) => {
                    self.refuse_url(VERB, "capability", app_id, Some(OPEN_URL_CAPABILITY));
                    return Ok(RpcResponse::Denied);
                }
                Err(error) => {
                    self.refuse_url(VERB, error.reason.as_str(), app_id, Some(&error.detail));
                    return Ok(RpcResponse::Denied);
                }
            }
        }
        if self.state.return_slot.is_some() {
            self.refuse_url(VERB, "slot_occupied", &caller_key, None);
            return Ok(RpcResponse::Busy);
        }
        let slot_caller = match (&caller, &self.state.phase) {
            (UrlCaller::Shell, Phase::Idle) => SlotCaller::Shell,
            (
                UrlCaller::App(app_id),
                Phase::Running {
                    session_id,
                    item_id,
                },
            ) if item_id == app_id => SlotCaller::App {
                session_id: session_id.clone(),
                item_id: item_id.clone(),
            },
            (UrlCaller::App(_), Phase::Running { .. } | Phase::Idle) => {
                self.refuse_url(
                    VERB,
                    "not_foreground",
                    &caller_key,
                    Some(self.state.phase.name()),
                );
                return Ok(RpcResponse::Denied);
            }
            (_, phase) => {
                let name = phase.name();
                self.refuse_url(VERB, "phase", &caller_key, Some(name));
                return Ok(RpcResponse::Busy);
            }
        };
        let handler = match self.url_handler.handler_for(scheme) {
            Ok(Some(handler)) => handler,
            Ok(None) => {
                self.refuse_url(VERB, "no_handler", &caller_key, Some(scheme));
                return Ok(RpcResponse::NoHandler);
            }
            Err(reason) => {
                return Err(AuthorityError::Backend(format!(
                    "handler resolution: {reason}"
                )))
            }
        };
        if let Err(error) = self.resolver.resolve(&handler) {
            self.refuse_url(VERB, "no_handler", &caller_key, Some(&error.to_string()));
            return Ok(RpcResponse::NoHandler);
        }
        let url = url.to_owned();
        match slot_caller {
            SlotCaller::Shell => {
                // The handler is a real foreground session launched by the shell's request.
                let handoff = UrlHandoff {
                    caller: SlotCaller::Shell,
                    handler_item_id: handler.clone(),
                    handler_session_id: format!("session-{}", self.state.next_session),
                    url,
                    flags,
                };
                let transition = UrlTransition::Opening {
                    handoff: handoff.clone(),
                };
                let (session_id, outcome) =
                    self.start_session(&handler, Some(transition), |system, _session_id| {
                        system.open_url(&handoff)
                    })?;
                let Some(outcome) = outcome else {
                    return Ok(RpcResponse::Error {
                        message: "URL handoff failed".into(),
                    });
                };
                self.finish_url_handoff(&handoff)?;
                Ok(match outcome {
                    UrlHandoffOutcome::Launched => RpcResponse::Launched { session_id },
                    UrlHandoffOutcome::Delivered => RpcResponse::Delivered,
                })
            }
            SlotCaller::App { .. } if handler == caller_key => {
                // The handler itself is in front: the URL is delivered to it, no handoff.
                let handoff = UrlHandoff {
                    caller: slot_caller,
                    handler_item_id: handler,
                    handler_session_id: self.session_id().unwrap_or_default(),
                    url,
                    flags,
                };
                match self.system.open_url(&handoff) {
                    Ok(_) => Ok(RpcResponse::Delivered),
                    Err(reason) => Err(self.handoff_failure(&handoff.handler_item_id, &reason)),
                }
            }
            SlotCaller::App { .. } => {
                let session_id = format!("session-{}", self.state.next_session);
                let handoff = UrlHandoff {
                    caller: slot_caller.clone(),
                    handler_item_id: handler.clone(),
                    handler_session_id: session_id.clone(),
                    url,
                    flags,
                };
                let before_intent = self.state.clone();
                self.state.next_session += 1;
                if let Err(error) = self.begin_url_transition(UrlTransition::Opening {
                    handoff: handoff.clone(),
                }) {
                    self.state = before_intent;
                    return Err(error);
                }
                let outcome = match self.system.open_url(&handoff) {
                    Ok(outcome) => outcome,
                    Err(reason) => {
                        self.state = before_intent;
                        self.persist()?;
                        return Err(self.handoff_failure(&handler, &reason));
                    }
                };
                self.finish_url_handoff(&handoff)?;
                Ok(match outcome {
                    UrlHandoffOutcome::Launched => RpcResponse::Launched { session_id },
                    UrlHandoffOutcome::Delivered => RpcResponse::Delivered,
                })
            }
        }
    }
    fn handoff_failure(&mut self, handler: &str, reason: &str) -> AuthorityError {
        let line = lifecycle_failure_line(ReasonCode::SystemdStartFailed.as_str(), handler, reason);
        self.log(&line);
        AuthorityError::Backend(reason.to_owned())
    }
    /// `ReturnToCaller` from the verified handler app.
    pub fn return_to_caller(
        &mut self,
        app_id: &str,
        url: Option<&str>,
    ) -> Result<RpcResponse, AuthorityError> {
        const VERB: &str = "return_to_caller";
        if let Some(url) = url {
            return Ok(match validate_url(url) {
                Err(rejection) => {
                    self.refuse_url(VERB, "invalid_url", app_id, Some(rejection.as_str()));
                    RpcResponse::InvalidUrl
                }
                Ok(_) => {
                    self.refuse_url(VERB, "return_url_reserved", app_id, Some("auth_session"));
                    RpcResponse::Denied
                }
            });
        }
        let Some(slot) = self.state.return_slot.clone() else {
            self.refuse_url(VERB, "no_return_slot", app_id, None);
            return Ok(RpcResponse::Denied);
        };
        if slot.handler_item_id != app_id {
            self.refuse_url(VERB, "not_handler", app_id, Some(&slot.handler_item_id));
            return Ok(RpcResponse::Denied);
        }
        match &slot.caller {
            SlotCaller::Shell => {
                // Home: the handler session ends through the protected return path.
                self.state.return_slot = None;
                let line = url_handoff_line("returned_home", &slot);
                self.log(&line);
                self.intake_safe_return()?;
                self.reconcile()?;
                Ok(RpcResponse::Restored)
            }
            SlotCaller::App { .. } if slot.caller_gone => {
                self.state.return_slot = None;
                let line = url_handoff_line("caller_gone_answered", &slot);
                self.log(&line);
                self.persist()?;
                Ok(RpcResponse::CallerGone)
            }
            SlotCaller::App { .. } => {
                self.begin_restore(&slot)?;
                if let Err(reason) = self.system.restore_caller(&slot) {
                    let line = lifecycle_failure_line(
                        ReasonCode::OwnerNotActive.as_str(),
                        &slot.handler_item_id,
                        &format!("caller restore failed: {reason}"),
                    );
                    self.log(&line);
                    return Err(AuthorityError::Backend(reason));
                }
                self.finish_restore()?;
                let line = url_handoff_line("restored", &slot);
                self.log(&line);
                Ok(RpcResponse::Restored)
            }
        }
    }
    /// While an app caller waits behind the handler, the handler's own unit is reconciled too:
    /// when it ends, the caller is restored and the slot clears.
    fn reconcile_handler(&mut self) -> Result<(), AuthorityError> {
        let Some(slot) = self.state.return_slot.clone() else {
            return Ok(());
        };
        if slot.caller_gone || !matches!(slot.caller, SlotCaller::App { .. }) {
            return Ok(());
        }
        let snapshot = match self.system.lifecycle(&slot.handler_item_id) {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => return Ok(()),
            Err(reason) => {
                return self.recover(format!(
                    "{}: {reason}",
                    ReasonCode::SystemdStateUnknown.as_str()
                ));
            }
        };
        let (receipt, precision) = match snapshot.app {
            AppUnitState::Active => return Ok(()),
            AppUnitState::InactiveSuccess => (Receipt::Returned, EndPrecision::Observed),
            AppUnitState::InactiveFailure { summary } => {
                (Receipt::Crash { summary }, EndPrecision::Approximate)
            }
        };
        self.close_history_entry(&slot.handler_session_id, receipt, precision);
        let line = url_handoff_line("handler_exited", &slot);
        self.log(&line);
        self.begin_restore(&slot)?;
        if let Err(reason) = self.system.restore_caller(&slot) {
            return self.recover(format!(
                "{}: caller restore failed: {reason}",
                ReasonCode::OwnerNotActive.as_str()
            ));
        }
        self.finish_restore()
    }
    /// The app caller ended while the handler was in front: the handler becomes the foreground
    /// session and the slot remembers that the caller is gone. Nothing is published, because
    /// the shell is not back in front.
    fn promote_handler_if_caller_ended(
        &mut self,
        observation: &Observation,
    ) -> Result<bool, AuthorityError> {
        let Some(slot) = self.state.return_slot.clone() else {
            return Ok(false);
        };
        let SlotCaller::App { session_id, .. } = &slot.caller else {
            return Ok(false);
        };
        if slot.caller_gone
            || !matches!(
                self.state.phase,
                Phase::Starting { .. } | Phase::Running { .. }
            )
            || self.session_id().as_deref() != Some(session_id.as_str())
        {
            return Ok(false);
        }
        let (receipt, precision) = match observation {
            Observation::SessionExitedCleanly => (Receipt::Returned, EndPrecision::Observed),
            Observation::SessionCrashed { summary } => (
                Receipt::Crash {
                    summary: summary.clone(),
                },
                EndPrecision::Approximate,
            ),
            _ => return Ok(false),
        };
        self.close_history_entry(session_id, receipt, precision);
        self.state.phase = Phase::Running {
            session_id: slot.handler_session_id.clone(),
            item_id: slot.handler_item_id.clone(),
        };
        let line = url_handoff_line("caller_gone", &slot);
        self.log(&line);
        self.state.return_slot = Some(ReturnSlot {
            caller_gone: true,
            ..slot
        });
        self.persist()?;
        Ok(true)
    }
    /// A protected return ends the whole excursion: a handler running beside the phase session
    /// is stopped first, then the ladder runs on the phase session as usual.
    fn release_slot_for_safe_return(&mut self) -> Result<(), AuthorityError> {
        let Some(slot) = self.state.return_slot.take() else {
            return Ok(());
        };
        if matches!(slot.caller, SlotCaller::App { .. }) && !slot.caller_gone {
            if self
                .system
                .request_graceful_stop(&slot.handler_item_id, &slot.handler_session_id)
                .is_err()
            {
                if let Err(reason) = self
                    .system
                    .enforce_termination(&slot.handler_item_id, &slot.handler_session_id)
                {
                    self.state.return_slot = Some(slot);
                    return self
                        .recover(format!("{}: {reason}", ReasonCode::AppExitFailed.as_str()));
                }
            }
            self.close_history_entry(
                &slot.handler_session_id,
                Receipt::Returned,
                EndPrecision::Observed,
            );
        }
        let line = url_handoff_line("safe_return", &slot);
        self.log(&line);
        self.persist()
    }
    /// Allocates the next session, persists the start intent, runs `start`, records the session
    /// and its first observation. A failed start is recorded as crash drift, never hidden.
    fn start_session<T>(
        &mut self,
        item_id: &str,
        url_transition: Option<UrlTransition>,
        start: impl FnOnce(&mut B, &str) -> Result<T, String>,
    ) -> Result<(String, Option<T>), AuthorityError> {
        let id = format!("session-{}", self.state.next_session);
        let before_intent = self.state.clone();
        self.state.resident_monitor = None;
        self.state.next_session += 1;
        self.state.phase = Phase::Starting {
            session_id: id.clone(),
            item_id: item_id.to_owned(),
            start_invoked: false,
        };
        self.state.url_transition = url_transition;
        if let Err(error) = self.persist() {
            self.state = before_intent;
            return Err(error);
        }
        let start_result = start(&mut self.system, &id);
        let durable_intent = self.state.clone();
        self.state.phase = Phase::Starting {
            session_id: id.clone(),
            item_id: item_id.to_owned(),
            start_invoked: true,
        };
        self.state.history.push_front(HistoryEntry {
            session_id: id.clone(),
            item_id: item_id.to_owned(),
            receipt: None,
            started_at: None,
            ended_at: None,
            post_app_owner: None,
        });
        self.state.history.truncate(self.recent_bound);
        self.publish(WireEvent::ObservedStarting);
        if start_result.is_err() {
            self.state.url_transition = None;
        }
        if let Err(error) = self.persist() {
            self.state = durable_intent;
            return Err(error);
        }
        match start_result {
            Ok(value) => {
                self.observe(Observation::SessionRunning)?;
                Ok((id, Some(value)))
            }
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
                Ok((id, None))
            }
        }
    }
    /// Pending events deliverable to `client_id`: past its cursor, minus the observed
    /// Starting/Running of ended sessions (see [`Self::ended_session_observation_bound`]).
    fn deliverable_events(&self, client_id: &str) -> impl Iterator<Item = &PublishedEvent> {
        let sequence = self.state.acknowledged.get(client_id).copied().unwrap_or(0);
        let ended_bound = self.ended_session_observation_bound();
        self.state
            .pending
            .iter()
            .filter(move |e| e.sequence > sequence)
            .filter(move |e| {
                !(matches!(
                    e.event,
                    WireEvent::ObservedStarting | WireEvent::ObservedRunning
                ) && e.sequence < ended_bound)
            })
    }
    /// The router/menu view: pending events as RPC payloads, selected for `contract`. A v1
    /// client never sees the v2-only close-progress events, but its cursor and compaction are
    /// untouched by them (the same delivery-filter discipline as the ended-session bound).
    fn rpc_events_for(&self, client_id: &str, contract: u32) -> Vec<(u64, RpcEvent)> {
        self.deliverable_events(client_id)
            .filter(|e| contract >= SESSION_RPC_CONTRACT_V2 || !is_close_progress(&e.event))
            .map(|e| (e.sequence, wire_to_rpc(&e.event)))
            .collect()
    }
    /// Time until the nearest armed deadline that the current phase can still enforce, or
    /// `None`. [`run_service_loop`] wakes at the deadline itself instead of waiting for the
    /// next periodic tick; `Duration::ZERO` means a deadline is already due. A deadline whose
    /// phase has moved on is deliberately ignored so a stale value can never busy-spin the
    /// loop.
    pub fn time_until_next_deadline(&self) -> Option<Duration> {
        let now = self.clock.now();
        let graceful = self
            .graceful_deadline
            .filter(|_| matches!(self.state.phase, Phase::StoppingGracefully { .. }));
        let presentation = self.presentation_deadline.filter(|_| {
            matches!(
                self.state.phase,
                Phase::Restoring {
                    rung: RestorationRung::PresentationAcknowledged,
                    ..
                }
            )
        });
        [graceful, presentation]
            .into_iter()
            .flatten()
            .map(|deadline| {
                Duration::from_nanos(deadline.as_nanos().saturating_sub(now.as_nanos()))
            })
            .min()
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
/// enforces its deadlines with no client RPC. It also wakes at an armed deadline itself
/// ([`Authority::time_until_next_deadline`]), so an escalation lands at its deadline, not up to a
/// tick later. Waiting uses a channel timeout, never a spin; a tick that overruns the interval is
/// followed by a full idle interval. A tick error is logged once until it changes or clears. A
/// connection-source error or a `serve` error ends the loop.
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
        let mut wait = next_tick.saturating_duration_since(Instant::now());
        if let Some(deadline_wait) = authority.time_until_next_deadline() {
            wait = wait.min(deadline_wait);
        }
        match connections.recv_timeout(wait) {
            Ok(Ok(connection)) => serve(authority, connection)?,
            Ok(Err(error)) => return Err(error),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
        let started = Instant::now();
        let deadline_due = authority
            .time_until_next_deadline()
            .is_some_and(|wait| wait.is_zero());
        if started < next_tick && !deadline_due {
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
        let item_id = request.item_id.clone();
        let (session_id, _) = self.start_session(&item_id, None, |system, session_id| {
            system.start_foreground(&request, session_id)
        })?;
        Ok(LaunchResult::Accepted { session_id })
    }
    fn events_for(&self, client_id: &str) -> Vec<(u64, SessionEvent)> {
        self.deliverable_events(client_id)
            .filter_map(|e| wire_to_port(&e.event).map(|event| (e.sequence, event)))
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

fn url_refusal_line(verb: &str, reason: &str, caller: &str, detail: Option<&str>) -> String {
    let mut line = format!(
        "pf-session-authorityd: url_refused verb={verb} reason={reason} caller={}",
        json_string(caller)
    );
    if let Some(detail) = detail {
        line.push_str(&format!(" detail={}", json_string(detail)));
    }
    line
}

fn url_handoff_line(event: &str, slot: &ReturnSlot) -> String {
    let caller = match &slot.caller {
        SlotCaller::Shell => "shell".to_owned(),
        SlotCaller::App { item_id, .. } => item_id.clone(),
    };
    format!(
        "pf-session-authorityd: url_handoff event={event} caller={} handler={} session={}",
        json_string(&caller),
        json_string(&slot.handler_item_id),
        json_string(&slot.handler_session_id)
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
/// The v1 port view of a wire event. Contract v2 close-progress events have no
/// [`SessionEvent`] shape and are never delivered through this surface.
fn wire_to_port(event: &WireEvent) -> Option<SessionEvent> {
    match event {
        WireEvent::ObservedStarting => Some(SessionEvent::Observed(ObservedSessionState::Starting)),
        WireEvent::ObservedRunning => Some(SessionEvent::Observed(ObservedSessionState::Running)),
        WireEvent::ObservationComplete => Some(SessionEvent::Observed(
            ObservedSessionState::ObservationComplete,
        )),
        WireEvent::Terminal {
            session_id,
            receipt,
        } => Some(SessionEvent::Terminal(receipt_to_port(receipt, session_id))),
        WireEvent::RecoveryRequired { session_id, reason } => {
            Some(SessionEvent::RecoveryRequired(RecoveryRequired {
                session_id: session_id.clone(),
                reason: reason.clone(),
            }))
        }
        WireEvent::StoppingGracefully { .. }
        | WireEvent::ForceStopping { .. }
        | WireEvent::StopFailed { .. } => None,
    }
}

#[cfg(test)]
mod tests;
