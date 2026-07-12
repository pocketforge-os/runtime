//! tsp-xubv.3 — settings-UI **liveness** harness.
//!
//! Proves the behavioral half of the settings UI: a toggle written through the
//! **pf-prefs authority path** flips a **running, already-subscribed** app WITHOUT
//! restarting it — the owner's "turn haptics off mid-session" case. The render +
//! navigation half lives in `settings-render.c` + `driver.py`; this binary drives
//! the REAL merged `.1` store + `.2` observer (`pf-prefs`, `pocketforge`) through
//! their PUBLIC API only, mirroring `crates/pocketforge/tests/prefs_change_event.rs`.
//!
//! It exercises BOTH write paths the `.2` honesty rider ratified:
//!   1. EXTERNAL-process write (the prototype's true model: the C/Python UI is a
//!      separate process) — `PrefsStore::apply` (the same seam `pf-settings` uses,
//!      owner Q1) then observed via `reload_prefs()`.
//!   2. CONTROL-PLANE write (the supervisor-integrated future) — `set_preference`,
//!      fired to the observer directly.
//!
//! Plus: store round-trip, the brightness scalar (Q3 contract-only — round-trips
//! store + observer, no hardware effect asserted), and the a133 honest-absent leg
//! (the primitive collapses absence and suppression to one silent no-op).
//!
//! Every assertion prints a `CHECK` line; any failure makes the process exit 1.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::Duration;

use pf_prefs::{PrefValue, PrefsStore};
use pocketforge::backends::InProcessBackend;
use pocketforge::{Backend, Descriptor, Pf, RumbleStatus};

static FAILS: AtomicU32 = AtomicU32::new(0);
static CHECKS: AtomicU32 = AtomicU32::new(0);

fn check(name: &str, cond: bool) {
    CHECKS.fetch_add(1, Ordering::SeqCst);
    if cond {
        println!("CHECK ok   : {name}");
    } else {
        FAILS.fetch_add(1, Ordering::SeqCst);
        println!("CHECK FAIL : {name}");
    }
}

/// The committed E1 descriptor fixtures (a133 omits `rumble`; a523 has it). Resolved
/// from the crate dir at compile time so cwd does not matter. Read-only consumption —
/// this harness never edits `crates/*`.
fn descriptor(id: &str) -> Descriptor {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../crates/pocketforge/tests/fixtures")
        .join(format!("{id}-capabilities.toml"));
    Descriptor::load(&path)
        .unwrap_or_else(|e| panic!("load descriptor {id} from {}: {e}", path.display()))
}

/// A scratch prefs dir unique to this process + tag (mirrors the `.2` test convention;
/// no external temp-crate dep).
fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pf-xubv3-live-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn main() {
    println!("=== tsp-xubv.3 settings-UI liveness harness ===");

    liveness_haptics_external_reload();
    liveness_haptics_control_plane();
    store_round_trip();
    brightness_scalar_contract_only();
    a133_honest_absent();

    let checks = CHECKS.load(Ordering::SeqCst);
    let fails = FAILS.load(Ordering::SeqCst);
    println!("=== summary: {} checks, {} failures ===", checks, fails);
    if fails > 0 {
        std::process::exit(1);
    }
}

/// THE headline liveness proof (external-process write → reload seam): the UI writes
/// through `PrefsStore::apply` in a separate process, a running subscribed app reacts
/// on `reload_prefs()`, no restart. This is the prototype's true path (owner rider).
fn liveness_haptics_external_reload() {
    println!("\n-- liveness: haptics OFF/ON via external write + reload (no restart) --");
    let dir = scratch("haptics-reload");
    let store = Arc::new(PrefsStore::at(&dir));
    let backend = InProcessBackend::shared_with_store(Arc::new(descriptor("a523")), store.clone());
    let pf = Pf::over_in_process(backend.clone());

    // A running app subscribes BEFORE any change and is NOT restarted for the rest of this fn.
    let rx = backend.subscribe_preference("hapticsEnabled");
    check("a523 default: pulse Fired", pf.vibration().pulse(40) == RumbleStatus::Fired);

    // The settings UI (a separate process) writes through the SAME authority seam the
    // pf-settings CLI uses. A separate PrefsStore handle over the same dir stands in for it.
    let ui_writer = PrefsStore::at(&dir);
    ui_writer.apply("hapticsEnabled", PrefValue::Bool(false)).expect("authority write ok");

    // Until the host reloads, the running app's cache is unchanged -> no spurious event.
    check(
        "no event before reload (external write not yet observed)",
        rx.recv_timeout(Duration::from_millis(100)) == Err(RecvTimeoutError::Timeout),
    );

    // Host learns of the change and reloads -> observer fires AND the primitive flips LIVE.
    backend.reload_prefs();
    let evt = rx.recv_timeout(Duration::from_secs(1));
    check("reload fires PrefsDidChange(false)", evt == Ok(PrefValue::Bool(false)));
    check(
        "running app's next pulse flips to NoopSuppressed (no restart)",
        pf.vibration().pulse(40) == RumbleStatus::NoopSuppressed,
    );

    // Toggle back through the same path -> live re-enable.
    ui_writer.apply("hapticsEnabled", PrefValue::Bool(true)).expect("authority write ok");
    backend.reload_prefs();
    let evt = rx.recv_timeout(Duration::from_secs(1));
    check("reload fires PrefsDidChange(true)", evt == Ok(PrefValue::Bool(true)));
    check(
        "running app's next pulse flips back to Fired (no restart)",
        pf.vibration().pulse(40) == RumbleStatus::Fired,
    );
}

/// The control-plane path (the supervisor-integrated future): `set_preference` fires the
/// observer directly, no reload needed. Proven ahead of that integration.
fn liveness_haptics_control_plane() {
    println!("\n-- liveness: haptics via control-plane write (direct observer) --");
    let dir = scratch("haptics-ctrl");
    let store = Arc::new(PrefsStore::at(&dir));
    let backend = InProcessBackend::shared_with_store(Arc::new(descriptor("a523")), store);
    let pf = Pf::over_in_process(backend.clone());

    let rx = backend.subscribe_preference("hapticsEnabled");
    check("a523 default: pulse Fired", pf.vibration().pulse(40) == RumbleStatus::Fired);

    backend.set_preference_bool("hapticsEnabled", false);
    let evt = rx.recv_timeout(Duration::from_secs(1));
    check("control-plane write fires observer(false) directly", evt == Ok(PrefValue::Bool(false)));
    check("pulse NoopSuppressed", pf.vibration().pulse(40) == RumbleStatus::NoopSuppressed);

    backend.set_preference_bool("hapticsEnabled", true);
    let evt = rx.recv_timeout(Duration::from_secs(1));
    check("control-plane write fires observer(true) directly", evt == Ok(PrefValue::Bool(true)));
    check("pulse Fired again", pf.vibration().pulse(40) == RumbleStatus::Fired);
}

/// The authority write PERSISTS (owner Q2 store) — an independent handle reads it back,
/// exactly what `pf-settings get` (or a fresh session) sees.
fn store_round_trip() {
    println!("\n-- store round-trip: authority write persists to disk --");
    let dir = scratch("round-trip");
    let ui_writer = PrefsStore::at(&dir);
    ui_writer.apply("hapticsEnabled", PrefValue::Bool(false)).expect("write ok");
    ui_writer.apply("brightness", PrefValue::Scalar(40)).expect("write ok");

    let reader = PrefsStore::at(&dir);
    let prefs = reader.load().expect("load ok");
    check("persisted hapticsEnabled == false", !prefs.haptics_enabled());

    // A brand-new backend over the same store honors the persisted value from init (no writes).
    let store = Arc::new(PrefsStore::at(&dir));
    let backend = InProcessBackend::shared_with_store(Arc::new(descriptor("a523")), store);
    let pf = Pf::over_in_process(backend);
    check(
        "fresh session honors persisted suppression at the primitive",
        pf.vibration().pulse(40) == RumbleStatus::NoopSuppressed,
    );
    check("fresh session reads persisted brightness == 40", pf.settings().brightness() == 40);
}

/// Brightness (owner Q3 CONTRACT-ONLY): the scalar round-trips store + observer, with NO
/// hardware apply leg (that is the hardware-gated follow-on tsp-xubv.5). We assert the read
/// + the observer ONLY — deliberately no hardware effect.
fn brightness_scalar_contract_only() {
    println!("\n-- brightness scalar: adjust + persist + observe (no hardware apply) --");
    let dir = scratch("brightness");
    let store = Arc::new(PrefsStore::at(&dir));
    let backend = InProcessBackend::shared_with_store(Arc::new(descriptor("a523")), store);
    let pf = Pf::over_in_process(backend.clone());

    check("brightness default 100", pf.settings().brightness() == 100);
    let rx = backend.subscribe_preference("brightness");

    // The UI's L/R-adjust writes a Scalar through the authority seam (control-plane form here).
    backend.set_preference("brightness", PrefValue::Scalar(40));
    let evt = rx.recv_timeout(Duration::from_secs(1));
    check("brightness change observed(40)", evt == Ok(PrefValue::Scalar(40)));
    check("facade reads brightness == 40 (contract-only, no sysfs)", pf.settings().brightness() == 40);
}

/// a133 honest-absent leg: the descriptor has NO rumble actuator, so the settings UI renders
/// the Haptics row honest-absent (non-focus-stop; see settings-render.c/driver.py) and the
/// primitive is NoopAbsent regardless of any haptics preference — absence and suppression
/// collapse to the SAME silent no-op (the E4 unification, presence half).
fn a133_honest_absent() {
    println!("\n-- a133 honest-absent: rumble absent -> NoopAbsent regardless of preference --");
    let dir = scratch("a133");
    let store = Arc::new(PrefsStore::at(&dir));
    let backend = InProcessBackend::shared_with_store(Arc::new(descriptor("a133")), store);
    let pf = Pf::over_in_process(backend.clone());

    check("a133 rumble NOT present in descriptor", !pf.backend().is_present("rumble"));
    check("a133 pulse NoopAbsent at default", pf.vibration().pulse(40) == RumbleStatus::NoopAbsent);

    // Even if a haptics preference were written (the UI would never let you — the row is not a
    // focus stop), the primitive stays NoopAbsent: absence dominates. Same silent no-op as a523
    // suppression, distinguished only by the frozen diagnostic discriminant.
    backend.set_preference_bool("hapticsEnabled", false);
    check(
        "a133 pulse stays NoopAbsent after a haptics write (absence dominates)",
        pf.vibration().pulse(40) == RumbleStatus::NoopAbsent,
    );
    backend.set_preference_bool("hapticsEnabled", true);
    check(
        "a133 pulse still NoopAbsent (never Fired — no motor)",
        pf.vibration().pulse(40) == RumbleStatus::NoopAbsent,
    );
}
