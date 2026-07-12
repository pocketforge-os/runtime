//! The **v0 in-process backend** — direct, descriptor-derived capability arbitration with no
//! socket and no daemon. It is the Rust port of the E5 sim's `sim/control/broker_stub.py`:
//! the four-way taxonomy, descriptor-derived presence (zero per-device code), the unified
//! rumble no-op shape, the default-deny privacy tier, and the accessibility-preference seam.
//!
//! HONESTY (R-A): this is a **cooperative** facade. Linked into an app running as `gamer` with
//! the `input`/`video`/`render` groups it holds ambient `/dev/*` authority regardless — it
//! proves the *contract + ergonomics + graceful missing-hardware degradation*, NOT confinement.
//! Real enforcement is the out-of-process broker (`.3`) on the Phase-2 substrate; INPUT is the
//! one v0-enforceable cap (`uinput`+`EVIOCGRAB`, `.6`).

use std::collections::HashMap;
use std::io::Write as _;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use pf_prefs::{PrefValue, Prefs, PrefsStore, SCHEMA};

use crate::backend::{acquire_decision, query_decision, Backend, Pose, PoseDelta, RumbleStatus};
use crate::descriptor::Descriptor;
use crate::error::{CapError, PermissionState};

/// Mutable cooperative state behind the backend.
#[derive(Default)]
struct State {
    /// Cooperatively stored capability values (`set_capability`/`get_capability`).
    stored: HashMap<String, Vec<u8>>,
    /// The in-memory cache of the accessibility/user preference document (E4). Backed by the
    /// persistent [`PrefsStore`] when one is attached (`with_store`); otherwise a pure in-memory
    /// document seeded from schema defaults (today's store-less behavior). The per-capability
    /// primitives read this AT the point of actuation (the E4 enforcement point).
    prefs: Prefs,
    /// Explicit consent decisions (the E3 seam) overlaid on the default policy.
    consent: HashMap<String, PermissionState>,
    /// Latest injected IMU pose (the `.4` physical model replaces this with integration).
    pose: Pose,
    /// query() change-event subscribers, keyed by capability name.
    subscribers: HashMap<String, Vec<Sender<PermissionState>>>,
    /// `PrefsDidChange` subscribers, keyed by preference key (mirrors `subscribers` above).
    pref_subscribers: HashMap<String, Vec<Sender<PrefValue>>>,
}

/// The in-process v0 capability backend. Cheap to clone-share via [`InProcessBackend::shared`].
pub struct InProcessBackend {
    descriptor: Arc<Descriptor>,
    /// The persistent preference store, when attached. `None` => a store-less backend whose
    /// preferences live only in memory for the session (the shape every pre-E4.2 test uses, kept
    /// so those tests stay hermetic — they never touch the ambient filesystem). Production wiring
    /// ([`crate::connect`]) attaches [`PrefsStore::open_default`] so a flip persists.
    prefs_store: Option<Arc<PrefsStore>>,
    state: Mutex<State>,
}

impl InProcessBackend {
    /// Build a store-less backend over a descriptor (preferences are in-memory for the session).
    pub fn new(descriptor: Arc<Descriptor>) -> InProcessBackend {
        InProcessBackend { descriptor, prefs_store: None, state: Mutex::new(State::default()) }
    }

    /// Build a shared (`Arc`) store-less backend — the form the reference [`crate::server`] wraps.
    pub fn shared(descriptor: Arc<Descriptor>) -> Arc<InProcessBackend> {
        Arc::new(InProcessBackend::new(descriptor))
    }

    /// Build a **store-backed** backend: the preference document is loaded from `store` at init
    /// (tolerant — a missing file yields all-defaults) and every control-plane write persists back
    /// through the store's persist-and-signal seam. External writes (the `pf-settings` CLI in a
    /// separate process) become visible + observable via [`InProcessBackend::reload_prefs`].
    pub fn with_store(descriptor: Arc<Descriptor>, store: Arc<PrefsStore>) -> InProcessBackend {
        // Tolerant load: a bad/corrupt store must not sink the session — fall back to defaults and
        // report on stderr (the CLI is the surface that shows a typed load error).
        let prefs = store.load().unwrap_or_else(|e| {
            let _ = writeln!(std::io::stderr(), "pocketforge: prefs load failed ({e}); using defaults");
            Prefs::defaults()
        });
        InProcessBackend {
            descriptor,
            prefs_store: Some(store),
            state: Mutex::new(State { prefs, ..State::default() }),
        }
    }

    /// Build a shared store-backed backend.
    pub fn shared_with_store(
        descriptor: Arc<Descriptor>,
        store: Arc<PrefsStore>,
    ) -> Arc<InProcessBackend> {
        Arc::new(InProcessBackend::with_store(descriptor, store))
    }

    /// The descriptor this backend arbitrates over.
    pub fn descriptor(&self) -> &Arc<Descriptor> {
        &self.descriptor
    }

    /// Record an explicit consent decision (the E3 control-plane seam — and the sim's
    /// injection-as-API control surface). Fires the query() change-event for `name`.
    pub fn set_consent(&self, name: &str, decision: PermissionState) {
        let key = name.to_ascii_lowercase();
        let mut st = self.state.lock().unwrap();
        st.consent.insert(key.clone(), decision);
        notify(&mut st, &key, decision);
    }

    /// Apply a partial pose delta (the sim's `setPhysicalModel`-style injection), returning the
    /// new pose, or `HardwareAbsent` if the descriptor has no IMU.
    pub fn set_pose_delta(&self, delta: PoseDelta) -> Result<Pose, CapError> {
        if !self.present("imu") {
            return Err(CapError::HardwareAbsent);
        }
        let mut st = self.state.lock().unwrap();
        st.pose.apply(&delta);
        Ok(st.pose)
    }

    /// Subscribe to query() changes for a capability (the Permissions-API change-event shape).
    pub fn subscribe(&self, name: &str) -> Receiver<PermissionState> {
        let (tx, rx) = channel();
        let key = name.to_ascii_lowercase();
        self.state.lock().unwrap().subscribers.entry(key).or_default().push(tx);
        rx
    }

    /// Subscribe to `PrefsDidChange` for a preference `name` — the E4 observer, mirroring
    /// [`InProcessBackend::subscribe`]. The returned [`Receiver`] yields the NEW effective
    /// [`PrefValue`] whenever the preference's effective value moves via ANY write path: a
    /// control-plane write ([`InProcessBackend::set_preference`] / `set_preference_bool`, fired
    /// directly) or an external `pf-settings` CLI write picked up by [`reload_prefs`]. Preference
    /// keys are the schema's camelCase keys (`hapticsEnabled`, `reduceMotion`, `monoAudio`,
    /// `brightness`) — NOT lowercased, unlike capability names, so they match the store 1:1.
    pub fn subscribe_preference(&self, name: &str) -> Receiver<PrefValue> {
        let (tx, rx) = channel();
        self.state.lock().unwrap().pref_subscribers.entry(name.to_string()).or_default().push(tx);
        rx
    }

    /// The control-plane preference write (the injection-as-API seam, and what `set_preference_bool`
    /// delegates to). Validates + persists through the store (when attached), updates the in-memory
    /// cache, and fires `PrefsDidChange` to subscribers **iff the effective value moved**. Invalid
    /// keys/values are reported on stderr and dropped (this seam is infallible by contract — the
    /// fallible surface is the `pf-settings` CLI, which shows the typed error).
    pub fn set_preference(&self, name: &str, value: PrefValue) {
        let mut st = self.state.lock().unwrap();
        // Compute the effective-value transition against the in-memory cache first (this is what
        // in-process observers care about), then persist the same write to disk.
        let change = match st.prefs.set(name, value) {
            Ok(change) => change,
            Err(e) => {
                let _ = writeln!(std::io::stderr(), "pocketforge: set_preference({name}) rejected: {e}");
                return;
            }
        };
        if let Some(store) = &self.prefs_store {
            if let Err(e) = store.apply(name, value) {
                let _ = writeln!(std::io::stderr(), "pocketforge: pref persist of {name} failed: {e}");
            }
        }
        if let Some(change) = change {
            notify_pref(&mut st, name, change.new);
        }
    }

    /// Re-read the persistent store and fire `PrefsDidChange` for every preference whose effective
    /// value moved since the last cache — the v0 way an EXTERNAL write (the `pf-settings` CLI, or
    /// the `.3` UI, running in a separate process) becomes visible to a running app and its
    /// observers. It is the honest v0 stand-in for a supervisor file-watch/inotify signal: the
    /// host calls it when it learns the store changed. A store-less backend is a no-op.
    pub fn reload_prefs(&self) {
        let Some(store) = &self.prefs_store else { return };
        let fresh = match store.load() {
            Ok(p) => p,
            Err(e) => {
                let _ = writeln!(std::io::stderr(), "pocketforge: prefs reload failed: {e}");
                return;
            }
        };
        let mut st = self.state.lock().unwrap();
        // Fire for each schema key whose effective value changed. (`value` is infallible for a
        // schema key; both docs are validated, so unwrap cannot panic here.)
        for spec in SCHEMA {
            let old = st.prefs.value(spec.key).expect("schema key");
            let new = fresh.value(spec.key).expect("schema key");
            if old != new {
                notify_pref(&mut st, spec.key, new);
            }
        }
        st.prefs = fresh;
    }

    fn present(&self, name: &str) -> bool {
        self.descriptor.cap_present(name)
    }
}

/// Push a permission state to every live subscriber for `name`, pruning dead senders.
fn notify(st: &mut State, name: &str, state: PermissionState) {
    if let Some(subs) = st.subscribers.get_mut(name) {
        subs.retain(|tx| tx.send(state).is_ok());
    }
}

/// Push the new effective preference value to every live `PrefsDidChange` subscriber for `key`,
/// pruning dead senders (mirrors [`notify`] for the query() change-event bus).
fn notify_pref(st: &mut State, key: &str, value: PrefValue) {
    if let Some(subs) = st.pref_subscribers.get_mut(key) {
        subs.retain(|tx| tx.send(value).is_ok());
    }
}

impl Backend for InProcessBackend {
    fn is_present(&self, name: &str) -> bool {
        self.present(name)
    }

    fn query(&self, name: &str) -> PermissionState {
        let key = name.to_ascii_lowercase();
        // An explicit consent decision overrides the default policy (the E3 overlay).
        if let Some(c) = self.state.lock().unwrap().consent.get(&key) {
            return *c;
        }
        query_decision(self.present(&key), &key)
    }

    fn is_granted(&self, name: &str) -> bool {
        self.query(name) == PermissionState::Granted
    }

    fn acquire(&self, name: &str) -> Result<(), CapError> {
        // Presence first: distinguishes HardwareAbsent / Unsupported from a permission refusal.
        let default = acquire_decision(self.present(name), name);
        if default.is_ok() {
            // Present + not default-deny by the static policy, but an explicit consent decision
            // can still gate it (and a default-deny cap reaches here only via consent override).
            return match self.query(name) {
                PermissionState::Granted => Ok(()),
                PermissionState::Prompt | PermissionState::Denied => Err(CapError::ConsentDenied),
            };
        }
        // default-deny present caps surface as ConsentDenied unless consent overrode to Granted.
        if matches!(default, Err(CapError::ConsentDenied))
            && self.query(name) == PermissionState::Granted
        {
            return Ok(());
        }
        default
    }

    fn rumble_pulse(&self, _ms: u32) -> RumbleStatus {
        // The unified no-op shape (mirrors broker_stub.RumbleHandle.pulse): absence and
        // accessibility-suppression collapse to the same typed no-op; presence+enabled fires.
        let present = self.present("rumble");
        let haptics = self.preference_bool("hapticsEnabled", true);
        if !present {
            RumbleStatus::NoopAbsent
        } else if !haptics {
            RumbleStatus::NoopSuppressed
        } else {
            RumbleStatus::Fired // honesty: the sim does not actuate silicon; real buzz is hw-gated
        }
    }

    fn get_pose(&self) -> Result<Pose, CapError> {
        if !self.present("imu") {
            return Err(CapError::HardwareAbsent);
        }
        Ok(self.state.lock().unwrap().pose)
    }

    fn set_pose(&self, pose: Pose) -> Result<Pose, CapError> {
        if !self.present("imu") {
            return Err(CapError::HardwareAbsent);
        }
        let mut st = self.state.lock().unwrap();
        st.pose = pose;
        Ok(st.pose)
    }

    fn get_capability(&self, name: &str) -> Result<Vec<u8>, CapError> {
        self.acquire(name)?;
        let key = name.to_ascii_lowercase();
        Ok(self.state.lock().unwrap().stored.get(&key).cloned().unwrap_or_default())
    }

    fn set_capability(&self, name: &str, value: &[u8]) -> Result<(), CapError> {
        self.acquire(name)?;
        let key = name.to_ascii_lowercase();
        self.state.lock().unwrap().stored.insert(key, value.to_vec());
        Ok(())
    }

    fn preference_bool(&self, name: &str, default: bool) -> bool {
        // A schema key resolves through the store/schema (stored value, else schema default); an
        // unknown key (not a real preference) falls back to the caller's default — preserving the
        // pre-store trait contract "return default if unset" for names outside the schema.
        self.state.lock().unwrap().prefs.get_bool(name).unwrap_or(default)
    }

    fn set_preference_bool(&self, name: &str, value: bool) {
        self.set_preference(name, PrefValue::Bool(value));
    }

    fn preference_scalar(&self, name: &str, default: i64) -> i64 {
        // Same resolution as `preference_bool`: a schema scalar (`brightness`) reads through the
        // store/schema; any other name falls back to the caller's default.
        self.state.lock().unwrap().prefs.get_scalar(name).unwrap_or(default)
    }

    fn subscribe_preference(&self, name: &str) -> Option<Receiver<PrefValue>> {
        Some(InProcessBackend::subscribe_preference(self, name))
    }
}
