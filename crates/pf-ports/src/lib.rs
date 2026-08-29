//! Synchronous, pull-based boundary contracts for a semantic shell core.
//!
//! The traits use [`Deadline`] values supplied by a [`Clock`]. They neither sleep nor
//! depend on an async runtime, which makes core behavior reproducible under [`TestClock`].

use pf_scene::{AxisMove, Scene, SurfaceMetrics};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::time::{Duration, SystemTime};

/// Monotonic time in nanoseconds from an adapter-defined origin.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct MonotonicTime(u64);

impl MonotonicTime {
    pub const ZERO: Self = Self(0);

    pub const fn from_nanos(nanos: u64) -> Self {
        Self(nanos)
    }

    pub const fn as_nanos(self) -> u64 {
        self.0
    }

    pub fn saturating_add(self, duration: Duration) -> Self {
        Self(
            self.0
                .saturating_add(duration.as_nanos().min(u128::from(u64::MAX)) as u64),
        )
    }
}

/// An absolute monotonic deadline.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Deadline(pub MonotonicTime);

/// Provides monotonic time and constructs absolute deadlines.
///
/// Responsibility: be the only time source used by deadline-driven core logic.
/// Forbidden leakage: wall-clock/calendar time, sleeps, timers tied to an async runtime,
/// or product scheduling policy.
pub trait Clock {
    fn now(&self) -> MonotonicTime;

    fn deadline_after(&self, duration: Duration) -> Deadline {
        Deadline(self.now().saturating_add(duration))
    }
}

#[derive(Clone, Debug, Default)]
pub struct TestClock {
    now: MonotonicTime,
}

impl TestClock {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn advance(&mut self, duration: Duration) {
        self.now = self.now.saturating_add(duration);
    }
}

impl Clock for TestClock {
    fn now(&self) -> MonotonicTime {
        self.now
    }
}

/// Device-independent input understood by shell logic.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShellAction {
    Move(AxisMove),
    Activate,
    Back,
    Custom(String),
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct InputSourceId(pub String);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActionEvent {
    Action(ShellAction),
    ActiveSourceChanged(Option<InputSourceId>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActionPoll {
    Event(ActionEvent),
    DeadlineReached,
    Closed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActionSourceError {
    Unavailable,
    CorruptSequence,
}

/// Delivers ordered semantic actions and active-input-source changes.
///
/// Responsibility: translate adapter input into ordered [`ActionEvent`] values and obey
/// monotonic deadlines. Forbidden leakage: evdev events, SDL values, file descriptors,
/// scan codes, or physical button-letter names in this trait's signatures.
pub trait ActionSource {
    fn next_action(&mut self, deadline: Deadline) -> Result<ActionPoll, ActionSourceError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectiveBinding {
    pub binding_id: String,
    pub printed_label: String,
    pub source_fallback: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GlyphResult {
    Resolved(EffectiveBinding),
    UnsupportedAction,
    NoActiveSource,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GlyphError {
    ResolverUnavailable,
    InvalidBinding,
}

/// Resolves a semantic action to its effective binding, printed label, and source fallback.
///
/// Responsibility: expose user-facing binding meaning for the active source. Forbidden
/// leakage: renderer glyph handles, font objects, raw input codes, or binding policy changes.
pub trait GlyphResolver {
    fn resolve(&self, action: &ShellAction) -> Result<GlyphResult, GlyphError>;
}

pub type CatalogRevision = u64;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogItem {
    pub id: String,
    pub title: String,
    pub favorite: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogSnapshot {
    pub revision: CatalogRevision,
    pub items: Vec<CatalogItem>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProviderResult<T> {
    Ready(T),
    Pending,
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FavoriteCommitResult {
    Committed(CatalogRevision),
    RevisionConflict { current: CatalogRevision },
    ItemNotFound,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CatalogError {
    ProviderFailure,
    InvalidSnapshot,
}

/// Supplies immutable, revisioned catalog snapshots and revision-producing favorite commits.
///
/// Responsibility: preserve snapshot consistency and return typed provider outcomes.
/// Forbidden leakage: database handles, network clients, mutable snapshot references,
/// provider-specific payloads, route selection, sorting policy, or customer copy.
pub trait CatalogPort {
    fn snapshot(&self) -> Result<ProviderResult<CatalogSnapshot>, CatalogError>;
    fn set_favorite(
        &mut self,
        item_id: &str,
        favorite: bool,
        expected_revision: CatalogRevision,
    ) -> Result<FavoriteCommitResult, CatalogError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LaunchRequest {
    pub item_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LaunchResult {
    Accepted { session_id: String },
    RejectedBusy,
    ItemUnavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ObservedSessionState {
    Starting,
    Running,
    Suspended,
    ObservationComplete,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TerminalReceipt {
    Returned { session_id: String },
    ForcedClose { session_id: String },
    Crash { session_id: String, summary: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryRequired {
    pub session_id: String,
    pub reason: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionEvent {
    Observed(ObservedSessionState),
    Terminal(TerminalReceipt),
    RecoveryRequired(RecoveryRequired),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionPoll {
    Event(SessionEvent),
    DeadlineReached,
    Idle,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionError {
    BackendUnavailable,
    ReceiptBeforeObservationComplete,
    InvalidLifecycle,
}

/// Submits launches and exposes observed lifecycle/history with typed terminal receipts.
///
/// Responsibility: publish `Returned`, `ForcedClose`, or `Crash` only after observation
/// completion, and report [`RecoveryRequired`] separately. Forbidden leakage: process IDs,
/// signals, child-process handles, backend IPC types, launch-placement policy, or UI routes.
pub trait SessionPort {
    fn launch(&mut self, request: LaunchRequest) -> Result<LaunchResult, SessionError>;
    fn next_event(&mut self, deadline: Deadline) -> Result<SessionPoll, SessionError>;
    fn history(&self) -> &[SessionEvent];
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct PreferenceKey(pub String);

#[derive(Clone, Debug, PartialEq)]
pub enum PreferenceValue {
    Bool(bool),
    Integer(i64),
    Text(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct EffectivePreference {
    pub key: PreferenceKey,
    pub effective: PreferenceValue,
    pub stored: PreferenceValue,
    pub applied: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangeAuthority(pub String);

#[derive(Clone, Debug, PartialEq)]
pub struct PreferenceChange {
    pub key: PreferenceKey,
    pub value: PreferenceValue,
    pub authority: ChangeAuthority,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PreferenceChangeResult {
    Accepted,
    UnsupportedKey,
    Unauthorized,
    StoredNotApplied,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PreferencePoll {
    Changed(EffectivePreference),
    DeadlineReached,
    Closed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PreferenceError {
    BackendUnavailable,
    InvalidValue,
}

/// Reads/subscribes to effective supported preferences and submits authority-scoped changes.
///
/// Responsibility: distinguish applied effective state from merely stored state per key.
/// Forbidden leakage: preference-file paths, storage schemas, hardware control handles,
/// unsupported keys presented as effective, or implicit authority escalation.
pub trait PreferencePort {
    fn read(&self, key: &PreferenceKey) -> Result<Option<EffectivePreference>, PreferenceError>;
    fn next_change(&mut self, deadline: Deadline) -> Result<PreferencePoll, PreferenceError>;
    fn submit_change(
        &mut self,
        change: PreferenceChange,
    ) -> Result<PreferenceChangeResult, PreferenceError>;
}

// System-control ports intentionally report the state an adapter actually applied. A
// successful call is never, by itself, evidence that the requested value became effective.

/// Power control honesty contract.
///
/// [`PowerPort::capabilities`] is authoritative for each action. Requests return a typed
/// acceptance or refusal, and idle-policy writes return both requested and applied state.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PowerAction {
    PowerOff,
    Restart,
    Sleep,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Support {
    Supported,
    Unsupported,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PowerCapability {
    pub action: PowerAction,
    pub support: Support,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PowerRequestResult {
    Accepted,
    Unsupported,
    Refused { reason: String },
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IdlePolicy {
    pub sleep_after: Option<Duration>,
    pub power_off_after: Option<Duration>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppliedIdlePolicy {
    pub requested: IdlePolicy,
    pub applied: IdlePolicy,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PowerError {
    BackendUnavailable,
    InvalidPolicy,
}

pub trait PowerPort {
    fn capabilities(&self) -> Result<Vec<PowerCapability>, PowerError>;
    fn request(&mut self, action: PowerAction) -> Result<PowerRequestResult, PowerError>;
    fn idle_policy(&self) -> Result<IdlePolicy, PowerError>;
    fn set_idle_policy(&mut self, policy: IdlePolicy) -> Result<AppliedIdlePolicy, PowerError>;
}

/// Wall-clock control honesty contract.
///
/// Setters return requested and applied values. Manual time changes are separately gated by
/// [`TimeCapabilities::manual_set_time`] and unsupported NTP is distinct from inactive NTP.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NtpState {
    Supported,
    Active,
    Inactive,
    Unsupported,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimeCapabilities {
    pub manual_set_time: Support,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimeState {
    pub wall_clock: SystemTime,
    pub timezone: String,
    pub ntp_state: NtpState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppliedValue<T> {
    pub requested: T,
    pub applied: T,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TimeError {
    BackendUnavailable,
    InvalidTimezone,
    InvalidTime,
    Unsupported,
}

pub trait TimePort {
    fn capabilities(&self) -> Result<TimeCapabilities, TimeError>;
    fn read(&self) -> Result<TimeState, TimeError>;
    fn set_timezone(&mut self, timezone: String) -> Result<AppliedValue<String>, TimeError>;
    fn set_ntp_enabled(&mut self, enabled: bool) -> Result<AppliedValue<bool>, TimeError>;
    fn set_time(&mut self, wall_clock: SystemTime) -> Result<AppliedValue<SystemTime>, TimeError>;
}

/// WiFi control honesty contract.
///
/// State and scans are observations, while mutations report typed outcomes. Credentials are
/// opaque and their debug representation is always redacted. Bluetooth is intentionally not
/// represented by this WiFi-first boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WifiSecurity {
    Open,
    Personal,
    Enterprise,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WifiNetwork {
    pub ssid: String,
    pub security: WifiSecurity,
    pub strength: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NetworkState {
    pub interface_present: bool,
    pub enabled: bool,
    pub connected_ssid: Option<String>,
    pub signal: Option<u8>,
}

#[derive(Clone, Eq, PartialEq)]
pub struct WifiCredential(Vec<u8>);

impl WifiCredential {
    pub fn new(secret: impl Into<Vec<u8>>) -> Self {
        Self(secret.into())
    }

    pub fn expose_secret(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for WifiCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WifiCredential([REDACTED])")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectProgress {
    Authenticating,
    Associating,
    ObtainingAddress,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConnectResult {
    Progress(ConnectProgress),
    Connected { ssid: String },
    Refused,
    NetworkNotFound,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppliedNetworkEnabled {
    pub requested: bool,
    pub applied: NetworkState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NetworkError {
    BackendUnavailable,
    InterfaceAbsent,
    ScanFailed,
    InvalidCredential,
}

pub trait NetworkPort {
    fn state(&self) -> Result<NetworkState, NetworkError>;
    fn scan(&mut self) -> Result<Vec<WifiNetwork>, NetworkError>;
    fn connect(
        &mut self,
        ssid: &str,
        credential: WifiCredential,
    ) -> Result<ConnectResult, NetworkError>;
    fn forget(&mut self, ssid: &str) -> Result<bool, NetworkError>;
    fn set_enabled(&mut self, enabled: bool) -> Result<AppliedNetworkEnabled, NetworkError>;
}

/// File-transfer service honesty contract.
///
/// Service inventory includes unsupported services for honest display. Mutations return the
/// requested value, the applied service state, and any operational warning.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TransferService {
    Sftp,
    UsbMassStorage,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransferServiceState {
    pub service: TransferService,
    pub support: Support,
    pub enabled: bool,
    pub endpoint_info: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransferWarning {
    ExclusiveStorageAccessRequired,
    Message(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppliedTransferState {
    pub requested: bool,
    pub applied: TransferServiceState,
    pub warning: Option<TransferWarning>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransferError {
    BackendUnavailable,
    UnknownService,
    Refused,
}

pub trait TransferPort {
    fn services(&self) -> Result<Vec<TransferServiceState>, TransferError>;
    fn set_enabled(
        &mut self,
        service: TransferService,
        enabled: bool,
    ) -> Result<AppliedTransferState, TransferError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PresentAck {
    pub sequence: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PresentFailure {
    SurfaceLost,
    Rejected,
    Backend(String),
}

pub type PresentResult = Result<PresentAck, PresentFailure>;

/// Presents semantic scenes to a surface and reports acknowledgement or failure.
///
/// Responsibility: expose current [`SurfaceMetrics`] and acknowledge each presentation.
/// Forbidden leakage: rendering policy, focus policy, routes, renderer/device handles,
/// framebuffer details, theme resolution, or ownership of scene mutation.
pub trait FrameHost {
    fn metrics(&self) -> SurfaceMetrics;
    fn present(&mut self, scene: &Scene) -> PresentResult;
}

/// Script item for [`FakeActionSource`].
#[derive(Clone, Debug)]
pub enum ScriptedAction {
    At(MonotonicTime, ActionEvent),
    Error(ActionSourceError),
    Close,
}

pub struct FakeActionSource<C> {
    clock: C,
    script: VecDeque<ScriptedAction>,
}

impl<C> FakeActionSource<C> {
    pub fn new(clock: C, script: impl IntoIterator<Item = ScriptedAction>) -> Self {
        Self {
            clock,
            script: script.into_iter().collect(),
        }
    }

    /// Returns the fake's clock so a test can advance time explicitly.
    pub fn clock_mut(&mut self) -> &mut C {
        &mut self.clock
    }
}

impl<C: Clock> ActionSource for FakeActionSource<C> {
    fn next_action(&mut self, deadline: Deadline) -> Result<ActionPoll, ActionSourceError> {
        match self.script.front() {
            Some(ScriptedAction::At(at, _)) if *at > self.clock.now() || *at > deadline.0 => {
                Ok(ActionPoll::DeadlineReached)
            }
            Some(_) => match self.script.pop_front().expect("front existed") {
                ScriptedAction::At(_, event) => Ok(ActionPoll::Event(event)),
                ScriptedAction::Error(error) => Err(error),
                ScriptedAction::Close => Ok(ActionPoll::Closed),
            },
            None => Ok(ActionPoll::Closed),
        }
    }
}

#[derive(Clone, Debug)]
pub struct FakeGlyphResolver {
    pub result: Result<GlyphResult, GlyphError>,
}

impl GlyphResolver for FakeGlyphResolver {
    fn resolve(&self, _action: &ShellAction) -> Result<GlyphResult, GlyphError> {
        self.result.clone()
    }
}

#[derive(Clone, Debug)]
pub struct FakeCatalog {
    state: Result<ProviderResult<CatalogSnapshot>, CatalogError>,
}

impl FakeCatalog {
    pub fn fixture(snapshot: CatalogSnapshot) -> Self {
        Self {
            state: Ok(ProviderResult::Ready(snapshot)),
        }
    }

    pub fn returning(state: Result<ProviderResult<CatalogSnapshot>, CatalogError>) -> Self {
        Self { state }
    }
}

impl CatalogPort for FakeCatalog {
    fn snapshot(&self) -> Result<ProviderResult<CatalogSnapshot>, CatalogError> {
        self.state.clone()
    }

    fn set_favorite(
        &mut self,
        item_id: &str,
        favorite: bool,
        expected_revision: u64,
    ) -> Result<FavoriteCommitResult, CatalogError> {
        let ProviderResult::Ready(snapshot) = self.state.as_mut().map_err(|error| error.clone())?
        else {
            return Err(CatalogError::ProviderFailure);
        };
        if snapshot.revision != expected_revision {
            return Ok(FavoriteCommitResult::RevisionConflict {
                current: snapshot.revision,
            });
        }
        let Some(item) = snapshot.items.iter_mut().find(|item| item.id == item_id) else {
            return Ok(FavoriteCommitResult::ItemNotFound);
        };
        item.favorite = favorite;
        snapshot.revision += 1;
        Ok(FavoriteCommitResult::Committed(snapshot.revision))
    }
}

#[derive(Clone, Debug)]
pub enum ScriptedSession {
    Event(SessionEvent),
    Deadline,
    Idle,
    Error(SessionError),
}

pub struct FakeSession {
    launch_result: Result<LaunchResult, SessionError>,
    script: VecDeque<ScriptedSession>,
    history: Vec<SessionEvent>,
    observation_complete: bool,
}

impl FakeSession {
    pub fn new(
        launch_result: Result<LaunchResult, SessionError>,
        script: impl IntoIterator<Item = ScriptedSession>,
    ) -> Self {
        Self {
            launch_result,
            script: script.into_iter().collect(),
            history: Vec::new(),
            observation_complete: false,
        }
    }
}

impl SessionPort for FakeSession {
    fn launch(&mut self, _request: LaunchRequest) -> Result<LaunchResult, SessionError> {
        self.observation_complete = false;
        self.launch_result.clone()
    }

    fn next_event(&mut self, _deadline: Deadline) -> Result<SessionPoll, SessionError> {
        match self.script.pop_front().unwrap_or(ScriptedSession::Idle) {
            ScriptedSession::Event(SessionEvent::Terminal(_)) if !self.observation_complete => {
                Err(SessionError::ReceiptBeforeObservationComplete)
            }
            ScriptedSession::Event(event) => {
                if event == SessionEvent::Observed(ObservedSessionState::ObservationComplete) {
                    self.observation_complete = true;
                }
                self.history.push(event.clone());
                Ok(SessionPoll::Event(event))
            }
            ScriptedSession::Deadline => Ok(SessionPoll::DeadlineReached),
            ScriptedSession::Idle => Ok(SessionPoll::Idle),
            ScriptedSession::Error(error) => Err(error),
        }
    }

    fn history(&self) -> &[SessionEvent] {
        &self.history
    }
}

pub struct FakePreferencePort {
    values: HashMap<PreferenceKey, EffectivePreference>,
    changes: VecDeque<Result<PreferencePoll, PreferenceError>>,
    allowed_authority: ChangeAuthority,
}

impl FakePreferencePort {
    pub fn new(
        values: impl IntoIterator<Item = EffectivePreference>,
        allowed_authority: ChangeAuthority,
    ) -> Self {
        Self {
            values: values
                .into_iter()
                .map(|value| (value.key.clone(), value))
                .collect(),
            changes: VecDeque::new(),
            allowed_authority,
        }
    }

    pub fn script_poll(&mut self, poll: Result<PreferencePoll, PreferenceError>) {
        self.changes.push_back(poll);
    }
}

impl PreferencePort for FakePreferencePort {
    fn read(&self, key: &PreferenceKey) -> Result<Option<EffectivePreference>, PreferenceError> {
        Ok(self.values.get(key).cloned())
    }

    fn next_change(&mut self, _deadline: Deadline) -> Result<PreferencePoll, PreferenceError> {
        self.changes
            .pop_front()
            .unwrap_or(Ok(PreferencePoll::DeadlineReached))
    }

    fn submit_change(
        &mut self,
        change: PreferenceChange,
    ) -> Result<PreferenceChangeResult, PreferenceError> {
        if change.authority != self.allowed_authority {
            return Ok(PreferenceChangeResult::Unauthorized);
        }
        let Some(current) = self.values.get_mut(&change.key) else {
            return Ok(PreferenceChangeResult::UnsupportedKey);
        };
        current.stored = change.value.clone();
        if !current.applied {
            return Ok(PreferenceChangeResult::StoredNotApplied);
        }
        current.effective = change.value;
        self.changes
            .push_back(Ok(PreferencePoll::Changed(current.clone())));
        Ok(PreferenceChangeResult::Accepted)
    }
}

pub struct FakePowerPort {
    pub capabilities_result: Result<Vec<PowerCapability>, PowerError>,
    pub idle_policy_result: Result<IdlePolicy, PowerError>,
    requests: VecDeque<Result<PowerRequestResult, PowerError>>,
    policy_writes: VecDeque<Result<AppliedIdlePolicy, PowerError>>,
}

impl FakePowerPort {
    pub fn new(capabilities: Vec<PowerCapability>, idle_policy: IdlePolicy) -> Self {
        Self {
            capabilities_result: Ok(capabilities),
            idle_policy_result: Ok(idle_policy),
            requests: VecDeque::new(),
            policy_writes: VecDeque::new(),
        }
    }

    pub fn script_request(&mut self, result: Result<PowerRequestResult, PowerError>) {
        self.requests.push_back(result);
    }

    pub fn script_policy_write(&mut self, result: Result<AppliedIdlePolicy, PowerError>) {
        self.policy_writes.push_back(result);
    }
}

impl PowerPort for FakePowerPort {
    fn capabilities(&self) -> Result<Vec<PowerCapability>, PowerError> {
        self.capabilities_result.clone()
    }

    fn request(&mut self, action: PowerAction) -> Result<PowerRequestResult, PowerError> {
        self.requests.pop_front().unwrap_or_else(|| {
            Ok(
                if self.capabilities_result.as_ref().is_ok_and(|items| {
                    items
                        .iter()
                        .any(|item| item.action == action && item.support == Support::Supported)
                }) {
                    PowerRequestResult::Accepted
                } else {
                    PowerRequestResult::Unsupported
                },
            )
        })
    }

    fn idle_policy(&self) -> Result<IdlePolicy, PowerError> {
        self.idle_policy_result.clone()
    }

    fn set_idle_policy(&mut self, policy: IdlePolicy) -> Result<AppliedIdlePolicy, PowerError> {
        let result = self.policy_writes.pop_front().unwrap_or_else(|| {
            Ok(AppliedIdlePolicy {
                requested: policy.clone(),
                applied: policy,
            })
        })?;
        self.idle_policy_result = Ok(result.applied.clone());
        Ok(result)
    }
}

pub struct FakeTimePort {
    pub capabilities_result: Result<TimeCapabilities, TimeError>,
    pub state_result: Result<TimeState, TimeError>,
    timezone_writes: VecDeque<Result<AppliedValue<String>, TimeError>>,
    ntp_writes: VecDeque<Result<AppliedValue<bool>, TimeError>>,
    time_writes: VecDeque<Result<AppliedValue<SystemTime>, TimeError>>,
}

impl FakeTimePort {
    pub fn new(capabilities: TimeCapabilities, state: TimeState) -> Self {
        Self {
            capabilities_result: Ok(capabilities),
            state_result: Ok(state),
            timezone_writes: VecDeque::new(),
            ntp_writes: VecDeque::new(),
            time_writes: VecDeque::new(),
        }
    }

    pub fn script_timezone(&mut self, result: Result<AppliedValue<String>, TimeError>) {
        self.timezone_writes.push_back(result);
    }

    pub fn script_ntp(&mut self, result: Result<AppliedValue<bool>, TimeError>) {
        self.ntp_writes.push_back(result);
    }

    pub fn script_time(&mut self, result: Result<AppliedValue<SystemTime>, TimeError>) {
        self.time_writes.push_back(result);
    }
}

impl TimePort for FakeTimePort {
    fn capabilities(&self) -> Result<TimeCapabilities, TimeError> {
        self.capabilities_result.clone()
    }

    fn read(&self) -> Result<TimeState, TimeError> {
        self.state_result.clone()
    }

    fn set_timezone(&mut self, timezone: String) -> Result<AppliedValue<String>, TimeError> {
        let result = self.timezone_writes.pop_front().unwrap_or_else(|| {
            Ok(AppliedValue {
                requested: timezone.clone(),
                applied: timezone,
            })
        })?;
        if let Ok(state) = &mut self.state_result {
            state.timezone = result.applied.clone();
        }
        Ok(result)
    }

    fn set_ntp_enabled(&mut self, enabled: bool) -> Result<AppliedValue<bool>, TimeError> {
        let result = if let Some(result) = self.ntp_writes.pop_front() {
            result
        } else {
            if self
                .state_result
                .as_ref()
                .is_ok_and(|state| state.ntp_state == NtpState::Unsupported)
            {
                return Err(TimeError::Unsupported);
            }
            Ok(AppliedValue {
                requested: enabled,
                applied: enabled,
            })
        }?;
        if let Ok(state) = &mut self.state_result {
            state.ntp_state = if result.applied {
                NtpState::Active
            } else {
                NtpState::Inactive
            };
        }
        Ok(result)
    }

    fn set_time(&mut self, wall_clock: SystemTime) -> Result<AppliedValue<SystemTime>, TimeError> {
        if self
            .capabilities_result
            .as_ref()
            .is_ok_and(|capabilities| capabilities.manual_set_time == Support::Unsupported)
        {
            return Err(TimeError::Unsupported);
        }
        let result = self.time_writes.pop_front().unwrap_or({
            Ok(AppliedValue {
                requested: wall_clock,
                applied: wall_clock,
            })
        })?;
        if let Ok(state) = &mut self.state_result {
            state.wall_clock = result.applied;
        }
        Ok(result)
    }
}

pub struct FakeNetworkPort {
    pub state_result: Result<NetworkState, NetworkError>,
    scans: VecDeque<Result<Vec<WifiNetwork>, NetworkError>>,
    connections: VecDeque<Result<ConnectResult, NetworkError>>,
    forgets: VecDeque<Result<bool, NetworkError>>,
    enabled_writes: VecDeque<Result<AppliedNetworkEnabled, NetworkError>>,
}

impl FakeNetworkPort {
    pub fn new(state: NetworkState) -> Self {
        Self {
            state_result: Ok(state),
            scans: VecDeque::new(),
            connections: VecDeque::new(),
            forgets: VecDeque::new(),
            enabled_writes: VecDeque::new(),
        }
    }

    pub fn script_scan(&mut self, result: Result<Vec<WifiNetwork>, NetworkError>) {
        self.scans.push_back(result);
    }

    pub fn script_connect(&mut self, result: Result<ConnectResult, NetworkError>) {
        self.connections.push_back(result);
    }

    pub fn script_forget(&mut self, result: Result<bool, NetworkError>) {
        self.forgets.push_back(result);
    }

    pub fn script_enabled(&mut self, result: Result<AppliedNetworkEnabled, NetworkError>) {
        self.enabled_writes.push_back(result);
    }
}

impl NetworkPort for FakeNetworkPort {
    fn state(&self) -> Result<NetworkState, NetworkError> {
        self.state_result.clone()
    }

    fn scan(&mut self) -> Result<Vec<WifiNetwork>, NetworkError> {
        self.scans.pop_front().unwrap_or_else(|| Ok(Vec::new()))
    }

    fn connect(
        &mut self,
        ssid: &str,
        _credential: WifiCredential,
    ) -> Result<ConnectResult, NetworkError> {
        let result = self
            .connections
            .pop_front()
            .unwrap_or_else(|| Ok(ConnectResult::Connected { ssid: ssid.into() }))?;
        if let (ConnectResult::Connected { ssid }, Ok(state)) = (&result, &mut self.state_result) {
            state.connected_ssid = Some(ssid.clone());
        }
        Ok(result)
    }

    fn forget(&mut self, ssid: &str) -> Result<bool, NetworkError> {
        let forgotten = self.forgets.pop_front().unwrap_or(Ok(true))?;
        if forgotten {
            if let Ok(state) = &mut self.state_result {
                if state.connected_ssid.as_deref() == Some(ssid) {
                    state.connected_ssid = None;
                    state.signal = None;
                }
            }
        }
        Ok(forgotten)
    }

    fn set_enabled(&mut self, enabled: bool) -> Result<AppliedNetworkEnabled, NetworkError> {
        let result = self.enabled_writes.pop_front().unwrap_or_else(|| {
            let mut applied = self.state_result.clone().unwrap_or(NetworkState {
                interface_present: true,
                enabled,
                connected_ssid: None,
                signal: None,
            });
            applied.enabled = enabled;
            Ok(AppliedNetworkEnabled {
                requested: enabled,
                applied,
            })
        })?;
        self.state_result = Ok(result.applied.clone());
        Ok(result)
    }
}

pub struct FakeTransferPort {
    pub services_result: Result<Vec<TransferServiceState>, TransferError>,
    writes: VecDeque<Result<AppliedTransferState, TransferError>>,
}

impl FakeTransferPort {
    pub fn new(services: Vec<TransferServiceState>) -> Self {
        Self {
            services_result: Ok(services),
            writes: VecDeque::new(),
        }
    }

    pub fn script_enabled(&mut self, result: Result<AppliedTransferState, TransferError>) {
        self.writes.push_back(result);
    }
}

impl TransferPort for FakeTransferPort {
    fn services(&self) -> Result<Vec<TransferServiceState>, TransferError> {
        self.services_result.clone()
    }

    fn set_enabled(
        &mut self,
        service: TransferService,
        enabled: bool,
    ) -> Result<AppliedTransferState, TransferError> {
        let result = if let Some(result) = self.writes.pop_front() {
            result?
        } else {
            let states = self
                .services_result
                .as_ref()
                .map_err(|error| error.clone())?;
            let Some(current) = states.iter().find(|state| state.service == service) else {
                return Err(TransferError::UnknownService);
            };
            if current.support == Support::Unsupported {
                return Err(TransferError::Refused);
            }
            let mut applied = current.clone();
            applied.enabled = enabled;
            AppliedTransferState {
                requested: enabled,
                applied,
                warning: None,
            }
        };
        if let Ok(states) = &mut self.services_result {
            if let Some(state) = states
                .iter_mut()
                .find(|state| state.service == result.applied.service)
            {
                *state = result.applied.clone();
            }
        }
        Ok(result)
    }
}

#[derive(Clone)]
pub struct FakeFrameHost {
    metrics: SurfaceMetrics,
    presented: Vec<Scene>,
    results: VecDeque<PresentResult>,
    next_sequence: u64,
}

impl FakeFrameHost {
    pub fn new(metrics: SurfaceMetrics) -> Self {
        Self {
            metrics,
            presented: Vec::new(),
            results: VecDeque::new(),
            next_sequence: 0,
        }
    }

    pub fn inject(&mut self, result: PresentResult) {
        self.results.push_back(result);
    }

    pub fn presented(&self) -> &[Scene] {
        &self.presented
    }
}

impl FrameHost for FakeFrameHost {
    fn metrics(&self) -> SurfaceMetrics {
        self.metrics
    }

    fn present(&mut self, scene: &Scene) -> PresentResult {
        self.presented.push(scene.clone());
        if let Some(result) = self.results.pop_front() {
            return result;
        }
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        Ok(PresentAck { sequence })
    }
}

impl fmt::Display for PresentFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for PresentFailure {}

#[cfg(test)]
mod tests;
