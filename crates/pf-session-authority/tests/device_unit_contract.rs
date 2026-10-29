#[test]
fn device_unit_admits_the_pinned_compositor_identity() {
    let unit = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../systemd/pf-session-authorityd.service"),
    )
    .expect("read the workspace authority unit");
    let exec_start = unit
        .lines()
        .find(|line| line.starts_with("ExecStart="))
        .expect("authority unit must define ExecStart");

    assert!(
        exec_start.contains("--compositor-user pf-gamescope"),
        "device unit did not pin the dedicated compositor user: {exec_start}"
    );
    assert!(
        exec_start.contains("--compositor-unit pocketforge-gamescope.service"),
        "device unit did not pin the installed compositor unit: {exec_start}"
    );
    assert!(unit.lines().any(|line| line == "UMask=0007"));

    assert!(!exec_start.contains("gamescope-session.service"));
    assert!(!unit.lines().any(|line| line == "UMask=0000"));
}
