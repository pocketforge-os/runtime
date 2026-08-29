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
