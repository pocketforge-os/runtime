//! Authority-side proof for the broker's protected SafeReturn intake (`tsp-f3fm.202.1`).
//!
//! The broker's real SafeReturn worker sends its frame over a real Unix socket to the REAL
//! shell-socket acceptor as the authenticated root `pf-input-broker.service`. That authority runs
//! the production `CommandSystem`
//! with default systemd templates over a fake `CommandExecutor`, which models `systemctl`. The
//! authority itself is unchanged: this crate is not launcher-vendored, and
//! `crates/pf-session-authority` is.
//!
//! Proven here:
//! * a SafeReturn from a non-shell client (the broker never launched, polled or acknowledged
//!   anything) starts the graceful stop of the running app;
//! * when that app ignores SIGTERM and systemd SIGKILLs it at `TimeoutStopSec`, the unit is
//!   `failed` with `Result=timeout` (`InactiveFailure`). During `StoppingGracefully` that still
//!   completes as `Returned`, never `Crash`/`ForcedClose`, and never escalates to `systemctl
//!   kill` — the pinned race-fallback contract the authority's own escalation (tsp-ihne1.10)
//!   must never double-kill.

use std::fs;
use std::io::Cursor;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pf_app_manifest::Resolver;
use pf_input_broker::safe_return::{CONNECT_TIMEOUT, IO_TIMEOUT};
use pf_input_broker::SafeReturnIntake;
use pf_ports::TestClock;
use pf_session_authority::{
    serve_connection, spawn_rpc_acceptor, Authority, CommandExecutor, CommandOutput, CommandSystem,
    CommandTemplates, MemoryStore, Phase, Receipt, RpcEvent, RpcObservation, RpcRequest,
    RpcResponse, ShellPolicy, DEFAULT_CONNECTION_LIMITS, SESSION_RPC_CONTRACT_V1,
};

const APP_ID: &str = "org.example.game";
const APP_UNIT: &str = "pf-app@org.example.game.service";
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

type TestAuthority = Authority<MemoryStore, CommandSystem<FakeSystemd>, TestClock>;

/// What `systemctl stop` leaves behind in this fake.
#[derive(Clone, Copy)]
enum StopModel {
    /// The unit is still deactivating when the authority next looks (the SIGKILL lands later).
    StillDeactivating,
    /// The unit already failed with Result=timeout (systemd's own TimeoutStopSec SIGKILL won
    /// the race) by the time the non-blocking stop job lands.
    AlreadyFailedAtStop,
}

#[derive(Default)]
struct Units {
    app_state: String,
    app_result: String,
    target_active: bool,
    owner_active: bool,
    calls: Vec<String>,
}

/// A stateful stand-in for `systemctl`, shared so the test can inspect and steer it.
#[derive(Clone)]
struct FakeSystemd {
    units: Arc<Mutex<Units>>,
    stop: StopModel,
}

impl FakeSystemd {
    fn new(stop: StopModel) -> Self {
        let units = Units {
            app_state: "inactive".into(),
            app_result: "success".into(),
            ..Units::default()
        };
        Self {
            units: Arc::new(Mutex::new(units)),
            stop,
        }
    }

    fn sigkill_at_timeout(&self) {
        let mut u = self.units.lock().unwrap();
        u.app_state = "failed".into();
        u.app_result = "timeout".into();
        u.target_active = false;
    }

    fn calls(&self) -> Vec<String> {
        self.units.lock().unwrap().calls.clone()
    }
}

impl CommandExecutor for FakeSystemd {
    fn execute(&mut self, program: &str, args: &[String]) -> Result<i32, String> {
        let line = format!("{program} {}", args.join(" "));
        let mut u = self.units.lock().unwrap();
        u.calls.push(line.clone());
        match args {
            [verb, unit] if verb == "start" && unit == APP_UNIT => {
                u.app_state = "active".into();
                u.app_result = "success".into();
                u.target_active = true;
                u.owner_active = false; // the shell steps aside for the foreground app
            }
            [verb, flag, unit] if verb == "stop" && flag == "--no-block" && unit == APP_UNIT => {
                match self.stop {
                    StopModel::StillDeactivating => u.app_state = "deactivating".into(),
                    StopModel::AlreadyFailedAtStop => {
                        u.app_state = "failed".into();
                        u.app_result = "timeout".into();
                        u.target_active = false;
                    }
                }
            }
            [verb, unit] if verb == "start" && unit == "pf-shell-selected.service" => {
                u.owner_active = true;
            }
            _ => return Err(format!("unexpected command: {line}")),
        }
        Ok(0)
    }

    fn query(&mut self, program: &str, args: &[String]) -> Result<CommandOutput, String> {
        let mut u = self.units.lock().unwrap();
        u.calls.push(format!("{program} {}", args.join(" ")));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let output = |code: i32, stdout: &str| {
            Ok(CommandOutput {
                code,
                stdout: format!("{stdout}\n"),
            })
        };
        match args.as_slice() {
            ["show", "--property", "ActiveState", "--property", "Job", "--property", "BoundBy", "pocketforge-gamescope-session.target"] =>
            {
                // This suite pins the legacy fb0 return path: no compositor session owns the
                // display, so the authority must continue through foreground.target and the
                // selected shell owner exactly as it did before session-aware reconciliation.
                output(0, "ActiveState=inactive\nJob=\nBoundBy=")
            }
            ["show", "--property", "ActiveState", "--value", unit] if *unit == APP_UNIT => {
                output(0, &u.app_state)
            }
            ["show", "--property", "Result", "--value", unit] if *unit == APP_UNIT => {
                output(0, &u.app_result)
            }
            ["is-active", "--quiet", "pocketforge-foreground.target"] => {
                output(if u.target_active { 0 } else { 3 }, "")
            }
            ["is-active", "--quiet", "pf-shell-selected.service"] => {
                output(if u.owner_active { 0 } else { 3 }, "")
            }
            _ => Err(format!("unexpected query: {program} {}", args.join(" "))),
        }
    }
}

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "pf-broker-authority-{name}-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ))
}

/// One installed app plus the platform contract the resolver checks it against.
fn resolver() -> Resolver {
    let dir = scratch("resolver");
    let app = dir.join("apps").join(APP_ID);
    fs::create_dir_all(app.join("bin")).unwrap();
    fs::write(
        app.join("app.toml"),
        format!(
            "[app]\nid = \"{APP_ID}\"\nuse = [\"input\"]\n\
             [runtime]\nfamily = \"pocketforge/a133-powervr\"\nabi = \"1\"\nplatform-version = \"20\"\n\
             [launch]\nexec = \"bin/app\"\n"
        ),
    )
    .unwrap();
    let exe = app.join("bin/app");
    fs::write(&exe, b"#!/bin/sh\n").unwrap();
    let mut permissions = fs::metadata(&exe).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&exe, permissions).unwrap();
    let platform = dir.join("platform.toml");
    fs::write(
        &platform,
        "schema_version = 1\n\
         runtime_family = \"pocketforge/a133-powervr\"\n\
         runtime_abi = \"1\"\n\
         platform_version = \"20\"\n\
         supported_capabilities = [\"input\"]\n",
    )
    .unwrap();
    Resolver::new(dir.join("apps"), platform)
}

fn authority(systemd: &FakeSystemd) -> TestAuthority {
    Authority::open_with_resolver(
        MemoryStore::default(),
        CommandSystem::with_executor(CommandTemplates::default(), systemd.clone()),
        TestClock::new(),
        3,
        // The daemon's own grace. The TestClock never reaches it, so any termination seen
        // here is systemd's TimeoutStopSec, not the authority's fallback.
        Duration::from_secs(10),
        resolver(),
    )
    .unwrap()
}

/// An in-memory RPC through the same framed serve path the daemon uses.
fn rpc(authority: &mut TestAuthority, request: RpcRequest) -> RpcResponse {
    let mut input = Vec::new();
    pf_wire::write_frame(&mut input, &serde_json::to_vec(&request).unwrap()).unwrap();
    let mut output = Vec::new();
    serve_connection(authority, &mut Cursor::new(input), &mut output).unwrap();
    serde_json::from_slice(&pf_wire::read_frame(&mut Cursor::new(output)).unwrap()).unwrap()
}

fn launch_running(authority: &mut TestAuthority) -> String {
    let RpcResponse::Accepted { session_id } = rpc(
        authority,
        RpcRequest::Launch {
            item_id: APP_ID.into(),
        },
    ) else {
        panic!("launch not accepted")
    };
    assert!(matches!(authority.state().phase, Phase::Running { .. }));
    session_id
}

/// Press guide once: the broker's real worker sends its frame to a real socket, and this thread
/// serves that one connection through the authenticated acceptor used by
/// `pf-session-authorityd`.
fn broker_safe_return(authority: &mut TestAuthority) {
    let sock = scratch("authority.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let (requests, incoming) = channel();
    let policy = ShellPolicy::custom(
        1000,
        "pf-shell-selected.service".into(),
        "pf-input-broker.service".into(),
        |_| {
            Ok(pf_peer_identity::PeerServiceIdentity {
                cred: pf_peer_identity::PeerCred {
                    pid: 4242,
                    uid: 0,
                    gid: 0,
                },
                unit: "pf-input-broker.service".into(),
            })
        },
    )
    .unwrap();
    spawn_rpc_acceptor(listener, requests, DEFAULT_CONNECTION_LIMITS, policy);
    let (log_tx, log) = channel();
    let intake = SafeReturnIntake::spawn_with(
        &sock,
        CONNECT_TIMEOUT,
        IO_TIMEOUT,
        Box::new(move |line| {
            let _ = log_tx.send(line.to_owned());
        }),
    )
    .unwrap();
    assert!(intake.request(), "the press dispatches a request");
    let pending = incoming
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .unwrap();
    pending.dispatch(authority);
    // Event-driven wait: the round trip is already complete, so the outcome line is in the
    // channel; the 5 s bound is a safety net, not a timing window.
    let line = log.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(line.ends_with(": ok"), "{line}");
    let _ = fs::remove_file(sock);
}

fn terminal_events(authority: &mut TestAuthority) -> Vec<RpcEvent> {
    let RpcResponse::Events { events, .. } = rpc(
        authority,
        RpcRequest::Events {
            client_id: "restored-shell".into(),
            contract: SESSION_RPC_CONTRACT_V1,
        },
    ) else {
        panic!("events")
    };
    events
        .into_iter()
        .map(|(_, event)| event)
        .filter(|event| {
            matches!(
                event,
                RpcEvent::Returned { .. }
                    | RpcEvent::ForcedClose { .. }
                    | RpcEvent::Crash { .. }
                    | RpcEvent::RecoveryRequired { .. }
            )
        })
        .collect()
}

fn assert_awaiting_presentation_with_returned(authority: &TestAuthority) {
    assert!(
        matches!(
            &authority.state().phase,
            Phase::Restoring {
                receipt: Receipt::Returned,
                ..
            }
        ),
        "{:?}",
        authority.state().phase
    );
}

#[test]
fn safe_return_from_a_non_shell_client_starts_the_graceful_stop() {
    let systemd = FakeSystemd::new(StopModel::StillDeactivating);
    let mut a = authority(&systemd);
    launch_running(&mut a);

    broker_safe_return(&mut a);

    assert!(
        matches!(a.state().phase, Phase::StoppingGracefully { .. }),
        "{:?}",
        a.state().phase
    );
    let calls = systemd.calls();
    assert_eq!(
        calls
            .iter()
            .filter(|c| *c == &format!("systemctl stop --no-block {APP_UNIT}"))
            .count(),
        1,
        "{calls:?}"
    );
    assert!(!calls.iter().any(|c| c.contains(" kill ")), "{calls:?}");
    assert!(
        calls.contains(
            &"systemctl show --property ActiveState --property Job --property BoundBy pocketforge-gamescope-session.target"
                .to_owned()
        ),
        "{calls:?}"
    );
}

#[test]
fn timeout_inactive_failure_during_graceful_stop_is_returned() {
    let systemd = FakeSystemd::new(StopModel::StillDeactivating);
    let mut a = authority(&systemd);
    let session_id = launch_running(&mut a);
    broker_safe_return(&mut a);
    assert!(matches!(a.state().phase, Phase::StoppingGracefully { .. }));

    // The app ignored SIGTERM; systemd SIGKILLs it at TimeoutStopSec=2s.
    systemd.sigkill_at_timeout();
    assert!(matches!(rpc(&mut a, RpcRequest::Tick), RpcResponse::Ok));
    assert_awaiting_presentation_with_returned(&a);

    rpc(
        &mut a,
        RpcRequest::Observe {
            observation: RpcObservation::PresentationAcknowledged,
        },
    );
    let terminal = terminal_events(&mut a);
    assert!(
        matches!(terminal.as_slice(), [RpcEvent::Returned { session_id: id }] if *id == session_id),
        "{terminal:?}"
    );
    let calls = systemd.calls();
    assert!(!calls.iter().any(|c| c.contains(" kill ")), "{calls:?}");
    assert!(calls.contains(&"systemctl start pf-shell-selected.service".to_owned()));
}

#[test]
fn stop_requested_after_the_backstop_sigkill_is_returned() {
    // `systemctl stop --no-block` returns immediately, and its job can land on a unit that
    // systemd's own TimeoutStopSec SIGKILL has already failed (Result=timeout). The SafeReturn
    // RPC itself then walks the ladder up to presentation: Returned, never a second kill.
    let systemd = FakeSystemd::new(StopModel::AlreadyFailedAtStop);
    let mut a = authority(&systemd);
    let session_id = launch_running(&mut a);

    broker_safe_return(&mut a);
    assert_awaiting_presentation_with_returned(&a);

    rpc(
        &mut a,
        RpcRequest::Observe {
            observation: RpcObservation::PresentationAcknowledged,
        },
    );
    let terminal = terminal_events(&mut a);
    assert!(
        matches!(terminal.as_slice(), [RpcEvent::Returned { session_id: id }] if *id == session_id),
        "{terminal:?}"
    );
    assert!(!systemd.calls().iter().any(|c| c.contains(" kill ")));
}
