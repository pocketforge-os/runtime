//! Daemon initial-state coverage in the consolidated orientation integration target.

use pf_prefs::PrefsStore;
use pf_prefsd::{serve_until_with_timeout_and_resolver, PreferenceWriter};
use pf_session_authority::PersistedState;
use std::fs;
use std::io::Read;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

static SCRATCH_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);

struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let sequence = SCRATCH_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "pf-session-authority-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

struct Daemon(Child);

impl Daemon {
    fn spawn(root: &Path) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_pf-session-authorityd"))
            .args([
                "--state-dir",
                root.join("state").to_str().unwrap(),
                "--socket",
                root.join("authority.sock").to_str().unwrap(),
                "--command-preset",
                "desktop-sim",
                "--orientation-mode",
                "static",
                "--shell-user",
                "root",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        Self(child)
    }

    fn wait_for_socket(&mut self, socket: &Path) {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        while Instant::now() < deadline {
            if socket.exists() {
                return;
            }
            if let Some(status) = self.0.try_wait().unwrap() {
                panic!("daemon exited before readiness: {status}");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("daemon did not publish its socket before the startup deadline");
    }

    fn wait_for_exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        while Instant::now() < deadline {
            if let Some(status) = self.0.try_wait().unwrap() {
                return status;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("daemon did not fail closed before the startup deadline");
    }

    fn spawn_live(root: &Path, descriptor: &Path, prefs_socket: &Path, fixture: &Path) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_pf-session-authorityd"))
            .args([
                "--state-dir",
                root.join("state").to_str().unwrap(),
                "--socket",
                root.join("authority.sock").to_str().unwrap(),
                "--command-preset",
                "desktop-sim",
                "--orientation-mode",
                "live",
                "--shell-user",
                "root",
                "--descriptor",
                descriptor.to_str().unwrap(),
                "--prefsd-socket",
                prefs_socket.to_str().unwrap(),
                "--orientation-fixture-root",
                fixture.to_str().unwrap(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        Self(child)
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if self.0.try_wait().unwrap().is_none() {
            self.0.kill().unwrap();
            self.0.wait().unwrap();
        }
    }
}

#[test]
fn daemon_publishes_fresh_idle_state_before_socket_readiness() {
    let scratch = Scratch::new("fresh-state");
    let state = scratch.path().join("state/authority.json");
    let socket = scratch.path().join("authority.sock");
    let mut daemon = Daemon::spawn(scratch.path());

    daemon.wait_for_socket(&socket);
    let published: PersistedState = serde_json::from_slice(&fs::read(&state).unwrap()).unwrap();

    assert_eq!(published.phase.name(), "idle");
    assert!(published.history.is_empty());
}

#[test]
fn daemon_preserves_valid_existing_state_at_readiness() {
    let scratch = Scratch::new("existing-state");
    let state_dir = scratch.path().join("state");
    let state = state_dir.join("authority.json");
    let socket = scratch.path().join("authority.sock");
    fs::create_dir(&state_dir).unwrap();
    let existing = serde_json::to_vec_pretty(&PersistedState::default()).unwrap();
    fs::write(&state, &existing).unwrap();
    let mut daemon = Daemon::spawn(scratch.path());

    daemon.wait_for_socket(&socket);

    assert_eq!(fs::read(&state).unwrap(), existing);
}

#[test]
fn daemon_fails_closed_without_replacing_corrupt_initial_state() {
    let scratch = Scratch::new("corrupt-state");
    let state_dir = scratch.path().join("state");
    let state = state_dir.join("authority.json");
    let socket = scratch.path().join("authority.sock");
    fs::create_dir(&state_dir).unwrap();
    let corrupt = b"{not-json\n";
    fs::write(&state, corrupt).unwrap();
    let mut daemon = Daemon::spawn(scratch.path());

    let status = daemon.wait_for_exit();

    assert!(!status.success());
    assert_eq!(fs::read(&state).unwrap(), corrupt);
    assert!(!socket.exists());
}

#[test]
fn live_daemon_binds_for_absent_unreadable_and_late_panel_orientation() {
    for (label, unreadable) in [("absent", false), ("unreadable", true), ("late", false)] {
        let scratch = Scratch::new(label);
        let descriptor = scratch.path().join("capabilities.toml");
        fs::write(
            &descriptor,
            "[[screens]]\nid='screen_main'\nrole='primary'\nrotation='cw90'\n",
        )
        .unwrap();
        let fixture = scratch.path().join("fixture");
        fs::create_dir_all(&fixture).unwrap();
        fs::write(fixture.join("switches.json"), "{}").unwrap();
        if unreadable {
            let connector = fixture.join("drm/card0-DSI-1");
            fs::create_dir_all(connector.join("panel_orientation")).unwrap();
            fs::write(connector.join("status"), "connected\n").unwrap();
        }

        let prefs_socket = scratch.path().join("prefsd.sock");
        let listener = UnixListener::bind(&prefs_socket).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let prefs_state = scratch.path().join("prefs");
        let prefs = std::thread::spawn(move || {
            serve_until_with_timeout_and_resolver(
                listener,
                &PrefsStore::at(prefs_state),
                unsafe { libc::geteuid() },
                &thread_stop,
                Duration::from_millis(20),
                |_| Ok(Some(PreferenceWriter::Settings)),
            )
            .unwrap();
        });

        let socket = scratch.path().join("authority.sock");
        let mut daemon = Daemon::spawn_live(scratch.path(), &descriptor, &prefs_socket, &fixture);
        daemon.wait_for_socket(&socket);

        if label == "late" {
            let connector = fixture.join("drm/card0-DSI-1");
            fs::create_dir_all(&connector).unwrap();
            fs::write(connector.join("status"), "connected\n").unwrap();
            fs::write(connector.join("panel_orientation"), "left side up\n").unwrap();
        }

        std::thread::sleep(if label == "late" {
            Duration::from_millis(1_500)
        } else {
            Duration::from_millis(150)
        });
        assert!(daemon.0.try_wait().unwrap().is_none(), "{label}");
        daemon.0.kill().unwrap();
        daemon.0.wait().unwrap();
        let mut stderr = String::new();
        daemon
            .0
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        let expected_reason = if unreadable { "unreadable" } else { "missing" };
        assert!(
            stderr.contains(&format!(
                "panel_orientation_fallback reason={expected_reason} fallback=descriptor"
            )),
            "{label}: {stderr}"
        );
        if label == "late" {
            assert!(
                stderr.contains("panel_orientation_recovered source=drm_connector"),
                "{stderr}"
            );
        }

        drop(daemon);
        stop.store(true, Ordering::Release);
        prefs.join().unwrap();
    }
}

#[test]
fn live_daemon_keeps_socket_hidden_until_non_panel_snapshot_is_complete() {
    let scratch = Scratch::new("incomplete-snapshot");
    let descriptor = scratch.path().join("capabilities.toml");
    fs::write(
        &descriptor,
        "[[screens]]\nid='screen_main'\nrole='primary'\nrotation='cw90'\n",
    )
    .unwrap();
    let fixture = scratch.path().join("fixture");
    let connector = fixture.join("drm/card0-DSI-1");
    fs::create_dir_all(&connector).unwrap();
    fs::write(connector.join("status"), "connected\n").unwrap();
    fs::write(connector.join("panel_orientation"), "right side up\n").unwrap();
    fs::write(fixture.join("switches.json"), "{}").unwrap();

    let prefs_socket = scratch.path().join("prefsd.sock");
    let listener = UnixListener::bind(&prefs_socket).unwrap();
    let (accepted, connected) = std::sync::mpsc::sync_channel(0);
    let prefs = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        accepted.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(500));
        drop(stream);
    });

    let socket = scratch.path().join("authority.sock");
    let mut daemon = Daemon::spawn_live(scratch.path(), &descriptor, &prefs_socket, &fixture);
    connected.recv_timeout(STARTUP_TIMEOUT).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        !socket.exists(),
        "authority readiness must remain hidden while a required preference read is incomplete"
    );

    prefs.join().unwrap();
    let status = daemon.wait_for_exit();
    assert!(!status.success());
    assert!(!socket.exists());
}
