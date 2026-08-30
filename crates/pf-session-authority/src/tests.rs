use super::*;
use pf_ports::{SessionEvent, TestClock};
use std::cell::Cell;
use std::path::PathBuf;
use std::time::SystemTime;

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
    start_available: bool,
    fail_force: bool,
    fail_graceful: bool,
    fail_owner: bool,
    fail_start: bool,
}

#[derive(Default)]
struct FakeExecutor {
    calls: Vec<(String, Vec<String>)>,
    codes: VecDeque<i32>,
}
impl CommandExecutor for FakeExecutor {
    fn execute(&mut self, program: &str, args: &[String]) -> Result<i32, String> {
        self.calls.push((program.to_owned(), args.to_vec()));
        Ok(self.codes.pop_front().unwrap_or(0))
    }
}

fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("pf-authority-{name}-{}", std::process::id()))
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
fn command_system_expands_templates_and_classifies_unavailable() {
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
    assert!(system
        .start_foreground(
            &LaunchRequest {
                item_id: "game".into()
            },
            "s1"
        )
        .unwrap());
    assert!(!system
        .start_foreground(
            &LaunchRequest {
                item_id: "missing".into()
            },
            "s2"
        )
        .unwrap());
    let executor = system.into_executor();
    assert_eq!(executor.calls[0].1, ["start", "game", "s1"]);
}

#[test]
fn default_command_templates_use_one_session_keyed_unit_for_the_lifecycle() {
    let mut system =
        CommandSystem::with_executor(CommandTemplates::default(), FakeExecutor::default());
    let request = LaunchRequest {
        item_id: "game".into(),
    };

    assert!(system.start_foreground(&request, "session-1").unwrap());
    system.request_graceful_stop("session-1").unwrap();
    system.enforce_termination("session-1").unwrap();

    let executor = system.into_executor();
    let units: Vec<&str> = executor
        .calls
        .iter()
        .map(|(_, args)| args.last().unwrap().as_str())
        .collect();
    assert_eq!(
        units,
        vec![
            "pf-foreground@session-1.service",
            "pf-foreground@session-1.service",
            "pf-foreground@session-1.service",
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
        item_id: "game".into(),
    };
    let marker = dir.join("sessions/session-1.running");

    assert!(system.start_foreground(&request, "session-1").unwrap());
    assert!(marker.is_file());
    system.request_graceful_stop("session-1").unwrap();
    assert!(!marker.exists());
    system.request_graceful_stop("session-1").unwrap();
    system.enforce_termination("session-1").unwrap();
    system.activate_selected_owner().unwrap();
    assert!(dir.join("shell-selected").is_file());

    fs::remove_dir_all(dir).unwrap();
}
impl FakeSystem {
    fn available() -> Self {
        Self {
            start_available: true,
            ..Self::default()
        }
    }
}
impl SessionSystem for FakeSystem {
    fn start_foreground(&mut self, _: &LaunchRequest, _: &str) -> Result<bool, String> {
        self.calls.push("start".into());
        if self.fail_start {
            Err("start failed".into())
        } else {
            Ok(self.start_available)
        }
    }
    fn request_graceful_stop(&mut self, _: &str) -> Result<(), String> {
        self.calls.push("graceful".into());
        if self.fail_graceful {
            Err("unavailable".into())
        } else {
            Ok(())
        }
    }
    fn enforce_termination(&mut self, _: &str) -> Result<(), String> {
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
}

fn authority() -> Authority<MemoryStore, FakeSystem, TestClock> {
    Authority::open(
        MemoryStore::default(),
        FakeSystem::available(),
        TestClock::new(),
        3,
        Duration::from_millis(10),
    )
    .unwrap()
}
fn launch_running(a: &mut Authority<MemoryStore, FakeSystem, TestClock>) -> String {
    let LaunchResult::Accepted { session_id } = a
        .launch(LaunchRequest {
            item_id: "game".into(),
        })
        .unwrap()
    else {
        panic!()
    };
    a.observe(Observation::SessionRunning).unwrap();
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
            item_id: "game".into(),
        })
        .unwrap()
    else {
        panic!()
    };
    a.observe(Observation::SessionRunning).unwrap();
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
            item_id: "game".into(),
        })
        .unwrap(),
        LaunchResult::Accepted { .. }
    ));
    assert!(matches!(a.state.phase, Phase::Starting { .. }));
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
        Authority::open_with_now_fn(
            MemoryStore::default(),
            FakeSystem::available(),
            TestClock::new(),
            3,
            Duration::from_millis(10),
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

    let mut never_running = make();
    never_running
        .launch(LaunchRequest {
            item_id: "never-running".into(),
        })
        .unwrap();
    never_running
        .observe(Observation::SessionExitedCleanly)
        .unwrap();
    never_running.observe(Observation::UnitInactive).unwrap();
    restore(&mut never_running);
    let entry = never_running.state.history.front().unwrap();
    assert_eq!(entry.started_at, None);
    assert_eq!(entry.ended_at.unwrap().precision, EndPrecision::Observed);
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
    let mut a = authority();
    launch_running(&mut a);
    a.intake_safe_return().unwrap();
    a.tick().unwrap();
    a.system.fail_force = true;
    a.clock.advance(Duration::from_millis(10));
    a.tick().unwrap();
    assert!(matches!(a.state.phase, Phase::RecoveryRequired { .. }));

    let mut a = authority();
    launch_running(&mut a);
    a.system.fail_graceful = true;
    a.system.fail_force = true;
    a.intake_safe_return().unwrap();
    a.tick().unwrap();
    assert!(matches!(a.state.phase, Phase::RecoveryRequired { .. }));
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
fn recent_is_bounded_busy_is_rejected_and_binding_updates_persist() {
    let mut a = authority();
    a.update_safe_return_binding(4).unwrap();
    a.update_safe_return_binding(3).unwrap();
    assert_eq!(a.state.safe_return_binding_revision, 4);
    for n in 0..5 {
        assert!(matches!(
            a.launch(LaunchRequest {
                item_id: format!("g{n}")
            })
            .unwrap(),
            LaunchResult::Accepted { .. }
        ));
        assert_eq!(
            a.launch(LaunchRequest {
                item_id: "busy".into()
            })
            .unwrap(),
            LaunchResult::RejectedBusy
        );
        a.observe(Observation::SessionExitedCleanly).unwrap();
        a.observe(Observation::UnitInactive).unwrap();
        restore(&mut a);
    }
    assert_eq!(a.state.history.len(), 3);
    assert_eq!(a.state.history.front().unwrap().item_id, "g4");
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
    let mut a = Authority::open(
        FileStore::new(&path),
        FakeSystem::available(),
        TestClock::new(),
        3,
        Duration::from_millis(10),
    )
    .unwrap();
    let LaunchResult::Accepted { session_id: id } = a
        .launch(LaunchRequest {
            item_id: "game".into(),
        })
        .unwrap()
    else {
        panic!()
    };
    a.observe(Observation::SessionRunning).unwrap();
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
fn interrupted_write_ahead_start_intent_reconciles_to_typed_recovery() {
    let mut system = FakeSystem::available();
    system.fail_start = true;
    let mut a = Authority::open(
        MemoryStore::default(),
        system,
        TestClock::new(),
        3,
        Duration::from_millis(10),
    )
    .unwrap();
    assert_eq!(
        a.launch(LaunchRequest {
            item_id: "game".into()
        }),
        Err(AuthorityError::Backend("start failed".into()))
    );
    assert!(matches!(
        a.store.snapshot().unwrap().phase,
        Phase::Starting {
            start_invoked: false,
            ..
        }
    ));

    let (store, system, clock) = a.into_parts();
    let mut reopened = Authority::open(store, system, clock, 3, Duration::from_millis(10)).unwrap();
    reopened.reconcile().unwrap();
    assert!(matches!(
        reopened.state.phase,
        Phase::RecoveryRequired { ref reason, .. }
            if reason.contains("interrupted start intent")
    ));
    assert!(reopened
        .events_for("launcher")
        .iter()
        .any(|(_, event)| matches!(event, SessionEvent::RecoveryRequired(_))));
}

#[derive(Default)]
struct RefusingStore {
    saves: usize,
}
impl StateStore for RefusingStore {
    fn load(&self) -> Result<Option<PersistedState>, AuthorityError> {
        Ok(None)
    }
    fn save(&mut self, _: &PersistedState) -> Result<(), AuthorityError> {
        self.saves += 1;
        Err(AuthorityError::Persistence("refused".into()))
    }
}

#[test]
fn start_is_never_invoked_when_write_ahead_intent_save_fails() {
    let mut a = Authority::open(
        RefusingStore::default(),
        FakeSystem::available(),
        TestClock::new(),
        3,
        Duration::from_millis(10),
    )
    .unwrap();
    assert_eq!(
        a.launch(LaunchRequest {
            item_id: "game".into()
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
