//! The E4 `PrefsDidChange` **observer** + store-backed at-the-primitive honoring (`tsp-xubv.2`).
//!
//! Mirrors `change_event.rs` (the permission-query change event) for the preference bus: a
//! subscriber observes a preference transition, and the transition is honored AT the primitive
//! (rumble) via the SAME no-op shape as missing hardware. Both the CONTROL-PLANE write path and
//! the EXTERNAL-process (`pf-settings` CLI) write-via-reload path are exercised — the reload seam
//! is PART of the "observer fires on any write path" story, not an exemption from it
//! (owner-ratified 2026-07-11).

mod common;

use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::Duration;

use pf_prefs::{PrefValue, PrefsStore};
use pocketforge::backends::InProcessBackend;
use pocketforge::{Backend, Pf, RumbleStatus};

/// A scratch prefs dir unique to this process + tag (no external temp-crate dep; matches the
/// pf-prefs store tests' convention).
fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("pf-xubv2-test-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[test]
fn control_plane_write_fires_observer_and_honors_at_the_primitive() {
    // a523 has a rumble motor → pulses Fire until the user disables haptics.
    let store = Arc::new(PrefsStore::at(scratch("ctrlplane")));
    let backend = InProcessBackend::shared_with_store(Arc::new(common::descriptor("a523")), store);
    let pf = Pf::over_in_process(backend.clone());

    // Subscribe BEFORE the change (mirrors change_event.rs).
    let rx = backend.subscribe_preference("hapticsEnabled");

    // Default on → the primitive fires.
    assert_eq!(pf.vibration().pulse(40), RumbleStatus::Fired);

    // Control-plane write (the sim's injection-as-API surface) → observer fires AND the primitive
    // flips to the SUPPRESSED no-op, via the same path as an absent motor.
    backend.set_preference_bool("hapticsEnabled", false);
    let evt = rx.recv_timeout(Duration::from_secs(1)).expect("PrefsDidChange delivered");
    assert_eq!(evt, PrefValue::Bool(false));
    assert_eq!(pf.vibration().pulse(40), RumbleStatus::NoopSuppressed);

    // Flip back → another event, primitive fires again.
    backend.set_preference_bool("hapticsEnabled", true);
    let evt = rx.recv_timeout(Duration::from_secs(1)).expect("re-enable event delivered");
    assert_eq!(evt, PrefValue::Bool(true));
    assert_eq!(pf.vibration().pulse(40), RumbleStatus::Fired);
}

#[test]
fn control_plane_write_persists_to_the_store() {
    // The control-plane write must PERSIST (owner ruling Q2 store), not just live in memory —
    // a fresh backend over the same store reads the flipped value at init.
    let dir = scratch("persist");
    let store = Arc::new(PrefsStore::at(&dir));
    let backend = InProcessBackend::shared_with_store(Arc::new(common::descriptor("a523")), store.clone());
    backend.set_preference_bool("hapticsEnabled", false);

    // Same value on disk, read by an independent handle (what `pf-settings get` sees).
    assert!(!store.load().unwrap().haptics_enabled());

    // A brand-new session over the same store honors it at the primitive from init — no writes.
    let reopened = InProcessBackend::shared_with_store(Arc::new(common::descriptor("a523")), store);
    let pf2 = Pf::over_in_process(reopened);
    assert_eq!(pf2.vibration().pulse(40), RumbleStatus::NoopSuppressed);
}

#[test]
fn external_cli_write_becomes_observable_via_reload() {
    // The EXTERNAL-process leg (owner rider): an out-of-band writer — the `pf-settings` CLI, which
    // is exactly `parse_value` + `PrefsStore::apply` — flips the store; the running app's observer
    // fires when the host calls `reload_prefs()` (the v0 supervisor-file-watch stand-in).
    let dir = scratch("reload");
    let store = Arc::new(PrefsStore::at(&dir));
    let backend = InProcessBackend::shared_with_store(Arc::new(common::descriptor("a523")), store.clone());
    let pf = Pf::over_in_process(backend.clone());

    let rx = backend.subscribe_preference("hapticsEnabled");
    assert_eq!(pf.vibration().pulse(40), RumbleStatus::Fired);

    // Out-of-band write through the SAME seam the CLI uses (a separate `PrefsStore` handle stands
    // in for the separate CLI process pointing at the same $PF_PREFS_DIR).
    let cli_view = PrefsStore::at(&dir);
    cli_view.apply("hapticsEnabled", PrefValue::Bool(false)).unwrap();

    // Until the host reloads, the in-process cache is unchanged → no spurious event yet.
    assert_eq!(rx.recv_timeout(Duration::from_millis(100)), Err(RecvTimeoutError::Timeout));

    // Host learns of the change and reloads → observer fires AND the primitive honors it.
    backend.reload_prefs();
    let evt = rx.recv_timeout(Duration::from_secs(1)).expect("reload fires PrefsDidChange");
    assert_eq!(evt, PrefValue::Bool(false));
    assert_eq!(pf.vibration().pulse(40), RumbleStatus::NoopSuppressed);
}

#[test]
fn reload_without_a_change_fires_no_event() {
    let dir = scratch("noop-reload");
    let store = Arc::new(PrefsStore::at(&dir));
    let backend = InProcessBackend::shared_with_store(Arc::new(common::descriptor("a523")), store);
    let rx = backend.subscribe_preference("hapticsEnabled");
    // Nothing on disk changed → reload is a no-op, no spurious event.
    backend.reload_prefs();
    assert_eq!(rx.recv_timeout(Duration::from_millis(100)), Err(RecvTimeoutError::Timeout));
}

#[test]
fn brightness_is_contract_only_readable_and_observable() {
    // Owner ruling Q3: brightness is CONTRACT-ONLY — a readable scalar + a firing observer, with
    // NO sysfs apply leg. Prove the read + the observer; there is deliberately no hardware effect.
    let dir = scratch("brightness");
    let store = Arc::new(PrefsStore::at(&dir));
    let backend = InProcessBackend::shared_with_store(Arc::new(common::descriptor("a523")), store);
    let pf = Pf::over_in_process(backend.clone());

    assert_eq!(pf.settings().brightness(), 100); // schema default
    let rx = backend.subscribe_preference("brightness");
    backend.set_preference("brightness", PrefValue::Scalar(40));
    let evt = rx.recv_timeout(Duration::from_secs(1)).expect("brightness change observed");
    assert_eq!(evt, PrefValue::Scalar(40));
    assert_eq!(pf.settings().brightness(), 40);
}

#[test]
fn mono_audio_is_honored_on_the_routing_layer() {
    // monoAudio flips the routing-layer OutputMix (the sim-visible semantic); real DSP down-mix is
    // post-v0. Prove the read + the observer + the routing-layer effect.
    use pocketforge::OutputMix;
    let dir = scratch("mono");
    let store = Arc::new(PrefsStore::at(&dir));
    let backend = InProcessBackend::shared_with_store(Arc::new(common::descriptor("a523")), store);
    let pf = Pf::over_in_process(backend.clone());

    assert_eq!(pf.audio().output_mix(), OutputMix::Stereo);
    let rx = backend.subscribe_preference("monoAudio");
    backend.set_preference_bool("monoAudio", true);
    assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), PrefValue::Bool(true));
    assert_eq!(pf.audio().output_mix(), OutputMix::Mono);
}

#[test]
fn reduce_motion_is_a_readable_observable_flag_with_no_v0_machinery() {
    // reduceMotion is a readable + observable flag; v0 ships NO cosmetic-motion machinery to
    // suppress, so the ONLY assertions are read + observe (the documented seam).
    let dir = scratch("reduce-motion");
    let store = Arc::new(PrefsStore::at(&dir));
    let backend = InProcessBackend::shared_with_store(Arc::new(common::descriptor("a523")), store);
    let pf = Pf::over_in_process(backend.clone());

    assert!(!pf.settings().reduce_motion());
    let rx = backend.subscribe_preference("reduceMotion");
    backend.set_preference_bool("reduceMotion", true);
    assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), PrefValue::Bool(true));
    assert!(pf.settings().reduce_motion());
}

/// THE UNIFICATION TEST (`tsp-xubv.2` acceptance): suppression ≡ absence under IDENTICAL calling
/// code. One helper drives any backend the same way; an a523 with haptics disabled and an a133
/// with no motor both produce the SAME app-visible silent no-op, distinguished ONLY by the frozen
/// diagnostic discriminant (`NoopSuppressed` vs `NoopAbsent`). `.4` builds its sim matrix on this.
#[test]
fn suppression_and_absence_are_one_silent_no_op_under_identical_code() {
    // The IDENTICAL calling code — zero device/preference special-casing.
    fn drive(pf: &Pf) -> RumbleStatus {
        pf.vibration().pulse(40)
    }

    // a523-shaped: rumble PRESENT, but the user disabled haptics → suppressed.
    let s_a523 = Arc::new(PrefsStore::at(scratch("unif-a523")));
    let a523 = InProcessBackend::shared_with_store(Arc::new(common::descriptor("a523")), s_a523);
    a523.set_preference_bool("hapticsEnabled", false);
    let pf_a523 = Pf::over_in_process(a523);

    // a133-shaped: rumble ABSENT (no motor); haptics preference is irrelevant.
    let s_a133 = Arc::new(PrefsStore::at(scratch("unif-a133")));
    let a133 = InProcessBackend::shared_with_store(Arc::new(common::descriptor("a133")), s_a133);
    let pf_a133 = Pf::over_in_process(a133);

    let r_suppressed = drive(&pf_a523);
    let r_absent = drive(&pf_a133);

    // App-visible SEMANTIC is identical: both are a silent no-op (neither `Fired`), so an app that
    // just calls `pulse()` behaves identically on both — no special-casing, no error to handle.
    assert!(matches!(r_suppressed, RumbleStatus::NoopSuppressed | RumbleStatus::NoopAbsent));
    assert!(matches!(r_absent, RumbleStatus::NoopSuppressed | RumbleStatus::NoopAbsent));
    assert_ne!(r_suppressed, RumbleStatus::Fired);
    assert_ne!(r_absent, RumbleStatus::Fired);

    // The DIAGNOSTIC layer (frozen discriminants, deliberate honesty) still distinguishes WHY —
    // for surfaces like pf-hwprobe — without forking the app-visible behavior.
    assert_eq!(r_suppressed, RumbleStatus::NoopSuppressed);
    assert_eq!(r_absent, RumbleStatus::NoopAbsent);
}

#[test]
fn store_less_backend_keeps_in_memory_prefs_and_observer() {
    // The pre-E4.2 store-less constructor still works: in-memory prefs + a firing observer, no
    // disk touched (keeps every existing test hermetic).
    let backend = InProcessBackend::shared(Arc::new(common::descriptor("a523")));
    let pf = Pf::over_in_process(backend.clone());
    let rx = backend.subscribe_preference("hapticsEnabled");
    assert_eq!(pf.vibration().pulse(40), RumbleStatus::Fired);
    backend.set_preference_bool("hapticsEnabled", false);
    assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), PrefValue::Bool(false));
    assert_eq!(pf.vibration().pulse(40), RumbleStatus::NoopSuppressed);
}
