//! Synchronous, pull-based boundary contracts for a semantic shell core.
//!
//! The traits use [`Deadline`] values supplied by a [`Clock`]. They neither sleep nor
//! depend on an async runtime, which makes core behavior reproducible under [`TestClock`].

use pf_scene::{AxisMove, Scene, SurfaceMetrics};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::time::Duration;

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
