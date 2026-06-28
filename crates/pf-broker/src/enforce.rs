//! The **enforcing backend** — the broker's trust-path core. It wraps the v0 capability backend
//! and adds, ON TOP of the inner presence/consent/default-deny decisions, the things that make
//! this a BROKER rather than a cooperative library:
//!
//!   1. **manifest ceiling** — an `acquire`/`get`/`set`/`query` for a capability NOT in the app's
//!      validated `app.toml use=[]` is `PolicyBlocked`/`Denied`, regardless of hardware (the
//!      authority graph, not the app, bounds what is reachable);
//!   2. **default-deny** — preserved from the inner backend (privacy caps stay consent-gated);
//!   3. **entropy is the ungated exception** — auto-granted with no ceiling, no consent, no quota
//!      (the CSPRNG is non-exhaustible, so gating it buys no security and only adds friction);
//!   4. **per-capability quotas** — a dangerous cap (`location`) is rate-capped via the session
//!      [`QuotaLedger`]; exhaustion is `PolicyBlocked`.
//!
//! Because it implements [`Backend`], the existing `.2` wire server serves it unchanged — so an
//! out-of-process app (E6) hits IDENTICAL semantics to the in-process backend EXCEPT where the
//! manifest/quotas legitimately tighten them. That is the backend-swap, now with real teeth.
//!
//! HONESTY (R-A): this enforces the AUTHORITY GRAPH + default-deny cooperatively over the socket;
//! it does NOT yet confine a process that bypasses the socket (no namespaces/seccomp today). Real
//! fd-isolation into an app namespace is substrate-gated (owned kernel M2.B-E + paused M1.D).

use std::collections::BTreeSet;
use std::sync::Arc;

use pocketforge::backend::{Backend, Pose, RumbleStatus};
use pocketforge::error::{CapError, PermissionState};
use pocketforge::QuotaLedger;

use crate::manifest::ValidatedManifest;

/// Capabilities that carry a per-session usage quota in the broker (the dangerous/privacy tier).
const QUOTA_CAPS: &[&str] = &["location", "gnss"];

/// The deliberately ungated capability (see module docs).
const UNGATED: &str = "entropy";

/// A [`Backend`] that enforces an app's validated manifest + quotas over an inner backend.
pub struct EnforcingBackend {
    inner: Arc<dyn Backend>,
    allowed: BTreeSet<String>,
    quotas: Arc<QuotaLedger>,
}

impl EnforcingBackend {
    /// Wrap `inner` with the ceiling from `manifest` and a fresh quota ledger.
    pub fn new(inner: Arc<dyn Backend>, manifest: &ValidatedManifest) -> EnforcingBackend {
        EnforcingBackend::with_quotas(inner, manifest, Arc::new(QuotaLedger::new()))
    }

    /// As [`new`](Self::new) but over an explicit (e.g. test-seeded) quota ledger.
    pub fn with_quotas(
        inner: Arc<dyn Backend>,
        manifest: &ValidatedManifest,
        quotas: Arc<QuotaLedger>,
    ) -> EnforcingBackend {
        let allowed = manifest.allowed_caps().map(|c| c.to_ascii_lowercase()).collect();
        EnforcingBackend { inner, allowed, quotas }
    }

    fn allows(&self, name: &str) -> bool {
        self.allowed.contains(&name.to_ascii_lowercase())
    }

    fn is_ungated(name: &str) -> bool {
        name.eq_ignore_ascii_case(UNGATED)
    }

    fn is_quota_cap(name: &str) -> bool {
        let n = name.to_ascii_lowercase();
        QUOTA_CAPS.iter().any(|c| *c == n)
    }

    /// The session quota ledger (so a test/control plane can seed it).
    pub fn quotas(&self) -> &Arc<QuotaLedger> {
        &self.quotas
    }
}

impl Backend for EnforcingBackend {
    fn is_present(&self, name: &str) -> bool {
        // Honest hardware presence (two-stage hasCapability needs it); presence is not authority.
        self.inner.is_present(name)
    }

    fn query(&self, name: &str) -> PermissionState {
        if Self::is_ungated(name) {
            return PermissionState::Granted;
        }
        if !self.allows(name) {
            return PermissionState::Denied; // outside the ceiling ⇒ denied, do not leak authority
        }
        self.inner.query(name)
    }

    fn is_granted(&self, name: &str) -> bool {
        self.query(name) == PermissionState::Granted
    }

    fn acquire(&self, name: &str) -> Result<(), CapError> {
        if Self::is_ungated(name) {
            return Ok(()); // entropy: ungated exception (non-exhaustible CSPRNG)
        }
        if !self.allows(name) {
            return Err(CapError::PolicyBlocked); // manifest ceiling
        }
        // Inner presence/consent/default-deny decision first (so a denied cap does not burn quota).
        self.inner.acquire(name)?;
        if Self::is_quota_cap(name) && !self.quotas.try_consume(name, 1) {
            return Err(CapError::PolicyBlocked); // per-capability quota exhausted
        }
        Ok(())
    }

    fn rumble_pulse(&self, ms: u32) -> RumbleStatus {
        // Cosmetic tier never errors; an undeclared haptic is SUPPRESSED (same no-op shape as E4).
        if !self.allows("vibration") && !self.allows("rumble") {
            return RumbleStatus::NoopSuppressed;
        }
        self.inner.rumble_pulse(ms)
    }

    fn get_pose(&self) -> Result<Pose, CapError> {
        if !self.allows("imu") && !self.allows("accelerometer") && !self.allows("gyroscope") {
            return Err(CapError::PolicyBlocked);
        }
        self.inner.get_pose()
    }

    fn set_pose(&self, pose: Pose) -> Result<Pose, CapError> {
        // Pose injection is a control-plane op; gate it by the same imu ceiling.
        if !self.allows("imu") && !self.allows("accelerometer") && !self.allows("gyroscope") {
            return Err(CapError::PolicyBlocked);
        }
        self.inner.set_pose(pose)
    }

    fn get_capability(&self, name: &str) -> Result<Vec<u8>, CapError> {
        if !Self::is_ungated(name) && !self.allows(name) {
            return Err(CapError::PolicyBlocked);
        }
        self.inner.get_capability(name)
    }

    fn set_capability(&self, name: &str, value: &[u8]) -> Result<(), CapError> {
        if !Self::is_ungated(name) && !self.allows(name) {
            return Err(CapError::PolicyBlocked);
        }
        self.inner.set_capability(name, value)
    }

    fn preference_bool(&self, name: &str, default: bool) -> bool {
        // Accessibility/user preferences (E4) are user-owned, not app authority — pass through.
        self.inner.preference_bool(name, default)
    }

    fn set_preference_bool(&self, name: &str, value: bool) {
        self.inner.set_preference_bool(name, value);
    }
}
