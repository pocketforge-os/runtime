//! Linux source-adapter coverage in the consolidated orientation integration target.

use pf_prefs::PrefsStore;
use pf_prefsd::{serve_until_with_timeout_and_resolver, Client, PreferenceWriter};
use pf_session_authority::orientation::{
    refresh_from_source, OrientationController, OrientationInputSource, OrientationInputs,
    PanelOrientationFailure, PanelOrientationSource, QuarterTurn, SensorObservation,
};
use pf_session_authority::orientation_source::{LinuxOrientationPaths, LinuxOrientationSource};
use std::fs;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

static SCRATCH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn scratch() -> PathBuf {
    let sequence = SCRATCH_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("pf-o-src-{}-{sequence}", std::process::id()));
    if path.exists() {
        fs::remove_dir_all(&path).unwrap();
    }
    fs::create_dir_all(&path).unwrap();
    path
}

fn write(path: impl AsRef<Path>, value: impl AsRef<[u8]>) {
    let path = path.as_ref();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, value).unwrap();
}

#[test]
fn linux_source_joins_every_input_and_observes_live_preference_changes() {
    let root = scratch();
    let descriptor = root.join("capabilities.toml");
    write(
        &descriptor,
        r#"
[[screens]]
id = "screen_main"
role = "primary"
rotation = "cw90"

[[sensors]]
id = "imu"
kind = "imu"
iio_device = "iio:device0"
mount_matrix = [[1,0,0],[0,1,0],[0,0,1]]

[[joints]]
id = "screen"
kind = "swivel"
default = 0
switches = [{ ev_type="EV_SW", code="SW_LID", source="Hall", active=[90,180] }]
[[joints.postures]]
id = "deck"
range = [0,90]
screen_rotation = { screen_main="none" }
[[joints.postures]]
id = "turned"
range = [90,180]
screen_rotation = { screen_main="cw90" }
"#,
    );
    write(root.join("drm/card0-DSI-1/status"), "connected\n");
    write(
        root.join("drm/card0-DSI-1/panel_orientation"),
        "right side up\n",
    );
    write(root.join("sensor-orientation"), "right-up\n");
    write(
        root.join("switches.json"),
        r#"{"Hall":{"SW_LID":true},"Buttons":{"SW_ROTATE_LOCK":false,"SW_TABLET_MODE":true}}"#,
    );
    write(
        root.join("iio/iio:device0/in_accel_mount_matrix"),
        "1,0,0;0,1,0;0,0,1\n",
    );
    write(
        root.join("apps/org.example.game/app.toml"),
        "[app]\nid=\"org.example.game\"\nuse=[]\n\
         [runtime]\nfamily=\"pocketforge/a133-powervr\"\nabi=\"1\"\n\
         [display]\norientations=[\"landscape_flipped\"]\n",
    );

    let prefs_socket = root.join("prefsd.sock");
    let listener = UnixListener::bind(&prefs_socket).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let state = root.join("prefs");
    let daemon = std::thread::spawn(move || {
        serve_until_with_timeout_and_resolver(
            listener,
            &PrefsStore::at(state),
            unsafe { libc::geteuid() },
            &thread_stop,
            Duration::from_millis(20),
            |_| Ok(Some(PreferenceWriter::Settings)),
        )
        .unwrap();
    });
    let prefs = Client::new(&prefs_socket);
    prefs.set("auto_rotate", false.into()).unwrap();
    prefs.set("user_rotation", "left".into()).unwrap();

    let mut source = LinuxOrientationSource::open(LinuxOrientationPaths {
        descriptor: descriptor.clone(),
        prefs_socket: prefs_socket.clone(),
        app_root: root.join("apps"),
        drm_class: root.join("drm"),
        device_tree: root.join("dt"),
        input_class: root.join("input-class"),
        input_devices: root.join("input-devices"),
        iio_devices: root.join("iio"),
        sensor_orientation_file: Some(root.join("sensor-orientation")),
        switch_state_file: Some(root.join("switches.json")),
    })
    .unwrap();
    let first = source
        .snapshot(Some("org.example.game"), &OrientationInputs::default())
        .unwrap();
    assert_eq!(first.panel_orientation, QuarterTurn::Right);
    assert_eq!(first.descriptor_panel_orientation, Some(QuarterTurn::Right));
    assert_eq!(first.screen_posture, QuarterTurn::Right);
    assert!(first.accelerometer_present);
    assert!(first.accelerometer_mount_matches);
    assert_eq!(first.accelerometer, Some(SensorObservation::Right));
    assert_eq!(first.rotate_locks, [false]);
    assert_eq!(first.tablet_mode, Some(true));
    assert!(!first.tablet_mode_is_posture);
    assert!(!first.auto_rotate);
    assert_eq!(first.user_rotation, QuarterTurn::Left);
    assert_eq!(first.app_orientations.len(), 1);

    // Negative/control transition: the same live source observes an unlocked preference without
    // reconstruction, so the reducer may return to its present accelerometer proposal.
    prefs.set("auto_rotate", true.into()).unwrap();
    let second = source.snapshot(Some("org.example.game"), &first).unwrap();
    assert!(second.auto_rotate);
    assert_eq!(second.accelerometer, Some(SensorObservation::Right));

    for (label, switches) in [
        (
            "SW_ROTATE_LOCK",
            r#"{"Hall":{"SW_LID":true},"Buttons":{"SW_ROTATE_LOCK":{"error":"EIO"},"SW_TABLET_MODE":true}}"#,
        ),
        (
            "SW_TABLET_MODE",
            r#"{"Hall":{"SW_LID":true},"Buttons":{"SW_ROTATE_LOCK":false,"SW_TABLET_MODE":{"error":"EIO"}}}"#,
        ),
        (
            "SW_LID",
            r#"{"Hall":{"SW_LID":{"error":"EIO"}},"Buttons":{"SW_ROTATE_LOCK":false,"SW_TABLET_MODE":true}}"#,
        ),
    ] {
        write(root.join("switches.json"), switches);
        let error = source
            .snapshot(Some("org.example.game"), &second)
            .unwrap_err();
        assert!(error.contains(label), "{error}");
    }
    write(
        root.join("switches.json"),
        r#"{"Hall":{"SW_LID":true},"Buttons":{"SW_ROTATE_LOCK":false,"SW_TABLET_MODE":true}}"#,
    );
    let recovered = source.snapshot(Some("org.example.game"), &second).unwrap();
    assert_eq!(recovered.rotate_locks, second.rotate_locks);
    assert_eq!(recovered.tablet_mode, second.tablet_mode);
    assert_eq!(recovered.screen_posture, second.screen_posture);

    for label in ["enumerate", "capabilities", "snapshot"] {
        let input_class = root.join(format!("{label}-input-class"));
        let input_devices = root.join(format!("{label}-input-devices"));
        if label == "capabilities" {
            write(input_class.join("event0/device/name"), "Buttons\n");
        } else if label == "snapshot" {
            write(input_class.join("event0/device/capabilities/sw"), "1000\n");
            write(input_class.join("event0/device/name"), "Buttons\n");
            write(input_devices.join("event0"), "not an evdev node\n");
        }
        let mut failing = LinuxOrientationSource::open(LinuxOrientationPaths {
            descriptor: descriptor.clone(),
            prefs_socket: prefs_socket.clone(),
            app_root: root.join("apps"),
            drm_class: root.join("drm"),
            device_tree: root.join("dt"),
            input_class,
            input_devices,
            iio_devices: root.join("iio"),
            sensor_orientation_file: Some(root.join("sensor-orientation")),
            switch_state_file: None,
        })
        .unwrap();
        let mut permissive_previous = second.clone();
        permissive_previous.rotate_locks = vec![false];
        permissive_previous.tablet_mode = Some(true);
        for (prior_label, previous) in [
            ("startup", OrientationInputs::default()),
            ("permissive", permissive_previous),
        ] {
            let snapshot = failing
                .snapshot(Some("org.example.game"), &previous)
                .unwrap_or_else(|error| {
                    panic!("{label}/{prior_label}: missing evdev paths were fatal: {error}")
                });
            assert_eq!(snapshot.rotate_locks, [true], "{label}/{prior_label}");
            assert_eq!(snapshot.tablet_mode, Some(false), "{label}/{prior_label}");
            assert_eq!(
                snapshot.screen_posture,
                QuarterTurn::Normal,
                "{label}/{prior_label}"
            );
        }
    }

    for (label, unavailable_switch) in [
        (
            "absent-required",
            r#"{ ev_type="EV_SW", code="SW_LID", source="Missing", active=[90,180] }"#,
        ),
        (
            "unsupported-required",
            r#"{ ev_type="EV_SW", code="SW_UNKNOWN", source="Hall", active=[90,180] }"#,
        ),
    ] {
        let joint_descriptor = root.join(format!("{label}-capabilities.toml"));
        write(
            &joint_descriptor,
            format!(
                r#"
[[screens]]
id = "screen_main"
role = "primary"
rotation = "cw90"

[[joints]]
id = "screen"
kind = "swivel"
default = 0
switches = [
  {unavailable_switch},
  {{ ev_type="EV_SW", code="SW_LID", source="Hall", active=[90,180] }},
]
[[joints.postures]]
id = "deck"
range = [0,90]
screen_rotation = {{ screen_main="none" }}
[[joints.postures]]
id = "turned"
range = [90,180]
screen_rotation = {{ screen_main="cw90" }}
"#
            ),
        );
        let mut failing = LinuxOrientationSource::open(LinuxOrientationPaths {
            descriptor: joint_descriptor,
            prefs_socket: prefs_socket.clone(),
            app_root: root.join("apps"),
            drm_class: root.join("drm"),
            device_tree: root.join("dt"),
            input_class: root.join("unused-input-class"),
            input_devices: root.join("unused-input-devices"),
            iio_devices: root.join("iio"),
            sensor_orientation_file: Some(root.join("sensor-orientation")),
            switch_state_file: Some(root.join("switches.json")),
        })
        .unwrap();
        let snapshot = failing
            .snapshot(Some("org.example.game"), &second)
            .unwrap_or_else(|error| panic!("{label}: unavailable joint source was fatal: {error}"));
        assert_eq!(snapshot.screen_posture, QuarterTurn::Normal, "{label}");
    }

    stop.store(true, Ordering::Release);
    daemon.join().unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn panel_orientation_absent_unreadable_and_late_uses_a_degraded_fallback_then_recovers() {
    let root = scratch();
    let descriptor = root.join("capabilities.toml");
    write(
        &descriptor,
        "[[screens]]\nid='screen_main'\nrole='primary'\nrotation='cw90'\n",
    );
    write(root.join("switches.json"), "{}");

    let prefs_socket = root.join("prefsd.sock");
    let listener = UnixListener::bind(&prefs_socket).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let state = root.join("prefs");
    let daemon = std::thread::spawn(move || {
        serve_until_with_timeout_and_resolver(
            listener,
            &PrefsStore::at(state),
            unsafe { libc::geteuid() },
            &thread_stop,
            Duration::from_millis(20),
            |_| Ok(Some(PreferenceWriter::Settings)),
        )
        .unwrap();
    });

    let paths = LinuxOrientationPaths {
        descriptor,
        prefs_socket,
        app_root: root.join("apps"),
        drm_class: root.join("drm"),
        device_tree: root.join("dt"),
        input_class: root.join("input-class"),
        input_devices: root.join("input-devices"),
        iio_devices: root.join("iio"),
        sensor_orientation_file: Some(root.join("sensor-orientation")),
        switch_state_file: Some(root.join("switches.json")),
    };

    for (label, prepare, failure) in [
        ("absent", false, PanelOrientationFailure::Missing),
        ("unreadable", true, PanelOrientationFailure::Unreadable),
    ] {
        let case_root = root.join(label);
        let mut case_paths = paths.clone();
        case_paths.drm_class = case_root.join("drm");
        case_paths.device_tree = case_root.join("dt");
        if prepare {
            write(
                case_paths.drm_class.join("card0-DSI-1/status"),
                "connected\n",
            );
            fs::create_dir_all(case_paths.drm_class.join("card0-DSI-1/panel_orientation")).unwrap();
        }
        let mut source = LinuxOrientationSource::open(case_paths).unwrap();
        let snapshot = source
            .snapshot(None, &OrientationInputs::default())
            .unwrap();
        assert_eq!(snapshot.panel_orientation, QuarterTurn::Right, "{label}");
        assert_eq!(
            snapshot.panel_orientation_source,
            PanelOrientationSource::DescriptorFallback(failure),
            "{label}"
        );
    }

    let controller = OrientationController::new(OrientationInputs::default(), 1);
    let active_app = Arc::new(Mutex::new(None));
    let mut inputs = OrientationInputs::default();
    let mut source = LinuxOrientationSource::open(paths.clone()).unwrap();
    let fallback = refresh_from_source(&mut source, &controller, &active_app, &mut inputs).unwrap();
    assert!(fallback
        .degraded
        .iter()
        .any(|reason| reason == "panel_orientation_fallback"));

    write(paths.drm_class.join("card0-DSI-1/status"), "connected\n");
    write(
        paths.drm_class.join("card0-DSI-1/panel_orientation"),
        "left side up\n",
    );
    let recovered =
        refresh_from_source(&mut source, &controller, &active_app, &mut inputs).unwrap();
    assert!(recovered.serial > fallback.serial);
    assert_eq!(recovered.output, QuarterTurn::Left);
    assert!(!recovered
        .degraded
        .iter()
        .any(|reason| reason == "panel_orientation_fallback"));

    let unknown_descriptor = root.join("unknown-capabilities.toml");
    write(
        &unknown_descriptor,
        "[[screens]]\nid='screen_main'\nrole='primary'\n",
    );
    let mut unknown_paths = paths;
    unknown_paths.descriptor = unknown_descriptor;
    unknown_paths.drm_class = root.join("unknown-drm");
    let unknown = LinuxOrientationSource::open(unknown_paths)
        .unwrap()
        .snapshot(None, &OrientationInputs::default())
        .unwrap();
    assert_eq!(unknown.panel_orientation, QuarterTurn::Normal);
    assert_eq!(
        unknown.panel_orientation_source,
        PanelOrientationSource::UnknownFallback(PanelOrientationFailure::Missing)
    );

    stop.store(true, Ordering::Release);
    daemon.join().unwrap();
    fs::remove_dir_all(root).unwrap();
}
