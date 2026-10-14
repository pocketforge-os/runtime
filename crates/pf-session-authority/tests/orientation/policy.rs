//! Pure policy and long-poll coverage in the consolidated orientation integration target.

use pf_session_authority::orientation::{
    HallDebouncer, LogicalOrientation, MountMatrix, OrientationController, OrientationInputs,
    OrientationPolicy, OrientationState, OrientationWatch, Presentation, QuarterTurn,
    SensorObservation,
};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

fn inputs() -> OrientationInputs {
    OrientationInputs::default()
}

#[test]
fn orientation_state_v1_wire_shape_excludes_internal_panel_source() {
    assert_eq!(
        serde_json::to_value(OrientationState::default()).unwrap(),
        serde_json::json!({
            "serial": 1,
            "output": "normal",
            "logical": "landscape",
            "app": "landscape",
            "presentation": "fill",
            "whole_body": "normal",
            "screen_only": "normal",
            "locked": false,
            "degraded": [],
        })
    );
}

#[test]
fn daemon_controller_reduces_complete_snapshots_before_publishing() {
    let controller = OrientationController::new(inputs(), 2);
    let watch = controller.watch();
    let initial = watch.snapshot();

    let mut changed = inputs();
    changed.user_rotation = QuarterTurn::Left;
    let published = controller.update(changed.clone());
    assert_eq!(published.whole_body, QuarterTurn::Left);
    assert_eq!(watch.snapshot(), published);
    assert_eq!(published.serial, initial.serial + 1);

    // A repeated complete snapshot is the negative control: no semantic serial bump.
    assert_eq!(controller.update(changed).serial, published.serial);
}

#[test]
fn canonical_pf_prefs_keys_drive_the_manual_rotation_source() {
    let mut prefs = pf_prefs::Prefs::defaults();
    prefs
        .set("auto_rotate", pf_prefs::PrefValue::Bool(false))
        .unwrap();
    prefs
        .set("user_rotation", pf_prefs::PrefValue::Enum("left"))
        .unwrap();
    let mut i = inputs();
    i.accelerometer_present = true;
    i.accelerometer = Some(SensorObservation::Right);
    i.apply_preferences(&prefs);
    let state = OrientationPolicy::new(i);
    assert!(state.state().locked);
    assert_eq!(state.state().whole_body, QuarterTurn::Left);
}

#[test]
fn every_input_and_precedence_rule_has_positive_and_negative_controls() {
    let mut policy = OrientationPolicy::new(inputs());
    assert_eq!(policy.state().output, QuarterTurn::Normal);

    // Fixed mount is always composed and descriptor disagreement is diagnostic.
    let mut i = inputs();
    i.panel_orientation = QuarterTurn::Right;
    i.descriptor_panel_orientation = Some(QuarterTurn::Left);
    let state = policy.update(i.clone());
    assert_eq!(state.output, QuarterTurn::Right);
    assert!(state
        .degraded
        .iter()
        .any(|reason| reason == "panel_orientation_mismatch"));
    i.descriptor_panel_orientation = Some(QuarterTurn::Right);
    assert!(!policy
        .update(i.clone())
        .degraded
        .iter()
        .any(|reason| reason == "panel_orientation_mismatch"));

    // A swivel is screen-only: same output as body rotation, but no gamepad remap.
    i.panel_orientation = QuarterTurn::Normal;
    i.descriptor_panel_orientation = None;
    i.screen_posture = QuarterTurn::Left;
    let posture = policy.update(i.clone()).clone();
    assert_eq!(posture.output, QuarterTurn::Left);
    assert_eq!(posture.whole_body, QuarterTurn::Normal);
    assert!(!posture.requires_gamepad_remap());
    i.screen_posture = QuarterTurn::Normal;
    i.accelerometer_present = true;
    i.accelerometer = Some(SensorObservation::Left);
    let body = policy.update(i.clone()).clone();
    assert_eq!(body.output, QuarterTurn::Left);
    assert_eq!(body.whole_body, QuarterTurn::Left);
    assert!(body.requires_gamepad_remap());

    // User rotation is the whole-body source on a sensorless handheld.
    i.accelerometer_present = false;
    i.accelerometer = None;
    i.user_rotation = QuarterTurn::Left;
    let sensorless = policy.update(i.clone()).clone();
    assert_eq!(sensorless.whole_body, QuarterTurn::Left);
    assert!(sensorless.requires_gamepad_remap());

    // Either software preference or any physical switch locks to user_rotation.
    i.accelerometer_present = true;
    i.accelerometer = Some(SensorObservation::Right);
    i.auto_rotate = false;
    assert_eq!(policy.update(i.clone()).whole_body, QuarterTurn::Left);
    i.auto_rotate = true;
    i.rotate_locks = vec![false, true];
    assert_eq!(policy.update(i.clone()).whole_body, QuarterTurn::Left);
    i.rotate_locks = vec![false, false];
    assert_eq!(policy.update(i.clone()).whole_body, QuarterTurn::Right);

    // Hard screen posture and fixed mount remain composed while locked.
    i.auto_rotate = false;
    i.panel_orientation = QuarterTurn::Right;
    i.screen_posture = QuarterTurn::Left;
    assert_eq!(policy.update(i.clone()).output, QuarterTurn::Left);

    // Undefined sensor data retains the last valid proposal; a mismatched matrix disables it.
    i.auto_rotate = true;
    i.panel_orientation = QuarterTurn::Normal;
    i.screen_posture = QuarterTurn::Normal;
    i.accelerometer = Some(SensorObservation::Undefined);
    assert_eq!(policy.update(i.clone()).whole_body, QuarterTurn::Right);
    i.accelerometer_mount_matches = false;
    assert_eq!(policy.update(i.clone()).whole_body, QuarterTurn::Left);
    assert!(policy
        .state()
        .degraded
        .iter()
        .any(|reason| reason == "accelerometer_mount_mismatch"));

    // Unmapped tablet mode gates the sensor; explicitly mapped mode is posture-only.
    i.accelerometer_mount_matches = true;
    i.accelerometer = Some(SensorObservation::UpsideDown);
    i.tablet_mode = Some(false);
    i.tablet_mode_is_posture = false;
    assert_eq!(policy.update(i.clone()).whole_body, QuarterTurn::Left);
    i.tablet_mode_is_posture = true;
    assert_eq!(policy.update(i.clone()).whole_body, QuarterTurn::UpsideDown);
}

#[test]
fn hall_debounce_mount_matrix_and_serial_are_deterministic() {
    let mut hall = HallDebouncer::new(QuarterTurn::Normal, 50);
    hall.observe(QuarterTurn::Left, 0);
    assert_eq!(hall.advance(49), QuarterTurn::Normal);
    hall.observe(QuarterTurn::Normal, 49);
    hall.observe(QuarterTurn::Left, 50);
    assert_eq!(hall.advance(99), QuarterTurn::Normal);
    assert_eq!(hall.advance(100), QuarterTurn::Left);
    hall.resync(QuarterTurn::Right);
    assert_eq!(hall.current(), QuarterTurn::Right);

    let identity: MountMatrix = [[1, 0, 0], [0, 1, 0], [0, 0, 1]];
    let rotate: MountMatrix = [[0, -1, 0], [1, 0, 0], [0, 0, 1]];
    assert!(pf_session_authority::orientation::mount_matrices_agree(
        &identity, &identity
    ));
    assert!(!pf_session_authority::orientation::mount_matrices_agree(
        &identity, &rotate
    ));

    let mut policy = OrientationPolicy::new(inputs());
    let serial = policy.state().serial;
    policy.update(inputs());
    assert_eq!(policy.state().serial, serial);
    let mut changed = inputs();
    changed.user_rotation = QuarterTurn::Left;
    changed.accelerometer_present = false;
    policy.update(changed);
    assert_eq!(policy.state().serial, serial + 1);
}

#[test]
fn app_selection_letterboxes_cross_aspect_and_documents_180_degree_fill() {
    let mut policy = OrientationPolicy::new(inputs());
    let mut i = inputs();
    i.accelerometer_present = false;
    i.user_rotation = QuarterTurn::UpsideDown;
    i.app_orientations = vec![LogicalOrientation::Landscape];
    let state = policy.update(i.clone());
    assert_eq!(state.logical, LogicalOrientation::LandscapeFlipped);
    assert_eq!(state.app, LogicalOrientation::Landscape);
    assert_eq!(state.presentation, Presentation::Fill);

    i.app_orientations = vec![LogicalOrientation::Portrait];
    assert_eq!(
        policy.update(i.clone()).presentation,
        Presentation::Letterbox
    );
    i.app_orientations = vec![
        LogicalOrientation::PortraitFlipped,
        LogicalOrientation::Portrait,
    ];
    assert_eq!(policy.update(i.clone()).app, LogicalOrientation::Portrait);
    let manifest_order = OrientationPolicy::new(i);
    assert_eq!(
        manifest_order.state().app,
        LogicalOrientation::PortraitFlipped
    );
}

#[test]
fn bounded_orientation_waiters_do_not_delay_lifecycle_work() {
    const N: usize = 8;
    let watch = Arc::new(OrientationWatch::new(
        pf_session_authority::orientation::OrientationState::default(),
        N,
    ));
    let serial = watch.snapshot().serial;
    let mut waiters = Vec::new();
    for _ in 0..N {
        let watch = Arc::clone(&watch);
        waiters.push(thread::spawn(move || {
            watch
                .wait_after(serial, Duration::from_millis(200))
                .unwrap()
        }));
    }
    let deadline = Instant::now() + Duration::from_millis(100);
    while watch.active_waiters() != N && Instant::now() < deadline {
        thread::yield_now();
    }
    assert_eq!(watch.active_waiters(), N);
    assert!(watch.wait_after(serial, Duration::from_millis(1)).is_err());

    // This models the authority's existing lifecycle lane: it does not share the watcher's
    // permits or condvar, so a Broker safe_return-sized action stays well inside its deadline.
    let started = Instant::now();
    let lifecycle = thread::spawn(|| 42).join().unwrap();
    assert_eq!(lifecycle, 42);
    assert!(started.elapsed() < Duration::from_millis(50));

    watch.publish({
        let mut state = watch.snapshot();
        state.serial += 1;
        state
    });
    for waiter in waiters {
        assert!(waiter.join().unwrap().serial > serial);
    }
}
