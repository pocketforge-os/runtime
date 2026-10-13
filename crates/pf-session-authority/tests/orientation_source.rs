use pf_prefs::PrefsStore;
use pf_prefsd::{serve_until_with_timeout_and_resolver, Client, PreferenceWriter};
use pf_session_authority::orientation::{
    OrientationInputSource, OrientationInputs, QuarterTurn, SensorObservation,
};
use pf_session_authority::orientation_source::{LinuxOrientationPaths, LinuxOrientationSource};
use std::fs;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

fn scratch() -> PathBuf {
    let path = std::env::temp_dir().join(format!("pf-o-src-{}", std::process::id()));
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
        "[display]\norientations=[\"landscape_flipped\"]\n",
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

    stop.store(true, Ordering::Release);
    daemon.join().unwrap();
    fs::remove_dir_all(root).unwrap();
}
