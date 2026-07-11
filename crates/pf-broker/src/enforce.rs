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
//!      [`QuotaLedger`]; exhaustion is `PolicyBlocked`;
//!   5. (`tsp-ht0p.3`) **portal-flow consent** — an [`EnforcingBackend`] built with
//!      [`with_consent_portal`](EnforcingBackend::with_consent_portal) drives the E3 seam: a
//!      dangerous-tier `acquire` with no live [`AppOpsLedger`] grant Prompts the supervisor,
//!      records the resulting grant (once/always) in the ledger, and applies it to the inner
//!      [`InProcessBackend`] via `set_consent` so the change-event bus fires. The legacy
//!      constructors ([`new`](EnforcingBackend::new) / [`with_quotas`](EnforcingBackend::with_quotas))
//!      stay cooperative: the inner's `set_consent` remains authoritative for tests and the
//!      pre-`.3` co-op path (back-compat guarantee for `tsp-ht0p.2`).
//!
//! Because it implements [`Backend`], the existing `.2` wire server serves it unchanged — so an
//! out-of-process app (E6) hits IDENTICAL semantics to the in-process backend EXCEPT where the
//! manifest/quotas legitimately tighten them. That is the backend-swap, now with real teeth.
//!
//! HONESTY (R-A): this enforces the AUTHORITY GRAPH + default-deny cooperatively over the socket;
//! it does NOT yet confine a process that bypasses the socket (no namespaces/seccomp today). Real
//! fd-isolation into an app namespace is substrate-gated (owned kernel M2.B-E + paused M1.D).

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use pocketforge::backend::{Backend, Pose, RumbleStatus};
use pocketforge::backends::InProcessBackend;
use pocketforge::error::{CapError, PermissionState};
use pocketforge::QuotaLedger;

use crate::appops::{AppOpsLedger, GrantCheck, GrantKey, Scope};
use crate::consent::{AskContext, AskDecision, AskRequest, SupervisorAsk};
use crate::manifest::ValidatedManifest;
use crate::tier::{tier_of, Tier};

/// Capabilities that carry a per-session usage quota in the broker (the dangerous/privacy tier).
const QUOTA_CAPS: &[&str] = &["location", "gnss"];

/// The deliberately ungated capability (see module docs).
const UNGATED: &str = "entropy";

/// The optional consent-portal wiring (`.3`). When present, [`EnforcingBackend::acquire`] on a
/// dangerous-tier capability consults the ledger and drives the supervisor-ask seam.
struct ConsentPortal {
    /// The concrete inner backend — held so we can call `set_consent` to apply a grant/revoke
    /// (fires the existing change-event bus).
    consent_seam: Arc<InProcessBackend>,
    /// The persistent AppOps ledger (the source of truth for dangerous-tier authorization once
    /// the portal is wired).
    appops: Arc<AppOpsLedger>,
    /// The supervisor-ask seam (typically [`NullSupervisor`] when unwired, [`crate::consent::SimulatedSupervisor`] in tests).
    supervisor: Arc<dyn SupervisorAsk>,
    /// A copy of the ceiling for [`AppOpsLedger::record_grant`] ceiling checks and for ceiling
    /// recomputation on revoke.
    manifest: ValidatedManifest,
    /// The rendered app name shown in the dialog (`identity.display_name` from the manifest —
    /// tests pass a synthetic string).
    app_name: String,
    /// Broker-scoped monotonic ask-id.
    next_ask_id: AtomicU64,
}

/// A [`Backend`] that enforces an app's validated manifest + quotas over an inner backend.
pub struct EnforcingBackend {
    inner: Arc<dyn Backend>,
    allowed: BTreeSet<String>,
    quotas: Arc<QuotaLedger>,
    portal: Option<ConsentPortal>,
}

impl EnforcingBackend {
    /// Wrap `inner` with the ceiling from `manifest` and a fresh quota ledger. **Legacy /
    /// co-op path** — no portal is wired; the inner backend's `set_consent` seam is authoritative
    /// for dangerous caps (this is what `tsp-ht0p.2` and every pre-`.3` test exercise).
    pub fn new(inner: Arc<dyn Backend>, manifest: &ValidatedManifest) -> EnforcingBackend {
        EnforcingBackend::with_quotas(inner, manifest, Arc::new(QuotaLedger::new()))
    }

    /// As [`new`](Self::new) but over an explicit (e.g. test-seeded) quota ledger. Legacy / co-op
    /// path — no portal wired.
    pub fn with_quotas(
        inner: Arc<dyn Backend>,
        manifest: &ValidatedManifest,
        quotas: Arc<QuotaLedger>,
    ) -> EnforcingBackend {
        let allowed = manifest.allowed_caps().map(|c| c.to_ascii_lowercase()).collect();
        EnforcingBackend { inner, allowed, quotas, portal: None }
    }

    /// Wrap `inner` with the ceiling + quota ledger AND wire the `.3` consent portal: on a
    /// dangerous-tier acquire with no live [`AppOpsLedger`] grant, the [`SupervisorAsk`] seam is
    /// asked; the response is recorded in the ledger AND applied to `inner` via
    /// `set_consent` so the change-event bus fires (a subscribed app re-queries).
    ///
    /// `app_name` is the string rendered in the supervisor dialog (typically
    /// `identity.display_name` from the app manifest; tests pass a synthetic name).
    pub fn with_consent_portal(
        inner: Arc<InProcessBackend>,
        manifest: ValidatedManifest,
        quotas: Arc<QuotaLedger>,
        appops: Arc<AppOpsLedger>,
        supervisor: Arc<dyn SupervisorAsk>,
        app_name: impl Into<String>,
    ) -> EnforcingBackend {
        let allowed = manifest.allowed_caps().map(|c| c.to_ascii_lowercase()).collect();
        let portal = ConsentPortal {
            consent_seam: inner.clone(),
            appops,
            supervisor,
            manifest,
            app_name: app_name.into(),
            next_ask_id: AtomicU64::new(1),
        };
        EnforcingBackend {
            inner: inner as Arc<dyn Backend>,
            allowed,
            quotas,
            portal: Some(portal),
        }
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

    /// The active AppOps ledger, when the consent portal is wired. Handy for the
    /// `pf-permissions` CLI wiring + STEP-2 assertions.
    pub fn appops(&self) -> Option<&Arc<AppOpsLedger>> {
        self.portal.as_ref().map(|p| &p.appops)
    }

    /// The validated manifest, when the consent portal is wired. Used by the CLI revoke path to
    /// build the [`GrantKey`] under the same normalization the portal uses.
    pub fn manifest(&self) -> Option<&ValidatedManifest> {
        self.portal.as_ref().map(|p| &p.manifest)
    }

    /// The consent seam (the shared `InProcessBackend`), when the portal is wired — the CLI
    /// revoke path calls `set_consent` here so a running subscriber sees the change event.
    pub fn consent_seam(&self) -> Option<&Arc<InProcessBackend>> {
        self.portal.as_ref().map(|p| &p.consent_seam)
    }

    /// Resolve the tier of a capability under this portal's manifest — for `egress`, the tier
    /// depends on the specific host declared (a specific host is Dangerous; the manifest's
    /// signature-tier gate has already refused a broad host at launch, so anything reaching this
    /// point on egress is a specific-host case). Returns `(tier, modifier)`: `modifier` is the
    /// ledger key modifier (the specific host for a single-host egress manifest, `None`
    /// otherwise).
    fn resolve_tier(portal: &ConsentPortal, cap_lc: &str) -> (Tier, Option<String>) {
        if cap_lc == "egress" {
            let hosts: Vec<&str> = portal.manifest.egress_hosts().collect();
            let modifier = match hosts.as_slice() {
                [one] => Some((*one).to_string()),
                // Multi-host: the modifier for the ledger row is ambiguous from the acquire path
                // alone; ledger keys on the base cap and the tier is Dangerous. Per-host
                // granularity is a `.4` follow-on.
                _ => None,
            };
            let tier = tier_of(cap_lc, modifier.as_deref().or(Some("_declared_")));
            return (tier, modifier);
        }
        (tier_of(cap_lc, None), None)
    }

    /// Fire the change event on the shared inner backend ONLY when the state actually changes —
    /// avoids spurious events on idempotent transitions (a revoked cap that is acquired again
    /// stays Denied; no second event should fire).
    fn apply_consent_if_changed(portal: &ConsentPortal, cap_lc: &str, state: PermissionState) {
        if portal.consent_seam.query(cap_lc) != state {
            portal.consent_seam.set_consent(cap_lc, state);
        }
    }

    /// The dangerous-tier authorization path: consult the ledger; if no live grant, ask the
    /// supervisor and record the result. Returns `Ok(())` when the app may proceed (the caller
    /// then decrements the quota), `Err(ConsentDenied)` otherwise.
    fn dangerous_authorize(&self, portal: &ConsentPortal, name: &str) -> Result<(), CapError> {
        let cap_lc = name.to_ascii_lowercase();
        let (_tier, modifier) = Self::resolve_tier(portal, &cap_lc);
        let key = GrantKey::new(&portal.manifest.app_id, &cap_lc, modifier.as_deref());

        // (1) Live ledger check first.
        match portal.appops.check(&key) {
            GrantCheck::Always => {
                Self::apply_consent_if_changed(portal, &cap_lc, PermissionState::Granted);
                return Ok(());
            }
            GrantCheck::OnceAvailable => {
                portal.appops.consume_once(&key).map_err(|_| CapError::ConsentDenied)?;
                Self::apply_consent_if_changed(portal, &cap_lc, PermissionState::Granted);
                return Ok(());
            }
            GrantCheck::Revoked | GrantCheck::OnceUsed => {
                // Standing user decision (revoke) OR a consumed once-scope grant: no re-prompt in
                // v1 (see the appops module doc — "forget → back to Prompt" is a deliberate
                // non-feature in v1). Apply Denied only if not already Denied to avoid spurious
                // change events on idempotent re-acquire.
                Self::apply_consent_if_changed(portal, &cap_lc, PermissionState::Denied);
                return Err(CapError::ConsentDenied);
            }
            GrantCheck::NoGrant => {}
        }

        // (2) No live grant — ask the supervisor.
        let ask_id = portal.next_ask_id.fetch_add(1, Ordering::Relaxed);
        let req = AskRequest::v0(
            ask_id,
            portal.manifest.app_id.clone(),
            portal.app_name.clone(),
            cap_lc.clone(),
            modifier.clone(),
            AskContext::Launch, // v0 policy per Q4 ruling: consent only at launch/app-switch
        );
        // Fast-path: a NullSupervisor deploy default returns Deny WITHOUT touching the ledger —
        // absence of a supervisor is not a user choice, so we do NOT record a standing deny (a
        // spurious row here would pre-poison the ledger for the day a real supervisor asks). The
        // trait's `is_null` marker keeps this a compile-time-free type check.
        if portal.supervisor.is_null() {
            Self::apply_consent_if_changed(portal, &cap_lc, PermissionState::Denied);
            return Err(CapError::ConsentDenied);
        }
        let response = portal.supervisor.ask(req);

        match response.decision {
            AskDecision::Deny => {
                Self::apply_consent_if_changed(portal, &cap_lc, PermissionState::Denied);
                Err(CapError::ConsentDenied)
            }
            AskDecision::AllowOnce => {
                portal
                    .appops
                    .record_grant(
                        &portal.manifest,
                        &cap_lc,
                        modifier.as_deref(),
                        Scope::Once,
                        response.ask_id,
                        response.input,
                        response.supervisor_note,
                    )
                    .map_err(|_| CapError::ConsentDenied)?;
                portal.appops.consume_once(&key).ok();
                Self::apply_consent_if_changed(portal, &cap_lc, PermissionState::Granted);
                Ok(())
            }
            AskDecision::AllowAlways => {
                portal
                    .appops
                    .record_grant(
                        &portal.manifest,
                        &cap_lc,
                        modifier.as_deref(),
                        Scope::Always,
                        response.ask_id,
                        response.input,
                        response.supervisor_note,
                    )
                    .map_err(|_| CapError::ConsentDenied)?;
                Self::apply_consent_if_changed(portal, &cap_lc, PermissionState::Granted);
                Ok(())
            }
        }
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
        // Portal wiring: report the LEDGER's live view for dangerous caps so subscribers see the
        // right state after grant/revoke. Everything else defers to the inner (motion-sensor
        // presence, cosmetic no-ops, ...).
        if let Some(portal) = &self.portal {
            let cap_lc = name.to_ascii_lowercase();
            let (tier, modifier) = Self::resolve_tier(portal, &cap_lc);
            if tier == Tier::Dangerous {
                let key = GrantKey::new(&portal.manifest.app_id, &cap_lc, modifier.as_deref());
                return match portal.appops.check(&key) {
                    GrantCheck::Always | GrantCheck::OnceAvailable => PermissionState::Granted,
                    GrantCheck::Revoked | GrantCheck::OnceUsed => PermissionState::Denied,
                    GrantCheck::NoGrant => PermissionState::Prompt,
                };
            }
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
        // With the portal wired, the ledger + supervisor drive dangerous-tier authorization;
        // otherwise the legacy cooperative path (inner.acquire alone) stands.
        if let Some(portal) = &self.portal {
            let cap_lc = name.to_ascii_lowercase();
            let (tier, _modifier) = Self::resolve_tier(portal, &cap_lc);
            if tier == Tier::Dangerous {
                self.dangerous_authorize(portal, name)?;
                if Self::is_quota_cap(name) && !self.quotas.try_consume(name, 1) {
                    return Err(CapError::PolicyBlocked); // per-capability quota exhausted
                }
                return Ok(());
            }
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
