use std::process::Command;

use pf_ports::{Deadline, MonotonicTime, PreferencePoll, PreferencePort, PreferenceValue};
use pf_prefs::PrefsStore;
use pf_prefs_port::PrefsPreferencePort;

#[test]
fn external_cli_write_is_observed_by_preference_port() {
    let dir = std::env::temp_dir().join(format!("pf-prefs-port-cli-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut port = PrefsPreferencePort::for_user(PrefsStore::at(&dir)).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_pf-settings"))
        .args(["set", "textScale", "200%"])
        .env("PF_PREFS_DIR", &dir)
        .env_remove("XDG_STATE_HOME")
        .env_remove("HOME")
        .output()
        .expect("spawn pf-settings");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let event = port.next_change(Deadline(MonotonicTime::ZERO)).unwrap();
    let PreferencePoll::Changed(change) = event else {
        panic!("CLI write did not reach the port subscriber");
    };
    assert_eq!(change.stored, PreferenceValue::Text("200%".into()));
    assert_eq!(change.effective, PreferenceValue::Text("100%".into()));
    assert!(!change.applied);
    let _ = std::fs::remove_dir_all(dir);
}
