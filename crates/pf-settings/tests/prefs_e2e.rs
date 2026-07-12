//! `tsp-xubv.4` — the E4 sim end-to-end proof + the unification CI matrix.
//!
//! This is the running-app leg the epic acceptance names: a payload app (built on the same
//! [`Pf`] facade a real app links) acquires rumble + subscribes to `PrefsDidChange` and observes
//! the flip in real time across BOTH write paths — the direct control-plane write and the
//! `pf-settings` CLI as a REAL EXTERNAL SUBPROCESS (the .2 ratified HONESTY RIDER). The
//! external-CLI leg is the load-bearing distinction from the .2 unit tests, which stand in for
//! the external process with a separate in-process [`PrefsStore`] handle; here the subprocess is
//! genuinely separate (`Command::new(env!("CARGO_BIN_EXE_pf-settings"))`), so the reload seam is
//! exercised across a real process boundary.
//!
//! The UNIFICATION is asserted with ZERO test-code diff: [`run_unified_scenario`] is one function
//! body; the a133-absent row and the a523-suppressed row differ ONLY in an argument (the
//! descriptor id + the diagnostic status expected on the honest-`RumbleStatus` layer). Both rows'
//! app-visible semantic — the [`PayloadApp`] sees a silent no-op after the flip, never a `Fired`
//! and never an error to handle — is asserted identically.
//!
//! CI matrix: `.github/workflows/prefs-e2e.yml` runs `cargo test -p pf-settings --test prefs_e2e`
//! with `matrix.descriptor: [a133, a523]` — the two green rows ARE the unification proof.

mod common;

use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::Duration;

use pf_prefs::{PrefValue, PrefsStore};
use pocketforge::backends::InProcessBackend;
use pocketforge::{OutputMix, Pf, RumbleStatus};

use common::{run_pf_settings, scratch_prefs_dir, PayloadApp};

/// Short wait budget for observer deliveries — the event fires synchronously on the writer
/// thread, so a large timeout only slows failures.
const OBSERVER_WAIT: Duration = Duration::from_secs(1);

// ---------------------------------------------------------------------------------------
// The two write paths — both fire the observer + honor the primitive on the SAME seam.
// ---------------------------------------------------------------------------------------

/// (a) The **control-plane** write path: an in-process write via the SettingsManager (the sim's
/// injection-as-API surface, also what the `.3` UI drives in v0). One process; the observer fires
/// directly; the store persists.
#[test]
fn a523_control_plane_flip_fires_observer_and_suppresses_pulse() {
    let prefs_dir = scratch_prefs_dir("a523-ctrl");
    let store = Arc::new(PrefsStore::at(&prefs_dir));
    let backend = InProcessBackend::shared_with_store(Arc::new(common::descriptor("a523")), store);
    let pf = Pf::over_in_process(backend.clone());
    let app = PayloadApp::start(&pf, "hapticsEnabled").expect("observer available");

    // Default on, motor present → the primitive fires.
    assert_eq!(app.pulse(), RumbleStatus::Fired);

    // The running app's own SettingsManager is READ-only-to-apps; the CONTROL PLANE (the sim's
    // injection-as-API seam) is what actually drives the write. Same in-process facade.
    pf.settings().set_bool("hapticsEnabled", false);

    // Observer fires in the RUNNING app — this is the "reacts live" property under the running
    // payload, not just a store snapshot.
    let evt = app
        .observed(OBSERVER_WAIT)
        .expect("PrefsDidChange delivered to the payload app");
    assert_eq!(evt, PrefValue::Bool(false));

    // Next pulse: the primitive reads the flipped preference, silently no-ops, NOT `Fired`. The
    // *diagnostic* layer honestly reports `NoopSuppressed` (the a523 half of the unification).
    let after = app.pulse();
    assert_ne!(after, RumbleStatus::Fired, "primitive must not fire under suppression");
    assert_eq!(after, RumbleStatus::NoopSuppressed);

    // Flip back → observer fires again, primitive fires again.
    pf.settings().set_bool("hapticsEnabled", true);
    let evt = app.observed(OBSERVER_WAIT).expect("re-enable event delivered");
    assert_eq!(evt, PrefValue::Bool(true));
    assert_eq!(app.pulse(), RumbleStatus::Fired);

    common::cleanup(&prefs_dir);
}

/// (b) The **external-process CLI** write path — the ratified `.2` HONESTY RIDER — driven by a
/// **real subprocess**: `Command::new(env!("CARGO_BIN_EXE_pf-settings"))`. The running app learns
/// of the change via `reload_prefs()` (the v0 supervisor-file-watch stand-in), then observes it.
///
/// This is the load-bearing E2E distinction from the .2 unit tests: those stand in for the
/// external writer with a second `PrefsStore` handle in the same process; here the writer is a
/// separate process, so the reload seam is exercised across a real process boundary.
#[test]
fn a523_external_cli_flip_fires_observer_via_reload_and_suppresses_pulse() {
    let prefs_dir = scratch_prefs_dir("a523-cli");
    let store = Arc::new(PrefsStore::at(&prefs_dir));
    let backend =
        InProcessBackend::shared_with_store(Arc::new(common::descriptor("a523")), store.clone());
    let pf = Pf::over_in_process(backend.clone());
    let app = PayloadApp::start(&pf, "hapticsEnabled").expect("observer available");

    assert_eq!(app.pulse(), RumbleStatus::Fired);

    // Real external process write via the built pf-settings binary — no in-process shortcut.
    let out = run_pf_settings(&prefs_dir, &["set", "hapticsEnabled", "false"]);
    assert!(
        out.status.success(),
        "pf-settings set failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    // The disk-level write happened (the CLI walks parse → apply → persist).
    assert!(!store.load().unwrap().haptics_enabled(), "disk value must have flipped");

    // Until the running app reloads, its in-process cache is unchanged → no spurious event; and
    // the primitive still fires (the v0 store is not shared memory).
    assert_eq!(
        app.try_observe(Duration::from_millis(150)),
        Err(RecvTimeoutError::Timeout),
        "no PrefsDidChange must fire before the host reloads (v0 semantics)",
    );
    assert_eq!(app.pulse(), RumbleStatus::Fired, "pre-reload primitive still sees the cache");

    // The host learns of the change → reload fires the observer AND updates the in-memory cache
    // that the primitive reads. The RIDER leg proved end-to-end across a real process boundary.
    backend.reload_prefs();
    let evt = app.observed(OBSERVER_WAIT).expect("reload delivers PrefsDidChange");
    assert_eq!(evt, PrefValue::Bool(false));
    let after = app.pulse();
    assert_ne!(after, RumbleStatus::Fired);
    assert_eq!(after, RumbleStatus::NoopSuppressed);

    common::cleanup(&prefs_dir);
}

// ---------------------------------------------------------------------------------------
// The a133 half — motor ABSENT. The observer STILL fires on any preference write (the flag is
// user-mutable state independent of hardware presence) but the primitive stays a silent no-op
// with the DIAGNOSTIC honestly reporting `NoopAbsent`, not `NoopSuppressed`.
// ---------------------------------------------------------------------------------------

#[test]
fn a133_control_plane_flip_fires_observer_and_pulse_is_noop_absent_throughout() {
    let prefs_dir = scratch_prefs_dir("a133-ctrl");
    let store = Arc::new(PrefsStore::at(&prefs_dir));
    let backend = InProcessBackend::shared_with_store(Arc::new(common::descriptor("a133")), store);
    let pf = Pf::over_in_process(backend.clone());
    let app = PayloadApp::start(&pf, "hapticsEnabled").expect("observer available");

    // No motor: the primitive is a silent no-op regardless of the preference.
    assert_eq!(app.pulse(), RumbleStatus::NoopAbsent);

    // The observer still fires — the preference is user-mutable state, hardware-presence is a
    // separate axis. This is what makes a shared payload-app snippet legal on both descriptors.
    pf.settings().set_bool("hapticsEnabled", false);
    let evt = app.observed(OBSERVER_WAIT).expect("PrefsDidChange delivered");
    assert_eq!(evt, PrefValue::Bool(false));

    // The primitive: SAME app-visible semantic (silent no-op, not Fired). Diagnostic still Absent.
    let after = app.pulse();
    assert_ne!(after, RumbleStatus::Fired);
    assert_eq!(after, RumbleStatus::NoopAbsent);

    common::cleanup(&prefs_dir);
}

#[test]
fn a133_external_cli_flip_fires_observer_via_reload_and_pulse_is_noop_absent_throughout() {
    let prefs_dir = scratch_prefs_dir("a133-cli");
    let store = Arc::new(PrefsStore::at(&prefs_dir));
    let backend =
        InProcessBackend::shared_with_store(Arc::new(common::descriptor("a133")), store.clone());
    let pf = Pf::over_in_process(backend.clone());
    let app = PayloadApp::start(&pf, "hapticsEnabled").expect("observer available");

    assert_eq!(app.pulse(), RumbleStatus::NoopAbsent);

    let out = run_pf_settings(&prefs_dir, &["set", "hapticsEnabled", "false"]);
    assert!(out.status.success(), "pf-settings set failed");
    assert!(!store.load().unwrap().haptics_enabled());

    assert_eq!(
        app.try_observe(Duration::from_millis(150)),
        Err(RecvTimeoutError::Timeout),
        "no pre-reload event",
    );

    backend.reload_prefs();
    let evt = app.observed(OBSERVER_WAIT).expect("reload delivers PrefsDidChange");
    assert_eq!(evt, PrefValue::Bool(false));
    let after = app.pulse();
    assert_ne!(after, RumbleStatus::Fired);
    assert_eq!(after, RumbleStatus::NoopAbsent);

    common::cleanup(&prefs_dir);
}

// ---------------------------------------------------------------------------------------
// THE UNIFICATION — a133-absent ≡ a523-suppressed under ZERO app/test-code diff.
//
// `run_unified_scenario` is ONE function body; the a133 and a523 rows differ only in a parameter
// (descriptor id + the honest diagnostic expected). Two `#[test]`s call it — no branch on the
// descriptor inside the payload code; the "different device" fact falls out of the descriptor
// data. The CI matrix (`.github/workflows/prefs-e2e.yml`) runs the whole file twice; the two
// green rows ARE the unification proof.
// ---------------------------------------------------------------------------------------

fn run_unified_scenario(descriptor_id: &str, expected_off_diagnostic: RumbleStatus) {
    let prefs_dir = scratch_prefs_dir(&format!("unif-{descriptor_id}"));
    let store = Arc::new(PrefsStore::at(&prefs_dir));
    let backend =
        InProcessBackend::shared_with_store(Arc::new(common::descriptor(descriptor_id)), store);
    let pf = Pf::over_in_process(backend.clone());
    let app = PayloadApp::start(&pf, "hapticsEnabled").expect("observer available");

    // Same call, either descriptor. The application layer never branches; it just calls pulse.
    let _before = app.pulse();

    // Same call, either descriptor: the user flips the preference to off. Sim's injection-as-API.
    pf.settings().set_bool("hapticsEnabled", false);

    // Same call, either descriptor: the observer fires — the running app reacts live regardless of
    // whether the hardware would have actuated.
    let evt = app.observed(OBSERVER_WAIT).expect("PrefsDidChange under identical call code");
    assert_eq!(evt, PrefValue::Bool(false));

    // Same call, either descriptor: the primitive is a silent no-op (NEVER `Fired`, NEVER an error
    // the app must handle) — the app-visible semantic collapse the epic promises.
    let after = app.pulse();
    assert_ne!(after, RumbleStatus::Fired, "app-visible semantic must be silent no-op");

    // The DIAGNOSTIC layer (frozen discriminants, deliberate honesty) still distinguishes WHY,
    // for surfaces like pf-hwprobe — WITHOUT forking the app-visible behavior above.
    assert_eq!(after, expected_off_diagnostic, "honest diagnostic per descriptor row");

    common::cleanup(&prefs_dir);
}

/// The a133 row of the unification matrix — motor ABSENT, `NoopAbsent`.
#[test]
fn unified_zero_code_diff_a133_absent() {
    run_unified_scenario("a133", RumbleStatus::NoopAbsent);
}

/// The a523 row of the unification matrix — motor PRESENT + user disabled → `NoopSuppressed`.
#[test]
fn unified_zero_code_diff_a523_suppressed() {
    run_unified_scenario("a523", RumbleStatus::NoopSuppressed);
}

// ---------------------------------------------------------------------------------------
// The other v1 preferences — observed + honored on the seams the merged .2 designated.
// One representative test per preference. `mono_audio` uses the EXTERNAL-CLI path (rider
// coverage for a non-haptics key); `reduce_motion` uses the control plane (v0 has no
// machinery to suppress — the seam is documented, and we assert observe + read-flip);
// `brightness` is contract-only (read + observer, no sysfs).
// ---------------------------------------------------------------------------------------

#[test]
fn mono_audio_flip_via_external_cli_flips_routing_layer() {
    let prefs_dir = scratch_prefs_dir("mono-cli");
    let store = Arc::new(PrefsStore::at(&prefs_dir));
    let backend =
        InProcessBackend::shared_with_store(Arc::new(common::descriptor("a523")), store);
    let pf = Pf::over_in_process(backend.clone());
    let app = PayloadApp::start(&pf, "monoAudio").expect("observer available");

    assert_eq!(pf.audio().output_mix(), OutputMix::Stereo);

    let out = run_pf_settings(&prefs_dir, &["set", "monoAudio", "true"]);
    assert!(out.status.success());

    backend.reload_prefs();
    let evt = app.observed(OBSERVER_WAIT).expect("monoAudio observed via reload");
    assert_eq!(evt, PrefValue::Bool(true));
    // The routing-layer semantic honored (docs/PREFERENCES.md §4: sim-visible; real DSP is post-v0).
    assert_eq!(pf.audio().output_mix(), OutputMix::Mono);

    common::cleanup(&prefs_dir);
}

#[test]
fn reduce_motion_flip_via_control_plane_fires_observer_and_read_flips() {
    let prefs_dir = scratch_prefs_dir("reducemotion-ctrl");
    let store = Arc::new(PrefsStore::at(&prefs_dir));
    let backend =
        InProcessBackend::shared_with_store(Arc::new(common::descriptor("a523")), store);
    let pf = Pf::over_in_process(backend.clone());
    let app = PayloadApp::start(&pf, "reduceMotion").expect("observer available");

    assert!(!pf.settings().reduce_motion(), "reduceMotion defaults off");

    // v0 has NO cosmetic-motion machinery to suppress (docs/PREFERENCES.md §4) — assert the
    // documented seam: observe + read-flip. Suppressor machinery is a future additive consumer.
    pf.settings().set_bool("reduceMotion", true);
    let evt = app.observed(OBSERVER_WAIT).expect("reduceMotion observed");
    assert_eq!(evt, PrefValue::Bool(true));
    assert!(pf.settings().reduce_motion());

    common::cleanup(&prefs_dir);
}

#[test]
fn brightness_is_contract_only_readable_and_observable_via_external_cli() {
    // Owner ruling Q3: brightness is contract-only in v1 — a readable + observable scalar, and
    // deliberately NO sysfs apply leg anywhere in this epic. We prove the read + the observer
    // fires; a hardware-apply assertion would be a scope violation.
    let prefs_dir = scratch_prefs_dir("brightness-cli");
    let store = Arc::new(PrefsStore::at(&prefs_dir));
    let backend =
        InProcessBackend::shared_with_store(Arc::new(common::descriptor("a523")), store);
    let pf = Pf::over_in_process(backend.clone());
    let app = PayloadApp::start(&pf, "brightness").expect("observer available");

    assert_eq!(pf.settings().brightness(), 100);

    let out = run_pf_settings(&prefs_dir, &["set", "brightness", "40"]);
    assert!(out.status.success());

    backend.reload_prefs();
    let evt = app.observed(OBSERVER_WAIT).expect("brightness observed via reload");
    assert_eq!(evt, PrefValue::Scalar(40));
    assert_eq!(pf.settings().brightness(), 40);

    common::cleanup(&prefs_dir);
}
