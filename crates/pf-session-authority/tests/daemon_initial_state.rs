use pf_session_authority::PersistedState;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
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
