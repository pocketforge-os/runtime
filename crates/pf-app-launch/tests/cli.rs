use std::process::Command;

#[test]
fn production_cli_has_exactly_one_app_id_argument() {
    let binary = env!("CARGO_BIN_EXE_pf-app-launch");
    let missing = Command::new(binary).output().unwrap();
    assert_eq!(missing.status.code(), Some(64));
    assert_eq!(
        String::from_utf8(missing.stderr).unwrap(),
        "pf-app-launch: launch_refused reason=invalid_id item_id=\"\"\n"
    );

    let extra = Command::new(binary)
        .args(["org.example.app", "unexpected"])
        .output()
        .unwrap();
    assert_eq!(extra.status.code(), Some(64));
    assert_eq!(
        String::from_utf8(extra.stderr).unwrap(),
        "pf-app-launch: launch_refused reason=invalid_id item_id=\"org.example.app\"\n"
    );

    let option = Command::new(binary).arg("--root").output().unwrap();
    assert_eq!(option.status.code(), Some(65));
    assert_eq!(
        String::from_utf8(option.stderr).unwrap(),
        "pf-app-launch: launch_refused reason=invalid_id item_id=\"--root\"\n"
    );
}
