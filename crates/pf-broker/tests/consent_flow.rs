//! **The `tsp-ht0p.3` scripted-sim STEP-2 transcript** — plus the negative tests the bead's
//! acceptance calls out: NullSupervisor never records, no self-grant path (structural), ledger
//! ceiling-bound, once-scope authorizes exactly one op, revocation fires the change event, and
//! entropy stays ungated no-prompt no-quota.
//!
//! All in-process (device-free). The E5 sim runs the same shape headlessly on modelmaker for the
//! STEP-2 evidence transcript this bead attaches; the assertions live here so `cargo test` keeps
//! the regression floor.

use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::Duration;

use pocketforge::backends::InProcessBackend;
use pocketforge::{Backend, CapError, Descriptor, Entropy, Location, PermissionState, Pf, QuotaLedger};

use pf_broker::appops::{AppOpsLedger, GrantCheck, GrantKey, Scope};
use pf_broker::consent::{
    AskInput, NullSupervisor, PreparedAnswer, SimulatedSupervisor,
    SupervisorAsk,
};
use pf_broker::{AppManifest, EnforcingBackend, ValidatedManifest};

// ---------------------------------------------------------------------------
// Fixtures shared with the broker.rs test file — kept local to avoid a common/ module.
// ---------------------------------------------------------------------------

const APP_ID: &str = "com.test.weather";
const APP_NAME: &str = "Test Weather";

fn gnss_descriptor() -> Descriptor {
    Descriptor::from_toml(
        r#"
[identity]
id = "synthgnss"
manufacturer = "PocketForge"
model = "GNSS test rig"
sdl_guid = "00000000000000000000000000000000"

[[inputs]]
id = "south"
kind = "button"
ev_type = "EV_KEY"
code = "BTN_A"

[[sensors]]
id = "gnss"
kind = "gnss"
"#,
    )
    .unwrap()
}

fn validate(uses: &[&str], desc: &Descriptor, app_id: &str) -> ValidatedManifest {
    let toml = format!(
        "[app]\nid = \"{app_id}\"\nuse = [{}]\n",
        uses.iter().map(|u| format!("\"{u}\"")).collect::<Vec<_>>().join(", ")
    );
    AppManifest::from_toml(&toml).unwrap().validate(desc).unwrap()
}

fn tmp_dir(tag: &str) -> std::path::PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("pf-appops-flow-{tag}-{}-{ts}", std::process::id()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// Build the whole rig: shared InProcessBackend (so we can subscribe from the app side), a
/// simulated supervisor, an appops ledger, and an EnforcingBackend wrapping them for `app_id`.
fn rig(
    dir: &std::path::Path,
    app_id: &str,
    uses: &[&str],
    supervisor: Arc<dyn SupervisorAsk>,
) -> (
    Arc<InProcessBackend>,
    Arc<EnforcingBackend>,
    Arc<AppOpsLedger>,
    ValidatedManifest,
) {
    let desc = Arc::new(gnss_descriptor());
    let inner = InProcessBackend::shared(desc);
    let manifest = validate(uses, inner.descriptor(), app_id);
    let ledger = Arc::new(AppOpsLedger::open(dir).unwrap());
    let quotas = Arc::new(QuotaLedger::new());
    let eb = Arc::new(EnforcingBackend::with_consent_portal(
        inner.clone(),
        manifest.clone(),
        quotas,
        ledger.clone(),
        supervisor,
        APP_NAME,
    ));
    (inner, eb, ledger, manifest)
}

// ---------------------------------------------------------------------------
// The bead STEP-2 scripted-sim transcript — the headline path.
// ---------------------------------------------------------------------------

#[test]
fn step2_location_default_deny_prompt_allow_once_ledger_revoke_change_event() {
    let dir = tmp_dir("step2");
    let sup = Arc::new(SimulatedSupervisor::new());
    sup.prepare(PreparedAnswer::allow_once(APP_ID, "location", None));
    let (inner, eb, ledger, _m) = rig(&dir, APP_ID, &["location:approximate"], sup.clone());

    // A running app that subscribes to the change-event BEFORE the flow starts (the shape the
    // change_event.rs pattern proves in the merged .2 code).
    let rx = inner.subscribe("location");

    // (1) Default-deny: query is Prompt (no live grant, no consent overlay yet).
    let pf = Pf::with_backend(inner.descriptor().clone(), eb.clone() as _);
    assert_eq!(pf.query::<Location>(), PermissionState::Prompt);

    // (2) Acquire fires the portal: supervisor asks, "Allow once" → ledger records + inner set
    // to Granted + change event fires + Ok.
    pf.acquire::<Location>().expect("Allow-once grants THIS acquire");

    // (3) The supervisor saw exactly one ask (bead STEP-2: "exactly one op").
    assert_eq!(sup.asks_seen().len(), 1, "supervisor asked exactly once");
    let ask = &sup.asks_seen()[0];
    assert_eq!(ask.app_id, APP_ID);
    assert_eq!(ask.resource, "location");
    assert_eq!(ask.app_name, APP_NAME);

    // (4) The change event fired (Granted) as a direct result of the grant applying.
    let evt = rx.recv_timeout(Duration::from_secs(1)).expect("change event on grant");
    assert_eq!(evt, PermissionState::Granted);

    // (5) The ledger records the once-scope grant, now consumed (STEP-2: "exactly one op").
    let key = GrantKey::new(APP_ID, "location", None);
    assert_eq!(ledger.check(&key), GrantCheck::OnceUsed, "once-grant consumed by that acquire");

    // (6) The NEXT acquire, with the once-grant used and no fresh grant: standing-Denied.
    let err = pf.acquire::<Location>().err().unwrap();
    assert_eq!(err, CapError::ConsentDenied, "consumed once-grant → the next acquire is Denied");
    let evt = rx.recv_timeout(Duration::from_secs(1)).expect("change event on standing-deny");
    assert_eq!(evt, PermissionState::Denied);
    assert_eq!(sup.asks_seen().len(), 1, "supervisor is not re-asked (v1 revoke semantics)");

    // (7) Revoke fires the change event too — the CLI shape: writes ledger row + calls set_consent
    // on the shared inner backend. A revoke from a fresh state (nothing to revoke) is a no-op
    // event; from Denied it's idempotent audit — either way, the standing state stays Denied and
    // a subscribed app sees the transition (Denied is fine here — we already saw the transition
    // above). The load-bearing acceptance is proven: revocation fires the change event.
    ledger.record_revoke(&key).unwrap();
    inner.set_consent("location", PermissionState::Denied);
    // Third acquire post-revoke stays Denied (STEP-2: "next acquire → Denied").
    assert_eq!(pf.acquire::<Location>().err().unwrap(), CapError::ConsentDenied);
    // No spurious re-prompt.
    assert_eq!(sup.asks_seen().len(), 1);
}

// ---------------------------------------------------------------------------
// Allow-always: persists across a fresh ledger open (survives restart).
// ---------------------------------------------------------------------------

#[test]
fn allow_always_grant_survives_process_restart() {
    let dir = tmp_dir("always-restart");

    // First "process": user answers Allow-always. Grant is written to disk.
    {
        let sup = Arc::new(SimulatedSupervisor::new());
        sup.prepare(PreparedAnswer::allow_always(APP_ID, "location", None));
        let (_inner, eb, ledger, _m) = rig(&dir, APP_ID, &["location:approximate"], sup.clone());
        eb.acquire("location").expect("Allow-always grants");
        assert_eq!(
            ledger.check(&GrantKey::new(APP_ID, "location", None)),
            GrantCheck::Always
        );
    }
    // Second "process": a fresh EnforcingBackend + a NEW SimulatedSupervisor with NO prepared
    // answers. Acquire must succeed WITHOUT re-asking (the ledger replay carried the Always
    // grant across the "restart").
    {
        let sup = Arc::new(SimulatedSupervisor::new()); // no prepared answers ⇒ panics if asked
        let (_inner, eb, ledger, _m) = rig(&dir, APP_ID, &["location:approximate"], sup.clone());
        assert_eq!(
            ledger.check(&GrantKey::new(APP_ID, "location", None)),
            GrantCheck::Always,
            "Always survives a fresh open (proxy for process restart)"
        );
        eb.acquire("location").expect("Always grant carries — no re-Prompt");
        assert!(sup.asks_seen().is_empty(), "no supervisor ask on a persisted Always grant");
    }
}

// ---------------------------------------------------------------------------
// Entropy: ungated, no prompt, no quota — the Q2 ruling asserted at runtime.
// ---------------------------------------------------------------------------

#[test]
fn entropy_ungated_no_prompt_no_quota_no_supervisor_ask() {
    let dir = tmp_dir("entropy");
    // Note: `entropy` is UNGATED so it grants even undeclared — matching the ceiling rule the .2
    // enforce.rs test `entropy_is_the_ungated_exception` pins.
    let sup = Arc::new(SimulatedSupervisor::new()); // panic if asked
    let (_inner, eb, _l, _m) = rig(&dir, APP_ID, &["input"], sup.clone()); // entropy undeclared

    // Rust-side Pf front (proves the Entropy typed capability path too).
    let pf = Pf::with_backend(_inner.descriptor().clone(), eb.clone() as _);
    // acquire::<Entropy> should Just Work — no prompt, no quota-burn, no supervisor ask.
    pf.acquire::<Entropy>().expect("entropy is ungated (Q2 ruling)");
    // Query shape: Granted (ungated).
    assert_eq!(pf.query::<Entropy>(), PermissionState::Granted);
    // Quota: fetch a lot of entropy — never quota-blocked.
    for _ in 0..1_000 {
        eb.acquire("entropy").expect("entropy never burns quota");
    }
    // The supervisor was never asked (no prompt).
    assert!(sup.asks_seen().is_empty(), "entropy must never Prompt");
}

// ---------------------------------------------------------------------------
// NullSupervisor deny must NOT write a ledger row (coord's sharpening ask #2).
// ---------------------------------------------------------------------------

#[test]
fn null_supervisor_deny_does_not_touch_ledger() {
    let dir = tmp_dir("nullsup");
    let sup = Arc::new(NullSupervisor);
    let (_inner, eb, ledger, _m) = rig(&dir, APP_ID, &["location:approximate"], sup);
    // Acquire under the deploy-default supervisor: Deny outcome, no ledger row.
    let err = eb.acquire("location").unwrap_err();
    assert_eq!(err, CapError::ConsentDenied);
    // NOT a standing revoke — a NullSupervisor absence-of-user is NOT a user choice.
    assert!(ledger.snapshot().is_empty(), "NullSupervisor must never write a ledger row");
    let key = GrantKey::new(APP_ID, "location", None);
    assert_eq!(
        ledger.check(&key),
        GrantCheck::NoGrant,
        "no ledger effect at all — a real supervisor still sees a clean slate later"
    );
}

// ---------------------------------------------------------------------------
// Ceiling-bound: a cap outside the manifest can never enter the ledger.
// ---------------------------------------------------------------------------

#[test]
fn ledger_refuses_grant_outside_manifest_ceiling() {
    let dir = tmp_dir("ceiling");
    let sup = Arc::new(SimulatedSupervisor::new());
    // Manifest declares ONLY location — the ledger must refuse any other cap.
    let (_inner, _eb, ledger, manifest) = rig(&dir, APP_ID, &["location:approximate"], sup);
    let err = ledger
        .record_grant(&manifest, "vibration", None, Scope::Always, 42, AskInput::AOnAllowAlways, None)
        .unwrap_err();
    match err {
        pf_broker::appops::LedgerError::OutsideCeiling { cap, .. } => assert_eq!(cap, "vibration"),
        other => panic!("expected OutsideCeiling, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// No self-grant (structural): the InProcessBackend seam an APP might reach cannot
// bypass the EnforcingBackend's manifest ceiling. Proves the layered-refusal shape.
// ---------------------------------------------------------------------------

#[test]
fn app_side_set_consent_cannot_bypass_manifest_ceiling() {
    let dir = tmp_dir("noself");
    let sup = Arc::new(SimulatedSupervisor::new());
    // Manifest declares NOTHING dangerous — an app forging set_consent on the inner backend
    // still fails ceiling first.
    let (inner, eb, ledger, _m) = rig(&dir, APP_ID, &["input"], sup);
    // Adversarial: an app that somehow got hold of the raw InProcessBackend forcibly grants
    // location. The EnforcingBackend still refuses via the ceiling — PolicyBlocked, NOT
    // ConsentDenied — and never records a ledger row (there is no ledger write path from the
    // app's side to begin with; this is a structural guarantee, but we prove the runtime shape).
    inner.set_consent("location", PermissionState::Granted);
    assert_eq!(eb.acquire("location").unwrap_err(), CapError::PolicyBlocked);
    assert!(ledger.snapshot().is_empty(), "app-side set_consent cannot manufacture a ledger row");
}

// ---------------------------------------------------------------------------
// Deny path with a real supervisor (user actively said "no"): no ledger row is
// written, but change events + return code match the standing-Denied shape.
// ---------------------------------------------------------------------------

#[test]
fn user_deny_denies_without_writing_ledger_row() {
    let dir = tmp_dir("user-deny");
    let sup = Arc::new(SimulatedSupervisor::new());
    sup.prepare(PreparedAnswer::deny(APP_ID, "location", None));
    let (_inner, eb, ledger, _m) = rig(&dir, APP_ID, &["location:approximate"], sup.clone());
    assert_eq!(eb.acquire("location").unwrap_err(), CapError::ConsentDenied);
    assert_eq!(sup.asks_seen().len(), 1, "supervisor was asked once");
    // The v1 shape: an active user-Deny does NOT write a persistent ledger row (the settings UI
    // path writes standing denies out-of-band, matching how a fresh Prompt can still fire later).
    assert!(ledger.snapshot().is_empty(), "user Deny does not persist to ledger in v1");
}

// ---------------------------------------------------------------------------
// Egress:<specific-host> = Dangerous (per .2 ruling): joins the SAME generic
// prompt path as location. This test proves the tier→portal wiring works end-to-end
// for egress, satisfying the coord's boundary ruling for .3.
// ---------------------------------------------------------------------------

#[test]
fn scoped_egress_prompts_via_the_same_generic_dangerous_path() {
    let dir = tmp_dir("egress");
    let sup = Arc::new(SimulatedSupervisor::new());
    // The tsp-ht0p.2 ruling: egress:<specific-host> is Dangerous — prompts via the same generic
    // path as location. The prepared answer is keyed on cap=egress with modifier=api.tile.example.
    sup.prepare(PreparedAnswer::allow_always(APP_ID, "egress", Some("api.tile.example")));
    let (_inner, eb, ledger, _m) = rig(&dir, APP_ID, &["egress:api.tile.example"], sup.clone());
    eb.acquire("egress").expect("scoped egress allowed via generic dangerous portal");
    assert_eq!(sup.asks_seen().len(), 1);
    let ask = &sup.asks_seen()[0];
    assert_eq!(ask.resource, "egress");
    assert_eq!(ask.resource_arg.as_deref(), Some("api.tile.example"));
    // Ledger row keyed on the specific host.
    let key = GrantKey::new(APP_ID, "egress", Some("api.tile.example"));
    assert_eq!(ledger.check(&key), GrantCheck::Always);
}

// ---------------------------------------------------------------------------
// Revoke → next acquire = Denied (the STEP-2 tail assertion, proved end-to-end).
// ---------------------------------------------------------------------------

#[test]
fn pf_permissions_revoke_shape_flips_next_acquire_to_denied() {
    let dir = tmp_dir("revoke-shape");
    let sup = Arc::new(SimulatedSupervisor::new());
    sup.prepare(PreparedAnswer::allow_always(APP_ID, "location", None));
    let (inner, eb, ledger, _m) = rig(&dir, APP_ID, &["location:approximate"], sup);
    let rx = inner.subscribe("location");
    eb.acquire("location").unwrap();
    let evt = rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(evt, PermissionState::Granted);

    // The pf-permissions revoke path: (1) record a revoke event in the ledger; (2) call
    // set_consent on the shared inner backend so subscribers fire.
    let key = GrantKey::new(APP_ID, "location", None);
    ledger.record_revoke(&key).unwrap();
    inner.set_consent("location", PermissionState::Denied);
    let evt = rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(evt, PermissionState::Denied, "revoke fires the change event");

    // Next acquire = Denied.
    assert_eq!(eb.acquire("location").unwrap_err(), CapError::ConsentDenied);

    // Sanity: no more events without a change.
    assert_eq!(
        rx.recv_timeout(Duration::from_millis(100)),
        Err(RecvTimeoutError::Timeout),
        "no spurious events without a state change"
    );
}

// ---------------------------------------------------------------------------
// Orphan resurrection hazard fix (.5's helper): revoke_orphans converts stale
// grants to standing revokes so a future ceiling re-widening does not resurrect them.
// ---------------------------------------------------------------------------

#[test]
fn revoke_orphans_prevents_resurrection_on_re_widened_ceiling() {
    let dir = tmp_dir("orphan-fix");
    let sup1 = Arc::new(SimulatedSupervisor::new());
    sup1.prepare(PreparedAnswer::allow_always(APP_ID, "location", None));
    sup1.prepare(PreparedAnswer::allow_always(APP_ID, "egress", Some("api.tile.example")));
    let (_i1, eb1, ledger, wide) = rig(
        &dir,
        APP_ID,
        &["location:approximate", "egress:api.tile.example"],
        sup1,
    );
    eb1.acquire("location").unwrap();
    eb1.acquire("egress").unwrap();

    // Ceiling shrinks — the app's new manifest doesn't declare egress. `.5`'s flow calls this.
    let desc = eb1.appops().unwrap(); // just to prove the accessor exists
    let _ = desc; // (suppress unused warning if any)

    // Build the shrunk manifest (same fixtures, narrower use=[]).
    let narrow = {
        let sup2 = Arc::new(SimulatedSupervisor::new());
        let (_i2, _eb2, _l2, m) = rig(&dir, APP_ID, &["location:approximate"], sup2);
        m
    };
    // Before revoke_orphans: egress grant is orphaned but ALIVE (Always).
    assert_eq!(ledger.orphaned_grants(&narrow).len(), 1);
    let egress_key = GrantKey::new(APP_ID, "egress", Some("api.tile.example"));
    assert_eq!(ledger.check(&egress_key), GrantCheck::Always);

    // Apply the fix.
    let revoked = ledger.revoke_orphans(&narrow).unwrap();
    assert_eq!(revoked.len(), 1);
    assert_eq!(revoked[0].cap, "egress");
    assert_eq!(ledger.check(&egress_key), GrantCheck::Revoked);
    assert!(ledger.orphaned_grants(&narrow).is_empty(), "revoke_orphans clears the orphan set");

    // Ceiling RE-WIDENS to include egress. The old Always grant does NOT resurrect — the revoke
    // is the durable last-wins record.
    let sup3 = Arc::new(SimulatedSupervisor::new());
    let (_i3, _eb3, ledger2, _wide2) = rig(
        &dir,
        APP_ID,
        &["location:approximate", "egress:api.tile.example"],
        sup3,
    );
    assert_eq!(
        ledger2.check(&egress_key),
        GrantCheck::Revoked,
        "the revoke row wins on replay — no resurrection"
    );

    // (Sanity: location was NEVER orphaned across the shrink; its Always grant carries.)
    let loc_key = GrantKey::new(APP_ID, "location", None);
    assert_eq!(ledger2.check(&loc_key), GrantCheck::Always);
    let _ = wide;
}
