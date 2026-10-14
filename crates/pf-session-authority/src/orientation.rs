//! Compositor orientation policy.
//!
//! The reducer composes immutable panel calibration, hard screen-only posture, and a
//! whole-body choice. The whole-body source follows Android's user/sensor precedence:
//! an available accelerometer wins while unlocked; otherwise `user_rotation` wins. A lock is
//! active when any `SW_ROTATE_LOCK` is set **or** `auto_rotate` is false. Only whole-body turns
//! remap controls; swivel/posture turns never do.
//!
//! App orientation is compatibility policy, not a physical-output request. In particular an app
//! declaring only `landscape` still fills a native-landscape output when the device is held 180°
//! flipped. Its content therefore remains in native landscape and appears upside-down to that
//! user, matching Android `SCREEN_ORIENTATION_LANDSCAPE` rather than `sensorLandscape`. Apps that
//! want both directions must also declare `landscape_flipped`.

use pf_prefs::Prefs;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

pub const AUTO_ROTATE_KEY: &str = "auto_rotate";
pub const USER_ROTATION_KEY: &str = "user_rotation";
pub const MAX_ORIENTATION_WAIT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum QuarterTurn {
    #[default]
    Normal,
    Left,
    UpsideDown,
    Right,
}

impl QuarterTurn {
    const fn index(self) -> u8 {
        match self {
            Self::Normal => 0,
            Self::Left => 1,
            Self::UpsideDown => 2,
            Self::Right => 3,
        }
    }
    const fn from_index(value: u8) -> Self {
        match value % 4 {
            0 => Self::Normal,
            1 => Self::Left,
            2 => Self::UpsideDown,
            _ => Self::Right,
        }
    }
    pub const fn then(self, next: Self) -> Self {
        Self::from_index(self.index() + next.index())
    }
    const fn distance(self, other: Self) -> u8 {
        let delta = (self.index() + 4 - other.index()) % 4;
        if delta > 2 {
            4 - delta
        } else {
            delta
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogicalOrientation {
    #[default]
    Landscape,
    Portrait,
    LandscapeFlipped,
    PortraitFlipped,
}

impl LogicalOrientation {
    const fn turn(self) -> QuarterTurn {
        match self {
            Self::Landscape => QuarterTurn::Normal,
            Self::Portrait => QuarterTurn::Left,
            Self::LandscapeFlipped => QuarterTurn::UpsideDown,
            Self::PortraitFlipped => QuarterTurn::Right,
        }
    }
    const fn from_turn(turn: QuarterTurn) -> Self {
        match turn {
            QuarterTurn::Normal => Self::Landscape,
            QuarterTurn::Left => Self::Portrait,
            QuarterTurn::UpsideDown => Self::LandscapeFlipped,
            QuarterTurn::Right => Self::PortraitFlipped,
        }
    }
    const fn is_landscape(self) -> bool {
        matches!(self, Self::Landscape | Self::LandscapeFlipped)
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Presentation {
    #[default]
    Fill,
    Letterbox,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SensorObservation {
    Normal,
    Left,
    UpsideDown,
    Right,
    Undefined,
}

/// Why the kernel panel-orientation probe could not supply a trustworthy value.
/// This remains an internal reducer input; orientation RPC v1 exposes only the stable
/// `degraded` reason strings in [`OrientationState`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PanelOrientationFailure {
    Missing,
    Unreadable,
    Invalid,
}

/// Provenance for the fixed panel mount used by the reducer.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PanelOrientationSource {
    #[default]
    DrmConnector,
    DeviceTree,
    DescriptorFallback(PanelOrientationFailure),
    UnknownFallback(PanelOrientationFailure),
}

impl PanelOrientationSource {
    pub const fn is_fallback(self) -> bool {
        matches!(self, Self::DescriptorFallback(_) | Self::UnknownFallback(_))
    }
}

impl SensorObservation {
    fn turn(self) -> Option<QuarterTurn> {
        match self {
            Self::Normal => Some(QuarterTurn::Normal),
            Self::Left => Some(QuarterTurn::Left),
            Self::UpsideDown => Some(QuarterTurn::UpsideDown),
            Self::Right => Some(QuarterTurn::Right),
            Self::Undefined => None,
        }
    }
}

pub type MountMatrix = [[i8; 3]; 3];
pub fn mount_matrices_agree(descriptor: &MountMatrix, kernel: &MountMatrix) -> bool {
    descriptor == kernel
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OrientationInputs {
    pub panel_orientation: QuarterTurn,
    pub panel_orientation_source: PanelOrientationSource,
    pub descriptor_panel_orientation: Option<QuarterTurn>,
    pub screen_posture: QuarterTurn,
    pub accelerometer_present: bool,
    pub accelerometer_mount_matches: bool,
    pub accelerometer: Option<SensorObservation>,
    pub rotate_locks: Vec<bool>,
    pub auto_rotate: bool,
    pub user_rotation: QuarterTurn,
    pub tablet_mode: Option<bool>,
    pub tablet_mode_is_posture: bool,
    pub app_orientations: Vec<LogicalOrientation>,
}

impl Default for OrientationInputs {
    fn default() -> Self {
        Self {
            panel_orientation: QuarterTurn::Normal,
            panel_orientation_source: PanelOrientationSource::DrmConnector,
            descriptor_panel_orientation: None,
            screen_posture: QuarterTurn::Normal,
            accelerometer_present: false,
            accelerometer_mount_matches: true,
            accelerometer: None,
            rotate_locks: Vec::new(),
            auto_rotate: true,
            user_rotation: QuarterTurn::Normal,
            tablet_mode: None,
            tablet_mode_is_posture: false,
            app_orientations: vec![LogicalOrientation::Landscape],
        }
    }
}

impl OrientationInputs {
    pub fn apply_preferences(&mut self, prefs: &Prefs) {
        self.auto_rotate = prefs.auto_rotate();
        self.user_rotation = match prefs.user_rotation() {
            "left" => QuarterTurn::Left,
            "upsidedown" => QuarterTurn::UpsideDown,
            "right" => QuarterTurn::Right,
            _ => QuarterTurn::Normal,
        };
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OrientationState {
    pub serial: u64,
    pub output: QuarterTurn,
    pub logical: LogicalOrientation,
    pub app: LogicalOrientation,
    pub presentation: Presentation,
    pub whole_body: QuarterTurn,
    pub screen_only: QuarterTurn,
    pub locked: bool,
    pub degraded: Vec<String>,
}

impl Default for OrientationState {
    fn default() -> Self {
        Self {
            serial: 1,
            output: QuarterTurn::Normal,
            logical: LogicalOrientation::Landscape,
            app: LogicalOrientation::Landscape,
            presentation: Presentation::Fill,
            whole_body: QuarterTurn::Normal,
            screen_only: QuarterTurn::Normal,
            locked: false,
            degraded: Vec::new(),
        }
    }
}
impl OrientationState {
    pub fn requires_gamepad_remap(&self) -> bool {
        self.whole_body != QuarterTurn::Normal
    }
}

pub struct OrientationPolicy {
    inputs: OrientationInputs,
    state: OrientationState,
    last_sensor: Option<QuarterTurn>,
    prior_app: Option<LogicalOrientation>,
}

/// Daemon-owned bridge from changing input snapshots to the compositor-facing watch.
///
/// Keeping the reducer and watch behind one lock prevents an adapter from publishing a state
/// that was computed from an older input snapshot after a newer one. Adapters submit complete
/// snapshots; the reducer's serial changes only when the semantic output changes.
pub struct OrientationController {
    policy: Mutex<OrientationPolicy>,
    watch: Arc<OrientationWatch>,
}

/// One complete observation pass over the production orientation inputs.
///
/// Implementations retain the prior value for transiently undefined inputs (for example an
/// iio-sensor-proxy `undefined` observation) and return an error only when a trustworthy complete
/// snapshot cannot be formed. The authority performs its first probe before publishing its socket,
/// then re-samples after bind in the isolated input-driver thread.
pub trait OrientationInputSource: Send {
    fn snapshot(
        &mut self,
        active_app: Option<&str>,
        previous: &OrientationInputs,
    ) -> Result<OrientationInputs, String>;
}

pub type ActiveApp = Arc<Mutex<Option<String>>>;

/// Sample a source and atomically publish its reduced result. The daemon calls this once before its
/// socket is bound; later calls run after bind in the input-driver thread.
pub fn refresh_from_source(
    source: &mut dyn OrientationInputSource,
    controller: &OrientationController,
    active_app: &ActiveApp,
    inputs: &mut OrientationInputs,
) -> Result<OrientationState, String> {
    let active_app = active_app
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let next = source.snapshot(active_app.as_deref(), inputs)?;
    *inputs = next.clone();
    Ok(controller.update(next))
}

/// Run potentially blocking DRM/evdev/D-Bus/preference probes outside the lifecycle authority.
/// A slow source can delay only the next orientation observation, never SafeReturn or a deadline.
pub struct OrientationDriver {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Drop for OrientationDriver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn spawn_input_driver(
    mut source: impl OrientationInputSource + 'static,
    controller: Arc<OrientationController>,
    active_app: ActiveApp,
    mut inputs: OrientationInputs,
    interval: Duration,
) -> OrientationDriver {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let thread = thread::spawn(move || loop {
        thread::sleep(interval.max(Duration::from_millis(10)));
        if thread_stop.load(Ordering::Acquire) {
            break;
        }
        if let Err(error) = refresh_from_source(&mut source, &controller, &active_app, &mut inputs)
        {
            eprintln!("pf-session-authorityd: orientation_source_error detail={error:?}");
        }
    });
    OrientationDriver {
        stop,
        thread: Some(thread),
    }
}

impl OrientationController {
    pub fn new(inputs: OrientationInputs, max_waiters: usize) -> Self {
        let policy = OrientationPolicy::new(inputs);
        let watch = Arc::new(OrientationWatch::new(policy.state().clone(), max_waiters));
        Self {
            policy: Mutex::new(policy),
            watch,
        }
    }

    pub fn watch(&self) -> Arc<OrientationWatch> {
        Arc::clone(&self.watch)
    }

    pub fn update(&self, inputs: OrientationInputs) -> OrientationState {
        let state = self
            .policy
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .update(inputs)
            .clone();
        if self.watch.snapshot() != state {
            self.watch.publish(state.clone());
        }
        state
    }
}

impl OrientationPolicy {
    pub fn new(inputs: OrientationInputs) -> Self {
        let mut policy = Self {
            inputs,
            state: OrientationState::default(),
            last_sensor: None,
            prior_app: None,
        };
        policy.recompute(false);
        policy
    }
    pub fn state(&self) -> &OrientationState {
        &self.state
    }
    pub fn update(&mut self, inputs: OrientationInputs) -> &OrientationState {
        self.inputs = inputs;
        self.recompute(true);
        &self.state
    }
    fn recompute(&mut self, bump_serial: bool) {
        if let Some(turn) = self.inputs.accelerometer.and_then(SensorObservation::turn) {
            self.last_sensor = Some(turn);
        }
        let locked = !self.inputs.auto_rotate || self.inputs.rotate_locks.iter().any(|set| *set);
        let tablet_allows_sensor =
            self.inputs.tablet_mode_is_posture || self.inputs.tablet_mode.unwrap_or(true);
        let sensor_eligible = self.inputs.accelerometer_present
            && self.inputs.accelerometer_mount_matches
            && tablet_allows_sensor
            && !locked;
        let whole_body = if sensor_eligible {
            self.last_sensor.unwrap_or(self.inputs.user_rotation)
        } else {
            self.inputs.user_rotation
        };
        let output = self
            .inputs
            .panel_orientation
            .then(self.inputs.screen_posture)
            .then(whole_body);
        let logical = LogicalOrientation::from_turn(output);
        let default_allowed = [LogicalOrientation::Landscape];
        let allowed = if self.inputs.app_orientations.is_empty() {
            &default_allowed[..]
        } else {
            self.inputs.app_orientations.as_slice()
        };
        let app = choose_app(logical, self.prior_app, allowed);
        let presentation = if logical.is_landscape() == app.is_landscape() {
            Presentation::Fill
        } else {
            Presentation::Letterbox
        };
        let mut degraded = Vec::new();
        if self
            .inputs
            .descriptor_panel_orientation
            .is_some_and(|expected| expected != self.inputs.panel_orientation)
        {
            degraded.push("panel_orientation_mismatch".to_owned());
        }
        if self.inputs.panel_orientation_source.is_fallback() {
            degraded.push("panel_orientation_fallback".to_owned());
        }
        if self.inputs.accelerometer_present && !self.inputs.accelerometer_mount_matches {
            degraded.push("accelerometer_mount_mismatch".to_owned());
        }
        if self.inputs.accelerometer_present && self.last_sensor.is_none() {
            degraded.push("accelerometer_unavailable".to_owned());
        }
        let serial = self.state.serial;
        let next = OrientationState {
            serial,
            output,
            logical,
            app,
            presentation,
            whole_body,
            screen_only: self.inputs.screen_posture,
            locked,
            degraded,
        };
        if bump_serial && next != self.state {
            self.state = OrientationState {
                serial: serial.saturating_add(1),
                ..next
            };
        } else {
            self.state = next;
        }
        self.prior_app = Some(app);
    }
}

fn choose_app(
    logical: LogicalOrientation,
    prior: Option<LogicalOrientation>,
    allowed: &[LogicalOrientation],
) -> LogicalOrientation {
    if allowed.contains(&logical) {
        return logical;
    }
    let best = allowed
        .iter()
        .map(|candidate| logical.turn().distance(candidate.turn()))
        .min()
        .unwrap_or(0);
    if let Some(prior) = prior {
        if allowed.contains(&prior) && logical.turn().distance(prior.turn()) == best {
            return prior;
        }
    }
    allowed
        .iter()
        .copied()
        .find(|candidate| logical.turn().distance(candidate.turn()) == best)
        .unwrap_or(LogicalOrientation::Landscape)
}

pub struct HallDebouncer {
    current: QuarterTurn,
    candidate: Option<(QuarterTurn, u64)>,
    debounce_ms: u64,
}
impl HallDebouncer {
    pub fn new(initial: QuarterTurn, debounce_ms: u64) -> Self {
        Self {
            current: initial,
            candidate: None,
            debounce_ms,
        }
    }
    pub fn current(&self) -> QuarterTurn {
        self.current
    }
    pub fn observe(&mut self, value: QuarterTurn, now_ms: u64) {
        if value == self.current {
            self.candidate = None;
        } else if self.candidate.map(|(candidate, _)| candidate) != Some(value) {
            self.candidate = Some((value, now_ms));
        }
    }
    pub fn advance(&mut self, now_ms: u64) -> QuarterTurn {
        if let Some((candidate, since)) = self.candidate {
            if now_ms.saturating_sub(since) >= self.debounce_ms {
                self.current = candidate;
                self.candidate = None;
            }
        }
        self.current
    }
    pub fn resync(&mut self, value: QuarterTurn) {
        self.current = value;
        self.candidate = None;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitError {
    TooManyWaiters,
    Poisoned,
}

pub struct OrientationWatch {
    state: Mutex<OrientationState>,
    changed: Condvar,
    active: AtomicUsize,
    max_waiters: usize,
}
impl OrientationWatch {
    pub fn new(state: OrientationState, max_waiters: usize) -> Self {
        Self {
            state: Mutex::new(state),
            changed: Condvar::new(),
            active: AtomicUsize::new(0),
            max_waiters,
        }
    }
    pub fn snapshot(&self) -> OrientationState {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
    pub fn publish(&self, state: OrientationState) {
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = state;
        self.changed.notify_all();
    }
    pub fn active_waiters(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }

    pub fn max_waiters(&self) -> usize {
        self.max_waiters
    }
    pub fn wait_after(
        &self,
        after_serial: u64,
        timeout: Duration,
    ) -> Result<OrientationState, WaitError> {
        self.active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < self.max_waiters).then_some(active + 1)
            })
            .map_err(|_| WaitError::TooManyWaiters)?;
        let _permit = WaiterPermit(&self.active);
        let state = self.state.lock().map_err(|_| WaitError::Poisoned)?;
        let (state, _) = self
            .changed
            .wait_timeout_while(state, timeout.min(MAX_ORIENTATION_WAIT), |state| {
                state.serial == after_serial
            })
            .map_err(|_| WaitError::Poisoned)?;
        Ok(state.clone())
    }
}
struct WaiterPermit<'a>(&'a AtomicUsize);
impl Drop for WaiterPermit<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
