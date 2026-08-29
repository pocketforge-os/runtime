use super::*;
use pf_scene::{Bounds, Insets, Node, NodeAction, NodeId, Orientation, Role};

fn deadline(nanos: u64) -> Deadline {
    Deadline(MonotonicTime::from_nanos(nanos))
}

fn scene() -> Scene {
    let focus = NodeId::new("primary").unwrap();
    let child = Node::new(
        focus.clone(),
        Role::Button,
        "primary",
        Bounds::new(0.0, 0.0, 1.0, 1.0),
        "control",
    )
    .with_action(NodeAction::Activate);
    let root = Node::new(
        NodeId::new("root").unwrap(),
        Role::Group,
        "root",
        Bounds::new(0.0, 0.0, 1.0, 1.0),
        "group",
    )
    .with_children(vec![child]);
    Scene::new(root, focus).unwrap()
}

fn metrics() -> SurfaceMetrics {
    SurfaceMetrics {
        logical_width: 80.0,
        logical_height: 48.0,
        scale: 2.0,
        safe_insets: Insets::default(),
        orientation: Orientation::Landscape,
    }
}

#[test]
fn test_clock_and_action_deadlines_are_deterministic() {
    let mut clock = TestClock::new();
    assert_eq!(clock.deadline_after(Duration::from_nanos(5)), deadline(5));
    clock.advance(Duration::from_nanos(3));
    assert_eq!(clock.now(), MonotonicTime::from_nanos(3));

    let mut source = FakeActionSource::new(
        clock,
        [
            ScriptedAction::At(
                MonotonicTime::from_nanos(4),
                ActionEvent::Action(ShellAction::Activate),
            ),
            ScriptedAction::At(
                MonotonicTime::from_nanos(3),
                ActionEvent::ActiveSourceChanged(Some(InputSourceId("pad".into()))),
            ),
            ScriptedAction::Error(ActionSourceError::CorruptSequence),
            ScriptedAction::Close,
        ],
    );
    assert_eq!(
        source.next_action(deadline(3)),
        Ok(ActionPoll::DeadlineReached)
    );
    source.clock_mut().advance(Duration::from_nanos(1));
    assert_eq!(
        source.next_action(deadline(4)),
        Ok(ActionPoll::Event(ActionEvent::Action(
            ShellAction::Activate
        )))
    );
    assert_eq!(
        source.next_action(deadline(4)),
        Ok(ActionPoll::Event(ActionEvent::ActiveSourceChanged(Some(
            InputSourceId("pad".into())
        ))))
    );
    assert_eq!(
        source.next_action(deadline(4)),
        Err(ActionSourceError::CorruptSequence)
    );
    assert_eq!(source.next_action(deadline(4)), Ok(ActionPoll::Closed));
}

#[test]
fn action_and_glyph_result_variants_are_typed() {
    let mut source = FakeActionSource::new(
        TestClock::new(),
        [
            ScriptedAction::At(MonotonicTime::ZERO, ActionEvent::Action(ShellAction::Back)),
            ScriptedAction::At(MonotonicTime::ZERO, ActionEvent::ActiveSourceChanged(None)),
            ScriptedAction::Error(ActionSourceError::Unavailable),
        ],
    );
    assert!(matches!(
        source.next_action(deadline(0)),
        Ok(ActionPoll::Event(ActionEvent::Action(ShellAction::Back)))
    ));
    assert!(matches!(
        source.next_action(deadline(0)),
        Ok(ActionPoll::Event(ActionEvent::ActiveSourceChanged(None)))
    ));
    assert_eq!(
        source.next_action(deadline(0)),
        Err(ActionSourceError::Unavailable)
    );

    let variants = [
        Ok(GlyphResult::Resolved(EffectiveBinding {
            binding_id: "confirm".into(),
            printed_label: "Confirm".into(),
            source_fallback: "Activate".into(),
        })),
        Ok(GlyphResult::UnsupportedAction),
        Ok(GlyphResult::NoActiveSource),
        Err(GlyphError::ResolverUnavailable),
        Err(GlyphError::InvalidBinding),
    ];
    for expected in variants {
        let resolver = FakeGlyphResolver {
            result: expected.clone(),
        };
        assert_eq!(resolver.resolve(&ShellAction::Activate), expected);
    }
}

#[test]
fn catalog_variants_and_revision_commits() {
    let snapshot = CatalogSnapshot {
        revision: 7,
        items: vec![CatalogItem {
            id: "one".into(),
            title: "One".into(),
            favorite: false,
        }],
    };
    let mut catalog = FakeCatalog::fixture(snapshot.clone());
    assert_eq!(catalog.snapshot(), Ok(ProviderResult::Ready(snapshot)));
    assert_eq!(
        catalog.set_favorite("one", true, 6),
        Ok(FavoriteCommitResult::RevisionConflict { current: 7 })
    );
    assert_eq!(
        catalog.set_favorite("missing", true, 7),
        Ok(FavoriteCommitResult::ItemNotFound)
    );
    assert_eq!(
        catalog.set_favorite("one", true, 7),
        Ok(FavoriteCommitResult::Committed(8))
    );

    for state in [
        Ok(ProviderResult::Pending),
        Ok(ProviderResult::Unavailable),
        Err(CatalogError::InvalidSnapshot),
    ] {
        assert_eq!(FakeCatalog::returning(state.clone()).snapshot(), state);
    }
    let mut unavailable = FakeCatalog::returning(Ok(ProviderResult::Unavailable));
    assert_eq!(
        unavailable.set_favorite("one", true, 0),
        Err(CatalogError::ProviderFailure)
    );
}

#[test]
fn session_receipts_require_completed_observation() {
    let receipt = TerminalReceipt::Returned {
        session_id: "s1".into(),
    };
    let mut invalid = FakeSession::new(
        Ok(LaunchResult::Accepted {
            session_id: "s1".into(),
        }),
        [ScriptedSession::Event(SessionEvent::Terminal(
            receipt.clone(),
        ))],
    );
    assert!(matches!(
        invalid.launch(LaunchRequest {
            item_id: "one".into()
        }),
        Ok(LaunchResult::Accepted { .. })
    ));
    assert_eq!(
        invalid.next_event(deadline(0)),
        Err(SessionError::ReceiptBeforeObservationComplete)
    );
    assert!(invalid.history().is_empty());

    let recovery = RecoveryRequired {
        session_id: "s1".into(),
        reason: "repair".into(),
    };
    let mut valid = FakeSession::new(
        Ok(LaunchResult::RejectedBusy),
        [
            ScriptedSession::Event(SessionEvent::Observed(ObservedSessionState::Running)),
            ScriptedSession::Event(SessionEvent::Observed(ObservedSessionState::Suspended)),
            ScriptedSession::Event(SessionEvent::Observed(ObservedSessionState::Starting)),
            ScriptedSession::Event(SessionEvent::Observed(
                ObservedSessionState::ObservationComplete,
            )),
            ScriptedSession::Event(SessionEvent::Terminal(receipt.clone())),
            ScriptedSession::Event(SessionEvent::RecoveryRequired(recovery.clone())),
            ScriptedSession::Deadline,
            ScriptedSession::Idle,
            ScriptedSession::Error(SessionError::BackendUnavailable),
        ],
    );
    assert_eq!(
        valid.launch(LaunchRequest {
            item_id: "one".into()
        }),
        Ok(LaunchResult::RejectedBusy)
    );
    assert!(matches!(
        valid.next_event(deadline(0)),
        Ok(SessionPoll::Event(SessionEvent::Observed(
            ObservedSessionState::Running
        )))
    ));
    valid.next_event(deadline(0)).unwrap();
    valid.next_event(deadline(0)).unwrap();
    valid.next_event(deadline(0)).unwrap();
    assert_eq!(
        valid.next_event(deadline(0)),
        Ok(SessionPoll::Event(SessionEvent::Terminal(receipt)))
    );
    assert_eq!(
        valid.next_event(deadline(0)),
        Ok(SessionPoll::Event(SessionEvent::RecoveryRequired(recovery)))
    );
    assert_eq!(
        valid.next_event(deadline(0)),
        Ok(SessionPoll::DeadlineReached)
    );
    assert_eq!(valid.next_event(deadline(0)), Ok(SessionPoll::Idle));
    assert_eq!(
        valid.next_event(deadline(0)),
        Err(SessionError::BackendUnavailable)
    );

    for result in [
        Ok(LaunchResult::ItemUnavailable),
        Err(SessionError::InvalidLifecycle),
    ] {
        let mut fake = FakeSession::new(result.clone(), []);
        assert_eq!(
            fake.launch(LaunchRequest {
                item_id: "x".into()
            }),
            result
        );
    }
    for terminal in [
        TerminalReceipt::ForcedClose {
            session_id: "s".into(),
        },
        TerminalReceipt::Crash {
            session_id: "s".into(),
            summary: "fault".into(),
        },
    ] {
        assert!(!format!("{terminal:?}").is_empty());
    }
}

#[test]
fn preference_variants_preserve_stored_vs_applied() {
    let key = PreferenceKey("motion".into());
    let value = EffectivePreference {
        key: key.clone(),
        effective: PreferenceValue::Bool(false),
        stored: PreferenceValue::Bool(false),
        applied: true,
    };
    let authority = ChangeAuthority("user".into());
    let mut fake = FakePreferencePort::new([value], authority.clone());
    assert!(fake.read(&key).unwrap().is_some());
    assert_eq!(
        fake.submit_change(PreferenceChange {
            key: key.clone(),
            value: PreferenceValue::Bool(true),
            authority: ChangeAuthority("app".into())
        }),
        Ok(PreferenceChangeResult::Unauthorized)
    );
    assert_eq!(
        fake.submit_change(PreferenceChange {
            key: PreferenceKey("missing".into()),
            value: PreferenceValue::Bool(true),
            authority: authority.clone()
        }),
        Ok(PreferenceChangeResult::UnsupportedKey)
    );
    assert_eq!(
        fake.submit_change(PreferenceChange {
            key: key.clone(),
            value: PreferenceValue::Bool(true),
            authority
        }),
        Ok(PreferenceChangeResult::Accepted)
    );
    assert!(matches!(
        fake.next_change(deadline(0)),
        Ok(PreferencePoll::Changed(_))
    ));
    assert_eq!(
        fake.next_change(deadline(0)),
        Ok(PreferencePoll::DeadlineReached)
    );
    fake.script_poll(Ok(PreferencePoll::Closed));
    fake.script_poll(Err(PreferenceError::BackendUnavailable));
    fake.script_poll(Err(PreferenceError::InvalidValue));
    assert_eq!(fake.next_change(deadline(0)), Ok(PreferencePoll::Closed));
    assert_eq!(
        fake.next_change(deadline(0)),
        Err(PreferenceError::BackendUnavailable)
    );
    assert_eq!(
        fake.next_change(deadline(0)),
        Err(PreferenceError::InvalidValue)
    );

    let unapplied_key = PreferenceKey("contrast".into());
    let unapplied = EffectivePreference {
        key: unapplied_key.clone(),
        effective: PreferenceValue::Integer(1),
        stored: PreferenceValue::Integer(1),
        applied: false,
    };
    let mut fake = FakePreferencePort::new([unapplied], ChangeAuthority("user".into()));
    assert_eq!(
        fake.submit_change(PreferenceChange {
            key: unapplied_key,
            value: PreferenceValue::Integer(2),
            authority: ChangeAuthority("user".into())
        }),
        Ok(PreferenceChangeResult::StoredNotApplied)
    );
}

#[test]
fn frame_host_records_every_scene_and_exercises_failures() {
    let mut host = FakeFrameHost::new(metrics());
    host.inject(Err(PresentFailure::SurfaceLost));
    host.inject(Err(PresentFailure::Rejected));
    host.inject(Err(PresentFailure::Backend("offline".into())));
    let scene = scene();
    assert_eq!(host.metrics(), metrics());
    assert_eq!(host.present(&scene), Err(PresentFailure::SurfaceLost));
    assert_eq!(host.present(&scene), Err(PresentFailure::Rejected));
    assert!(matches!(
        host.present(&scene),
        Err(PresentFailure::Backend(_))
    ));
    assert_eq!(host.present(&scene), Ok(PresentAck { sequence: 0 }));
    assert_eq!(host.presented().len(), 4);
}

#[test]
fn power_fake_reports_support_refusal_failure_and_applied_policy() {
    let initial = IdlePolicy {
        sleep_after: None,
        power_off_after: Some(Duration::from_secs(300)),
    };
    let mut fake = FakePowerPort::new(
        vec![
            PowerCapability {
                action: PowerAction::PowerOff,
                support: Support::Supported,
            },
            PowerCapability {
                action: PowerAction::Restart,
                support: Support::Supported,
            },
            PowerCapability {
                action: PowerAction::Sleep,
                support: Support::Unsupported,
            },
        ],
        initial.clone(),
    );
    assert_eq!(fake.idle_policy(), Ok(initial));
    assert_eq!(
        fake.request(PowerAction::Sleep),
        Ok(PowerRequestResult::Unsupported)
    );
    fake.script_request(Ok(PowerRequestResult::Refused {
        reason: "busy".into(),
    }));
    assert!(matches!(
        fake.request(PowerAction::Restart),
        Ok(PowerRequestResult::Refused { .. })
    ));
    fake.script_request(Err(PowerError::BackendUnavailable));
    assert_eq!(
        fake.request(PowerAction::PowerOff),
        Err(PowerError::BackendUnavailable)
    );

    let requested = IdlePolicy {
        sleep_after: Some(Duration::from_secs(60)),
        power_off_after: None,
    };
    let applied = IdlePolicy {
        sleep_after: None,
        power_off_after: None,
    };
    fake.script_policy_write(Ok(AppliedIdlePolicy {
        requested: requested.clone(),
        applied: applied.clone(),
    }));
    assert_eq!(
        fake.set_idle_policy(requested),
        Ok(AppliedIdlePolicy {
            requested: IdlePolicy {
                sleep_after: Some(Duration::from_secs(60)),
                power_off_after: None
            },
            applied: applied.clone(),
        })
    );
    assert_eq!(fake.idle_policy(), Ok(applied));
}

#[test]
fn time_fake_gates_manual_time_and_preserves_applied_values() {
    let epoch = SystemTime::UNIX_EPOCH;
    let state = TimeState {
        wall_clock: epoch,
        timezone: "UTC".into(),
        ntp_state: NtpState::Active,
    };
    let mut fake = FakeTimePort::new(
        TimeCapabilities {
            manual_set_time: Support::Unsupported,
        },
        state,
    );
    assert_eq!(
        fake.set_time(epoch + Duration::from_secs(1)),
        Err(TimeError::Unsupported)
    );
    fake.script_timezone(Ok(AppliedValue {
        requested: "Mars/Base".into(),
        applied: "UTC".into(),
    }));
    assert_eq!(
        fake.set_timezone("Mars/Base".into()).unwrap().applied,
        "UTC"
    );
    fake.script_ntp(Err(TimeError::BackendUnavailable));
    assert_eq!(
        fake.set_ntp_enabled(false),
        Err(TimeError::BackendUnavailable)
    );
    assert_eq!(fake.read().unwrap().ntp_state, NtpState::Active);
    assert_eq!(NtpState::Supported, NtpState::Supported);
    assert_eq!(NtpState::Unsupported, NtpState::Unsupported);
}

#[test]
fn network_fake_redacts_credentials_and_scripts_progress_and_degradation() {
    let credential = WifiCredential::new(b"do-not-echo".to_vec());
    assert_eq!(credential.expose_secret(), b"do-not-echo");
    assert!(!format!("{credential:?}").contains("do-not-echo"));
    let state = NetworkState {
        interface_present: true,
        enabled: true,
        connected_ssid: None,
        signal: None,
    };
    let mut fake = FakeNetworkPort::new(state.clone());
    fake.script_scan(Ok(vec![WifiNetwork {
        ssid: "net".into(),
        security: WifiSecurity::Personal,
        strength: 72,
    }]));
    assert_eq!(fake.scan().unwrap()[0].strength, 72);
    fake.script_connect(Ok(ConnectResult::Progress(ConnectProgress::Authenticating)));
    assert_eq!(
        fake.connect("net", credential.clone()),
        Ok(ConnectResult::Progress(ConnectProgress::Authenticating))
    );
    fake.script_connect(Err(NetworkError::InvalidCredential));
    assert_eq!(
        fake.connect("net", credential),
        Err(NetworkError::InvalidCredential)
    );
    let degraded = NetworkState {
        enabled: false,
        ..state
    };
    fake.script_enabled(Ok(AppliedNetworkEnabled {
        requested: true,
        applied: degraded.clone(),
    }));
    assert_eq!(fake.set_enabled(true).unwrap().applied, degraded);
    fake.script_forget(Err(NetworkError::BackendUnavailable));
    assert_eq!(fake.forget("net"), Err(NetworkError::BackendUnavailable));
}

#[test]
fn transfer_fake_keeps_unsupported_visible_and_returns_warning_and_applied_state() {
    let sftp = TransferServiceState {
        service: TransferService::Sftp,
        support: Support::Supported,
        enabled: false,
        endpoint_info: Some("device.local:22".into()),
    };
    let usb = TransferServiceState {
        service: TransferService::UsbMassStorage,
        support: Support::Unsupported,
        enabled: false,
        endpoint_info: None,
    };
    let mut fake = FakeTransferPort::new(vec![sftp.clone(), usb]);
    assert_eq!(fake.services().unwrap().len(), 2);
    assert_eq!(
        fake.set_enabled(TransferService::UsbMassStorage, true),
        Err(TransferError::Refused)
    );
    fake.script_enabled(Ok(AppliedTransferState {
        requested: true,
        applied: sftp.clone(),
        warning: Some(TransferWarning::ExclusiveStorageAccessRequired),
    }));
    let result = fake.set_enabled(TransferService::Sftp, true).unwrap();
    assert!(matches!(
        result.warning,
        Some(TransferWarning::ExclusiveStorageAccessRequired)
    ));
    assert!(!result.applied.enabled);
    fake.script_enabled(Err(TransferError::BackendUnavailable));
    assert_eq!(
        fake.set_enabled(TransferService::Sftp, true),
        Err(TransferError::BackendUnavailable)
    );
}
