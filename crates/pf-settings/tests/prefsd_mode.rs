use pf_ports::{
    ChangeAuthority, PreferenceChange, PreferenceChangeResult, PreferenceError, PreferenceKey,
    PreferencePort, PreferenceValue,
};
use pf_prefs::PrefsStore;
use pf_prefs_port::{PrefsPreferencePort, USER_AUTHORITY};
use pf_prefsd::{serve_until_with_timeout, Client};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[test]
fn daemon_mode_is_coherent_and_never_falls_back_when_down() {
    let root = scratch();
    let state = root.join("daemon-state");
    let direct = root.join("direct-state");
    let socket = root.join("prefsd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let daemon_state = state.clone();
    let thread = std::thread::spawn(move || {
        serve_until_with_timeout(
            listener,
            &PrefsStore::at(daemon_state),
            unsafe { libc::geteuid() },
            &thread_stop,
            Duration::from_millis(100),
        )
        .unwrap();
    });

    // This integration-test executable owns its environment and contains one test, so the
    // process-wide switch cannot race another test.
    std::env::set_var("PF_PREFSD_SOCK", &socket);
    let mut port = PrefsPreferencePort::for_user(PrefsStore::at(&direct)).unwrap();
    assert_eq!(
        port.read(&PreferenceKey("brightness".into()))
            .unwrap()
            .unwrap()
            .stored,
        PreferenceValue::Integer(100)
    );
    assert_eq!(
        port.submit_change(PreferenceChange {
            key: PreferenceKey("brightness".into()),
            value: PreferenceValue::Integer(40),
            authority: ChangeAuthority(USER_AUTHORITY.into()),
        })
        .unwrap(),
        PreferenceChangeResult::StoredNotApplied
    );
    assert_eq!(Client::new(&socket).get("brightness").unwrap(), 40);

    let cli = Command::new(env!("CARGO_BIN_EXE_pf-settings"))
        .args(["set", "brightness", "60"])
        .env("PF_PREFSD_SOCK", &socket)
        .env("PF_PREFS_DIR", &direct)
        .output()
        .unwrap();
    assert!(
        cli.status.success(),
        "pf-settings failed: {}",
        String::from_utf8_lossy(&cli.stderr)
    );
    assert_eq!(
        port.read(&PreferenceKey("brightness".into()))
            .unwrap()
            .unwrap()
            .stored,
        PreferenceValue::Integer(60)
    );
    assert!(!direct.join("prefs.json").exists());

    std::fs::write(state.join("prefs.json"), b"{bad json").unwrap();
    assert_eq!(
        port.read(&PreferenceKey("brightness".into())),
        Err(PreferenceError::BackendUnavailable)
    );
    assert_eq!(
        port.submit_change(PreferenceChange {
            key: PreferenceKey("brightness".into()),
            value: PreferenceValue::Integer(40),
            authority: ChangeAuthority(USER_AUTHORITY.into()),
        }),
        Err(PreferenceError::BackendUnavailable)
    );
    let corrupt_cli = Command::new(env!("CARGO_BIN_EXE_pf-settings"))
        .args(["get", "brightness"])
        .env("PF_PREFSD_SOCK", &socket)
        .output()
        .unwrap();
    assert!(!corrupt_cli.status.success());
    assert!(!String::from_utf8_lossy(&corrupt_cli.stderr).contains("invalid value"));

    std::fs::remove_file(state.join("prefs.json")).unwrap();
    let original_mode = std::fs::metadata(&state).unwrap().permissions().mode();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o500)).unwrap();
    assert_eq!(
        port.submit_change(PreferenceChange {
            key: PreferenceKey("brightness".into()),
            value: PreferenceValue::Integer(40),
            authority: ChangeAuthority(USER_AUTHORITY.into()),
        }),
        Err(PreferenceError::BackendUnavailable)
    );
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(original_mode)).unwrap();

    stop.store(true, Ordering::Release);
    thread.join().unwrap();
    std::fs::remove_file(&socket).unwrap();
    assert!(matches!(
        PrefsPreferencePort::for_user(PrefsStore::at(&direct)),
        Err(PreferenceError::BackendUnavailable)
    ));
    let down = Command::new(env!("CARGO_BIN_EXE_pf-settings"))
        .args(["get", "brightness"])
        .env("PF_PREFSD_SOCK", &socket)
        .env("PF_PREFS_DIR", &direct)
        .output()
        .unwrap();
    assert!(!down.status.success());
    assert!(!direct.join("prefs.json").exists());

    std::env::remove_var("PF_PREFSD_SOCK");
    let _ = std::fs::remove_dir_all(root);
}

fn scratch() -> PathBuf {
    let root = std::env::temp_dir().join(format!("pf-settings-prefsd-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}
