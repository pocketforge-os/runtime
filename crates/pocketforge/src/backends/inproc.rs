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
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use crate::backend::{acquire_decision, query_decision, Backend, Pose, PoseDelta, RumbleStatus};
use crate::descriptor::Descriptor;
use crate::error::{CapError, PermissionState};

/// Mutable cooperative state behind the backend.
#[derive(Default)]
struct State {
    /// Cooperatively stored capability values (`set_capability`/`get_capability`).
    stored: HashMap<String, Vec<u8>>,
    /// Accessibility/user preferences the broker enforces at the primitive (E4). Default on.
    prefs: HashMap<String, bool>,
    /// Explicit consent decisions (the E3 seam) overlaid on the default policy.
    consent: HashMap<String, PermissionState>,
    /// Latest injected IMU pose (the `.4` physical model replaces this with integration).
    pose: Pose,
    /// query() change-event subscribers, keyed by capability name.
    subscribers: HashMap<String, Vec<Sender<PermissionState>>>,
}

/// The in-process v0 capability backend. Cheap to clone-share via [`InProcessBackend::shared`].
pub struct InProcessBackend {
    descriptor: Arc<Descriptor>,
    state: Mutex<State>,
}

impl InProcessBackend {
    /// Build a backend over a descriptor.
    pub fn new(descriptor: Arc<Descriptor>) -> InProcessBackend {
        InProcessBackend { descriptor, state: Mutex::new(State::default()) }
    }

    /// Build a shared (`Arc`) backend — the form the reference [`crate::server`] wraps.
    pub fn shared(descriptor: Arc<Descriptor>) -> Arc<InProcessBackend> {
        Arc::new(InProcessBackend::new(descriptor))
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
        self.state.lock().unwrap().prefs.get(name).copied().unwrap_or(default)
    }

    fn set_preference_bool(&self, name: &str, value: bool) {
        self.state.lock().unwrap().prefs.insert(name.to_string(), value);
    }
}
