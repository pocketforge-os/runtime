//! The real `pf-session-authorityd` keeps its self-driven tick while untrusted clients attack the
//! authenticated shell socket (tsp-f3fm.219, tsp-ght0z).
//!
//! The daemon is started from the persisted state bench boot P3 wedged in (restoring a crashed
//! session at the presentation-acknowledgement rung). Before its deadline, one client connects
//! twice from a process outside an authorized service. Both connections must be closed before
//! framing. The deadline must still expire on
//! schedule. Accepted stalled-client behavior is covered through the hermetic identity seam in
//! the library test `stalled_clients_never_block_the_service_loop_tick`.

use std::fs;
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const P3_STATE: &str = r#"{"phase":{"Restoring":{"session_id":"session-1","item_id":"org.pocketforge.poolsuite","receipt":{"Crash":{"summary":"systemd result: exit-code"}},"rung":"PresentationAcknowledged"}},"history":[{"session_id":"session-1","item_id":"org.pocketforge.poolsuite","receipt":null,"started_at":null,"ended_at":{"at":{"secs_since_epoch":1790000000,"nanos_since_epoch":0},"precision":"Approximate"}}],"pending":[{"sequence":1,"event":"ObservedStarting"},{"sequence":2,"event":"ObservedRunning"}],"next_sequence":3,"next_session":2,"safe_return_queue":0,"safe_return_binding_revision":0,"acknowledged":{}}"#;

/// Deadline (10 s) armed on the first tick (<= 1 s after start) plus scheduling slack.
const EXPIRY_BOUND: Duration = Duration::from_secs(15);

struct Daemon {
    child: Child,
    dir: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn start_daemon() -> (Daemon, PathBuf, PathBuf) {
    // AF_UNIX socket paths are limited to ~108 bytes; fall back to /tmp for a long TMPDIR.
    let base = std::env::temp_dir();
    let base = if base.as_os_str().len() > 60 {
        PathBuf::from("/tmp")
    } else {
        base
    };
    let dir = base.join(format!("pfsa-slow-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("state")).unwrap();
    let state = dir.join("state/authority.json");
    fs::write(&state, P3_STATE).unwrap();
    let socket = dir.join("a.sock");
    let child = Command::new(env!("CARGO_BIN_EXE_pf-session-authorityd"))
        .arg("--state-dir")
        .arg(dir.join("state"))
        .arg("--socket")
        .arg(&socket)
        .args(["--command-preset", "desktop-sim"])
        .args(["--orientation-mode", "static"])
        .args(["--shell-user", "root"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let daemon = Daemon { child, dir };
    let until = Instant::now() + Duration::from_secs(5);
    while !socket.exists() {
        assert!(Instant::now() < until, "daemon socket never appeared");
        std::thread::sleep(Duration::from_millis(20));
    }
    (daemon, socket, state)
}

fn phase(state: &Path) -> serde_json::Value {
    let value: serde_json::Value = serde_json::from_slice(&fs::read(state).unwrap()).unwrap();
    value["phase"].clone()
}

#[test]
fn untrusted_clients_are_closed_and_never_stall_the_presentation_deadline() {
    let (daemon, socket, state) = start_daemon();
    let started = Instant::now();
    let mut silent = UnixStream::connect(&socket).unwrap();
    silent
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut second = UnixStream::connect(&socket).unwrap();
    second
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut byte = [0];
    assert_eq!(silent.read(&mut byte).unwrap(), 0);
    assert_eq!(second.read(&mut byte).unwrap(), 0);

    let mut reached = None;
    while started.elapsed() < EXPIRY_BOUND {
        if phase(&state).get("RecoveryRequired").is_some() {
            reached = Some(started.elapsed());
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let reached = reached.unwrap_or_else(|| {
        panic!(
            "no RecoveryRequired within {EXPIRY_BOUND:?} while clients stalled; phase={}",
            phase(&state)
        )
    });
    assert!(
        reached >= Duration::from_secs(9),
        "expired early: {reached:?}"
    );
    let recovery = &phase(&state)["RecoveryRequired"];
    assert!(recovery["reason"]
        .as_str()
        .unwrap()
        .starts_with("presentation_not_acknowledged:"));

    drop((silent, second, daemon));
}
