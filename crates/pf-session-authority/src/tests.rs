use super::*;
use pf_ports::{Clock, MonotonicTime, SessionEvent, TestClock};
use std::cell::Cell;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

const APP_ID: &str = "org.example.game";
/// An app that declares the `open_url` capability.
const LINKER_ID: &str = "org.example.linker";
/// The URL handler (default browser).
const BROWSER_ID: &str = "org.example.browser";
static RESOLVER_SEQUENCE: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static WALL_TIME_SECS: Cell<u64> = const { Cell::new(1_234) };
}

fn fixed_wall_time() -> SystemTime {
    WALL_TIME_SECS.with(|seconds| SystemTime::UNIX_EPOCH + Duration::from_secs(seconds.get()))
}

fn set_wall_time(seconds: u64) {
    WALL_TIME_SECS.with(|wall_time| wall_time.set(seconds));
}

#[derive(Default)]
struct FakeSystem {
    calls: Vec<String>,
    fail_force: bool,
    fail_graceful: bool,
    fail_owner: bool,
    fail_start: bool,
    lifecycle: Option<SystemLifecycle>,
    /// Per-item lifecycle, consulted before `lifecycle` (the slot's handler unit).
    lifecycles: BTreeMap<String, SystemLifecycle>,
    handoffs: Vec<UrlHandoff>,
    restores: Vec<ReturnSlot>,
    fail_open_url: bool,
    fail_restore: bool,
    delivered: bool,
}

#[derive(Default)]
struct FakeExecutor {
    calls: Vec<(String, Vec<String>)>,
    codes: VecDeque<i32>,
    outputs: VecDeque<CommandOutput>,
}
impl CommandExecutor for FakeExecutor {
    fn execute(&mut self, program: &str, args: &[String]) -> Result<i32, String> {
        self.calls.push((program.to_owned(), args.to_vec()));
        Ok(self.codes.pop_front().unwrap_or(0))
    }

    fn query(&mut self, program: &str, args: &[String]) -> Result<CommandOutput, String> {
        self.calls.push((program.to_owned(), args.to_vec()));
        self.outputs
            .pop_front()
            .ok_or_else(|| "missing fake query output".into())
    }
}

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "pf-authority-{name}-{}-{}",
        std::process::id(),
        RESOLVER_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ))
}

fn test_resolver() -> Resolver {
    test_resolver_fixture().0
}

fn test_resolver_fixture() -> (Resolver, PathBuf) {
    let dir = scratch("resolver");
    let root = dir.join("apps");
    fs::create_dir_all(&root).unwrap();
    let mut ids = vec![APP_ID.to_owned(), "org.example.never-running".to_owned()];
    ids.extend((0..5).map(|n| format!("org.example.g{n}")));
    ids.extend([LINKER_ID.to_owned(), BROWSER_ID.to_owned()]);
    for id in &ids {
        let app = root.join(id);
        fs::create_dir_all(app.join("bin")).unwrap();
        // Only the linker declares the open_url capability (owner decision B).
        let capabilities = if id == LINKER_ID {
            "[\"input\", \"open_url\"]"
        } else {
            "[\"input\"]"
        };
        fs::write(
            app.join("app.toml"),
            format!(
                "[app]\nid = \"{id}\"\nuse = {capabilities}\n\
                 [runtime]\nfamily = \"pocketforge/a133-powervr\"\nabi = \"1\"\nplatform-version = \"20\"\n\
                 [launch]\nexec = \"bin/app\"\n"
            ),
        )
        .unwrap();
        let executable = app.join("bin/app");
        fs::write(&executable, b"#!/bin/sh\n").unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(executable, permissions).unwrap();
    }
    let platform = dir.join("platform.toml");
    fs::write(
        &platform,
        "schema_version = 1\n\
         runtime_family = \"pocketforge/a133-powervr\"\n\
         runtime_abi = \"1\"\n\
         platform_version = \"20\"\n\
         supported_capabilities = [\"input\", \"open_url\"]\n",
    )
    .unwrap();
    let executable = root.join(APP_ID).join("bin/app");
    (Resolver::new(root, platform), executable)
}

#[test]
fn file_store_reports_corruption_as_typed_recovery_error() {
    let dir = scratch("corrupt");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("state.json");
    fs::write(&path, b"not-json").unwrap();
    assert!(matches!(
        FileStore::new(&path).load(),
        Err(AuthorityError::CorruptState { path: got, .. }) if got == path
    ));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn file_store_loads_pre_timestamp_history_fixture() {
    let dir = scratch("pre-timestamp");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("state.json");
    fs::write(
        &path,
        include_bytes!("../tests/fixtures/pre_timestamp_state.json"),
    )
    .unwrap();

    let state = FileStore::new(&path).load().unwrap().unwrap();
    let entry = state.history.front().unwrap();
    assert_eq!(entry.item_id, "legacy-game");
    assert_eq!(entry.started_at, None);
    assert_eq!(entry.ended_at, None);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn command_system_expands_templates_and_reports_start_failure() {
    let executor = FakeExecutor {
        codes: VecDeque::from([0, 3]),
        ..FakeExecutor::default()
    };
    let templates = CommandTemplates::from_strings(
        "shim start {item_id} {session_id}",
        "shim stop {session_id}",
        "shim kill {session_id}",
        "shim owner",
    );
    let mut system = CommandSystem::with_executor(templates, executor);
    system
        .start_foreground(
            &LaunchRequest {
                item_id: APP_ID.into(),
            },
            "s1",
        )
        .unwrap();
    assert_eq!(
        system.start_foreground(
            &LaunchRequest {
                item_id: "org.example.missing".into()
            },
            "s2"
        ),
        Err("command exited 3".into())
    );
    let executor = system.into_executor();
    assert_eq!(executor.calls[0].1, ["start", APP_ID, "s1"]);
}

#[test]
fn default_command_templates_use_one_session_keyed_unit_for_the_lifecycle() {
    let mut system =
        CommandSystem::with_executor(CommandTemplates::default(), FakeExecutor::default());
    let request = LaunchRequest {
        item_id: APP_ID.into(),
    };

    system.start_foreground(&request, "session-1").unwrap();
    system.request_graceful_stop(APP_ID, "session-1").unwrap();
    system.enforce_termination(APP_ID, "session-1").unwrap();

    let executor = system.into_executor();
    let units: Vec<&str> = executor
        .calls
        .iter()
        .map(|(_, args)| args.last().unwrap().as_str())
        .collect();
    assert_eq!(
        units,
        vec![
            "pf-app@org.example.game.service",
            "pf-app@org.example.game.service",
            "pf-app@org.example.game.service",
        ]
    );
}

#[test]
fn command_system_queries_the_exact_app_target_and_selected_owner_units() {
    let executor = FakeExecutor {
        outputs: VecDeque::from([
            CommandOutput {
                code: 0,
                stdout: "inactive\n".into(),
            },
            CommandOutput {
                code: 0,
                stdout: "exit-code\n".into(),
            },
            CommandOutput {
                code: 3,
                stdout: String::new(),
            },
            CommandOutput {
                code: 0,
                stdout: String::new(),
            },
        ]),
        ..FakeExecutor::default()
    };
    let mut system = CommandSystem::with_executor(CommandTemplates::default(), executor);

    assert_eq!(
        system.lifecycle(APP_ID).unwrap(),
        Some(SystemLifecycle {
            app: AppUnitState::InactiveFailure {
                summary: "systemd result: exit-code".into(),
            },
            foreground_target_active: false,
            selected_owner_active: true,
        })
    );

    let calls = system.into_executor().calls;
    assert_eq!(
        calls,
        [
            (
                "systemctl".into(),
                vec![
                    "show".into(),
                    "--property".into(),
                    "ActiveState".into(),
                    "--value".into(),
                    "pf-app@org.example.game.service".into(),
                ]
            ),
            (
                "systemctl".into(),
                vec![
                    "show".into(),
                    "--property".into(),
                    "Result".into(),
                    "--value".into(),
                    "pf-app@org.example.game.service".into(),
                ]
            ),
            (
                "systemctl".into(),
                vec![
                    "is-active".into(),
                    "--quiet".into(),
                    "pocketforge-foreground.target".into(),
                ]
            ),
            (
                "systemctl".into(),
                vec![
                    "is-active".into(),
                    "--quiet".into(),
                    "pf-shell-selected.service".into(),
                ]
            ),
        ]
    );
}

#[test]
fn desktop_sim_templates_create_and_remove_session_markers() {
    let dir = scratch("desktop-sim");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let mut system = CommandSystem::new(CommandTemplates::desktop_sim(&dir));
    let request = LaunchRequest {
        item_id: APP_ID.into(),
    };
    let marker = dir.join("sessions/session-1.running");

    system.start_foreground(&request, "session-1").unwrap();
    assert!(marker.is_file());
    system.request_graceful_stop(APP_ID, "session-1").unwrap();
    assert!(!marker.exists());
    system.request_graceful_stop(APP_ID, "session-1").unwrap();
    system.enforce_termination(APP_ID, "session-1").unwrap();
    system.activate_selected_owner().unwrap();
    assert!(dir.join("shell-selected").is_file());

    fs::remove_dir_all(dir).unwrap();
}
impl FakeSystem {
    fn available() -> Self {
        Self::default()
    }
}
impl SessionSystem for FakeSystem {
    fn start_foreground(&mut self, _: &LaunchRequest, _: &str) -> Result<(), String> {
        self.calls.push("start".into());
        if self.fail_start {
            Err("start failed".into())
        } else {
            Ok(())
        }
    }
    fn request_graceful_stop(&mut self, _: &str, _: &str) -> Result<(), String> {
        self.calls.push("graceful".into());
        if self.fail_graceful {
            Err("unavailable".into())
        } else {
            Ok(())
        }
    }
    fn enforce_termination(&mut self, _: &str, _: &str) -> Result<(), String> {
        self.calls.push("force".into());
        if self.fail_force {
            Err("refused".into())
        } else {
            Ok(())
        }
    }
    fn activate_selected_owner(&mut self) -> Result<(), String> {
        self.calls.push("owner".into());
        if self.fail_owner {
            Err("unavailable".into())
        } else {
            Ok(())
        }
    }
    fn lifecycle(&mut self, item_id: &str) -> Result<Option<SystemLifecycle>, String> {
        Ok(self
            .lifecycles
            .get(item_id)
            .cloned()
            .or_else(|| self.lifecycle.clone()))
    }
    fn open_url(&mut self, handoff: &UrlHandoff) -> Result<UrlHandoffOutcome, String> {
        // The URL is data on the handoff. A real system passes it in the launch request; the
        // fake records exactly what it was given and never builds a command from it.
        self.calls
            .push(format!("open_url {}", handoff.handler_item_id));
        self.handoffs.push(handoff.clone());
        if self.fail_open_url {
            Err("handoff refused".into())
        } else if self.delivered {
            Ok(UrlHandoffOutcome::Delivered)
        } else {
            Ok(UrlHandoffOutcome::Launched)
        }
    }
    fn restore_caller(&mut self, slot: &ReturnSlot) -> Result<(), String> {
        self.calls.push(format!("restore {}", slot.handler_item_id));
        self.restores.push(slot.clone());
        if self.fail_restore {
            Err("restore refused".into())
        } else {
            Ok(())
        }
    }
}

fn authority() -> Authority<MemoryStore, FakeSystem, TestClock> {
    Authority::open_with_resolver(
        MemoryStore::default(),
        FakeSystem::available(),
        TestClock::new(),
        3,
        Duration::from_millis(10),
        test_resolver(),
    )
    .unwrap()
}
fn launch_running(a: &mut Authority<MemoryStore, FakeSystem, TestClock>) -> String {
    let LaunchResult::Accepted { session_id } = a
        .launch(LaunchRequest {
            item_id: APP_ID.into(),
        })
        .unwrap()
    else {
        panic!()
    };
    session_id
}
fn restore(a: &mut Authority<MemoryStore, FakeSystem, TestClock>) {
    let current = a.session_id().unwrap();
    a.observe(Observation::TargetReleased).unwrap();
    a.observe(Observation::SelectedOwnerActive).unwrap();
    assert!(!a.events_for("test").iter().any(|(_, e)| match e {
        SessionEvent::Terminal(TerminalReceipt::Returned { session_id })
        | SessionEvent::Terminal(TerminalReceipt::ForcedClose { session_id })
        | SessionEvent::Terminal(TerminalReceipt::Crash { session_id, .. }) =>
            session_id == &current,
        _ => false,
    }));
    a.observe(Observation::PresentationAcknowledged).unwrap();
}

#[test]
fn graceful_safe_return_observes_every_rung_before_returned() {
    let mut a = authority();
    let LaunchResult::Accepted { session_id: id } = a
        .launch(LaunchRequest {
            item_id: APP_ID.into(),
        })
        .unwrap()
    else {
        panic!()
    };
    a.intake_safe_return().unwrap();
    a.tick().unwrap();
    assert!(matches!(a.state.phase, Phase::StoppingGracefully { .. }));
    a.observe(Observation::UnitInactive).unwrap();
    restore(&mut a);
    let events = a.events_for("test");
    assert!(matches!(
        events[events.len() - 2].1,
        SessionEvent::Observed(ObservedSessionState::ObservationComplete)
    ));
    assert_eq!(
        events.last().unwrap().1,
        SessionEvent::Terminal(TerminalReceipt::Returned { session_id: id })
    );
}

#[test]
fn events_reconcile_systemd_ladder_but_return_waits_for_presentation_ack() {
    for app in [
        AppUnitState::InactiveSuccess,
        AppUnitState::InactiveFailure {
            summary: "systemd result: signal".into(),
        },
    ] {
        let mut a = authority();
        let LaunchResult::Accepted { session_id } = a
            .launch(LaunchRequest {
                item_id: APP_ID.into(),
            })
            .unwrap()
        else {
            panic!()
        };
        a.system.lifecycle = Some(SystemLifecycle {
            app: app.clone(),
            foreground_target_active: false,
            selected_owner_active: true,
        });

        let response = handle_rpc(
            &mut a,
            RpcRequest::Events {
                client_id: "restored-shell".into(),
                contract: SESSION_RPC_CONTRACT_V1,
            },
        )
        .unwrap();
        let RpcResponse::Events { events, .. } = response else {
            panic!()
        };
        assert!(matches!(
            a.state.phase,
            Phase::Restoring {
                rung: RestorationRung::PresentationAcknowledged,
                ..
            }
        ));
        assert!(!events
            .iter()
            .any(|(_, event)| matches!(event, RpcEvent::Returned { .. } | RpcEvent::Crash { .. })));
        assert_eq!(
            a.system
                .calls
                .iter()
                .filter(|call| *call == "owner")
                .count(),
            1
        );

        handle_rpc(
            &mut a,
            RpcRequest::Observe {
                observation: RpcObservation::PresentationAcknowledged,
            },
        )
        .unwrap();
        let terminal: Vec<_> = a
            .events_for("restored-shell")
            .into_iter()
            .filter_map(|(_, event)| match event {
                SessionEvent::Terminal(receipt) => Some(receipt),
                _ => None,
            })
            .collect();
        match app {
            AppUnitState::InactiveSuccess => {
                assert_eq!(terminal, [TerminalReceipt::Returned { session_id }])
            }
            AppUnitState::InactiveFailure { .. } => assert!(matches!(
                terminal.as_slice(),
                [TerminalReceipt::Crash {
                    session_id: id,
                    summary
                }] if id == &session_id && summary == "systemd result: signal"
            )),
            AppUnitState::Active => unreachable!(),
        }
    }
}

#[test]
fn invalid_and_unknown_ids_are_unavailable_without_any_system_command() {
    let mut a = authority();
    assert_eq!(
        a.launch(LaunchRequest {
            item_id: "../outside".into(),
        }),
        Ok(LaunchResult::ItemUnavailable)
    );
    assert_eq!(
        a.launch(LaunchRequest {
            item_id: "org.example.unknown".into(),
        }),
        Ok(LaunchResult::ItemUnavailable)
    );
    assert!(a.system.calls.is_empty());
    assert_eq!(a.state.next_session, 1);
    assert_eq!(
        launch_refusal_line("app_not_found", "org.example.unknown"),
        "pf-session-authorityd: launch_refused reason=app_not_found item_id=\"org.example.unknown\""
    );
}

#[test]
fn rpc_safe_return_while_running_starts_graceful_stop_and_consumes_queue() {
    let mut a = authority();
    launch_running(&mut a);

    assert!(matches!(
        handle_rpc(&mut a, RpcRequest::SafeReturn).unwrap(),
        RpcResponse::Ok
    ));

    assert!(matches!(a.state.phase, Phase::StoppingGracefully { .. }));
    assert_eq!(a.state.safe_return_queue, 0);
    assert_eq!(
        a.system
            .calls
            .iter()
            .filter(|call| *call == "graceful")
            .count(),
        1
    );
}

#[test]
fn rpc_safe_return_while_idle_does_not_affect_subsequent_launch() {
    let mut a = authority();

    assert!(matches!(
        handle_rpc(&mut a, RpcRequest::SafeReturn).unwrap(),
        RpcResponse::Ok
    ));
    assert!(matches!(a.state.phase, Phase::Idle));
    assert_eq!(a.state.safe_return_queue, 0);

    assert!(matches!(
        a.launch(LaunchRequest {
            item_id: APP_ID.into(),
        })
        .unwrap(),
        LaunchResult::Accepted { .. }
    ));
    assert!(matches!(a.state.phase, Phase::Running { .. }));
    assert_eq!(a.state.safe_return_queue, 0);
    assert_eq!(a.system.calls, ["start"]);
}

#[test]
fn grace_deadline_enforces_termination_and_publishes_forced_close() {
    let mut a = authority();
    let id = launch_running(&mut a);
    a.intake_safe_return().unwrap();
    a.tick().unwrap();
    a.clock.advance(Duration::from_millis(10));
    a.tick().unwrap();
    assert!(matches!(a.state.phase, Phase::ForceStopping { .. }));
    assert_eq!(a.system.calls.iter().filter(|c| *c == "force").count(), 1);
    a.observe(Observation::UnitInactive).unwrap();
    restore(&mut a);
    assert!(a.events_for("test").iter().any(|(_, e)| *e
        == SessionEvent::Terminal(TerminalReceipt::ForcedClose {
            session_id: id.clone()
        })));
}

#[test]
fn clean_exit_and_crash_are_typed_and_wait_for_presentation() {
    for (observation, crash) in [
        (Observation::SessionExitedCleanly, false),
        (
            Observation::SessionCrashed {
                summary: "segfault".into(),
            },
            true,
        ),
    ] {
        let mut a = authority();
        let id = launch_running(&mut a);
        a.observe(observation).unwrap();
        a.observe(Observation::UnitInactive).unwrap();
        restore(&mut a);
        let terminal = a
            .events_for("test")
            .into_iter()
            .map(|(_, e)| e)
            .find(|e| matches!(e, SessionEvent::Terminal(_)))
            .unwrap();
        if crash {
            assert_eq!(
                terminal,
                SessionEvent::Terminal(TerminalReceipt::Crash {
                    session_id: id,
                    summary: "segfault".into()
                })
            );
        } else {
            assert_eq!(
                terminal,
                SessionEvent::Terminal(TerminalReceipt::Returned { session_id: id })
            );
        }
    }
}

#[test]
fn fixed_wall_clock_stamps_end_observation_not_restoration_completion() {
    let make = || {
        set_wall_time(1_234);
        Authority::open_with_resolver_and_now_fn(
            MemoryStore::default(),
            FakeSystem::available(),
            TestClock::new(),
            3,
            Duration::from_millis(10),
            test_resolver(),
            fixed_wall_time,
        )
        .unwrap()
    };

    let mut returned = make();
    launch_running(&mut returned);
    returned.observe(Observation::SessionExitedCleanly).unwrap();
    set_wall_time(9_999);
    returned.observe(Observation::UnitInactive).unwrap();
    restore(&mut returned);
    let entry = returned.state.history.front().unwrap();
    assert_eq!(
        entry.started_at,
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_234))
    );
    assert_eq!(
        entry.ended_at,
        Some(EndStamp {
            at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_234),
            precision: EndPrecision::Observed,
        })
    );

    let mut forced_close = make();
    launch_running(&mut forced_close);
    forced_close.intake_safe_return().unwrap();
    forced_close.tick().unwrap();
    forced_close.clock.advance(Duration::from_millis(10));
    forced_close.tick().unwrap();
    set_wall_time(2_345);
    forced_close.observe(Observation::UnitInactive).unwrap();
    set_wall_time(9_999);
    restore(&mut forced_close);
    assert_eq!(
        forced_close.state.history.front().unwrap().ended_at,
        Some(EndStamp {
            at: SystemTime::UNIX_EPOCH + Duration::from_secs(2_345),
            precision: EndPrecision::Observed,
        })
    );

    let mut crashed = make();
    launch_running(&mut crashed);
    crashed
        .observe(Observation::SessionCrashed {
            summary: "fault".into(),
        })
        .unwrap();
    assert_eq!(
        crashed.state.history.front().unwrap().ended_at,
        Some(EndStamp {
            at: fixed_wall_time(),
            precision: EndPrecision::Approximate,
        })
    );
    crashed.observe(Observation::UnitInactive).unwrap();
    restore(&mut crashed);
}

#[test]
fn every_failure_rung_is_durable_recovery_required() {
    for rung in [
        FailureRung::Termination,
        FailureRung::UnitInactive,
        FailureRung::TargetReleased,
        FailureRung::OwnerActivation,
        FailureRung::OwnerActive,
        FailureRung::Presentation,
    ] {
        let mut a = authority();
        launch_running(&mut a);
        a.observe(Observation::Failed {
            rung,
            reason: "fault".into(),
        })
        .unwrap();
        assert!(matches!(
            a.store.snapshot().unwrap().phase,
            Phase::RecoveryRequired { .. }
        ));
        assert!(a
            .events_for("test")
            .iter()
            .any(|(_, e)| matches!(e, SessionEvent::RecoveryRequired(_))));
        assert!(!a
            .events_for("test")
            .iter()
            .any(|(_, e)| matches!(e, SessionEvent::Terminal(_))));
    }
    // tsp-ihne1.10: a failed kill on a close path is no longer a terminal recovery: the menu
    // spec (§9.6 "Stop failed") keeps the row retryable, so the phase is the durable StopFailed.
    let mut a = authority();
    launch_running(&mut a);
    a.intake_safe_return().unwrap();
    a.tick().unwrap();
    a.system.fail_force = true;
    a.clock.advance(Duration::from_millis(10));
    a.tick().unwrap();
    assert!(matches!(
        a.store.snapshot().unwrap().phase,
        Phase::StopFailed { .. }
    ));

    let mut a = authority();
    launch_running(&mut a);
    a.system.fail_graceful = true;
    a.system.fail_force = true;
    a.intake_safe_return().unwrap();
    a.tick().unwrap();
    assert!(matches!(
        a.store.snapshot().unwrap().phase,
        Phase::StopFailed { .. }
    ));
}

#[test]
fn owner_activation_command_failure_is_recovery() {
    let mut a = authority();
    launch_running(&mut a);
    a.observe(Observation::SessionExitedCleanly).unwrap();
    a.observe(Observation::UnitInactive).unwrap();
    a.system.fail_owner = true;
    a.observe(Observation::TargetReleased).unwrap();
    assert!(matches!(a.state.phase, Phase::RecoveryRequired { .. }));
}

#[test]
fn restart_mid_ladder_resumes_without_double_publication() {
    let mut a = authority();
    let id = launch_running(&mut a);
    a.observe(Observation::SessionExitedCleanly).unwrap();
    a.observe(Observation::UnitInactive).unwrap();
    a.observe(Observation::TargetReleased).unwrap();
    let (store, system, clock) = a.into_parts();
    let mut restarted =
        Authority::open(store, system, clock, 3, Duration::from_millis(10)).unwrap();
    restarted.observe(Observation::SelectedOwnerActive).unwrap();
    restarted
        .observe(Observation::PresentationAcknowledged)
        .unwrap();
    let once = restarted
        .events_for("test")
        .into_iter()
        .filter(|(_, e)| matches!(e, SessionEvent::Terminal(_)))
        .count();
    assert_eq!(once, 1);
    assert_eq!(
        restarted.history(),
        vec![SessionEvent::Terminal(TerminalReceipt::Returned {
            session_id: id
        })]
    );
    assert_eq!(
        restarted.observe(Observation::PresentationAcknowledged),
        Err(AuthorityError::InvalidObservation)
    );
}

#[test]
fn interrupted_start_intent_reconciles_from_the_exact_active_app_unit() {
    let persisted = PersistedState {
        next_session: 2,
        phase: Phase::Starting {
            session_id: "session-1".into(),
            item_id: APP_ID.into(),
            start_invoked: false,
        },
        ..PersistedState::default()
    };
    let mut store = MemoryStore::default();
    store.save(&persisted).unwrap();
    let system = FakeSystem {
        lifecycle: Some(SystemLifecycle {
            app: AppUnitState::Active,
            foreground_target_active: true,
            selected_owner_active: false,
        }),
        ..FakeSystem::available()
    };
    let mut authority = Authority::open_with_resolver(
        store,
        system,
        TestClock::new(),
        3,
        Duration::from_millis(10),
        test_resolver(),
    )
    .unwrap();

    authority.reconcile().unwrap();

    assert!(matches!(
        authority.state.phase,
        Phase::Running {
            ref session_id,
            ref item_id,
        } if session_id == "session-1" && item_id == APP_ID
    ));
    assert!(authority.system.calls.is_empty());
    assert_eq!(authority.state.history.len(), 1);
    assert_eq!(authority.state.history[0].item_id, APP_ID);
    let events = authority.events_for("restored-shell");
    assert!(matches!(
        events.as_slice(),
        [
            (_, SessionEvent::Observed(ObservedSessionState::Starting)),
            (_, SessionEvent::Observed(ObservedSessionState::Running))
        ]
    ));
}

#[test]
fn recent_is_bounded_busy_is_rejected_and_binding_updates_persist() {
    let mut a = authority();
    a.update_safe_return_binding(4).unwrap();
    a.update_safe_return_binding(3).unwrap();
    assert_eq!(a.state.safe_return_binding_revision, 4);
    for n in 0..5 {
        assert!(matches!(
            a.launch(LaunchRequest {
                item_id: format!("org.example.g{n}")
            })
            .unwrap(),
            LaunchResult::Accepted { .. }
        ));
        assert_eq!(
            a.launch(LaunchRequest {
                item_id: "org.example.busy".into()
            })
            .unwrap(),
            LaunchResult::RejectedBusy
        );
        a.observe(Observation::SessionExitedCleanly).unwrap();
        a.observe(Observation::UnitInactive).unwrap();
        restore(&mut a);
    }
    assert_eq!(a.state.history.len(), 3);
    assert_eq!(a.state.history.front().unwrap().item_id, "org.example.g4");
}

#[test]
fn protected_intake_survives_launcher_absence_and_app_cannot_consume_it() {
    let mut a = authority();
    launch_running(&mut a);
    // No foreground-app input API contains SafeReturn: only this independent durable intake does.
    a.intake_safe_return().unwrap();
    let (store, system, clock) = a.into_parts();
    let mut restarted =
        Authority::open(store, system, clock, 3, Duration::from_millis(10)).unwrap();
    restarted.reconcile().unwrap();
    assert!(matches!(
        restarted.state.phase,
        Phase::StoppingGracefully { .. }
    ));
}

#[test]
fn file_store_survives_a_real_atomic_restart() {
    let path = std::env::temp_dir().join(format!(
        "pf-session-authority-{}-state.json",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let mut a = Authority::open_with_resolver(
        FileStore::new(&path),
        FakeSystem::available(),
        TestClock::new(),
        3,
        Duration::from_millis(10),
        test_resolver(),
    )
    .unwrap();
    let LaunchResult::Accepted { session_id: id } = a
        .launch(LaunchRequest {
            item_id: APP_ID.into(),
        })
        .unwrap()
    else {
        panic!()
    };
    drop(a);
    let restarted = Authority::open(
        FileStore::new(&path),
        FakeSystem::available(),
        TestClock::new(),
        3,
        Duration::from_millis(10),
    )
    .unwrap();
    assert_eq!(restarted.session_id(), Some(id));
    std::fs::remove_file(path).unwrap();
}

#[test]
fn authority_accepts_then_start_failure_is_recorded_as_crash_drift() {
    let mut system = FakeSystem::available();
    system.fail_start = true;
    let mut a = Authority::open_with_resolver(
        MemoryStore::default(),
        system,
        TestClock::new(),
        3,
        Duration::from_millis(10),
        test_resolver(),
    )
    .unwrap();
    assert_eq!(
        a.launch(LaunchRequest {
            item_id: APP_ID.into()
        }),
        Ok(LaunchResult::Accepted {
            session_id: "session-1".into()
        })
    );
    assert!(matches!(
        a.store.snapshot().unwrap().phase,
        Phase::Restoring {
            receipt: Receipt::Crash { ref summary },
            rung: RestorationRung::UnitInactive,
            ..
        } if summary.contains("systemd_start_failed")
    ));
    assert_eq!(a.system.calls, ["start"]);
}

#[derive(Default)]
struct MustNotExec;

impl pf_app_launch::Exec for MustNotExec {
    fn exec(&mut self, _: &Path, _: &Path) -> io::Result<()> {
        panic!("mutated application must fail before exec")
    }
}

struct MutatingHelperSystem {
    resolver: Resolver,
    executable: PathBuf,
}

impl SessionSystem for MutatingHelperSystem {
    fn start_foreground(&mut self, request: &LaunchRequest, _: &str) -> Result<(), String> {
        fs::remove_file(&self.executable).map_err(|error| error.to_string())?;
        let xdg = self.executable.parent().unwrap().join("xdg");
        let error = pf_app_launch::launch_with(
            &self.resolver,
            &request.item_id,
            &xdg.join("config"),
            &xdg.join("state"),
            &mut MustNotExec,
        )
        .unwrap_err();
        Err(format!(
            "{}: helper exit {}",
            error.reason().as_str(),
            error.exit_code()
        ))
    }
    fn request_graceful_stop(&mut self, _: &str, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn enforce_termination(&mut self, _: &str, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn activate_selected_owner(&mut self) -> Result<(), String> {
        Ok(())
    }
}

#[test]
fn helper_revalidation_fails_closed_after_authority_acceptance_and_records_crash() {
    let (resolver, executable) = test_resolver_fixture();
    let system = MutatingHelperSystem {
        resolver: resolver.clone(),
        executable,
    };
    let mut authority = Authority::open_with_resolver(
        MemoryStore::default(),
        system,
        TestClock::new(),
        3,
        Duration::from_millis(10),
        resolver,
    )
    .unwrap();

    assert!(matches!(
        authority
            .launch(LaunchRequest {
                item_id: APP_ID.into(),
            })
            .unwrap(),
        LaunchResult::Accepted { .. }
    ));
    assert!(matches!(
        authority.state.phase,
        Phase::Restoring {
            receipt: Receipt::Crash { ref summary },
            ..
        } if summary.contains("exec_missing") && summary.contains("helper exit 66")
    ));
}

#[derive(Default)]
struct RefusingStore {
    saves: usize,
}
impl StateStore for RefusingStore {
    fn load(&self) -> Result<Option<PersistedState>, AuthorityError> {
        Ok(Some(PersistedState::default()))
    }
    fn save(&mut self, _: &PersistedState) -> Result<(), AuthorityError> {
        self.saves += 1;
        Err(AuthorityError::Persistence("refused".into()))
    }
}

#[derive(Default)]
struct FailOnceStore {
    state: Option<PersistedState>,
    successful_saves_before_failure: Option<usize>,
}

impl FailOnceStore {
    fn with_state(state: PersistedState) -> Self {
        Self {
            state: Some(state),
            successful_saves_before_failure: None,
        }
    }

    fn fail_after(&mut self, successful_saves: usize) {
        self.successful_saves_before_failure = Some(successful_saves);
    }
}

impl StateStore for FailOnceStore {
    fn load(&self) -> Result<Option<PersistedState>, AuthorityError> {
        Ok(self.state.clone())
    }

    fn save(&mut self, state: &PersistedState) -> Result<(), AuthorityError> {
        match self.successful_saves_before_failure {
            Some(0) => {
                self.successful_saves_before_failure = None;
                Err(AuthorityError::Persistence(
                    "injected transition save failure".into(),
                ))
            }
            Some(remaining) => {
                self.successful_saves_before_failure = Some(remaining - 1);
                self.state = Some(state.clone());
                Ok(())
            }
            None => {
                self.state = Some(state.clone());
                Ok(())
            }
        }
    }
}

#[test]
fn initial_state_publication_failure_aborts_open() {
    struct RefusingInitialStore;
    impl StateStore for RefusingInitialStore {
        fn load(&self) -> Result<Option<PersistedState>, AuthorityError> {
            Ok(None)
        }
        fn save(&mut self, _: &PersistedState) -> Result<(), AuthorityError> {
            Err(AuthorityError::Persistence(
                "initial publication refused".into(),
            ))
        }
    }

    let result = Authority::open_with_resolver(
        RefusingInitialStore,
        FakeSystem::available(),
        TestClock::new(),
        3,
        Duration::from_millis(10),
        test_resolver(),
    );

    assert!(matches!(
        result,
        Err(AuthorityError::Persistence(reason)) if reason == "initial publication refused"
    ));
}

#[test]
fn start_is_never_invoked_when_write_ahead_intent_save_fails() {
    let mut a = Authority::open_with_resolver(
        RefusingStore::default(),
        FakeSystem::available(),
        TestClock::new(),
        3,
        Duration::from_millis(10),
        test_resolver(),
    )
    .unwrap();
    assert_eq!(
        a.launch(LaunchRequest {
            item_id: APP_ID.into()
        }),
        Err(AuthorityError::Persistence("refused".into()))
    );
    assert!(a.system.calls.is_empty());
    assert!(matches!(a.state.phase, Phase::Idle));
    assert_eq!(a.state.next_session, 1);
}

#[test]
fn reopened_graceful_stop_is_immediately_due_without_old_monotonic_deadline() {
    let mut a = authority();
    launch_running(&mut a);
    a.intake_safe_return().unwrap();
    a.tick().unwrap();
    assert!(matches!(a.state.phase, Phase::StoppingGracefully { .. }));
    let (store, system, _) = a.into_parts();
    let mut reopened = Authority::open(
        store,
        system,
        TestClock::new(),
        3,
        Duration::from_millis(10),
    )
    .unwrap();
    reopened.reconcile().unwrap();
    assert!(matches!(reopened.state.phase, Phase::ForceStopping { .. }));
    assert_eq!(
        reopened
            .system
            .calls
            .iter()
            .filter(|call| *call == "force")
            .count(),
        1
    );
}

#[test]
fn acknowledged_client_cursor_survives_authority_restart_and_compacts_pending() {
    let mut a = authority();
    launch_running(&mut a);
    let delivered = a.events_for("launcher");
    let last = delivered.last().unwrap().0;
    a.acknowledge("launcher", last).unwrap();
    assert!(a.state.pending.is_empty());

    let (store, system, clock) = a.into_parts();
    let reopened = Authority::open(store, system, clock, 3, Duration::from_millis(10)).unwrap();
    assert!(reopened.events_for("launcher").is_empty());
}

// ---- tsp-f3fm.219: a crash/failed start must never wedge the authority silently ----

type Log = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

fn logged_authority(
    lifecycle: Option<SystemLifecycle>,
) -> (Authority<MemoryStore, FakeSystem, TestClock>, Log) {
    let log = Log::default();
    let sink = log.clone();
    let authority = Authority::open_with_resolver(
        MemoryStore::default(),
        FakeSystem {
            lifecycle,
            ..FakeSystem::available()
        },
        TestClock::new(),
        3,
        Duration::from_millis(10),
        test_resolver(),
    )
    .unwrap()
    .with_log_sink(move |line| sink.lock().unwrap().push(line.to_owned()));
    (authority, log)
}

/// systemd after P3's Poolsuite start crash: unit failed, target released, shell restarted.
fn crashed_unit_restored_owner() -> Option<SystemLifecycle> {
    Some(SystemLifecycle {
        app: AppUnitState::InactiveFailure {
            summary: "systemd result: exit-code".into(),
        },
        foreground_target_active: false,
        selected_owner_active: true,
    })
}

fn observed_running_or_starting(events: &[(u64, SessionEvent)]) -> Vec<u64> {
    events
        .iter()
        .filter(|(_, e)| {
            matches!(
                e,
                SessionEvent::Observed(
                    ObservedSessionState::Starting | ObservedSessionState::Running
                )
            )
        })
        .map(|(sequence, _)| *sequence)
        .collect()
}

#[test]
fn ended_session_starting_and_running_are_never_delivered_to_a_cursor_zero_client() {
    let mut a = authority();
    let first = launch_running(&mut a);
    // Positive control in the same run: while the session is live, a fresh client sees it.
    assert_eq!(
        observed_running_or_starting(&a.events_for("fresh")).len(),
        2
    );

    a.observe(Observation::SessionCrashed {
        summary: "systemd result: exit-code".into(),
    })
    .unwrap();
    assert!(a.state.history[0].ended_at.is_some());
    assert!(observed_running_or_starting(&a.events_for("restarted-shell")).is_empty());

    // Through the RPC the restarted shell uses, with reconcile advancing the ladder (P3 shape).
    a.system.lifecycle = crashed_unit_restored_owner();
    let RpcResponse::Events { events, .. } = handle_rpc(
        &mut a,
        RpcRequest::Events {
            client_id: "pf-shell".into(),
            contract: SESSION_RPC_CONTRACT_V1,
        },
    )
    .unwrap() else {
        panic!()
    };
    assert!(matches!(
        a.state.phase,
        Phase::Restoring {
            rung: RestorationRung::PresentationAcknowledged,
            ..
        }
    ));
    assert!(!events
        .iter()
        .any(|(_, e)| matches!(e, RpcEvent::Starting | RpcEvent::Running)));

    // Cursor safety: sequences are untouched, so the shell's later ack still validates/compacts.
    a.observe(Observation::PresentationAcknowledged).unwrap();
    a.system.lifecycle = None;
    let second = launch_running(&mut a);
    assert_ne!(first, second);
    let events = a.events_for("pf-shell");
    let live = observed_running_or_starting(&events);
    assert_eq!(
        live.len(),
        2,
        "the new session's Starting/Running are delivered"
    );
    let terminal = events
        .iter()
        .find(|(_, e)| matches!(e, SessionEvent::Terminal(_)))
        .unwrap()
        .0;
    assert!(live.iter().all(|sequence| *sequence > terminal));
    // Delivery filters; it never rewrites the queue, so the ended session's events are still
    // pending under their original sequences.
    assert!(a.state.pending.iter().any(|e| {
        matches!(
            e.event,
            WireEvent::ObservedStarting | WireEvent::ObservedRunning
        ) && e.sequence < terminal
    }));
    let last = events.last().unwrap().0;
    a.acknowledge("pf-shell", last).unwrap();
    assert!(a.events_for("pf-shell").is_empty());
    assert!(a.state.pending.is_empty());
}

#[test]
fn presentation_deadline_expires_on_tick_without_rpc_into_logged_recovery() {
    let (mut a, log) = logged_authority(None);
    launch_running(&mut a);
    a.system.lifecycle = crashed_unit_restored_owner();

    a.tick().unwrap();
    assert!(matches!(
        a.state.phase,
        Phase::Restoring {
            rung: RestorationRung::PresentationAcknowledged,
            ..
        }
    ));
    a.clock
        .advance(DEFAULT_PRESENTATION_TIMEOUT - Duration::from_millis(1));
    a.tick().unwrap();
    assert!(matches!(a.state.phase, Phase::Restoring { .. }));
    assert!(log.lock().unwrap().is_empty());

    a.clock.advance(Duration::from_millis(1));
    a.tick().unwrap();
    let Phase::RecoveryRequired {
        ref reason,
        ref pending_receipt,
        ..
    } = a.store.snapshot().unwrap().phase
    else {
        panic!("expected durable RecoveryRequired, got {:?}", a.state.phase)
    };
    assert_eq!(
        reason,
        "presentation_not_acknowledged: presentation not acknowledged within 10000 ms"
    );
    assert_eq!(
        pending_receipt,
        &Some(Receipt::Crash {
            summary: "systemd result: exit-code".into()
        })
    );
    assert!(a.events_for("pf-shell").iter().any(|(_, e)| matches!(
        e,
        SessionEvent::RecoveryRequired(RecoveryRequired { reason, .. })
            if reason.starts_with("presentation_not_acknowledged:")
    )));
    assert_eq!(
        *log.lock().unwrap(),
        [
            "pf-session-authorityd: lifecycle_failure reason=presentation_not_acknowledged \
          item_id=\"org.example.game\" detail=\"presentation not acknowledged within 10000 ms\""
        ]
    );
    // The receipt is written only at Idle: history still reads "restoration pending".
    assert!(a.state.history[0].ended_at.is_some());
    assert_eq!(a.state.history[0].receipt, None);
}

#[test]
fn late_presentation_ack_after_timeout_reaches_idle_with_receipt_in_history() {
    let (mut a, _log) = logged_authority(None);
    let id = launch_running(&mut a);
    a.system.lifecycle = crashed_unit_restored_owner();
    a.tick().unwrap();
    a.clock.advance(DEFAULT_PRESENTATION_TIMEOUT);
    a.tick().unwrap();
    assert!(matches!(a.state.phase, Phase::RecoveryRequired { .. }));

    handle_rpc(
        &mut a,
        RpcRequest::Observe {
            observation: RpcObservation::PresentationAcknowledged,
        },
    )
    .unwrap();
    assert!(matches!(a.store.snapshot().unwrap().phase, Phase::Idle));
    let receipt = Receipt::Crash {
        summary: "systemd result: exit-code".into(),
    };
    assert_eq!(a.state.history[0].receipt, Some(receipt));
    assert_eq!(
        a.history(),
        [SessionEvent::Terminal(TerminalReceipt::Crash {
            session_id: id,
            summary: "systemd result: exit-code".into(),
        })]
    );
    a.system.lifecycle = None;
    assert!(matches!(
        a.launch(LaunchRequest {
            item_id: APP_ID.into()
        })
        .unwrap(),
        LaunchResult::Accepted { .. }
    ));
}

#[test]
fn every_other_recovery_reason_stays_terminal_for_a_late_ack() {
    for rung in [
        FailureRung::Presentation,
        FailureRung::OwnerActive,
        FailureRung::TargetReleased,
    ] {
        let mut a = authority();
        launch_running(&mut a);
        a.observe(Observation::SessionExitedCleanly).unwrap();
        a.observe(Observation::Failed {
            rung,
            reason: "fault".into(),
        })
        .unwrap();
        assert_eq!(
            a.observe(Observation::PresentationAcknowledged),
            Err(AuthorityError::InvalidObservation)
        );
        assert!(matches!(a.state.phase, Phase::RecoveryRequired { .. }));
        assert_eq!(a.state.history[0].receipt, None);
    }
}

#[test]
fn restarted_daemon_rearms_a_full_presentation_deadline() {
    let (mut a, _log) = logged_authority(None);
    launch_running(&mut a);
    a.system.lifecycle = crashed_unit_restored_owner();
    a.tick().unwrap();
    a.clock.advance(DEFAULT_PRESENTATION_TIMEOUT * 2);
    let (store, system, clock) = a.into_parts();
    let log = Log::default();
    let sink = log.clone();
    let mut reopened = Authority::open(store, system, clock, 3, Duration::from_millis(10))
        .unwrap()
        .with_log_sink(move |line| sink.lock().unwrap().push(line.to_owned()));
    reopened.tick().unwrap();
    assert!(matches!(reopened.state.phase, Phase::Restoring { .. }));
    reopened.clock.advance(DEFAULT_PRESENTATION_TIMEOUT);
    reopened.tick().unwrap();
    assert!(matches!(
        reopened.state.phase,
        Phase::RecoveryRequired { .. }
    ));
    assert_eq!(log.lock().unwrap().len(), 1);
}

#[test]
fn busy_launch_is_refused_with_a_logged_reason_never_silently() {
    let (mut a, log) = logged_authority(None);
    launch_running(&mut a);
    let busy = LaunchRequest {
        item_id: "org.example.busy".into(),
    };
    assert_eq!(a.launch(busy.clone()), Ok(LaunchResult::RejectedBusy));

    a.system.lifecycle = crashed_unit_restored_owner();
    a.tick().unwrap();
    let response = handle_rpc(
        &mut a,
        RpcRequest::Launch {
            item_id: "org.example.busy".into(),
        },
    )
    .unwrap();
    assert!(matches!(response, RpcResponse::RejectedBusy));
    assert_eq!(
        *log.lock().unwrap(),
        [
            "pf-session-authorityd: launch_refused reason=busy item_id=\"org.example.busy\" \
             phase=running",
            "pf-session-authorityd: launch_refused reason=busy item_id=\"org.example.busy\" \
             phase=restoring",
        ]
    );
    // Refuse, never queue: the refused request left no trace in the session state.
    assert_eq!(a.state.next_session, 2);
    assert_eq!(a.state.history.len(), 1);
}

#[test]
fn service_loop_ticks_the_ladder_with_no_client_rpc() {
    let (a, log) = logged_authority(None);
    let mut a = a.with_presentation_timeout(Duration::ZERO);
    launch_running(&mut a);
    a.system.lifecycle = crashed_unit_restored_owner();

    let (connections, incoming) = std::sync::mpsc::channel::<io::Result<u32>>();
    connections.send(Ok(7)).unwrap();
    let closer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        drop(connections);
    });
    let mut served = Vec::new();
    run_service_loop(
        &mut a,
        &incoming,
        Duration::from_millis(5),
        |_, connection| {
            served.push(connection);
            Ok(())
        },
    )
    .unwrap();
    closer.join().unwrap();

    assert_eq!(served, [7], "only the one queued connection was served");
    assert!(matches!(
        a.state.phase,
        Phase::RecoveryRequired { ref reason, .. } if is_presentation_timeout(reason)
    ));
    assert_eq!(a.system.calls.iter().filter(|c| *c == "owner").count(), 1);
    assert!(
        log.lock().unwrap()[0].contains("lifecycle_failure reason=presentation_not_acknowledged")
    );
}

#[test]
fn service_loop_ends_on_a_connection_source_error() {
    let mut a = authority();
    let (connections, incoming) = std::sync::mpsc::channel::<io::Result<u32>>();
    connections
        .send(Err(io::Error::other("accept failed")))
        .unwrap();
    let error =
        run_service_loop(&mut a, &incoming, Duration::from_secs(60), |_, _| Ok(())).unwrap_err();
    assert_eq!(error.to_string(), "accept failed");
}

#[test]
fn recovery_required_json_without_a_pending_receipt_is_unchanged() {
    let phase = Phase::RecoveryRequired {
        session_id: "session-1".into(),
        item_id: APP_ID.into(),
        reason: "owner_not_active: fault".into(),
        pending_receipt: None,
    };
    let json = serde_json::to_string(&phase).unwrap();
    assert_eq!(
        json,
        r#"{"RecoveryRequired":{"session_id":"session-1","item_id":"org.example.game","reason":"owner_not_active: fault"}}"#
    );
    assert_eq!(serde_json::from_str::<Phase>(&json).unwrap(), phase);
}

fn short_socket_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from("/tmp").join(format!(
        "pfsa-{name}-{}-{}",
        std::process::id(),
        RESOLVER_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn raw_rpc(socket: &Path, request: &RpcRequest) -> RpcResponse {
    let mut stream = std::os::unix::net::UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    pf_wire::write_frame(&mut stream, &serde_json::to_vec(request).unwrap()).unwrap();
    serde_json::from_slice(&pf_wire::read_frame(&mut stream).unwrap()).unwrap()
}

const TEST_GAMER_UID: u32 = 1000;
const TEST_SHELL_UNIT: &str = "pf-shell-selected.service";
const TEST_BROKER_UNIT: &str = "pf-input-broker.service";

fn shell_policy_for(uid: u32, unit: &str) -> ShellPolicy {
    let unit = unit.to_owned();
    ShellPolicy::custom(
        TEST_GAMER_UID,
        TEST_SHELL_UNIT.into(),
        TEST_BROKER_UNIT.into(),
        move |_| {
            Ok(pf_peer_identity::PeerServiceIdentity {
                cred: pf_peer_identity::PeerCred {
                    pid: 4242,
                    uid,
                    gid: uid,
                },
                unit: unit.clone(),
            })
        },
    )
    .unwrap()
}

fn trusted_shell_policy() -> ShellPolicy {
    shell_policy_for(TEST_GAMER_UID, TEST_SHELL_UNIT)
}

fn assert_shell_socket_peer_refused(name: &str, uid: u32, unit: &str) {
    use std::io::Read as _;
    let dir = short_socket_dir(name);
    let socket = dir.join("a.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let (requests, incoming) = std::sync::mpsc::channel();
    spawn_rpc_acceptor(
        listener,
        requests,
        DEFAULT_CONNECTION_LIMITS,
        shell_policy_for(uid, unit),
    );

    let mut stream = std::os::unix::net::UnixStream::connect(&socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut byte = [0; 1];
    assert_eq!(stream.read(&mut byte).unwrap(), 0, "{name} was not closed");
    assert!(
        incoming.recv_timeout(Duration::from_millis(100)).is_err(),
        "{name} reached the authority loop"
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn shell_socket_refuses_apps_and_wrong_uid_services_at_accept() {
    assert_shell_socket_peer_refused(
        "shell-app-refused",
        TEST_GAMER_UID,
        "pf-app@org.example.game.service",
    );
    assert_shell_socket_peer_refused("shell-wrong-uid", TEST_GAMER_UID + 1, TEST_SHELL_UNIT);
    assert_shell_socket_peer_refused("broker-nonroot", TEST_GAMER_UID, TEST_BROKER_UNIT);
}

#[test]
fn shell_identity_can_send_the_full_shell_request_set() {
    let dir = short_socket_dir("shell-full-set");
    let socket = dir.join("a.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let (requests, incoming) = std::sync::mpsc::channel();
    spawn_rpc_acceptor(
        listener,
        requests,
        DEFAULT_CONNECTION_LIMITS,
        trusted_shell_policy(),
    );
    let requests = [
        RpcRequest::Launch {
            item_id: APP_ID.into(),
        },
        RpcRequest::SafeReturn,
        RpcRequest::Events {
            client_id: "shell".into(),
            contract: SESSION_RPC_CONTRACT_V1,
        },
        RpcRequest::Acknowledge {
            client_id: "shell".into(),
            sequence: 1,
        },
        RpcRequest::History,
        RpcRequest::Observe {
            observation: RpcObservation::SessionRunning,
        },
        RpcRequest::Tick,
        RpcRequest::OpenUrl {
            app_id: None,
            url: "https://example.test/".into(),
            flags: 0,
        },
        RpcRequest::ReturnToCaller {
            app_id: APP_ID.into(),
            url: None,
        },
    ];

    for request in requests {
        let client_socket = socket.clone();
        let sent = request.clone();
        let client = std::thread::spawn(move || raw_rpc(&client_socket, &sent));
        let pending = incoming
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(pending.origin(), &RpcOrigin::Shell);
        pending.respond(RpcResponse::Ok);
        assert!(matches!(client.join().unwrap(), RpcResponse::Ok));
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn broker_identity_is_limited_to_safe_return() {
    let dir = short_socket_dir("broker-scope");
    let socket = dir.join("a.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let (requests, incoming) = std::sync::mpsc::channel();
    spawn_rpc_acceptor(
        listener,
        requests,
        DEFAULT_CONNECTION_LIMITS,
        shell_policy_for(0, TEST_BROKER_UNIT),
    );
    let (mut authority, log) = logged_authority(None);
    launch_running(&mut authority);

    for (request, accepted) in [
        (
            RpcRequest::Launch {
                item_id: APP_ID.into(),
            },
            false,
        ),
        (
            RpcRequest::OpenUrl {
                app_id: None,
                url: "https://example.test/".into(),
                flags: 0,
            },
            false,
        ),
        (RpcRequest::SafeReturn, true),
    ] {
        let client_socket = socket.clone();
        let sent = request.clone();
        let client = std::thread::spawn(move || raw_rpc(&client_socket, &sent));
        let pending = incoming
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(pending.origin(), &RpcOrigin::Broker);
        pending.dispatch(&mut authority);
        let response = client.join().unwrap();
        assert_eq!(matches!(&response, RpcResponse::Ok), accepted);
        if !accepted {
            assert!(matches!(&response, RpcResponse::Denied));
        }
    }

    assert!(
        matches!(authority.state().phase, Phase::StoppingGracefully { .. }),
        "broker SafeReturn did not enter the close path: {:?}",
        authority.state().phase
    );
    let log = log.lock().unwrap();
    assert!(log.iter().any(|line| line.contains("method=launch")));
    assert!(log.iter().any(|line| line.contains("method=open_url")));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn overlapping_shell_and_broker_identity_never_gets_shell_authority() {
    let overlapping =
        ShellPolicy::custom(0, TEST_BROKER_UNIT.into(), TEST_BROKER_UNIT.into(), |_| {
            unreachable!("overlapping policy must be refused before identity resolution")
        });
    let error = overlapping.err().expect("overlap was accepted");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    assert!(error.to_string().contains("overlaps the root broker"));

    assert!(ShellPolicy::custom(
        TEST_GAMER_UID,
        TEST_SHELL_UNIT.into(),
        TEST_BROKER_UNIT.into(),
        |_| unreachable!("construction does not resolve a peer"),
    )
    .is_ok());
}

#[test]
fn stalled_clients_never_block_the_service_loop_tick() {
    use std::io::Write as _;
    let dir = short_socket_dir("stall");
    let socket = dir.join("a.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    // Generous I/O timeout: the tick must survive by structure, not by the timeout expiring.
    let limits = ConnectionLimits {
        io_timeout: Duration::from_secs(60),
        ..DEFAULT_CONNECTION_LIMITS
    };
    let (requests, incoming) = std::sync::mpsc::channel();
    spawn_rpc_acceptor(listener, requests, limits, trusted_shell_policy());
    let silent = std::os::unix::net::UnixStream::connect(&socket).unwrap();
    let mut partial = std::os::unix::net::UnixStream::connect(&socket).unwrap();
    partial.write_all(&[0, 0]).unwrap();

    let (a, log) = logged_authority(None);
    // TestClock does not advance on its own: a zero deadline expires on the first tick that
    // reaches the rung, so RecoveryRequired proves the loop ticked while both clients stalled.
    let mut a = a.with_presentation_timeout(Duration::ZERO);
    launch_running(&mut a);
    a.system.lifecycle = crashed_unit_restored_owner();
    let client_socket = socket.clone();
    let client = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        raw_rpc(&client_socket, &RpcRequest::History)
    });
    let mut phase_when_served = None;
    let stopped = run_service_loop(
        &mut a,
        &incoming,
        Duration::from_millis(5),
        |authority, pending: PendingRpc| {
            phase_when_served = Some(authority.state().phase.clone());
            let response = dispatch_rpc(authority, pending.request.clone());
            pending.respond(response);
            Err(io::Error::other("test served its request"))
        },
    );

    assert_eq!(stopped.unwrap_err().to_string(), "test served its request");
    assert!(matches!(
        client.join().unwrap(),
        RpcResponse::History { .. }
    ));
    assert!(
        matches!(
            phase_when_served,
            Some(Phase::RecoveryRequired { ref reason, .. }) if is_presentation_timeout(reason)
        ),
        "the deadline fired while two clients stalled: {phase_when_served:?}"
    );
    assert!(log.lock().unwrap()[0].contains("presentation_not_acknowledged"));
    drop((silent, partial));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn acceptor_closes_connections_beyond_its_bound() {
    use std::io::Read as _;
    let dir = short_socket_dir("bound");
    let socket = dir.join("a.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let limits = ConnectionLimits {
        io_timeout: Duration::from_secs(60),
        response_timeout: Duration::from_secs(60),
        max_connections: 1,
    };
    let (requests, _incoming) = std::sync::mpsc::channel();
    spawn_rpc_acceptor(listener, requests, limits, trusted_shell_policy());
    let _occupying = std::os::unix::net::UnixStream::connect(&socket).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let mut refused = std::os::unix::net::UnixStream::connect(&socket).unwrap();
    refused
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut byte = [0; 1];
    assert_eq!(
        refused.read(&mut byte).unwrap(),
        0,
        "closed without service"
    );
    fs::remove_dir_all(dir).unwrap();
}

// ---- tsp-ihne1.10: close-progress events (contract v2) for the system menu ----
//
// The menu never decides anything during a close; it renders the authority's progress. These
// tests pin the exact §9.6 event vocabulary on the router-facing v2 RPC stream, the retryable
// StopFailed phase, the negotiated contract, and the escalation hook the hang sampler attaches
// to (cr-collect samples just before enforce_termination; the close is never delayed).

/// Drains the router-facing v2 stream for `client` and acknowledges it, exactly as the
/// system-menu router polls: one cursor, every event once, in publication order.
fn drain_v2<S: StateStore, B: SessionSystem, C: Clock>(
    a: &mut Authority<S, B, C>,
    client: &str,
) -> Vec<RpcEvent> {
    let RpcResponse::Events { events, contract } = handle_rpc(
        a,
        RpcRequest::Events {
            client_id: client.into(),
            contract: SESSION_RPC_CONTRACT_V2,
        },
    )
    .unwrap() else {
        panic!("expected an events response")
    };
    assert_eq!(contract, SESSION_RPC_CONTRACT_V2);
    if let Some((last, _)) = events.last() {
        a.acknowledge(client, *last).unwrap();
    }
    events.into_iter().map(|(_, event)| event).collect()
}

#[test]
fn close_progress_stream_matches_menu_spec_9_6_when_the_app_stops_in_time() {
    let mut a = authority();
    let id = launch_running(&mut a);
    assert_eq!(
        drain_v2(&mut a, "menu"),
        [RpcEvent::Starting, RpcEvent::Running]
    );

    // A starts the graceful stop; the row becomes "Closing…" and the menu stays open.
    a.intake_safe_return().unwrap();
    a.tick().unwrap();
    assert!(matches!(a.state.phase, Phase::StoppingGracefully { .. }));
    assert_eq!(
        drain_v2(&mut a, "menu"),
        [RpcEvent::StoppingGracefully {
            session_id: id.clone()
        }]
    );

    // The app stops inside the grace window: no escalation ever happens.
    a.observe(Observation::UnitInactive).unwrap();
    restore(&mut a);
    assert_eq!(
        drain_v2(&mut a, "menu"),
        [
            RpcEvent::ObservationComplete,
            RpcEvent::Returned { session_id: id },
        ]
    );
    assert_eq!(
        a.system.calls.iter().filter(|c| *c == "force").count(),
        0,
        "a stop in time must never reach enforce_termination"
    );
}

#[test]
fn close_progress_stream_matches_menu_spec_9_6_when_the_stop_escalates() {
    let mut a = authority();
    let id = launch_running(&mut a);
    assert_eq!(
        drain_v2(&mut a, "menu"),
        [RpcEvent::Starting, RpcEvent::Running]
    );

    a.intake_safe_return().unwrap();
    a.tick().unwrap();
    // The app "didn't stop in time": the authority escalates by itself.
    a.clock.advance(Duration::from_millis(10));
    a.tick().unwrap();
    assert!(matches!(a.state.phase, Phase::ForceStopping { .. }));
    assert_eq!(
        drain_v2(&mut a, "menu"),
        [
            RpcEvent::StoppingGracefully {
                session_id: id.clone()
            },
            RpcEvent::ForceStopping {
                session_id: id.clone(),
                reason: ForceStopReason::StopTimeout,
            },
        ]
    );

    a.observe(Observation::UnitInactive).unwrap();
    restore(&mut a);
    // Returned with the escalated flag: the ForcedClose receipt is the flag (§9.6 toast
    // "‹App› was force-closed").
    assert_eq!(
        drain_v2(&mut a, "menu"),
        [
            RpcEvent::ObservationComplete,
            RpcEvent::ForcedClose { session_id: id },
        ]
    );
}

#[test]
fn close_progress_stream_matches_menu_spec_9_6_when_stop_fails_and_the_user_retries() {
    let mut a = authority();
    let id = launch_running(&mut a);
    assert_eq!(
        drain_v2(&mut a, "menu"),
        [RpcEvent::Starting, RpcEvent::Running]
    );

    a.intake_safe_return().unwrap();
    a.tick().unwrap();
    a.system.fail_force = true;
    a.clock.advance(Duration::from_millis(10));
    a.tick().unwrap();

    // §9.6 "Stop failed": the row keeps offering the retry, and no terminal receipt exists.
    assert!(matches!(
        a.store.snapshot().unwrap().phase,
        Phase::StopFailed {
            ref session_id,
            ref reason,
            ..
        } if session_id == &id && reason == "app_exit_failed: refused"
    ));
    assert_eq!(
        drain_v2(&mut a, "menu"),
        [
            RpcEvent::StoppingGracefully {
                session_id: id.clone()
            },
            RpcEvent::StopFailed {
                session_id: id.clone(),
                reason: "app_exit_failed: refused".into(),
            },
        ]
    );
    assert!(
        !a.events_for("test")
            .iter()
            .any(|(_, e)| matches!(e, SessionEvent::Terminal(_))),
        "a failed stop leaves the session open and retryable"
    );

    // The retry survives a daemon restart: StopFailed is durable, not a memory-only limbo.
    a.system.fail_force = false;
    let (store, system, clock) = a.into_parts();
    let mut a = Authority::open(store, system, clock, 3, Duration::from_millis(10)).unwrap();
    assert!(matches!(a.state.phase, Phase::StopFailed { .. }));

    // A retries: the full close sequence runs again from the top.
    assert!(matches!(
        handle_rpc(&mut a, RpcRequest::SafeReturn).unwrap(),
        RpcResponse::Ok
    ));
    assert!(matches!(a.state.phase, Phase::StoppingGracefully { .. }));
    a.clock.advance(Duration::from_millis(10));
    a.tick().unwrap();
    assert!(matches!(a.state.phase, Phase::ForceStopping { .. }));
    a.observe(Observation::UnitInactive).unwrap();
    restore(&mut a);
    assert_eq!(
        drain_v2(&mut a, "menu"),
        [
            RpcEvent::StoppingGracefully {
                session_id: id.clone()
            },
            RpcEvent::ForceStopping {
                session_id: id.clone(),
                reason: ForceStopReason::StopTimeout,
            },
            RpcEvent::ObservationComplete,
            RpcEvent::ForcedClose { session_id: id },
        ]
    );
}

#[derive(Clone, Default)]
struct SharedLogSystem {
    log: Log,
    fail_graceful: bool,
    fail_force: bool,
    lifecycle: Option<SystemLifecycle>,
}
impl SessionSystem for SharedLogSystem {
    fn start_foreground(&mut self, _: &LaunchRequest, _: &str) -> Result<(), String> {
        self.log.lock().unwrap().push("start".into());
        Ok(())
    }
    fn request_graceful_stop(&mut self, _: &str, _: &str) -> Result<(), String> {
        self.log.lock().unwrap().push("graceful".into());
        if self.fail_graceful {
            Err("unavailable".into())
        } else {
            Ok(())
        }
    }
    fn enforce_termination(&mut self, _: &str, _: &str) -> Result<(), String> {
        self.log.lock().unwrap().push("force".into());
        if self.fail_force {
            Err("refused".into())
        } else {
            Ok(())
        }
    }
    fn activate_selected_owner(&mut self) -> Result<(), String> {
        self.log.lock().unwrap().push("owner".into());
        Ok(())
    }
    fn lifecycle(&mut self, _: &str) -> Result<Option<SystemLifecycle>, String> {
        Ok(self.lifecycle.clone())
    }
}

fn sampler_line(context: &EscalationContext) -> String {
    format!(
        "hook:{}:{}:{}",
        context.reason.as_str(),
        context.session_id,
        context.item_id
    )
}

/// An authority whose escalation hook writes into the same log the system writes into, so the
/// tests can prove the hook fires immediately before the kill and never after it.
fn hooked_authority(
    system: SharedLogSystem,
    log: Log,
) -> Authority<MemoryStore, SharedLogSystem, TestClock> {
    Authority::open_with_resolver(
        MemoryStore::default(),
        system,
        TestClock::new(),
        3,
        Duration::from_millis(10),
        test_resolver(),
    )
    .unwrap()
    .with_escalation_hook(move |context| log.lock().unwrap().push(sampler_line(context)))
}

fn launch_hooked(a: &mut Authority<MemoryStore, SharedLogSystem, TestClock>) -> String {
    let LaunchResult::Accepted { session_id } = a
        .launch(LaunchRequest {
            item_id: APP_ID.into(),
        })
        .unwrap()
    else {
        panic!()
    };
    session_id
}

#[test]
fn escalation_hook_fires_just_before_every_close_path_kill() {
    // Path 1: the graceful window expired (the §9.6 escalation).
    let log: Log = Log::default();
    let mut a = hooked_authority(
        SharedLogSystem {
            log: log.clone(),
            ..SharedLogSystem::default()
        },
        log.clone(),
    );
    let id = launch_hooked(&mut a);
    a.intake_safe_return().unwrap();
    a.tick().unwrap();
    a.clock.advance(Duration::from_millis(10));
    a.tick().unwrap();
    assert!(matches!(a.state.phase, Phase::ForceStopping { .. }));
    assert_eq!(
        *log.lock().unwrap(),
        vec![
            "start".to_owned(),
            "graceful".to_owned(),
            format!("hook:stop_timeout:{id}:{APP_ID}"),
            "force".to_owned(),
        ],
        "the sampler runs synchronously immediately before enforce_termination"
    );

    // Path 2: a daemon restart lost the monotonic deadline; reconcile escalates at once.
    let log2: Log = Log::default();
    let mut armed = hooked_authority(
        SharedLogSystem {
            log: log2.clone(),
            ..SharedLogSystem::default()
        },
        log2.clone(),
    );
    let id2 = launch_hooked(&mut armed);
    armed.intake_safe_return().unwrap();
    armed.tick().unwrap();
    assert!(matches!(
        armed.state.phase,
        Phase::StoppingGracefully { .. }
    ));
    let (store, system, _) = armed.into_parts();
    let log3: Log = Log::default();
    let hook3 = log3.clone();
    let mut reopened = Authority::open(
        store,
        system,
        TestClock::new(),
        3,
        Duration::from_millis(10),
    )
    .unwrap()
    .with_escalation_hook(move |context| hook3.lock().unwrap().push(sampler_line(context)));
    reopened.reconcile().unwrap();
    assert!(matches!(reopened.state.phase, Phase::ForceStopping { .. }));
    // The reopened hook fires before the kill; the moved system still logs into log2, so the
    // pairing across the two logs proves the restart path escalates through the hook exactly once.
    assert_eq!(
        *log3.lock().unwrap(),
        vec![format!("hook:stop_timeout:{id2}:{APP_ID}")]
    );
    assert_eq!(
        *log2.lock().unwrap(),
        vec![
            "start".to_owned(),
            "graceful".to_owned(),
            "force".to_owned()
        ]
    );
    // The escalation event is durable for the router even across the restart.
    assert!(reopened.state.pending.iter().any(|e| matches!(
        e.event,
        WireEvent::ForceStopping {
            reason: ForceStopReason::StopTimeout,
            ..
        }
    )));

    // Path 3: the graceful-stop request itself failed, so the kill is immediate.
    let log4: Log = Log::default();
    let mut failed_request = hooked_authority(
        SharedLogSystem {
            log: log4.clone(),
            fail_graceful: true,
            ..SharedLogSystem::default()
        },
        log4.clone(),
    );
    let id4 = launch_hooked(&mut failed_request);
    failed_request.intake_safe_return().unwrap();
    failed_request.tick().unwrap();
    assert!(matches!(
        failed_request.state.phase,
        Phase::ForceStopping { .. }
    ));
    assert_eq!(
        *log4.lock().unwrap(),
        vec![
            "start".to_owned(),
            "graceful".to_owned(),
            format!("hook:stop_request_failed:{id4}:{APP_ID}"),
            "force".to_owned(),
        ]
    );
    assert!(failed_request.state.pending.iter().any(|e| matches!(
        e.event,
        WireEvent::ForceStopping {
            reason: ForceStopReason::StopRequestFailed,
            ..
        }
    )));
}

#[test]
fn escalation_hook_never_fires_when_systemd_beats_the_deadline() {
    // Negative: the app died to systemd's own TimeoutStopSec backstop during the graceful
    // window (Result=timeout). The pinned tsp-f3fm.202.1 contract completes that as Returned
    // with no second kill, so no escalation happens and the sampler must not run either.
    let log: Log = Log::default();
    let mut raced = hooked_authority(
        SharedLogSystem {
            log: log.clone(),
            ..SharedLogSystem::default()
        },
        log.clone(),
    );
    launch_hooked(&mut raced);
    raced.intake_safe_return().unwrap();
    raced.tick().unwrap();
    raced.system.lifecycle = Some(SystemLifecycle {
        app: AppUnitState::InactiveFailure {
            summary: "systemd result: timeout".into(),
        },
        foreground_target_active: false,
        selected_owner_active: true,
    });
    raced.tick().unwrap();
    assert!(matches!(
        raced.state.phase,
        Phase::Restoring {
            receipt: Receipt::Returned,
            ..
        }
    ));
    assert!(
        !log.lock()
            .unwrap()
            .iter()
            .any(|line| line.starts_with("hook:") || line == "force"),
        "the backstop race must not sample or kill again: {:?}",
        log.lock().unwrap()
    );

    // Positive control in the same invocation: when the app does not die by itself, the very
    // same setup escalates and the hook fires.
    let log2: Log = Log::default();
    let mut escalated = hooked_authority(
        SharedLogSystem {
            log: log2.clone(),
            ..SharedLogSystem::default()
        },
        log2.clone(),
    );
    launch_hooked(&mut escalated);
    escalated.intake_safe_return().unwrap();
    escalated.tick().unwrap();
    escalated.clock.advance(Duration::from_millis(10));
    escalated.tick().unwrap();
    assert!(matches!(escalated.state.phase, Phase::ForceStopping { .. }));
    assert!(log2
        .lock()
        .unwrap()
        .iter()
        .any(|line| line.starts_with("hook:stop_timeout:")));
}

#[test]
fn restart_over_an_already_stopped_unit_completes_as_returned_without_escalating() {
    // The daemon went down mid-graceful-stop and lost its monotonic deadline. While it was
    // down the app stopped — by itself or by systemd's own backstop. On restart, reconcile
    // must read the unit before it acts: no sampler, no second kill, and the close completes
    // as Returned — never a bogus ForceStopping/ForcedClose or a transient StopFailed.
    for app in [
        AppUnitState::InactiveSuccess,
        AppUnitState::InactiveFailure {
            summary: "systemd result: timeout".into(),
        },
    ] {
        let log: Log = Log::default();
        let mut armed = hooked_authority(
            SharedLogSystem {
                log: log.clone(),
                ..SharedLogSystem::default()
            },
            log.clone(),
        );
        let id = launch_hooked(&mut armed);
        armed.intake_safe_return().unwrap();
        armed.tick().unwrap();
        assert!(matches!(
            armed.state.phase,
            Phase::StoppingGracefully { .. }
        ));

        let (store, mut system, _) = armed.into_parts();
        system.lifecycle = Some(SystemLifecycle {
            app: app.clone(),
            foreground_target_active: false,
            selected_owner_active: true,
        });
        let hook_log: Log = Log::default();
        let hook_sink = hook_log.clone();
        let mut reopened = Authority::open(
            store,
            system,
            TestClock::new(),
            3,
            Duration::from_millis(10),
        )
        .unwrap()
        .with_escalation_hook(move |context| hook_sink.lock().unwrap().push(sampler_line(context)));

        reopened.reconcile().unwrap();

        assert!(
            hook_log.lock().unwrap().is_empty(),
            "no hang sample for a unit that already stopped: {:?}",
            hook_log.lock().unwrap()
        );
        assert!(
            !log.lock().unwrap().iter().any(|line| line == "force"),
            "a stopped unit must not be killed again: {:?}",
            log.lock().unwrap()
        );
        assert!(matches!(
            reopened.state.phase,
            Phase::Restoring {
                receipt: Receipt::Returned,
                ..
            }
        ));
        // The menu-facing stream shows the durable StoppingGracefully but no bogus escalation.
        let events = drain_v2(&mut reopened, "menu");
        assert!(events.iter().any(|event| matches!(
            event,
            RpcEvent::StoppingGracefully { session_id } if session_id == &id
        )));
        assert!(
            !events.iter().any(|event| matches!(
                event,
                RpcEvent::ForceStopping { .. }
                    | RpcEvent::StopFailed { .. }
                    | RpcEvent::ForcedClose { .. }
            )),
            "{events:?}"
        );
    }

    // Positive control in the same invocation: a restart over a unit that is STILL active
    // escalates through the hook exactly as before.
    let log3: Log = Log::default();
    let mut armed3 = hooked_authority(
        SharedLogSystem {
            log: log3.clone(),
            ..SharedLogSystem::default()
        },
        log3.clone(),
    );
    let id3 = launch_hooked(&mut armed3);
    armed3.intake_safe_return().unwrap();
    armed3.tick().unwrap();
    let (store3, mut system3, _) = armed3.into_parts();
    system3.lifecycle = Some(SystemLifecycle {
        app: AppUnitState::Active,
        foreground_target_active: true,
        selected_owner_active: false,
    });
    let hook_log3: Log = Log::default();
    let hook_sink3 = hook_log3.clone();
    let mut reopened3 = Authority::open(
        store3,
        system3,
        TestClock::new(),
        3,
        Duration::from_millis(10),
    )
    .unwrap()
    .with_escalation_hook(move |context| hook_sink3.lock().unwrap().push(sampler_line(context)));
    reopened3.reconcile().unwrap();
    assert!(matches!(reopened3.state.phase, Phase::ForceStopping { .. }));
    assert_eq!(
        *hook_log3.lock().unwrap(),
        vec![format!("hook:stop_timeout:{id3}:{APP_ID}")]
    );
    assert!(log3.lock().unwrap().iter().any(|line| line == "force"));
}

#[test]
fn a_failed_kill_is_a_durable_retryable_stop_failed_not_a_terminal_recovery() {
    let log: Log = Log::default();
    let hook_log = log.clone();
    let sink_log: Log = Log::default();
    let sink = sink_log.clone();
    let mut a = Authority::open_with_resolver(
        MemoryStore::default(),
        SharedLogSystem {
            log: log.clone(),
            fail_force: true,
            ..SharedLogSystem::default()
        },
        TestClock::new(),
        3,
        Duration::from_millis(10),
        test_resolver(),
    )
    .unwrap()
    .with_escalation_hook(move |context| hook_log.lock().unwrap().push(sampler_line(context)))
    .with_log_sink(move |line| sink.lock().unwrap().push(line.to_owned()));

    let id = launch_hooked(&mut a);
    a.intake_safe_return().unwrap();
    a.tick().unwrap();
    a.clock.advance(Duration::from_millis(10));
    a.tick().unwrap();

    assert!(
        matches!(
            a.store.snapshot().unwrap().phase,
            Phase::StopFailed {
                ref session_id,
                ref reason,
                ..
            } if session_id == &id && reason == "app_exit_failed: refused"
        ),
        "{:?}",
        a.state.phase
    );
    assert!(
        log.lock()
            .unwrap()
            .iter()
            .any(|line| *line == "hook:stop_timeout:session-1:org.example.game"),
        "the sampler ran even though the kill it precedes failed: {:?}",
        log.lock().unwrap()
    );
    // An error is never silent.
    assert_eq!(
        *sink_log.lock().unwrap(),
        [
            "pf-session-authorityd: lifecycle_failure reason=app_exit_failed \
             item_id=\"org.example.game\" detail=\"refused\""
        ]
    );
    assert!(!a
        .events_for("test")
        .iter()
        .any(|(_, e)| matches!(e, SessionEvent::Terminal(_))));

    // The unit dies anyway (systemd's backstop or the app on its own): the close completes
    // as a forced close rather than staying wedged.
    a.observe(Observation::UnitInactive).unwrap();
    assert!(matches!(
        a.state.phase,
        Phase::Restoring {
            receipt: Receipt::ForcedClose,
            ..
        }
    ));
}

#[test]
fn escalation_without_a_sampler_adds_no_delay_and_a_sleeping_sampler_proves_the_clock() {
    let prepare = |with_sampler: bool| {
        let log: Log = Log::default();
        let mut a = Authority::open_with_resolver(
            MemoryStore::default(),
            SharedLogSystem {
                log: log.clone(),
                ..SharedLogSystem::default()
            },
            TestClock::new(),
            3,
            Duration::from_millis(10),
            test_resolver(),
        )
        .unwrap();
        if with_sampler {
            a = a.with_escalation_hook(|_| std::thread::sleep(Duration::from_millis(150)));
        }
        let LaunchResult::Accepted { .. } = a
            .launch(LaunchRequest {
                item_id: APP_ID.into(),
            })
            .unwrap()
        else {
            panic!()
        };
        a.intake_safe_return().unwrap();
        a.tick().unwrap();
        a.clock.advance(Duration::from_millis(10));
        a
    };

    // Negative control: a sleeping sampler is measured as a delay by this same stopwatch, so
    // the fast unhooked run below cannot be a broken measurement.
    let mut sampler = prepare(true);
    let started = Instant::now();
    sampler.tick().unwrap();
    let with_sampler = started.elapsed();
    assert!(matches!(sampler.state.phase, Phase::ForceStopping { .. }));
    assert!(
        with_sampler >= Duration::from_millis(150),
        "the sampler's own cost must be visible to this measurement: {with_sampler:?}"
    );

    // The AC: with no sampler attached the identical escalation costs no measurable delay.
    let mut plain = prepare(false);
    let started = Instant::now();
    plain.tick().unwrap();
    let without_sampler = started.elapsed();
    assert!(matches!(plain.state.phase, Phase::ForceStopping { .. }));
    assert!(
        without_sampler < Duration::from_millis(50),
        "an unobserved escalation must not delay the close: {without_sampler:?}"
    );
}

#[test]
fn events_request_without_a_contract_field_decodes_as_v1() {
    let request: RpcRequest =
        serde_json::from_str(r#"{"method":"events","client_id":"menu"}"#).unwrap();
    assert!(matches!(
        request,
        RpcRequest::Events {
            ref client_id,
            contract: SESSION_RPC_CONTRACT_V1,
        } if client_id == "menu"
    ));
    // A v1 client's request stays byte-identical to the pre-negotiation shape.
    assert_eq!(
        serde_json::to_vec(&RpcRequest::Events {
            client_id: "menu".into(),
            contract: SESSION_RPC_CONTRACT_V1,
        })
        .unwrap(),
        br#"{"method":"events","client_id":"menu"}"#.to_vec()
    );
}

#[test]
fn v1_clients_never_see_close_progress_events_and_their_acks_still_compact() {
    let mut a = authority();
    let id = launch_running(&mut a);

    // An old shell polling while the session runs: its v1 stream holds only the v1 vocabulary,
    // and the response keeps the pre-v2 byte shape (no contract key).
    let RpcResponse::Events { events, contract } = handle_rpc(
        &mut a,
        RpcRequest::Events {
            client_id: "old-shell".into(),
            contract: SESSION_RPC_CONTRACT_V1,
        },
    )
    .unwrap() else {
        panic!()
    };
    assert_eq!(contract, SESSION_RPC_CONTRACT_V1);
    assert_eq!(
        events,
        vec![(1, RpcEvent::Starting), (2, RpcEvent::Running)]
    );
    assert_eq!(
        serde_json::to_vec(&RpcResponse::Events {
            events: Vec::new(),
            contract: SESSION_RPC_CONTRACT_V1,
        })
        .unwrap(),
        br#"{"result":"events","events":[]}"#.to_vec()
    );
    assert!(matches!(
        handle_rpc(
            &mut a,
            RpcRequest::Acknowledge {
                client_id: "old-shell".into(),
                sequence: 2,
            },
        )
        .unwrap(),
        RpcResponse::Ok
    ));

    // The close escalates while the v1 client is away: sequences 3 and 4 are the v2-only
    // StoppingGracefully/ForceStopping events, which the v1 stream must never carry.
    a.intake_safe_return().unwrap();
    a.tick().unwrap();
    a.clock.advance(Duration::from_millis(10));
    a.tick().unwrap();
    let RpcResponse::Events { events, .. } = handle_rpc(
        &mut a,
        RpcRequest::Events {
            client_id: "old-shell".into(),
            contract: SESSION_RPC_CONTRACT_V1,
        },
    )
    .unwrap() else {
        panic!()
    };
    assert_eq!(events, vec![], "no v2 event may reach a v1 client");

    a.observe(Observation::UnitInactive).unwrap();
    restore(&mut a);
    let RpcResponse::Events { events, .. } = handle_rpc(
        &mut a,
        RpcRequest::Events {
            client_id: "old-shell".into(),
            contract: SESSION_RPC_CONTRACT_V1,
        },
    )
    .unwrap() else {
        panic!()
    };
    assert_eq!(
        events,
        vec![
            (5, RpcEvent::ObservationComplete),
            (6, RpcEvent::ForcedClose { session_id: id }),
        ]
    );
    // Acking the last delivered sequence compacts past the filtered v2 events: the v1 cursor
    // is blind to them, never blocked by them.
    assert!(matches!(
        handle_rpc(
            &mut a,
            RpcRequest::Acknowledge {
                client_id: "old-shell".into(),
                sequence: 6,
            },
        )
        .unwrap(),
        RpcResponse::Ok
    ));
    assert!(a.state.pending.is_empty());
}

#[test]
fn an_unsupported_contract_is_refused_explicitly() {
    let mut a = authority();
    // Positive control: the newest contract this authority knows is served.
    assert!(matches!(
        handle_rpc(
            &mut a,
            RpcRequest::Events {
                client_id: "router".into(),
                contract: SESSION_RPC_CONTRACT_CURRENT,
            },
        )
        .unwrap(),
        RpcResponse::Events {
            contract: SESSION_RPC_CONTRACT_CURRENT,
            ..
        }
    ));
    // A client from the future gets a typed refusal, never a silently downgraded stream.
    let response = handle_rpc(
        &mut a,
        RpcRequest::Events {
            client_id: "router".into(),
            contract: SESSION_RPC_CONTRACT_CURRENT + 1,
        },
    )
    .unwrap();
    assert!(
        matches!(&response, RpcResponse::Error { message } if message.contains("contract")),
        "{response:?}"
    );
}

#[test]
fn close_progress_wire_shapes_are_pinned() {
    // Router-facing events (snake_case, internally tagged by "event").
    assert_eq!(
        serde_json::to_string(&RpcEvent::StoppingGracefully {
            session_id: "session-1".into(),
        })
        .unwrap(),
        r#"{"event":"stopping_gracefully","session_id":"session-1"}"#
    );
    assert_eq!(
        serde_json::to_string(&RpcEvent::ForceStopping {
            session_id: "session-1".into(),
            reason: ForceStopReason::StopTimeout,
        })
        .unwrap(),
        r#"{"event":"force_stopping","session_id":"session-1","reason":"stop_timeout"}"#
    );
    assert_eq!(
        serde_json::to_string(&RpcEvent::ForceStopping {
            session_id: "session-1".into(),
            reason: ForceStopReason::StopRequestFailed,
        })
        .unwrap(),
        r#"{"event":"force_stopping","session_id":"session-1","reason":"stop_request_failed"}"#
    );
    assert_eq!(
        serde_json::to_string(&RpcEvent::StopFailed {
            session_id: "session-1".into(),
            reason: "app_exit_failed: refused".into(),
        })
        .unwrap(),
        r#"{"event":"stop_failed","session_id":"session-1","reason":"app_exit_failed: refused"}"#
    );

    // Persisted wire events (external tagging, like every WireEvent before them).
    for (event, json) in [
        (
            WireEvent::StoppingGracefully {
                session_id: "session-1".into(),
            },
            r#"{"StoppingGracefully":{"session_id":"session-1"}}"#,
        ),
        (
            WireEvent::ForceStopping {
                session_id: "session-1".into(),
                reason: ForceStopReason::StopTimeout,
            },
            r#"{"ForceStopping":{"session_id":"session-1","reason":"stop_timeout"}}"#,
        ),
        (
            WireEvent::StopFailed {
                session_id: "session-1".into(),
                reason: "app_exit_failed: refused".into(),
            },
            r#"{"StopFailed":{"session_id":"session-1","reason":"app_exit_failed: refused"}}"#,
        ),
    ] {
        assert_eq!(serde_json::to_string(&event).unwrap(), json);
        assert_eq!(serde_json::from_str::<WireEvent>(json).unwrap(), event);
    }

    // The durable StopFailed phase.
    let phase = Phase::StopFailed {
        session_id: "session-1".into(),
        item_id: APP_ID.into(),
        reason: "app_exit_failed: refused".into(),
    };
    let json = serde_json::to_string(&phase).unwrap();
    assert_eq!(
        json,
        r#"{"StopFailed":{"session_id":"session-1","item_id":"org.example.game","reason":"app_exit_failed: refused"}}"#
    );
    assert_eq!(serde_json::from_str::<Phase>(&json).unwrap(), phase);
    assert_eq!(phase.name(), "stop_failed");

    // A v2 request names its contract; a v2 response echoes it.
    assert_eq!(
        serde_json::to_vec(&RpcRequest::Events {
            client_id: "menu".into(),
            contract: SESSION_RPC_CONTRACT_V2,
        })
        .unwrap(),
        br#"{"method":"events","client_id":"menu","contract":2}"#.to_vec()
    );
    assert_eq!(
        serde_json::to_vec(&RpcResponse::Events {
            events: Vec::new(),
            contract: SESSION_RPC_CONTRACT_V2,
        })
        .unwrap(),
        br#"{"result":"events","events":[],"contract":2}"#.to_vec()
    );
}

#[test]
fn v1_persisted_state_with_a_close_phase_still_decodes_and_escalates() {
    // A state file written by the pre-v2 daemon: no close-progress events in pending, no
    // contract field anywhere. It must load unchanged.
    let json = r#"{
        "phase": {"StoppingGracefully": {
            "session_id": "session-3",
            "item_id": "org.example.game",
            "boot_marker": "boot-1"
        }},
        "history": [],
        "pending": [
            {"sequence": 1, "event": "ObservedStarting"},
            {"sequence": 2, "event": {"Terminal": {"session_id": "session-2", "receipt": "Returned"}}}
        ],
        "next_sequence": 3,
        "next_session": 4,
        "safe_return_queue": 0,
        "safe_return_binding_revision": 0,
        "acknowledged": {"menu": 1}
    }"#;
    let state: PersistedState = serde_json::from_str(json).unwrap();
    assert!(matches!(
        state.phase,
        Phase::StoppingGracefully {
            ref session_id,
            ref boot_marker,
            ..
        } if session_id == "session-3" && boot_marker == "boot-1"
    ));
    assert_eq!(state.pending.len(), 2);
    assert!(matches!(
        state.pending[1].event,
        WireEvent::Terminal {
            receipt: Receipt::Returned,
            ..
        }
    ));

    // And the reopened authority drives it exactly like its own state: the lost deadline
    // escalates on the first reconcile.
    let mut store = MemoryStore::default();
    store.save(&state).unwrap();
    let mut a = Authority::open(
        store,
        FakeSystem::available(),
        TestClock::new(),
        3,
        Duration::from_millis(10),
    )
    .unwrap();
    a.reconcile().unwrap();
    assert!(matches!(a.state.phase, Phase::ForceStopping { .. }));
    assert_eq!(a.system.calls.iter().filter(|c| *c == "force").count(), 1);
}

/// A real-time clock for the deadline-wake test: monotonic nanoseconds like the daemon's.
struct RealClock(Instant);
impl Clock for RealClock {
    fn now(&self) -> MonotonicTime {
        MonotonicTime::from_nanos(self.0.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64)
    }
}

#[test]
fn service_loop_escalates_at_the_grace_deadline_without_waiting_for_a_tick() {
    let mut a = Authority::open_with_resolver(
        MemoryStore::default(),
        FakeSystem::available(),
        RealClock(Instant::now()),
        3,
        Duration::from_millis(150),
        test_resolver(),
    )
    .unwrap();
    let LaunchResult::Accepted { .. } = a
        .launch(LaunchRequest {
            item_id: APP_ID.into(),
        })
        .unwrap()
    else {
        panic!()
    };
    a.intake_safe_return().unwrap();
    a.reconcile().unwrap();
    assert!(matches!(a.state.phase, Phase::StoppingGracefully { .. }));

    // The periodic tick is 10 s away; the 150 ms deadline must wake the loop by itself, or the
    // device escalation would land up to a full tick late (inside systemd's 2 s backstop).
    let (connections, incoming) = std::sync::mpsc::channel::<io::Result<u32>>();
    let closer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1200));
        drop(connections);
    });
    let started = Instant::now();
    run_service_loop(&mut a, &incoming, Duration::from_secs(10), |_, _| Ok(())).unwrap();
    closer.join().unwrap();
    assert!(
        matches!(a.state.phase, Phase::ForceStopping { .. }),
        "the deadline must wake the loop before the 10 s tick: {:?}",
        a.state.phase
    );
    assert_eq!(a.system.calls.iter().filter(|c| *c == "force").count(), 1);
    assert!(started.elapsed() < Duration::from_secs(5));

    // Control: a session nobody asked to close is left alone by the same deadline-aware loop.
    let mut idle = Authority::open_with_resolver(
        MemoryStore::default(),
        FakeSystem::available(),
        RealClock(Instant::now()),
        3,
        Duration::from_millis(150),
        test_resolver(),
    )
    .unwrap();
    let LaunchResult::Accepted { .. } = idle
        .launch(LaunchRequest {
            item_id: APP_ID.into(),
        })
        .unwrap()
    else {
        panic!()
    };
    let (connections, incoming) = std::sync::mpsc::channel::<io::Result<u32>>();
    let closer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        drop(connections);
    });
    run_service_loop(&mut idle, &incoming, Duration::from_secs(10), |_, _| Ok(())).unwrap();
    closer.join().unwrap();
    assert!(matches!(idle.state.phase, Phase::Running { .. }));
    assert_eq!(
        idle.system.calls.iter().filter(|c| *c == "force").count(),
        0
    );
}

#[test]
fn default_graceful_stop_is_non_blocking_so_the_app_window_stays_the_units_own() {
    // The device escalation was unreachable while the graceful stop blocked for the unit's
    // whole TimeoutStopSec: `systemctl stop` only returned after systemd had already SIGKILLed
    // the app. --no-block hands the window back to the authority's own deadline.
    let templates = CommandTemplates::default();
    assert_eq!(
        templates.request_graceful_stop,
        words("systemctl stop --no-block pf-app@{item_id}.service")
    );
    // Budget: grace (1500 ms) + root sampler (≤300 ms, cr-collect) must stay inside the app
    // unit's own TimeoutStopSec=2 s so the authority escalates before the systemd backstop.
    assert_eq!(DEFAULT_GRACE_PERIOD, Duration::from_millis(1500));
}
// URL handoff (tsp-ght0z): OpenUrl / ReturnToCaller, the depth-1 return slot, typed results.
// ---------------------------------------------------------------------------

const URL: &str = "https://example.org/listen?track=7";

fn front(app_id: &str) -> RpcOrigin {
    RpcOrigin::Front {
        caller: Ok(app_id.to_owned()),
    }
}

fn open_url_request(app_id: Option<&str>, url: &str, flags: u32) -> RpcRequest {
    RpcRequest::OpenUrl {
        app_id: app_id.map(str::to_owned),
        url: url.to_owned(),
        flags,
    }
}

fn return_request(app_id: &str, url: Option<&str>) -> RpcRequest {
    RpcRequest::ReturnToCaller {
        app_id: app_id.to_owned(),
        url: url.map(str::to_owned),
    }
}

/// An authority with the linker app running in front and the browser installed as handler.
fn linker_in_front() -> (Authority<MemoryStore, FakeSystem, TestClock>, Log, String) {
    let (a, log) = logged_authority(None);
    let mut a = a.with_url_handler(FixedUrlHandler(BROWSER_ID.into()));
    let LaunchResult::Accepted { session_id } = a
        .launch(LaunchRequest {
            item_id: LINKER_ID.into(),
        })
        .unwrap()
    else {
        panic!()
    };
    assert!(matches!(a.state.phase, Phase::Running { .. }));
    (a, log, session_id)
}

fn transition_authority() -> Authority<FailOnceStore, FakeSystem, TestClock> {
    Authority::open_with_resolver(
        FailOnceStore::with_state(PersistedState::default()),
        FakeSystem::available(),
        TestClock::new(),
        3,
        Duration::from_millis(10),
        test_resolver(),
    )
    .unwrap()
    .with_url_handler(FixedUrlHandler(BROWSER_ID.into()))
}

fn reopen_transition_authority(
    store: FailOnceStore,
    system: FakeSystem,
) -> Authority<FailOnceStore, FakeSystem, TestClock> {
    Authority::open_with_resolver(
        store,
        system,
        TestClock::new(),
        3,
        Duration::from_millis(10),
        test_resolver(),
    )
    .unwrap()
    .with_url_handler(FixedUrlHandler(BROWSER_ID.into()))
}

fn refusals(log: &Log) -> Vec<String> {
    log.lock()
        .unwrap()
        .iter()
        .filter(|line| line.contains("url_refused"))
        .cloned()
        .collect()
}

#[test]
fn url_policy_accepts_http_and_https_and_refuses_everything_else() {
    assert_eq!(validate_url("http://example.org"), Ok("http"));
    assert_eq!(validate_url("HTTPS://Example.org/a?b=c#d"), Ok("https"));
    assert_eq!(
        validate_url("http://user@example.org:8080/path"),
        Ok("http")
    );
    assert_eq!(validate_url("https://[2001:db8::1]:443/path"), Ok("https"));
    let longest = format!("https://example.org/{}", "a".repeat(MAX_URL_BYTES - 20));
    assert_eq!(longest.len(), MAX_URL_BYTES);
    assert_eq!(validate_url(&longest), Ok("https"));
    for (url, rejection) in [
        (format!("{longest}a"), UrlRejection::TooLong),
        ("file:///etc/passwd".into(), UrlRejection::SchemeNotAllowed),
        ("data:text/html,hi".into(), UrlRejection::SchemeNotAllowed),
        ("javascript:alert(1)".into(), UrlRejection::SchemeNotAllowed),
        ("ftp://example.org".into(), UrlRejection::SchemeNotAllowed),
        ("example.org/path".into(), UrlRejection::NoScheme),
        ("http:example.org".into(), UrlRejection::NoAuthority),
        ("http:///path".into(), UrlRejection::EmptyHost),
        ("http://".into(), UrlRejection::EmptyHost),
        ("http://user@".into(), UrlRejection::EmptyHost),
        ("https://user@".into(), UrlRejection::EmptyHost),
        ("http://:443".into(), UrlRejection::EmptyHost),
        ("https://:443".into(), UrlRejection::EmptyHost),
        ("http://[::".into(), UrlRejection::EmptyHost),
        ("https://[::".into(), UrlRejection::EmptyHost),
        ("http://[]".into(), UrlRejection::EmptyHost),
        ("https://[::1]extra".into(), UrlRejection::EmptyHost),
        (
            "http://exa mple.org".into(),
            UrlRejection::ControlOrNonAscii,
        ),
        (
            "http://example.org/\n".into(),
            UrlRejection::ControlOrNonAscii,
        ),
        ("http://exämple.org".into(), UrlRejection::ControlOrNonAscii),
        ("".into(), UrlRejection::NoScheme),
    ] {
        assert_eq!(validate_url(&url), Err(rejection), "{url:?}");
    }
}

#[test]
fn front_open_url_launches_the_handler_with_the_url_as_data_and_opens_the_slot() {
    let (mut a, log, caller_session) = linker_in_front();
    let response = dispatch_rpc_from(
        &mut a,
        front(LINKER_ID),
        open_url_request(Some(LINKER_ID), URL, 0),
    );
    assert!(
        matches!(&response, RpcResponse::Launched { session_id } if session_id == "session-2"),
        "{response:?}"
    );
    // The caller keeps running in front of the authority's phase; the slot records the handoff.
    assert!(
        matches!(&a.state.phase, Phase::Running { session_id, item_id } if session_id == &caller_session && item_id == LINKER_ID)
    );
    assert_eq!(
        a.state.return_slot,
        Some(ReturnSlot {
            caller: SlotCaller::App {
                session_id: caller_session.clone(),
                item_id: LINKER_ID.into(),
            },
            handler_session_id: "session-2".into(),
            handler_item_id: BROWSER_ID.into(),
            caller_gone: false,
        })
    );
    // The URL reached the system as data on the handoff, never as a command token.
    let handoff = &a.system.handoffs[0];
    assert_eq!(handoff.url, URL);
    assert_eq!(handoff.handler_item_id, BROWSER_ID);
    assert_eq!(handoff.handler_session_id, "session-2");
    assert!(
        a.system
            .calls
            .iter()
            .all(|call| !call.contains("example.org")),
        "{:?}",
        a.system.calls
    );
    // The handler session is in history with a start stamp; the caller's entry stays open.
    let browser = a
        .state
        .history
        .iter()
        .find(|e| e.session_id == "session-2")
        .unwrap();
    assert_eq!(browser.item_id, BROWSER_ID);
    assert!(browser.started_at.is_some() && browser.ended_at.is_none());
    assert_eq!(a.state.next_session, 3);
    assert!(log.lock().unwrap().iter().any(|line| line
        == "pf-session-authorityd: url_handoff event=opened caller=\"org.example.linker\" handler=\"org.example.browser\" session=\"session-2\""));
    // A second OpenUrl never stacks: the slot is occupied.
    let response = dispatch_rpc_from(
        &mut a,
        front(LINKER_ID),
        open_url_request(Some(LINKER_ID), URL, 0),
    );
    assert!(matches!(response, RpcResponse::Busy), "{response:?}");
    assert!(refusals(&log)
        .last()
        .unwrap()
        .contains("reason=slot_occupied"));
    assert_eq!(a.system.handoffs.len(), 1);
}

#[test]
fn shell_handoff_replays_a_durable_intent_after_the_post_effect_save_fails() {
    let mut a = transition_authority();
    // Positive control: one durable pre-effect save succeeds. The injected failure is the first
    // save after the foreground-changing handoff.
    a.store.fail_after(1);
    let response = dispatch_rpc(&mut a, open_url_request(None, URL, 0));
    assert!(
        matches!(response, RpcResponse::Error { .. }),
        "{response:?}"
    );
    assert_eq!(a.system.handoffs.len(), 1, "the side effect happened once");
    a.system.lifecycle = Some(SystemLifecycle {
        app: AppUnitState::Active,
        foreground_target_active: true,
        selected_owner_active: false,
    });

    let (store, system, _) = a.into_parts();
    let mut reopened = reopen_transition_authority(store, system);
    reopened.reconcile().unwrap();
    assert_eq!(
        reopened.system.handoffs.len(),
        2,
        "restart replays the intent"
    );
    assert!(matches!(
        reopened.state.return_slot,
        Some(ReturnSlot {
            caller: SlotCaller::Shell,
            ..
        })
    ));
    assert!(matches!(reopened.state.phase, Phase::Running { .. }));
}

#[test]
fn app_handoff_replays_a_durable_intent_after_the_terminal_save_fails() {
    let mut a = transition_authority();
    assert!(matches!(
        a.launch(LaunchRequest {
            item_id: LINKER_ID.into()
        })
        .unwrap(),
        LaunchResult::Accepted { .. }
    ));
    a.store.fail_after(1);
    let response = dispatch_rpc_from(
        &mut a,
        front(LINKER_ID),
        open_url_request(Some(LINKER_ID), URL, 0),
    );
    assert!(
        matches!(response, RpcResponse::Error { .. }),
        "{response:?}"
    );
    assert_eq!(a.system.handoffs.len(), 1);

    let (store, system, _) = a.into_parts();
    let mut reopened = reopen_transition_authority(store, system);
    reopened.reconcile().unwrap();
    assert_eq!(reopened.system.handoffs.len(), 2);
    assert!(matches!(
        reopened.state.return_slot,
        Some(ReturnSlot {
            caller: SlotCaller::App { .. },
            ..
        })
    ));
    assert_eq!(
        reopened
            .state
            .history
            .iter()
            .filter(|entry| entry.session_id == "session-2")
            .count(),
        1,
        "replay commits one handler history entry"
    );
}

#[test]
fn explicit_return_replays_a_durable_restore_after_the_terminal_save_fails() {
    let mut a = transition_authority();
    assert!(matches!(
        a.launch(LaunchRequest {
            item_id: LINKER_ID.into()
        })
        .unwrap(),
        LaunchResult::Accepted { .. }
    ));
    assert!(matches!(
        dispatch_rpc_from(
            &mut a,
            front(LINKER_ID),
            open_url_request(Some(LINKER_ID), URL, 0)
        ),
        RpcResponse::Launched { .. }
    ));
    a.store.fail_after(1);
    let response = dispatch_rpc_from(&mut a, front(BROWSER_ID), return_request(BROWSER_ID, None));
    assert!(
        matches!(response, RpcResponse::Error { .. }),
        "{response:?}"
    );
    assert_eq!(a.system.restores.len(), 1);

    let (store, system, _) = a.into_parts();
    let mut reopened = reopen_transition_authority(store, system);
    reopened.reconcile().unwrap();
    assert_eq!(reopened.system.restores.len(), 2);
    assert!(reopened.state.return_slot.is_none());
}

#[test]
fn handler_exit_replays_a_durable_restore_after_the_terminal_save_fails() {
    let mut a = transition_authority();
    assert!(matches!(
        a.launch(LaunchRequest {
            item_id: LINKER_ID.into()
        })
        .unwrap(),
        LaunchResult::Accepted { .. }
    ));
    assert!(matches!(
        dispatch_rpc_from(
            &mut a,
            front(LINKER_ID),
            open_url_request(Some(LINKER_ID), URL, 0)
        ),
        RpcResponse::Launched { .. }
    ));
    a.system.lifecycles.insert(
        BROWSER_ID.into(),
        SystemLifecycle {
            app: AppUnitState::InactiveSuccess,
            foreground_target_active: true,
            selected_owner_active: false,
        },
    );
    a.store.fail_after(1);
    assert!(matches!(
        a.tick(),
        Err(AuthorityError::Persistence(reason)) if reason == "injected transition save failure"
    ));
    assert_eq!(a.system.restores.len(), 1);

    let (store, system, _) = a.into_parts();
    let mut reopened = reopen_transition_authority(store, system);
    reopened.reconcile().unwrap();
    assert_eq!(reopened.system.restores.len(), 2);
    assert!(reopened.state.return_slot.is_none());
    assert_eq!(
        reopened
            .state
            .history
            .iter()
            .find(|entry| entry.session_id == "session-2")
            .and_then(|entry| entry.receipt.as_ref()),
        Some(&Receipt::Returned)
    );
}

#[test]
fn front_open_url_delivered_when_a_running_handler_takes_the_url() {
    let (mut a, _log, _) = linker_in_front();
    a.system.delivered = true;
    let response = dispatch_rpc_from(
        &mut a,
        front(LINKER_ID),
        open_url_request(Some(LINKER_ID), URL, 0),
    );
    assert!(matches!(response, RpcResponse::Delivered), "{response:?}");
    assert!(a.state.return_slot.is_some());
    assert_eq!(a.system.handoffs[0].url, URL);
}

#[test]
fn front_open_url_without_an_installed_handler_is_no_handler() {
    let (a, log) = logged_authority(None);
    let mut a = a.with_url_handler(NoUrlHandler);
    launch_item(&mut a, LINKER_ID);
    let response = dispatch_rpc_from(
        &mut a,
        front(LINKER_ID),
        open_url_request(Some(LINKER_ID), URL, 0),
    );
    assert!(matches!(response, RpcResponse::NoHandler), "{response:?}");
    assert!(a.state.return_slot.is_none());
    assert!(a.system.handoffs.is_empty());
    assert!(refusals(&log)[0].contains("reason=no_handler"));

    // A configured handler that is not installed is also no_handler (never a start attempt).
    let (a, log) = logged_authority(None);
    let mut a = a.with_url_handler(FixedUrlHandler("org.example.missing-browser".into()));
    launch_item(&mut a, LINKER_ID);
    let response = dispatch_rpc_from(
        &mut a,
        front(LINKER_ID),
        open_url_request(Some(LINKER_ID), URL, 0),
    );
    assert!(matches!(response, RpcResponse::NoHandler), "{response:?}");
    assert!(a.system.handoffs.is_empty());
    assert!(refusals(&log)[0].contains("reason=no_handler"));
}

fn launch_item(a: &mut Authority<MemoryStore, FakeSystem, TestClock>, item_id: &str) {
    assert!(matches!(
        a.launch(LaunchRequest {
            item_id: item_id.into()
        })
        .unwrap(),
        LaunchResult::Accepted { .. }
    ));
}

#[test]
fn front_open_url_refuses_non_http_and_over_length_urls_before_any_handoff() {
    let (mut a, log, _) = linker_in_front();
    let too_long = format!("https://example.org/{}", "a".repeat(MAX_URL_BYTES));
    for (url, detail) in [
        ("file:///etc/passwd", "scheme_not_allowed"),
        ("data:text/html,<script>", "scheme_not_allowed"),
        ("javascript:alert(1)", "scheme_not_allowed"),
        (too_long.as_str(), "too_long"),
    ] {
        // Each caller gets its own window, so the policy, not the limiter, answers.
        a.url_rate.clear();
        let response = dispatch_rpc_from(
            &mut a,
            front(LINKER_ID),
            open_url_request(Some(LINKER_ID), url, 0),
        );
        assert!(
            matches!(response, RpcResponse::InvalidUrl),
            "{url:?}: {response:?}"
        );
        assert!(refusals(&log).last().unwrap().contains(&format!(
            "reason=invalid_url caller=\"org.example.linker\" detail=\"{detail}\""
        )));
    }
    assert!(a.system.handoffs.is_empty() && a.state.return_slot.is_none());
    // Positive control in the same authority: the same caller with an http URL is launched.
    a.url_rate.clear();
    let response = dispatch_rpc_from(
        &mut a,
        front(LINKER_ID),
        open_url_request(Some(LINKER_ID), "http://example.org", 0),
    );
    assert!(
        matches!(response, RpcResponse::Launched { .. }),
        "{response:?}"
    );
}

#[test]
fn front_open_url_pidfd_claim_mismatch_is_refused_and_logged() {
    let (mut a, log, _) = linker_in_front();
    // The front claims the foreground linker, but the forwarded process handle is another app.
    let response = dispatch_rpc_from(
        &mut a,
        front("org.example.g1"),
        open_url_request(Some(LINKER_ID), URL, 0),
    );
    assert!(matches!(response, RpcResponse::Denied), "{response:?}");
    assert_eq!(
        refusals(&log),
        ["pf-session-authorityd: url_refused verb=open_url reason=identity_mismatch caller=\"org.example.linker\" detail=\"derived=org.example.g1\""]
    );
    // No handle at all, a handle that is not an app, and a missing claim are refused too.
    for (origin, reason) in [
        (
            RpcOrigin::Front {
                caller: Err("missing_process_handle".into()),
            },
            "identity_unverified",
        ),
        (
            RpcOrigin::Front {
                caller: Err("pid=42 is not in a pf-app@ unit".into()),
            },
            "identity_unverified",
        ),
        (front(LINKER_ID), "missing_claim"),
    ] {
        let claim = if reason == "missing_claim" {
            None
        } else {
            Some(LINKER_ID)
        };
        let response = dispatch_rpc_from(&mut a, origin, open_url_request(claim, URL, 0));
        assert!(matches!(response, RpcResponse::Denied), "{response:?}");
        assert!(refusals(&log)
            .last()
            .unwrap()
            .contains(&format!("reason={reason}")));
    }
    assert!(a.system.handoffs.is_empty() && a.state.return_slot.is_none());
    // Positive control: claim and derived id agree.
    let response = dispatch_rpc_from(
        &mut a,
        front(LINKER_ID),
        open_url_request(Some(LINKER_ID), URL, 0),
    );
    assert!(
        matches!(response, RpcResponse::Launched { .. }),
        "{response:?}"
    );
}

#[test]
fn front_open_url_is_denied_without_the_capability_or_the_foreground() {
    // APP_ID is in front but does not declare open_url.
    let (a, log) = logged_authority(None);
    let mut a = a.with_url_handler(FixedUrlHandler(BROWSER_ID.into()));
    launch_running(&mut a);
    let response = dispatch_rpc_from(
        &mut a,
        front(APP_ID),
        open_url_request(Some(APP_ID), URL, 0),
    );
    assert!(matches!(response, RpcResponse::Denied), "{response:?}");
    assert!(refusals(&log)[0]
        .contains("reason=capability caller=\"org.example.game\" detail=\"open_url\""));
    // The linker declares it but is not the foreground app.
    let response = dispatch_rpc_from(
        &mut a,
        front(LINKER_ID),
        open_url_request(Some(LINKER_ID), URL, 0),
    );
    assert!(matches!(response, RpcResponse::Denied), "{response:?}");
    assert!(refusals(&log)[1].contains("reason=not_foreground"));
    // An unknown app id is denied with the resolver's reason.
    let response = dispatch_rpc_from(
        &mut a,
        front("org.example.nowhere"),
        open_url_request(Some("org.example.nowhere"), URL, 0),
    );
    assert!(matches!(response, RpcResponse::Denied), "{response:?}");
    assert!(refusals(&log)[2].contains("reason=app_not_found"));
    assert!(a.system.handoffs.is_empty());
}

#[test]
fn front_open_url_is_busy_while_the_phase_cannot_hand_off() {
    let (mut a, log, _) = linker_in_front();
    a.intake_safe_return().unwrap();
    a.tick().unwrap();
    assert!(matches!(a.state.phase, Phase::StoppingGracefully { .. }));
    let response = dispatch_rpc_from(
        &mut a,
        front(LINKER_ID),
        open_url_request(Some(LINKER_ID), URL, 0),
    );
    assert!(matches!(response, RpcResponse::Busy), "{response:?}");
    assert!(refusals(&log)[0]
        .contains("reason=phase caller=\"org.example.linker\" detail=\"stopping_gracefully\""));
    assert!(a.system.handoffs.is_empty());
}

#[test]
fn front_open_url_is_rate_limited_after_three_in_ten_seconds() {
    let (a, log) = logged_authority(None);
    let mut a = a.with_url_handler(NoUrlHandler);
    launch_item(&mut a, LINKER_ID);
    for _ in 0..URL_RATE_LIMIT {
        let response = dispatch_rpc_from(
            &mut a,
            front(LINKER_ID),
            open_url_request(Some(LINKER_ID), URL, 0),
        );
        assert!(matches!(response, RpcResponse::NoHandler), "{response:?}");
        a.clock.advance(Duration::from_secs(1));
    }
    let response = dispatch_rpc_from(
        &mut a,
        front(LINKER_ID),
        open_url_request(Some(LINKER_ID), URL, 0),
    );
    assert!(matches!(response, RpcResponse::RateLimited), "{response:?}");
    assert!(refusals(&log)
        .last()
        .unwrap()
        .contains("reason=rate_limited caller=\"org.example.linker\""));
    // Another caller's budget is separate (per app).
    let response = dispatch_rpc_from(&mut a, RpcOrigin::Shell, open_url_request(None, URL, 0));
    assert!(matches!(response, RpcResponse::Busy), "{response:?}");
    // The window slides: the first request was 3 s ago plus 7 s makes 10 s.
    a.clock.advance(Duration::from_secs(7));
    let response = dispatch_rpc_from(
        &mut a,
        front(LINKER_ID),
        open_url_request(Some(LINKER_ID), URL, 0),
    );
    assert!(matches!(response, RpcResponse::NoHandler), "{response:?}");
}

#[test]
fn reserved_flags_auth_session_everywhere_and_captive_portal_from_apps_are_refused() {
    let (mut a, log, _) = linker_in_front();
    for (flags, detail) in [
        (
            OPEN_URL_FLAG_AUTH_SESSION,
            "reason=flag_reserved caller=\"org.example.linker\" detail=\"auth_session\"",
        ),
        (
            OPEN_URL_FLAG_CAPTIVE_PORTAL,
            "reason=flag_shell_only caller=\"org.example.linker\" detail=\"captive_portal\"",
        ),
        (
            OPEN_URL_FLAG_AUTH_SESSION | OPEN_URL_FLAG_CAPTIVE_PORTAL,
            "reason=flag_reserved",
        ),
        (
            1 << 7,
            "reason=flag_unknown caller=\"org.example.linker\" detail=\"0x80\"",
        ),
    ] {
        a.url_rate.clear();
        let response = dispatch_rpc_from(
            &mut a,
            front(LINKER_ID),
            open_url_request(Some(LINKER_ID), URL, flags),
        );
        assert!(
            matches!(response, RpcResponse::Denied),
            "{flags:#x}: {response:?}"
        );
        assert!(
            refusals(&log).last().unwrap().contains(detail),
            "{flags:#x}: {:?}",
            refusals(&log)
        );
    }
    assert!(a.system.handoffs.is_empty() && a.state.return_slot.is_none());
    // Positive control: no flags from the same caller is launched.
    a.url_rate.clear();
    let response = dispatch_rpc_from(
        &mut a,
        front(LINKER_ID),
        open_url_request(Some(LINKER_ID), URL, 0),
    );
    assert!(
        matches!(response, RpcResponse::Launched { .. }),
        "{response:?}"
    );

    // The shell: AUTH_SESSION is still reserved, CAPTIVE_PORTAL is accepted and carried through.
    let (a, log) = logged_authority(None);
    let mut a = a.with_url_handler(FixedUrlHandler(BROWSER_ID.into()));
    let response = dispatch_rpc_from(
        &mut a,
        RpcOrigin::Shell,
        open_url_request(None, URL, OPEN_URL_FLAG_AUTH_SESSION),
    );
    assert!(matches!(response, RpcResponse::Denied), "{response:?}");
    assert!(refusals(&log)[0].contains("reason=flag_reserved caller=\"shell\""));
    let response = dispatch_rpc_from(
        &mut a,
        RpcOrigin::Shell,
        open_url_request(
            None,
            "http://network-test.debian.org/nm",
            OPEN_URL_FLAG_CAPTIVE_PORTAL,
        ),
    );
    assert!(
        matches!(response, RpcResponse::Launched { .. }),
        "{response:?}"
    );
    assert_eq!(a.system.handoffs[0].flags, OPEN_URL_FLAG_CAPTIVE_PORTAL);
    assert_eq!(a.system.handoffs[0].caller, SlotCaller::Shell);
}

#[test]
fn return_to_caller_restores_a_running_caller_and_clears_the_slot() {
    let (mut a, log, caller_session) = linker_in_front();
    assert!(matches!(
        dispatch_rpc_from(
            &mut a,
            front(LINKER_ID),
            open_url_request(Some(LINKER_ID), URL, 0)
        ),
        RpcResponse::Launched { .. }
    ));
    // Only the handler may return, and only through the front with its own identity.
    let response = dispatch_rpc_from(&mut a, front(LINKER_ID), return_request(LINKER_ID, None));
    assert!(matches!(response, RpcResponse::Denied), "{response:?}");
    assert!(refusals(&log)
        .last()
        .unwrap()
        .contains("reason=not_handler"));
    let response = dispatch_rpc_from(&mut a, front(BROWSER_ID), return_request(LINKER_ID, None));
    assert!(matches!(response, RpcResponse::Denied), "{response:?}");
    assert!(refusals(&log)
        .last()
        .unwrap()
        .contains("reason=identity_mismatch"));
    let response = dispatch_rpc_from(&mut a, RpcOrigin::Shell, return_request(BROWSER_ID, None));
    assert!(matches!(response, RpcResponse::Denied), "{response:?}");
    assert!(refusals(&log).last().unwrap().contains("reason=front_only"));
    assert!(a.system.restores.is_empty());

    let response = dispatch_rpc_from(&mut a, front(BROWSER_ID), return_request(BROWSER_ID, None));
    assert!(matches!(response, RpcResponse::Restored), "{response:?}");
    assert_eq!(a.system.restores.len(), 1);
    assert_eq!(a.system.restores[0].handler_item_id, BROWSER_ID);
    assert!(a.state.return_slot.is_none());
    assert!(
        matches!(&a.state.phase, Phase::Running { session_id, .. } if session_id == &caller_session)
    );
    // Nothing was published: the shell is not back in front.
    assert!(!a
        .events_for("test")
        .iter()
        .any(|(_, e)| matches!(e, SessionEvent::Terminal(_))));
    // Without a slot, a return is denied.
    let response = dispatch_rpc_from(&mut a, front(BROWSER_ID), return_request(BROWSER_ID, None));
    assert!(matches!(response, RpcResponse::Denied), "{response:?}");
    assert!(refusals(&log)
        .last()
        .unwrap()
        .contains("reason=no_return_slot"));
    // And the caller can open again (depth 1, sequentially).
    let response = dispatch_rpc_from(
        &mut a,
        front(LINKER_ID),
        open_url_request(Some(LINKER_ID), URL, 0),
    );
    assert!(
        matches!(response, RpcResponse::Launched { .. }),
        "{response:?}"
    );
}

#[test]
fn return_to_caller_answers_caller_gone_after_the_memory_policy_killed_the_caller() {
    let (mut a, log, caller_session) = linker_in_front();
    assert!(matches!(
        dispatch_rpc_from(
            &mut a,
            front(LINKER_ID),
            open_url_request(Some(LINKER_ID), URL, 0)
        ),
        RpcResponse::Launched { .. }
    ));
    // systemd: the backgrounded caller's unit was killed (SIGKILL by the memory policy), the
    // handler is still active.
    a.system.lifecycles.insert(
        LINKER_ID.into(),
        SystemLifecycle {
            app: AppUnitState::InactiveFailure {
                summary: "systemd result: signal".into(),
            },
            foreground_target_active: true,
            selected_owner_active: false,
        },
    );
    a.system.lifecycles.insert(
        BROWSER_ID.into(),
        SystemLifecycle {
            app: AppUnitState::Active,
            foreground_target_active: true,
            selected_owner_active: false,
        },
    );
    a.tick().unwrap();
    // The handler was promoted to the foreground session; the caller's history closed.
    assert!(
        matches!(&a.state.phase, Phase::Running { session_id, item_id } if session_id == "session-2" && item_id == BROWSER_ID),
        "{:?}",
        a.state.phase
    );
    let slot = a.state.return_slot.clone().unwrap();
    assert!(slot.caller_gone);
    let caller = a
        .state
        .history
        .iter()
        .find(|e| e.session_id == caller_session)
        .unwrap();
    assert!(matches!(caller.receipt, Some(Receipt::Crash { .. })));
    assert!(caller.ended_at.is_some());
    assert!(
        a.system.calls.iter().all(|call| call != "owner"),
        "{:?}",
        a.system.calls
    );
    assert!(!a
        .events_for("test")
        .iter()
        .any(|(_, e)| matches!(e, SessionEvent::Terminal(_))));
    assert!(log
        .lock()
        .unwrap()
        .iter()
        .any(|line| line.contains("url_handoff event=caller_gone")));

    let response = dispatch_rpc_from(&mut a, front(BROWSER_ID), return_request(BROWSER_ID, None));
    assert!(matches!(response, RpcResponse::CallerGone), "{response:?}");
    assert!(a.state.return_slot.is_none());
    assert!(a.system.restores.is_empty(), "nothing to restore");
    // The browser is now an ordinary foreground session: its exit runs the normal ladder.
    a.system.lifecycles.insert(
        BROWSER_ID.into(),
        SystemLifecycle {
            app: AppUnitState::InactiveSuccess,
            foreground_target_active: false,
            selected_owner_active: true,
        },
    );
    a.tick().unwrap();
    a.observe(Observation::PresentationAcknowledged).unwrap();
    assert!(matches!(a.state.phase, Phase::Idle));
    assert!(a.events_for("test").iter().any(|(_, e)| matches!(
        e,
        SessionEvent::Terminal(TerminalReceipt::Returned { session_id }) if session_id == "session-2"
    )));
}

#[test]
fn handler_exit_restores_the_caller_and_a_failed_restore_is_recovery() {
    let (mut a, log, caller_session) = linker_in_front();
    assert!(matches!(
        dispatch_rpc_from(
            &mut a,
            front(LINKER_ID),
            open_url_request(Some(LINKER_ID), URL, 0)
        ),
        RpcResponse::Launched { .. }
    ));
    a.system.lifecycles.insert(
        LINKER_ID.into(),
        SystemLifecycle {
            app: AppUnitState::Active,
            foreground_target_active: true,
            selected_owner_active: false,
        },
    );
    a.system.lifecycles.insert(
        BROWSER_ID.into(),
        SystemLifecycle {
            app: AppUnitState::InactiveSuccess,
            foreground_target_active: true,
            selected_owner_active: false,
        },
    );
    a.tick().unwrap();
    assert!(a.state.return_slot.is_none());
    assert_eq!(a.system.restores.len(), 1);
    assert!(
        matches!(&a.state.phase, Phase::Running { session_id, .. } if session_id == &caller_session)
    );
    let browser = a
        .state
        .history
        .iter()
        .find(|e| e.session_id == "session-2")
        .unwrap();
    assert_eq!(browser.receipt, Some(Receipt::Returned));
    assert!(log
        .lock()
        .unwrap()
        .iter()
        .any(|line| line.contains("url_handoff event=handler_exited")));

    // Negative control: the restore itself fails, which is durable recovery.
    let (mut a, _log, _) = linker_in_front();
    assert!(matches!(
        dispatch_rpc_from(
            &mut a,
            front(LINKER_ID),
            open_url_request(Some(LINKER_ID), URL, 0)
        ),
        RpcResponse::Launched { .. }
    ));
    a.system.fail_restore = true;
    a.system.lifecycles.insert(
        BROWSER_ID.into(),
        SystemLifecycle {
            app: AppUnitState::InactiveFailure {
                summary: "systemd result: exit-code".into(),
            },
            foreground_target_active: true,
            selected_owner_active: false,
        },
    );
    a.tick().unwrap();
    assert!(
        matches!(
            &a.state.phase,
            Phase::RecoveryRequired { reason, .. } if reason.starts_with("owner_not_active: caller restore failed")
        ),
        "{:?}",
        a.state.phase
    );
}

#[test]
fn safe_return_with_a_slot_stops_the_handler_then_runs_the_caller_ladder() {
    let (mut a, log, caller_session) = linker_in_front();
    assert!(matches!(
        dispatch_rpc_from(
            &mut a,
            front(LINKER_ID),
            open_url_request(Some(LINKER_ID), URL, 0)
        ),
        RpcResponse::Launched { .. }
    ));
    assert!(matches!(
        dispatch_rpc(&mut a, RpcRequest::SafeReturn),
        RpcResponse::Ok
    ));
    assert!(a.state.return_slot.is_none());
    assert!(
        matches!(&a.state.phase, Phase::StoppingGracefully { session_id, .. } if session_id == &caller_session)
    );
    // Handler first, then the caller: two graceful stops, no restore.
    assert_eq!(
        a.system.calls.iter().filter(|c| *c == "graceful").count(),
        2
    );
    assert!(a.system.restores.is_empty());
    assert!(log
        .lock()
        .unwrap()
        .iter()
        .any(|line| line.contains("url_handoff event=safe_return")));
    a.observe(Observation::UnitInactive).unwrap();
    restore(&mut a);
    assert!(matches!(a.state.phase, Phase::Idle));
}

#[test]
fn shell_open_url_launches_the_handler_as_a_session_and_return_goes_home() {
    let (a, log) = logged_authority(None);
    let mut a = a.with_url_handler(FixedUrlHandler(BROWSER_ID.into()));
    let response = dispatch_rpc(
        &mut a,
        open_url_request(
            None,
            "http://network-test.debian.org/nm",
            OPEN_URL_FLAG_CAPTIVE_PORTAL,
        ),
    );
    assert!(
        matches!(&response, RpcResponse::Launched { session_id } if session_id == "session-1"),
        "{response:?}"
    );
    assert!(matches!(&a.state.phase, Phase::Running { item_id, .. } if item_id == BROWSER_ID));
    assert_eq!(
        a.state.return_slot,
        Some(ReturnSlot {
            caller: SlotCaller::Shell,
            handler_session_id: "session-1".into(),
            handler_item_id: BROWSER_ID.into(),
            caller_gone: false,
        })
    );
    assert_eq!(
        a.system.handoffs[0].url,
        "http://network-test.debian.org/nm"
    );
    assert!(a
        .system
        .calls
        .iter()
        .all(|call| !call.contains("debian.org")));
    // The shell socket never accepts an app claim.
    let response = dispatch_rpc(&mut a, open_url_request(Some(LINKER_ID), URL, 0));
    assert!(matches!(response, RpcResponse::Denied), "{response:?}");
    assert!(refusals(&log)
        .last()
        .unwrap()
        .contains("reason=claim_without_identity"));
    // A launch while the handler is in front is the usual busy refusal.
    assert!(matches!(
        dispatch_rpc(
            &mut a,
            RpcRequest::Launch {
                item_id: APP_ID.into()
            }
        ),
        RpcResponse::RejectedBusy
    ));

    // Back at the arrival chip: the browser returns to the caller, which is Home.
    let response = dispatch_rpc_from(&mut a, front(BROWSER_ID), return_request(BROWSER_ID, None));
    assert!(matches!(response, RpcResponse::Restored), "{response:?}");
    assert!(a.state.return_slot.is_none());
    assert!(
        a.system.restores.is_empty(),
        "Home is reached through the protected return, not a restore"
    );
    assert!(matches!(a.state.phase, Phase::StoppingGracefully { .. }));
    a.observe(Observation::UnitInactive).unwrap();
    restore(&mut a);
    assert!(matches!(a.state.phase, Phase::Idle));
    assert!(a.events_for("test").iter().any(|(_, e)| matches!(
        e,
        SessionEvent::Terminal(TerminalReceipt::Returned { session_id }) if session_id == "session-1"
    )));
    // Idle again: the shell can open the next URL; while an app is in front it is busy.
    let response = dispatch_rpc(&mut a, open_url_request(None, URL, 0));
    assert!(
        matches!(response, RpcResponse::Launched { .. }),
        "{response:?}"
    );
}

#[test]
fn shell_open_url_preserves_the_backend_outcome_and_refuses_a_failed_handoff() {
    // Positive control: a running handler reports delivered and still owns the return slot.
    let (a, _log) = logged_authority(None);
    let mut a = a.with_url_handler(FixedUrlHandler(BROWSER_ID.into()));
    a.system.delivered = true;
    let response = dispatch_rpc(&mut a, open_url_request(None, URL, 0));
    assert!(matches!(response, RpcResponse::Delivered), "{response:?}");
    assert!(a.state.return_slot.is_some());

    // Negative control: a failed foreground switch is never reported as launched and leaves no
    // depth-1 return slot for a handler that did not take the URL.
    let (a, _log) = logged_authority(None);
    let mut a = a.with_url_handler(FixedUrlHandler(BROWSER_ID.into()));
    a.system.fail_open_url = true;
    let response = dispatch_rpc(&mut a, open_url_request(None, URL, 0));
    assert!(
        matches!(&response, RpcResponse::Error { message } if message.contains("handoff failed")),
        "{response:?}"
    );
    assert!(a.state.return_slot.is_none());
}

#[test]
fn shell_open_url_is_busy_while_an_app_is_in_front_and_without_a_handler_is_no_handler() {
    let (a, log) = logged_authority(None);
    let mut a = a.with_url_handler(FixedUrlHandler(BROWSER_ID.into()));
    launch_running(&mut a);
    let response = dispatch_rpc(&mut a, open_url_request(None, URL, 0));
    assert!(matches!(response, RpcResponse::Busy), "{response:?}");
    assert!(refusals(&log)[0].contains("reason=phase caller=\"shell\" detail=\"running\""));
    let (a, _log) = logged_authority(None);
    let mut a = a.with_url_handler(NoUrlHandler);
    let response = dispatch_rpc(&mut a, open_url_request(None, URL, 0));
    assert!(matches!(response, RpcResponse::NoHandler), "{response:?}");
    assert!(matches!(a.state.phase, Phase::Idle));
}

#[test]
fn return_to_caller_url_is_reserved_for_auth_sessions() {
    let (mut a, log, _) = linker_in_front();
    assert!(matches!(
        dispatch_rpc_from(
            &mut a,
            front(LINKER_ID),
            open_url_request(Some(LINKER_ID), URL, 0)
        ),
        RpcResponse::Launched { .. }
    ));
    let response = dispatch_rpc_from(
        &mut a,
        front(BROWSER_ID),
        return_request(BROWSER_ID, Some("javascript:x")),
    );
    assert!(matches!(response, RpcResponse::InvalidUrl), "{response:?}");
    let response = dispatch_rpc_from(
        &mut a,
        front(BROWSER_ID),
        return_request(BROWSER_ID, Some("https://app.example/callback?code=1")),
    );
    assert!(matches!(response, RpcResponse::Denied), "{response:?}");
    assert!(refusals(&log)
        .last()
        .unwrap()
        .contains("reason=return_url_reserved"));
    assert!(
        a.state.return_slot.is_some(),
        "a refused return leaves the slot"
    );
    assert!(a.system.restores.is_empty());
    // Positive control: the optional url absent.
    let response = dispatch_rpc_from(&mut a, front(BROWSER_ID), return_request(BROWSER_ID, None));
    assert!(matches!(response, RpcResponse::Restored), "{response:?}");
}

#[test]
fn a_failed_handoff_is_a_logged_error_and_leaves_no_slot() {
    let (mut a, log, _) = linker_in_front();
    a.system.fail_open_url = true;
    let response = dispatch_rpc_from(
        &mut a,
        front(LINKER_ID),
        open_url_request(Some(LINKER_ID), URL, 0),
    );
    assert!(
        matches!(&response, RpcResponse::Error { message } if message.contains("handoff refused")),
        "{response:?}"
    );
    assert!(a.state.return_slot.is_none());
    assert_eq!(a.state.next_session, 2, "no session was consumed");
    assert!(log.lock().unwrap().iter().any(|line| line.contains(
        "lifecycle_failure reason=systemd_start_failed item_id=\"org.example.browser\""
    )));
}

#[test]
fn command_system_refuses_the_handoff_and_never_execs_a_url_argv() {
    let mut system =
        CommandSystem::with_executor(CommandTemplates::default(), FakeExecutor::default());
    let handoff = UrlHandoff {
        caller: SlotCaller::App {
            session_id: "session-1".into(),
            item_id: LINKER_ID.into(),
        },
        handler_item_id: BROWSER_ID.into(),
        handler_session_id: "session-2".into(),
        url: "https://example.org/$(reboot);rm".into(),
        flags: 0,
    };
    assert!(system
        .open_url(&handoff)
        .unwrap_err()
        .contains("trusted session path"));
    let slot = ReturnSlot {
        caller: handoff.caller.clone(),
        handler_session_id: "session-2".into(),
        handler_item_id: BROWSER_ID.into(),
        caller_gone: false,
    };
    assert!(system.restore_caller(&slot).is_err());
    // Positive control: the same system still runs its templates for a plain start.
    system
        .start_foreground(
            &LaunchRequest {
                item_id: BROWSER_ID.into(),
            },
            "session-2",
        )
        .unwrap();
    let calls = system.into_executor().calls;
    assert_eq!(calls.len(), 1);
    assert!(calls.iter().all(|(program, args)| {
        !program.contains("example.org") && args.iter().all(|arg| !arg.contains("example.org"))
    }));
}

#[test]
fn return_slot_persists_and_older_state_without_one_still_loads() {
    let state = PersistedState::default();
    let json = serde_json::to_string(&state).unwrap();
    assert!(!json.contains("return_slot"), "{json}");
    let legacy: PersistedState = serde_json::from_str(
        r#"{"phase":"Idle","history":[],"pending":[],"next_sequence":1,"next_session":1,"safe_return_queue":0,"safe_return_binding_revision":0,"acknowledged":{}}"#,
    )
    .unwrap();
    assert_eq!(legacy.return_slot, None);
    let slot = ReturnSlot {
        caller: SlotCaller::App {
            session_id: "session-1".into(),
            item_id: LINKER_ID.into(),
        },
        handler_session_id: "session-2".into(),
        handler_item_id: BROWSER_ID.into(),
        caller_gone: true,
    };
    let json = serde_json::to_string(&slot).unwrap();
    assert_eq!(
        json,
        r#"{"caller":{"app":{"session_id":"session-1","item_id":"org.example.linker"}},"handler_session_id":"session-2","handler_item_id":"org.example.browser","caller_gone":true}"#
    );
    assert_eq!(serde_json::from_str::<ReturnSlot>(&json).unwrap(), slot);
    assert_eq!(
        serde_json::to_string(&SlotCaller::Shell).unwrap(),
        "\"shell\""
    );
    // The wire: a front envelope flattens the request beside its version.
    let envelope: FrontEnvelope = serde_json::from_str(
        r#"{"version":1,"method":"open_url","app_id":"org.example.linker","url":"https://x.y","flags":0}"#,
    )
    .unwrap();
    assert_eq!(envelope.version, FRONT_WIRE_VERSION);
    assert!(matches!(envelope.request, RpcRequest::OpenUrl { .. }));
    assert_eq!(
        serde_json::to_string(&RpcResponse::Launched {
            session_id: "session-2".into()
        })
        .unwrap(),
        r#"{"result":"launched","session_id":"session-2"}"#
    );
    for (response, expected) in [
        (RpcResponse::Delivered, "delivered"),
        (RpcResponse::NoHandler, "no_handler"),
        (RpcResponse::InvalidUrl, "invalid_url"),
        (RpcResponse::Denied, "denied"),
        (RpcResponse::Busy, "busy"),
        (RpcResponse::RateLimited, "rate_limited"),
        (RpcResponse::Restored, "restored"),
        (RpcResponse::CallerGone, "caller_gone"),
    ] {
        assert_eq!(
            serde_json::to_string(&response).unwrap(),
            format!(r#"{{"result":"{expected}"}}"#)
        );
    }
}

/// A fake SDK front for the socket-level test: the forwarded handle is this process's own
/// pidfd (a real `SCM_RIGHTS` transfer); the caller mapping stands in for the pf-app@ cgroup.
fn front_policy(front_ok: bool) -> FrontPolicy {
    let self_pid = std::process::id() as i32;
    FrontPolicy::custom(
        move |_stream| {
            if front_ok {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "peer pid=1 is session-1.scope (expected pf-sdk-front.service)",
                ))
            }
        },
        move |fd| {
            let process = pf_peer_identity::ReceivedProcess::from_fd(fd)
                .map_err(|error| error.to_string())?;
            if process.pid() == self_pid {
                Ok(LINKER_ID.to_owned())
            } else {
                Err(format!("pid={} is not in a pf-app@ unit", process.pid()))
            }
        },
    )
}

fn front_call(socket: &Path, envelope: &FrontEnvelope, with_handle: bool) -> RpcResponse {
    let stream = std::os::unix::net::UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let pidfd = pf_peer_identity::pidfd_open(std::process::id() as i32).unwrap();
    let handle = with_handle.then(|| std::os::fd::AsFd::as_fd(&pidfd));
    send_front_request(&stream, envelope, handle).unwrap();
    let mut stream = stream;
    serde_json::from_slice(&pf_wire::read_frame(&mut stream).unwrap()).unwrap()
}

#[test]
fn front_socket_refuses_a_non_front_peer_and_re_derives_the_caller_from_the_handle() {
    use std::io::Read as _;
    let dir = short_socket_dir("front");
    let socket = dir.join("front.sock");
    let envelope = |request: RpcRequest| FrontEnvelope {
        version: FRONT_WIRE_VERSION,
        request,
    };

    // A peer that is not the SDK front is closed without a single frame.
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let (requests, _incoming) = std::sync::mpsc::channel();
    spawn_front_acceptor(
        listener,
        requests,
        DEFAULT_CONNECTION_LIMITS,
        front_policy(false),
    );
    let mut refused = std::os::unix::net::UnixStream::connect(&socket).unwrap();
    refused
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut byte = [0; 1];
    assert_eq!(
        refused.read(&mut byte).unwrap(),
        0,
        "closed without service"
    );
    drop(refused);
    fs::remove_file(&socket).unwrap();

    // The SDK front: each request is served with the caller re-derived from the handle.
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let (requests, incoming) = std::sync::mpsc::channel();
    spawn_front_acceptor(
        listener,
        requests,
        DEFAULT_CONNECTION_LIMITS,
        front_policy(true),
    );
    let (a, log) = logged_authority(None);
    let mut a = a.with_url_handler(FixedUrlHandler(BROWSER_ID.into()));
    launch_item(&mut a, LINKER_ID);
    let client_socket = socket.clone();
    let client = std::thread::spawn(move || {
        let matching = front_call(
            &client_socket,
            &envelope(open_url_request(Some(LINKER_ID), URL, 0)),
            true,
        );
        let mismatch = front_call(
            &client_socket,
            &envelope(open_url_request(Some(APP_ID), URL, 0)),
            true,
        );
        let no_handle = front_call(
            &client_socket,
            &envelope(open_url_request(Some(LINKER_ID), URL, 0)),
            false,
        );
        let wrong_version = front_call(
            &client_socket,
            &FrontEnvelope {
                version: 2,
                request: open_url_request(Some(LINKER_ID), URL, 0),
            },
            true,
        );
        let shell_verb = front_call(&client_socket, &envelope(RpcRequest::History), true);
        (matching, mismatch, no_handle, wrong_version, shell_verb)
    });
    let mut served = 0;
    let stopped = run_service_loop(
        &mut a,
        &incoming,
        Duration::from_millis(5),
        |authority, pending: PendingRpc| {
            assert!(matches!(pending.origin(), RpcOrigin::Front { .. }));
            pending.dispatch(authority);
            served += 1;
            if served == 3 {
                Err(io::Error::other("served every front request"))
            } else {
                Ok(())
            }
        },
    );
    assert_eq!(
        stopped.unwrap_err().to_string(),
        "served every front request"
    );
    let (matching, mismatch, no_handle, wrong_version, shell_verb) = client.join().unwrap();
    assert!(
        matches!(matching, RpcResponse::Launched { .. }),
        "{matching:?}"
    );
    assert!(matches!(mismatch, RpcResponse::Denied), "{mismatch:?}");
    assert!(matches!(no_handle, RpcResponse::Denied), "{no_handle:?}");
    assert!(
        matches!(&wrong_version, RpcResponse::Error { message } if message.contains("version 2")),
        "{wrong_version:?}"
    );
    assert!(
        matches!(&shell_verb, RpcResponse::Error { message } if message.contains("not accepted on the front socket")),
        "{shell_verb:?}"
    );
    let refused = refusals(&log);
    assert!(refused[0].contains("reason=identity_mismatch caller=\"org.example.game\" detail=\"derived=org.example.linker\""), "{refused:?}");
    assert!(refused[1].contains("reason=identity_unverified caller=\"org.example.linker\" detail=\"missing_process_handle\""), "{refused:?}");
    assert_eq!(a.system.handoffs.len(), 1);
    fs::remove_dir_all(dir).unwrap();
}
