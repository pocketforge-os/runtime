//! **Supervisor-ask seam** — the portal-flow contract the E2 broker uses to have the M1.D
//! supervisor (paused) draw the consent dialog and return the user's selection. Written down
//! here as a Rust trait + concrete request/response types, matching the shape decided in
//! `runtime/spikes/consent-ui/DESIGN.md` §4 (the .1 seam contract handed to `.3`).
//!
//! ## Non-goals in v1
//!
//! * **No wire representation.** The M1.D supervisor is paused (`tsp-iuz.3`); when it lands, `.3`
//!   or a follow-on will pick the framing (JSON over a Unix socket, or the PFW1 wire, or an
//!   fd-passing scheme). The trait here is the *shape*; a concrete over-the-wire supervisor is a
//!   post-substrate leg. Tests + the STEP-2 sim proof use [`SimulatedSupervisor`].
//! * **No batching.** DESIGN.md §4 states asks are strictly serial (`.3` may fold later, atop
//!   the same seam). This trait honors that: one call per ask.
//! * **No timeouts.** DESIGN.md §4 states the broker times out the wait itself if needed; the
//!   supervisor answers exactly once per `ask_id`.

use std::collections::VecDeque;
use std::sync::Mutex;

/// Which button was focused when the dialog first drew, per DESIGN.md §4. Fixed at
/// [`AskFocus::Deny`] in v0 (least-privilege). Reserved for future policy tuning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskFocus {
    Deny,
    AllowOnce,
    AllowAlways,
}

/// Which scope button the supervisor is allowed to draw for this ask. In v0 always the full set;
/// reserved so a dangerous cap could restrict to `[Deny, AllowOnce]` (no persistent grant).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskScope {
    Deny,
    AllowOnce,
    AllowAlways,
}

/// Where in an app's lifecycle the ask was raised (DESIGN.md §4 + Q4 ruling: v0 asks fire only
/// at these two boundaries — the supervisor already owns fb0 there, no reclamation needed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskContext {
    Launch,
    AppSwitch,
}

/// The user's committed decision (DESIGN.md §4 response field).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskDecision {
    Deny,
    AllowOnce,
    AllowAlways,
}

/// The user's *raw gesture* (DESIGN.md §4 audit field). Distinguishes an explicit "A on Deny"
/// from a `B` cancel — both collapse to [`AskDecision::Deny`] but the audit trail keeps the
/// difference so a post-hoc review can see whether the user actively denied or backed out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskInput {
    AOnDeny,
    AOnAllowOnce,
    AOnAllowAlways,
    BCancel,
}

impl AskInput {
    /// The decision this raw gesture collapses to.
    pub fn decision(self) -> AskDecision {
        match self {
            AskInput::AOnDeny | AskInput::BCancel => AskDecision::Deny,
            AskInput::AOnAllowOnce => AskDecision::AllowOnce,
            AskInput::AOnAllowAlways => AskDecision::AllowAlways,
        }
    }

    /// The stable string used in the ledger record (see [`crate::appops`]).
    pub fn as_str(self) -> &'static str {
        match self {
            AskInput::AOnDeny => "a_on_deny",
            AskInput::AOnAllowOnce => "a_on_allow_once",
            AskInput::AOnAllowAlways => "a_on_allow_always",
            AskInput::BCancel => "b_cancel",
        }
    }

    /// Parse from the ledger record (round-trip with [`Self::as_str`]).
    pub fn parse(s: &str) -> Option<AskInput> {
        Some(match s {
            "a_on_deny" => AskInput::AOnDeny,
            "a_on_allow_once" => AskInput::AOnAllowOnce,
            "a_on_allow_always" => AskInput::AOnAllowAlways,
            "b_cancel" => AskInput::BCancel,
            _ => return None,
        })
    }
}

/// The broker → supervisor request (DESIGN.md §4). Every field marked mandatory there is
/// mandatory here; the optionals are `Option<T>`.
#[derive(Debug, Clone)]
pub struct AskRequest {
    pub ask_id: u64,
    pub app_id: String,
    pub app_name: String,
    /// The capability being asked for (`"location"`, `"gnss"`, `"egress"`, ...).
    pub resource: String,
    /// The scope modifier, when present (`"approximate"` for location, `"api.weather.com"` for
    /// egress). Rendered as a secondary line in the dialog.
    pub resource_arg: Option<String>,
    /// The manifest's `purpose = "..."` for this `use=[]` entry, if declared.
    pub purpose: Option<String>,
    pub default_focus: AskFocus,
    pub allowed_scopes: Vec<AskScope>,
    pub ask_context: AskContext,
}

impl AskRequest {
    /// Build a v0 request with the fixed default_focus + full allowed_scopes set (DESIGN.md §4
    /// defaults).
    pub fn v0(
        ask_id: u64,
        app_id: impl Into<String>,
        app_name: impl Into<String>,
        resource: impl Into<String>,
        resource_arg: Option<String>,
        ask_context: AskContext,
    ) -> AskRequest {
        AskRequest {
            ask_id,
            app_id: app_id.into(),
            app_name: app_name.into(),
            resource: resource.into(),
            resource_arg,
            purpose: None,
            default_focus: AskFocus::Deny,
            allowed_scopes: vec![AskScope::Deny, AskScope::AllowOnce, AskScope::AllowAlways],
            ask_context,
        }
    }
}

/// The supervisor → broker response (DESIGN.md §4). The response is BINDING — the broker writes
/// the ledger row from this, not from any dialog state it observed.
#[derive(Debug, Clone)]
pub struct AskResponse {
    pub ask_id: u64,
    pub decision: AskDecision,
    pub input: AskInput,
    pub elapsed_ms: u64,
    pub supervisor_note: Option<String>,
}

impl AskResponse {
    /// A deny response for `req` (used by [`NullSupervisor`] and as a fixture default).
    pub fn deny_for(req: &AskRequest) -> AskResponse {
        AskResponse {
            ask_id: req.ask_id,
            decision: AskDecision::Deny,
            input: AskInput::AOnDeny,
            elapsed_ms: 0,
            supervisor_note: None,
        }
    }
}

/// The seam the enforcing broker calls with an [`AskRequest`] and awaits an [`AskResponse`]. A
/// production supervisor is the paused M1.D piece; in `.3` we ship two in-tree impls (see below)
/// and defer wire framing to the substrate leg.
pub trait SupervisorAsk: Send + Sync {
    /// Ask the supervisor to draw the dialog and return the user's selection. May block.
    fn ask(&self, req: AskRequest) -> AskResponse;

    /// Marker: is this the deploy-default [`NullSupervisor`]? The enforcing backend uses this to
    /// short-circuit the "absence of supervisor" case WITHOUT writing a ledger row — a spurious
    /// standing deny would pre-poison the ledger for the day a real supervisor asks. Every
    /// non-null impl inherits the default `false`.
    fn is_null(&self) -> bool {
        false
    }
}

/// **Test / sim supervisor** — a pre-programmed answer queue keyed by `(app_id, cap, modifier)`.
/// The .1 prototype (`runtime/spikes/consent-ui/driver.py`) plays this role in the STEP-2
/// scripted sim transcript; here we model the same shape headlessly.
pub struct SimulatedSupervisor {
    q: Mutex<VecDeque<PreparedAnswer>>,
    default: Mutex<Option<AskResponse>>,
    asks_seen: Mutex<Vec<AskRequest>>,
}

/// One pre-programmed answer bound to the identity of a coming ask.
#[derive(Debug, Clone)]
pub struct PreparedAnswer {
    pub app_id: String,
    pub cap: String,
    pub modifier: Option<String>,
    pub decision: AskDecision,
    pub input: AskInput,
    pub supervisor_note: Option<String>,
    pub elapsed_ms: u64,
}

impl PreparedAnswer {
    /// A convenience: "user pressed A on Allow Once" for `(app, cap, modifier)`.
    pub fn allow_once(app_id: impl Into<String>, cap: impl Into<String>, modifier: Option<&str>) -> PreparedAnswer {
        PreparedAnswer {
            app_id: app_id.into(),
            cap: cap.into(),
            modifier: modifier.map(str::to_string),
            decision: AskDecision::AllowOnce,
            input: AskInput::AOnAllowOnce,
            supervisor_note: None,
            elapsed_ms: 0,
        }
    }
    /// A convenience: "user pressed A on Allow Always" for `(app, cap, modifier)`.
    pub fn allow_always(app_id: impl Into<String>, cap: impl Into<String>, modifier: Option<&str>) -> PreparedAnswer {
        PreparedAnswer {
            app_id: app_id.into(),
            cap: cap.into(),
            modifier: modifier.map(str::to_string),
            decision: AskDecision::AllowAlways,
            input: AskInput::AOnAllowAlways,
            supervisor_note: None,
            elapsed_ms: 0,
        }
    }
    /// A convenience: "user pressed A on Deny" for `(app, cap, modifier)`.
    pub fn deny(app_id: impl Into<String>, cap: impl Into<String>, modifier: Option<&str>) -> PreparedAnswer {
        PreparedAnswer {
            app_id: app_id.into(),
            cap: cap.into(),
            modifier: modifier.map(str::to_string),
            decision: AskDecision::Deny,
            input: AskInput::AOnDeny,
            supervisor_note: None,
            elapsed_ms: 0,
        }
    }
}

impl SimulatedSupervisor {
    /// A fresh supervisor with no prepared answers and no default. Any ask against it will panic
    /// unless [`set_default`](Self::set_default) or [`prepare`](Self::prepare) covers it.
    pub fn new() -> SimulatedSupervisor {
        SimulatedSupervisor {
            q: Mutex::new(VecDeque::new()),
            default: Mutex::new(None),
            asks_seen: Mutex::new(Vec::new()),
        }
    }

    /// Enqueue an answer to be matched against the next ask whose `(app_id, cap, modifier)` triple
    /// equals `answer`'s. Answers are consumed in FIFO order among those that match.
    pub fn prepare(&self, answer: PreparedAnswer) {
        self.q.lock().unwrap().push_back(answer);
    }

    /// Set a fallback response used when no prepared answer matches (typically an explicit
    /// [`AskResponse::deny_for`] in tests that want to prove the deny path).
    pub fn set_default(&self, response: AskResponse) {
        *self.default.lock().unwrap() = Some(response);
    }

    /// Every ask this supervisor has seen (in call order) — the audit trail tests assert on.
    pub fn asks_seen(&self) -> Vec<AskRequest> {
        self.asks_seen.lock().unwrap().clone()
    }
}

impl Default for SimulatedSupervisor {
    fn default() -> Self {
        SimulatedSupervisor::new()
    }
}

impl SupervisorAsk for SimulatedSupervisor {
    fn ask(&self, req: AskRequest) -> AskResponse {
        self.asks_seen.lock().unwrap().push(req.clone());
        let modifier_lc = req.resource_arg.as_ref().map(|s| s.to_ascii_lowercase());
        let cap_lc = req.resource.to_ascii_lowercase();
        // Match FIFO among candidates that name (app_id, cap, modifier) — leave others in the queue.
        let mut q = self.q.lock().unwrap();
        if let Some(idx) = q.iter().position(|a| {
            a.app_id == req.app_id
                && a.cap.to_ascii_lowercase() == cap_lc
                && a.modifier.as_ref().map(|s| s.to_ascii_lowercase()) == modifier_lc
        }) {
            let a = q.remove(idx).unwrap();
            return AskResponse {
                ask_id: req.ask_id,
                decision: a.decision,
                input: a.input,
                elapsed_ms: a.elapsed_ms,
                supervisor_note: a.supervisor_note,
            };
        }
        if let Some(d) = self.default.lock().unwrap().clone() {
            return AskResponse { ask_id: req.ask_id, ..d };
        }
        panic!(
            "SimulatedSupervisor: no prepared answer for ask_id={} app={} cap={} modifier={:?}",
            req.ask_id, req.app_id, req.resource, req.resource_arg
        );
    }
}

/// **Deploy-default supervisor** — every ask denies. Used when no supervisor is wired: an
/// absence-of-supervisor outcome is NOT a user choice, so this impl deliberately DOES NOT
/// (and MUST NOT) cause a ledger row to be written. The enforcing backend recognizes this impl
/// by type and returns `ConsentDenied` without a `record_grant` call (see
/// [`crate::EnforcingBackend`]); recording a standing-deny here would pre-poison the ledger for
/// the day a real supervisor asks. The negative test `null_supervisor_deny_does_not_touch_ledger`
/// pins this invariant.
pub struct NullSupervisor;

impl SupervisorAsk for NullSupervisor {
    fn ask(&self, req: AskRequest) -> AskResponse {
        AskResponse::deny_for(&req)
    }
    fn is_null(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ask_input_round_trip() {
        for v in [AskInput::AOnDeny, AskInput::AOnAllowOnce, AskInput::AOnAllowAlways, AskInput::BCancel] {
            assert_eq!(AskInput::parse(v.as_str()), Some(v));
        }
        assert_eq!(AskInput::parse("garbage"), None);
    }

    #[test]
    fn ask_input_collapses_correctly() {
        assert_eq!(AskInput::AOnDeny.decision(), AskDecision::Deny);
        assert_eq!(AskInput::BCancel.decision(), AskDecision::Deny);
        assert_eq!(AskInput::AOnAllowOnce.decision(), AskDecision::AllowOnce);
        assert_eq!(AskInput::AOnAllowAlways.decision(), AskDecision::AllowAlways);
    }

    #[test]
    fn simulated_supervisor_matches_by_triple_and_fifo() {
        let sup = SimulatedSupervisor::new();
        sup.prepare(PreparedAnswer::allow_once("com.a.b", "location", Some("approximate")));
        sup.prepare(PreparedAnswer::deny("com.a.b", "egress", Some("api.example")));

        // Location ask ⇒ allow_once.
        let r1 = sup.ask(AskRequest::v0(1, "com.a.b", "AB", "location", Some("approximate".into()), AskContext::Launch));
        assert_eq!(r1.decision, AskDecision::AllowOnce);
        // Egress ask (different (cap, mod)) still finds its answer even though it wasn't first.
        let r2 = sup.ask(AskRequest::v0(2, "com.a.b", "AB", "egress", Some("api.example".into()), AskContext::Launch));
        assert_eq!(r2.decision, AskDecision::Deny);
    }

    #[test]
    fn simulated_supervisor_defaults_when_unmatched() {
        let sup = SimulatedSupervisor::new();
        let mut d = AskResponse::deny_for(&AskRequest::v0(0, "x", "X", "y", None, AskContext::Launch));
        d.supervisor_note = Some("fallback".into());
        sup.set_default(d);
        let r = sup.ask(AskRequest::v0(9, "any.app", "Any", "location", None, AskContext::Launch));
        assert_eq!(r.decision, AskDecision::Deny);
        assert_eq!(r.supervisor_note.as_deref(), Some("fallback"));
        assert_eq!(r.ask_id, 9);
    }

    #[test]
    fn null_supervisor_denies() {
        let sup = NullSupervisor;
        let r = sup.ask(AskRequest::v0(1, "x", "X", "location", None, AskContext::Launch));
        assert_eq!(r.decision, AskDecision::Deny);
        assert_eq!(r.input, AskInput::AOnDeny);
    }
}
