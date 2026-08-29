use super::*;

const A133: &str =
    include_str!("../../platform-fixtures/shell-input-contract/trimui-smart-pro.json");
const A523: &str =
    include_str!("../../platform-fixtures/shell-input-contract/trimui-smart-pro-s.json");
const BUTTONLESS: &str =
    include_str!("../../platform-fixtures/shell-input-contract/fixture-buttonless.json");

fn contract(json: &str) -> DeviceContract {
    DeviceContract::parse_json(json).unwrap()
}
fn map(json: &str) -> EffectiveMap {
    EffectiveMap::from_persisted(contract(json), None).unwrap()
}
fn chord(a: &str, b: &str) -> Binding {
    Binding {
        shape: BindingShape::Chord,
        controls: vec![a.into(), b.into()],
        max_interval_ms: None,
        min_duration_ms: None,
    }
}

#[test]
fn per_device_shipped_defaults_cover_all_device_classes() {
    assert_eq!(
        map(A523).binding("global", "SafeReturn"),
        Some(&Binding::single("home"))
    );
    assert_eq!(
        map(A133).binding("global", "SafeReturn"),
        Some(&Binding::single("guide"))
    );
    assert_eq!(
        map(BUTTONLESS).binding("global", "SafeReturn"),
        Some(&chord("select", "start"))
    );
}

#[test]
fn a523_home_re_resolves_on_a133_once() {
    let old = map(A523).mappings().to_vec();
    let mut current =
        EffectiveMap::from_persisted(contract(A133), Some(("a523".into(), old))).unwrap();
    assert_eq!(
        current.binding("global", "SafeReturn"),
        Some(&Binding::single("guide"))
    );
    assert!(
        matches!(current.next_event(), Some(MapEvent::BindingReResolved { action, stored_device_id, current_device_id, .. }) if action == "SafeReturn" && stored_device_id == "a523" && current_device_id == "a133")
    );
    assert_eq!(current.next_event(), None);
}

#[test]
fn guide_re_resolves_to_buttonless_chord() {
    let old = map(A133).mappings().to_vec();
    let mut current =
        EffectiveMap::from_persisted(contract(BUTTONLESS), Some(("a133".into(), old))).unwrap();
    assert_eq!(
        current.binding("global", "SafeReturn"),
        Some(&chord("select", "start"))
    );
    assert!(
        matches!(current.next_event(), Some(MapEvent::BindingReResolved { action, .. }) if action == "SafeReturn")
    );
}

#[test]
fn carried_identity_is_loaded_from_the_keyed_store_then_re_resolved() {
    let mut store = MemoryStore::default();
    store.save("a523", map(A523).mappings()).unwrap();
    let mut current = EffectiveMap::load_carried(contract(A133), "a523", &store).unwrap();
    assert_eq!(
        current.binding("global", "SafeReturn"),
        Some(&Binding::single("guide"))
    );
    assert!(matches!(
        current.next_event(),
        Some(MapEvent::BindingReResolved { stored_device_id, .. }) if stored_device_id == "a523"
    ));
}

#[test]
fn confirm_commits_and_emits_only_after_commit() {
    let mut engine = RemapEngine::new(map(A133), MemoryStore::default());
    engine
        .begin("shell", "Activate", Binding::single("west"))
        .unwrap();
    assert_eq!(
        engine.map().binding("shell", "Activate"),
        Some(&Binding::single("east"))
    );
    assert_eq!(engine.map_mut().next_event(), None);
    assert_eq!(engine.confirm().unwrap(), TransactionOutcome::Committed);
    assert_eq!(
        engine.map().binding("shell", "Activate"),
        Some(&Binding::single("west"))
    );
    assert!(
        matches!(engine.map_mut().next_event(), Some(MapEvent::GlyphsUpdated { actions, .. }) if actions == ["Activate"])
    );
}

#[test]
fn timeout_interrupt_and_explicit_revert_all_rollback() {
    for (operation, expected) in [
        (
            RemapEngine::<MemoryStore>::timeout as fn(&mut RemapEngine<MemoryStore>) -> _,
            RollbackReason::Timeout,
        ),
        (
            RemapEngine::<MemoryStore>::interrupt,
            RollbackReason::Interrupted,
        ),
        (RemapEngine::<MemoryStore>::revert, RollbackReason::Reverted),
    ] {
        let mut engine = RemapEngine::new(map(A133), MemoryStore::default());
        engine
            .begin("shell", "Activate", Binding::single("west"))
            .unwrap();
        assert_eq!(
            operation(&mut engine).unwrap(),
            TransactionOutcome::RolledBack(expected)
        );
        assert_eq!(
            engine.map().binding("shell", "Activate"),
            Some(&Binding::single("east"))
        );
        assert_eq!(engine.map_mut().next_event(), None);
    }
}

#[test]
fn persistence_failure_is_an_interrupted_commit_with_no_visible_change_or_event() {
    let mut engine = RemapEngine::new(map(A133), MemoryStore::failing());
    engine
        .begin("shell", "Activate", Binding::single("west"))
        .unwrap();
    assert!(matches!(engine.confirm(), Err(MapError::Persistence(_))));
    assert_eq!(
        engine.map().binding("shell", "Activate"),
        Some(&Binding::single("east"))
    );
    assert_eq!(engine.map_mut().next_event(), None);
}

#[test]
fn rejects_safe_return_collision_across_contexts() {
    let mut engine = RemapEngine::new(map(A133), MemoryStore::default());
    assert_eq!(
        engine.begin("global", "SafeReturn", Binding::single("east")),
        Err(MapError::Collision {
            first: "SafeReturn".into(),
            second: "Activate".into()
        })
    );
}

#[test]
fn capture_rejects_reusing_back_for_activate_without_creating_partial_state() {
    let mut engine = RemapEngine::new(map(A133), MemoryStore::default());
    assert_eq!(
        engine.begin("shell", "Activate", Binding::single("south")),
        Err(MapError::Collision {
            first: "Activate".into(),
            second: "Back".into()
        })
    );
    assert_eq!(
        engine.map().binding("shell", "Activate"),
        Some(&Binding::single("east"))
    );
    assert_eq!(engine.confirm(), Err(MapError::NoTransaction));
}

#[test]
fn persisted_protected_collision_is_rejected_then_re_resolved_to_shipped_defaults() {
    let device = contract(A133);
    let shipped = device.effective_map.clone();
    let mut persisted = shipped.clone();
    let activate = persisted
        .iter_mut()
        .find(|mapping| mapping.action == "Activate")
        .unwrap();
    activate.binding = Binding::single("south");

    let controls = device.controls();
    assert_eq!(
        validate_candidate(&persisted, &controls),
        Err(MapError::Collision {
            first: "Activate".into(),
            second: "Back".into(),
        })
    );

    let mut effective =
        EffectiveMap::from_persisted(device, Some(("a133".into(), persisted))).unwrap();
    assert_eq!(effective.mappings(), shipped);
    assert!(matches!(
        effective.next_event(),
        Some(MapEvent::BindingReResolved {
            action,
            old_binding,
            effective_binding,
            ..
        }) if action == "Activate"
            && old_binding == Binding::single("south")
            && effective_binding == Binding::single("east")
    ));
}

#[test]
fn rejects_any_candidate_that_strands_a_protected_action() {
    let mut broken = contract(A133);
    broken.effective_map.retain(|m| m.action != "Back");
    assert_eq!(
        EffectiveMap::from_persisted(broken, None),
        Err(MapError::ProtectedActionUnreachable("Back".into()))
    );
}

#[test]
fn glyphs_resolve_every_binding_shape_and_printed_or_source_truth() {
    let mut device = contract(A133);
    let shapes = [
        Binding::single("east"),
        chord("select", "start"),
        Binding {
            shape: BindingShape::DoublePress,
            controls: vec!["guide".into()],
            max_interval_ms: Some(500),
            min_duration_ms: None,
        },
        Binding {
            shape: BindingShape::Hold,
            controls: vec!["select".into(), "l1".into(), "r1".into()],
            max_interval_ms: None,
            min_duration_ms: Some(1000),
        },
    ];
    let actions = ["Quick", "Search.open", "Search.submit", "Search.cancel"];
    for (action, binding) in actions.into_iter().zip(shapes) {
        device.effective_map.push(Mapping {
            context: "test".into(),
            action: action.into(),
            binding,
        });
    }
    let effective = EffectiveMap::from_persisted(device, None).unwrap();
    for action in actions {
        let GlyphResult::Resolved(glyph) = effective
            .resolve(&ShellAction::Custom(action.into()))
            .unwrap()
        else {
            panic!("shape did not resolve")
        };
        assert!(!glyph.binding_id.is_empty());
        assert!(glyph.source_fallback.starts_with("pf-"));
    }
    let GlyphResult::Resolved(face) = effective.resolve(&ShellAction::Activate).unwrap() else {
        panic!()
    };
    assert_eq!(face.printed_label, "A");
    let GlyphResult::Resolved(guide) = effective
        .resolve(&ShellAction::Custom("SafeReturn".into()))
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(guide.printed_label, "");
    assert_eq!(guide.source_fallback, "pf-guide");
}

#[test]
fn json_store_is_identity_keyed_and_round_trips() {
    let dir = std::env::temp_dir().join(format!("pf-input-map-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let path = dir.join("remaps.json");
    let mappings = map(A133).mappings().to_vec();
    let mut store = JsonRemapStore::at(&path);
    store.save("a133", &mappings).unwrap();
    store.save("a523", map(A523).mappings()).unwrap();
    assert_eq!(store.load("a133").unwrap(), Some(mappings));
    assert_ne!(store.load("a133").unwrap(), store.load("a523").unwrap());
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn json_store_save_rejects_future_version_without_touching_file() {
    let dir = std::env::temp_dir().join(format!(
        "pf-input-map-future-version-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("remaps.json");
    let original = br#"{"schema_version":2,"devices":{"future-device":[]}}"#;
    fs::write(&path, original).unwrap();
    let mut store = JsonRemapStore::at(&path);

    assert_eq!(
        store.save("a133", map(A133).mappings()),
        Err(MapError::UnsupportedVersion {
            found: 2,
            supported: SCHEMA_VERSION,
        })
    );
    assert_eq!(fs::read(&path).unwrap(), original);
    let _ = fs::remove_dir_all(dir);
}
