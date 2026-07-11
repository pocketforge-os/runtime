//! **Kickoff STEP-3 harness** (`tsp-ht0p.4`) — the four scripted assertions the bead work order
//! requires as merge-blocking evidence:
//!
//!   1. drive location reads past quota ⇒ throttled typed error; advance the FAKE clock ⇒
//!      refill, reads succeed. (Wall-clock token-bucket semantics — refill is a pure function
//!      of the elapsed nanos, no sleeps.)
//!   2. location READ does NOT consume the egress bucket (independence preserved from the
//!      merged `buckets_are_independent`).
//!   3. egress send to an UNDECLARED host is refused + logged; declared-host send consumes the
//!      byte-quota (per-host log) + op-bucket.
//!   4. ledger + egress-log dump asserted in the harness (the STEP-3 transcript that lands in
//!      the bead is the printed form of these dumps).
//!
//! Every assertion here is COOPERATIVE ACCOUNTING (owner Q1 ruling, 2026-07-11: v1 =
//! accounting + declaration + quota CONTRACT; enforcement seam design at
//! `docs/EGRESS-ENFORCEMENT-SEAM.md`, follow-on bead filed on merge). R-A honesty is baked
//! into the assertions: the harness demonstrates the CONTRACT surface, never claims
//! kernel-level teeth.

use std::sync::Arc;

use pocketforge::backends::InProcessBackend;
use pocketforge::managers::{
    EgressEventKind, EgressLog, EgressManager, ManualClock, QuotaLedger, EGRESS_QUOTA,
    EGRESS_REFILL_PER_SEC, LOCATION_READ_QUOTA, LOCATION_READ_REFILL_PER_SEC,
};
use pocketforge::{Backend, CapError, Descriptor, PermissionState};

use pf_broker::{AppManifest, EnforcingBackend};

/// A synthetic descriptor advertising GNSS (for location) and a rumble motor (for the vibration
/// no-op tier), matching the shape merged into `broker.rs` for the .2 STEP-1 tests. No
/// `iio_device` — the `location` presence derives from the `[[sensors]] kind="gnss"` row.
fn desc_gnss_and_rumble() -> Descriptor {
    Descriptor::from_toml(
        "[identity]\nid=\"step3\"\nmanufacturer=\"x\"\nmodel=\"y\"\nsdl_guid=\"00000000000000000000000000000000\"\n\
         [[inputs]]\nid=\"south\"\nkind=\"button\"\nev_type=\"EV_KEY\"\ncode=\"BTN_A\"\n\
         [[sensors]]\nid=\"gnss\"\nkind=\"gnss\"\n\
         [[actuators]]\nid=\"rumble\"\nkind=\"rumble\"\nev_type=\"EV_FF\"\ncode=\"FF_RUMBLE\"\nsysfs=\"pwm-vibrator\"\n",
    )
    .expect("synthetic descriptor parses")
}

fn manifest(uses: &[&str]) -> AppManifest {
    let toml = format!(
        "[app]\nid = \"com.test.step3\"\nuse = [{}]\n",
        uses.iter().map(|u| format!("\"{u}\"")).collect::<Vec<_>>().join(", ")
    );
    AppManifest::from_toml(&toml).expect("parse app.toml")
}

fn tmp_dir(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "pf-step3-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

// -------------------------------------------------------------------------------------------------
// Assertion (1) — location reads past quota throttle, then refill.
// -------------------------------------------------------------------------------------------------

#[test]
fn step3_1_location_reads_throttle_then_refill_under_wall_clock() {
    // Bead STEP-3 assertion (1): drive location reads past quota ⇒ throttled typed error;
    // advance the FAKE clock ⇒ bucket refills, reads succeed again.
    let desc = Arc::new(desc_gnss_and_rumble());
    let inner = InProcessBackend::shared(desc.clone());
    // Consent granted (E3 overlay) so the quota is the ONLY throttle in the picture.
    inner.set_consent("location", PermissionState::Granted);
    let validated = manifest(&["location:approximate"]).validate(&desc).unwrap();

    // The wall-clock quota ledger over a fake clock (STEP-3 requirement: no sleeps).
    let clock = ManualClock::new();
    let quotas = Arc::new(QuotaLedger::with_clock(clock.clone()));
    // Seed the location bucket at 2 tokens so the fifth acquire drains-then-throttles quickly.
    quotas.set_remaining("location", 2);
    let eb = EnforcingBackend::with_quotas(inner.clone(), &validated, quotas.clone());

    // Two acquires drain the bucket; the third is the typed throttle error.
    assert!(eb.acquire("location").is_ok(), "1st acquire under budget");
    assert!(eb.acquire("location").is_ok(), "2nd acquire under budget");
    assert_eq!(
        eb.acquire("location").err(),
        Some(CapError::PolicyBlocked),
        "3rd acquire throttled (typed PolicyBlocked, not a bool false)"
    );

    // Advance the FAKE clock by five wall-clock seconds — at 1/sec that refills 5 tokens
    // (capped at capacity, but the drain was to 0 so we get 5).
    clock.advance_secs(5);
    assert_eq!(quotas.remaining("location"), 5, "refill = advance × rate");

    // Reads succeed again — the throttle heals over wall time, matching the token-bucket
    // contract.
    for _ in 0..5 {
        assert!(eb.acquire("location").is_ok(), "post-refill acquire succeeds");
    }
    assert_eq!(
        eb.acquire("location").err(),
        Some(CapError::PolicyBlocked),
        "refilled tokens drain again"
    );
    // Sanity: the rate constant is what the docs say (guards a silent rate-change regression).
    assert_eq!(LOCATION_READ_REFILL_PER_SEC, 1.0);
}

// -------------------------------------------------------------------------------------------------
// Assertion (2) — location READ does NOT consume the egress bucket (independence).
// -------------------------------------------------------------------------------------------------

#[test]
fn step3_2_location_read_never_touches_egress_bucket() {
    // Bead STEP-3 assertion (2): a location READ does NOT consume the egress bucket
    // (independence preserved — the load-bearing invariant carried from merged
    // `buckets_are_independent`, now under the token-bucket implementation).
    let desc = Arc::new(desc_gnss_and_rumble());
    let inner = InProcessBackend::shared(desc.clone());
    inner.set_consent("location", PermissionState::Granted);
    let validated = manifest(&["location:approximate", "egress:tile.example"]).validate(&desc).unwrap();
    let clock = ManualClock::new();
    let quotas = Arc::new(QuotaLedger::with_clock(clock.clone()));
    let eb = EnforcingBackend::with_quotas(inner.clone(), &validated, quotas.clone());

    let egress_before = quotas.remaining("egress");
    assert_eq!(egress_before, EGRESS_QUOTA);

    // Drain a bunch of location reads.
    for _ in 0..10 {
        assert!(eb.acquire("location").is_ok());
    }
    assert_eq!(
        quotas.remaining("egress"),
        egress_before,
        "location reads NEVER leaked into the egress bucket"
    );

    // And vice-versa: consume egress ops (via the accounting-layer manager, since the
    // enforce-path acquire is a per-op consume too, but the STEP-3 focus is the read≠send
    // independence at the ledger).
    let log = Arc::new(EgressLog::open(tmp_dir("step3-2")).unwrap());
    let acct = EgressManager::accounting(
        "com.test.step3",
        ["tile.example".to_string()],
        log.clone(),
    );
    let em = EgressManager::with_accounting(quotas.clone(), acct);
    let loc_before = quotas.remaining("location");
    em.send("tile.example", 128).expect("declared send accounts");
    em.send("tile.example", 256).expect("declared send accounts");
    assert_eq!(
        quotas.remaining("location"),
        loc_before,
        "egress send NEVER leaked into the location bucket"
    );
    assert_eq!(quotas.remaining("egress"), EGRESS_QUOTA - 2);
}

// -------------------------------------------------------------------------------------------------
// Assertion (3) — egress send to an UNDECLARED host is refused + logged; declared-host send
// consumes the byte quota + records a `send` row.
// -------------------------------------------------------------------------------------------------

#[test]
fn step3_3_egress_undeclared_host_refused_and_logged_declared_host_accounted() {
    // Bead STEP-3 assertion (3): a send to an UNDECLARED host is REFUSED + LOGGED; a
    // declared-host send consumes byte quota + records the byte-ledger row.
    let dir = tmp_dir("step3-3");
    let log = Arc::new(EgressLog::open(&dir).unwrap());
    let quotas = Arc::new(QuotaLedger::new());
    let em = EgressManager::with_accounting(
        quotas.clone(),
        EgressManager::accounting(
            "com.test.step3",
            ["tile.example".to_string()],
            log.clone(),
        ),
    );

    // (a) UNDECLARED host: refused with typed PolicyBlocked; NO op-token consumed; a `refused`
    // row is appended to the persistent log.
    let ops_before = quotas.remaining("egress");
    assert_eq!(
        em.send("evil.example", 4096).err(),
        Some(CapError::PolicyBlocked),
        "undeclared host is refused (typed PolicyBlocked)"
    );
    assert_eq!(
        quotas.remaining("egress"),
        ops_before,
        "refused send spends NO op token (accounting-honest refusal)"
    );

    // (b) DECLARED host: accepted; op token consumed; `send` row appended with the byte count.
    let receipt = em.send("tile.example", 1500).expect("declared send accounts");
    assert_eq!(receipt.host, "tile.example");
    assert_eq!(receipt.bytes, 1500);
    assert_eq!(quotas.remaining("egress"), EGRESS_QUOTA - 1);

    // The persistent log now has one refused row + one send row.
    let events = log.snapshot_for("com.test.step3").unwrap();
    assert_eq!(events.len(), 2, "log has [refused, send]");
    assert_eq!(events[0].event, EgressEventKind::Refused);
    assert_eq!(events[0].host, "evil.example");
    assert_eq!(
        events[0].reason.as_deref(),
        Some("host evil.example not declared in use=[]")
    );
    assert_eq!(events[1].event, EgressEventKind::Send);
    assert_eq!(events[1].host, "tile.example");
    assert_eq!(events[1].bytes, 1500);
}

// -------------------------------------------------------------------------------------------------
// Assertion (4) — ledger + egress-log dump asserted in the harness (the STEP-3 transcript).
// -------------------------------------------------------------------------------------------------

#[test]
fn step3_4_ledger_and_egress_log_dumps_survive_a_reopen() {
    // Bead STEP-3 assertion (4): the ledger + egress-log dump asserted in the harness.
    // Fresh EgressLog::open replays the append-only records — same JSONL dialect as the AppOps
    // ledger, same fail-loud posture on malformed lines. Prove replay + per-host rollup here so
    // pf-permissions egress reads the same view a running broker would.
    let dir = tmp_dir("step3-4");

    // Write events through one manager…
    {
        let log = Arc::new(EgressLog::open(&dir).unwrap());
        let quotas = Arc::new(QuotaLedger::new());
        let em = EgressManager::with_accounting(
            quotas.clone(),
            EgressManager::accounting(
                "com.test.step3",
                ["tile.example".to_string(), "api.map.example".to_string()],
                log.clone(),
            ),
        );
        em.send("tile.example", 100).unwrap();
        em.send("api.map.example", 250).unwrap();
        em.send("tile.example", 400).unwrap();
        let _ = em.send("evil.example", 1); // refused, logged
    }

    // …then replay from disk with a FRESH EgressLog (the pf-permissions egress code path).
    let log = EgressLog::open(&dir).unwrap();
    let events = log.snapshot_for("com.test.step3").unwrap();
    assert_eq!(events.len(), 4, "3 sends + 1 refused survived replay");
    let sends: Vec<_> = events.iter().filter(|e| e.event == EgressEventKind::Send).collect();
    assert_eq!(sends.len(), 3);
    // Rollup — the surface pf-permissions egress prints.
    let totals = pocketforge::managers::egress_log::total_bytes_per_host(&events);
    assert_eq!(totals.get("tile.example"), Some(&500));
    assert_eq!(totals.get("api.map.example"), Some(&250));
    let refusals = pocketforge::managers::egress_log::refusals_per_host(&events);
    assert_eq!(refusals.get("evil.example"), Some(&1));
    assert_eq!(refusals.get("tile.example"), None, "declared host is never in refusals");

    // The rate + capacity constants are the printed contract (guards silent tuning drifts).
    assert_eq!(EGRESS_QUOTA, 16);
    assert_eq!(EGRESS_REFILL_PER_SEC, 0.25);
    assert_eq!(LOCATION_READ_QUOTA, 60);
}
