use super::*;

const A133: &str = include_str!("../tests/fixtures/trimui-smart-pro.json");
const A523: &str = include_str!("../tests/fixtures/trimui-smart-pro-s.json");
const BUTTONLESS: &str = include_str!("../tests/fixtures/fixture-buttonless.json");
const A133_V3: &str = include_str!("../tests/fixtures/trimui-smart-pro-v3.json");
const A523_V3: &str = include_str!("../tests/fixtures/trimui-smart-pro-s-v3.json");
const NO_STICK_V3: &str = include_str!("../tests/fixtures/fixture-no-stick-v3.json");
const MINIMAL_V3: &str = include_str!("../tests/fixtures/fixture-minimal-v3.json");

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
        axis: None,
        direction: None,
    }
}
fn hold(control: &str, min_duration_ms: u32) -> Binding {
    Binding {
        shape: BindingShape::Hold,
        controls: vec![control.into()],
        max_interval_ms: None,
        min_duration_ms: Some(min_duration_ms),
        axis: None,
        direction: None,
    }
}
fn double_press(controls: &[&str], max_interval_ms: u32) -> Binding {
    Binding {
        shape: BindingShape::DoublePress,
        controls: controls.iter().map(|c| c.to_string()).collect(),
        max_interval_ms: Some(max_interval_ms),
        min_duration_ms: None,
        axis: None,
        direction: None,
    }
}
fn on_screen() -> Binding {
    Binding {
        shape: BindingShape::OnScreen,
        controls: Vec::new(),
        max_interval_ms: None,
        min_duration_ms: None,
        axis: None,
        direction: None,
    }
}
fn stick_axis(control: &str, axis: Axis) -> Binding {
    Binding {
        shape: BindingShape::StickAxis,
        controls: vec![control.into()],
        max_interval_ms: None,
        min_duration_ms: None,
        axis: Some(axis),
        direction: None,
    }
}
fn stick_direction(control: &str, direction: Direction) -> Binding {
    Binding {
        shape: BindingShape::StickDirection,
        controls: vec![control.into()],
        max_interval_ms: None,
        min_duration_ms: None,
        axis: None,
        direction: Some(direction),
    }
}

/// Applies `edit` to a parsed copy of a contract fixture and re-serialises it, so negative
/// controls mutate exactly one fact of an otherwise valid document.
fn mutated(json: &str, edit: impl FnOnce(&mut serde_json::Value)) -> String {
    let mut document: serde_json::Value = serde_json::from_str(json).unwrap();
    edit(&mut document);
    document.to_string()
}
fn mapping_mut<'a>(
    document: &'a mut serde_json::Value,
    context: &str,
    action: &str,
) -> &'a mut serde_json::Value {
    document["effective_map"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|mapping| mapping["context"] == context && mapping["action"] == action)
        .unwrap_or_else(|| panic!("fixture has no {context}/{action} mapping"))
}

#[test]
fn per_device_shipped_defaults_cover_all_device_classes() {
    assert_eq!(
        map(A523).binding("global", "SystemMenu"),
        Some(&Binding::single("home"))
    );
    assert_eq!(
        map(A133).binding("global", "SystemMenu"),
        Some(&Binding::single("guide"))
    );
    assert_eq!(
        map(BUTTONLESS).binding("global", "SystemMenu"),
        Some(&chord("select", "start"))
    );
    for fixture in [A523, A133, BUTTONLESS] {
        assert_eq!(
            map(fixture).binding("global", "Start"),
            Some(&Binding::single("start"))
        );
    }
}

#[test]
fn a523_home_re_resolves_on_a133_once() {
    let old = map(A523).mappings().to_vec();
    let mut current =
        EffectiveMap::from_persisted(contract(A133), Some(("a523".into(), old))).unwrap();
    assert_eq!(
        current.binding("global", "SystemMenu"),
        Some(&Binding::single("guide"))
    );
    assert!(
        matches!(current.next_event(), Some(MapEvent::BindingReResolved { action, stored_device_id, current_device_id, .. }) if action == "SystemMenu" && stored_device_id == "a523" && current_device_id == "a133")
    );
    assert_eq!(current.next_event(), None);
}

#[test]
fn guide_re_resolves_to_buttonless_chord() {
    let old = map(A133).mappings().to_vec();
    let mut current =
        EffectiveMap::from_persisted(contract(BUTTONLESS), Some(("a133".into(), old))).unwrap();
    assert_eq!(
        current.binding("global", "SystemMenu"),
        Some(&chord("select", "start"))
    );
    assert!(
        matches!(current.next_event(), Some(MapEvent::BindingReResolved { action, .. }) if action == "SystemMenu")
    );
}

#[test]
fn carried_identity_is_loaded_from_the_keyed_store_then_re_resolved() {
    let mut store = MemoryStore::default();
    store.save("a523", map(A523).mappings()).unwrap();
    let mut current = EffectiveMap::load_carried(contract(A133), "a523", &store).unwrap();
    assert_eq!(
        current.binding("global", "SystemMenu"),
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
fn reset_to_shipped_restores_a_chained_move() {
    let mut engine = RemapEngine::new(map(A133), MemoryStore::default());
    engine
        .begin("shell", "Activate", Binding::single("west"))
        .unwrap();
    engine.confirm().unwrap();
    engine
        .begin("shell", "Back", Binding::single("east"))
        .unwrap();
    engine.confirm().unwrap();

    engine.reset_to_shipped().unwrap();

    assert_eq!(engine.map().mappings(), engine.map().shipped);
}

#[test]
fn reset_to_shipped_restores_a_pure_swap() {
    let device = contract(A133);
    let mut swapped = device.effective_map.clone();
    let activate = mapping_index(&swapped, "shell", "Activate").unwrap();
    let back = mapping_index(&swapped, "shell", "Back").unwrap();
    let activate_binding = swapped[activate].binding.clone();
    swapped[activate].binding = swapped[back].binding.clone();
    swapped[back].binding = activate_binding;
    let effective = EffectiveMap::from_persisted(device, Some(("a133".into(), swapped))).unwrap();
    let mut engine = RemapEngine::new(effective, MemoryStore::default());

    engine.reset_to_shipped().unwrap();

    assert_eq!(engine.map().mappings(), engine.map().shipped);
}

#[test]
fn load_backfills_an_omitted_mapping_from_the_shipped_profile() {
    let mut device = contract(A133);
    device.effective_map.push(Mapping {
        context: "shell".into(),
        action: "Quick".into(),
        binding: Binding::single("north"),
    });
    let mut persisted = device.effective_map.clone();
    let omitted = persisted
        .iter()
        .position(|mapping| mapping.action == "Quick")
        .unwrap();
    persisted.remove(omitted);
    let effective = EffectiveMap::from_persisted(device, Some(("a133".into(), persisted))).unwrap();
    // A document from an older contract generation must not strand the newer shipped
    // action: load backfills it from the shipped profile.
    assert_eq!(
        effective.binding("shell", "Quick"),
        effective.shipped_binding("shell", "Quick")
    );
    assert_eq!(
        effective.binding("shell", "Quick"),
        Some(&Binding::single("north"))
    );
    let mut engine = RemapEngine::new(effective, MemoryStore::default());

    engine.reset_to_shipped().unwrap();

    assert_eq!(engine.map().mappings(), engine.map().shipped);
}

#[test]
fn reset_to_shipped_failure_leaves_the_in_memory_map_unchanged() {
    let mut effective = map(A133);
    effective.mappings[0].binding = Binding::single("west");
    let unchanged = effective.mappings().to_vec();
    let mut engine = RemapEngine::new(effective, MemoryStore::failing());

    assert!(matches!(
        engine.reset_to_shipped(),
        Err(MapError::Persistence(_))
    ));
    assert_eq!(engine.map().mappings(), unchanged);
    assert_eq!(engine.map_mut().next_event(), None);
}

#[test]
fn reset_to_shipped_cancels_an_in_flight_preview() {
    let mut engine = RemapEngine::new(map(A133), MemoryStore::default());
    engine
        .begin("shell", "Activate", Binding::single("west"))
        .unwrap();

    engine.reset_to_shipped().unwrap();

    assert_eq!(engine.confirm(), Err(MapError::NoTransaction));
    assert_eq!(engine.map().mappings(), engine.map().shipped);
}

#[test]
fn system_menu_cannot_be_rebound_even_to_a_noncolliding_control() {
    let mut engine = RemapEngine::new(map(A133), MemoryStore::default());
    assert_eq!(
        engine.begin("global", "SystemMenu", Binding::single("l1")),
        Err(MapError::ProtectedActionImmutable("SystemMenu".into()))
    );
}

#[test]
fn rejects_start_collision_across_contexts() {
    let mut engine = RemapEngine::new(map(A133), MemoryStore::default());
    assert_eq!(
        engine.begin("global", "Start", Binding::single("east")),
        Err(MapError::Collision {
            first: "Start".into(),
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
fn legacy_persisted_map_keeps_user_remaps_and_adds_new_shipped_protected_actions() {
    let device = contract(A133);
    let shipped_start = device
        .effective_map
        .iter()
        .find(|mapping| mapping.action == "Start")
        .unwrap()
        .clone();
    let mut persisted = device.effective_map.clone();
    persisted.retain(|mapping| mapping.action != "Start");
    persisted
        .iter_mut()
        .find(|mapping| mapping.action == "Activate")
        .unwrap()
        .binding = Binding::single("west");

    let effective = EffectiveMap::from_persisted(device, Some(("a133".into(), persisted))).unwrap();

    assert_eq!(
        effective.binding("shell", "Activate"),
        Some(&Binding::single("west"))
    );
    assert!(effective.mappings().contains(&shipped_start));
}

#[test]
fn persisted_map_cannot_override_the_shipped_system_menu_binding() {
    let device = contract(A133);
    let shipped_menu = device
        .effective_map
        .iter()
        .find(|mapping| mapping.action == "SystemMenu")
        .unwrap()
        .binding
        .clone();
    let mut persisted = device.effective_map.clone();
    persisted
        .iter_mut()
        .find(|mapping| mapping.action == "SystemMenu")
        .unwrap()
        .binding = Binding::single("l1");
    persisted
        .iter_mut()
        .find(|mapping| mapping.action == "Activate")
        .unwrap()
        .binding = Binding::single("west");

    let effective = EffectiveMap::from_persisted(device, Some(("a133".into(), persisted))).unwrap();

    assert_eq!(
        effective.binding("global", "SystemMenu"),
        Some(&shipped_menu)
    );
    assert_eq!(
        effective.binding("shell", "Activate"),
        Some(&Binding::single("west"))
    );
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
    // Each shape lives in the context its action belongs to and avoids ruled SystemMenu
    // bindings, protected-face collisions and menu-reserved controls.
    let additions = [
        ("shell", "Quick", hold("select", 300)),
        ("search", "Search.open", double_press(&["l1", "r1"], 700)),
        ("search", "Search.submit", chord("north", "l1")),
        ("search", "Search.cancel", Binding::single("west")),
    ];
    for (context, action, binding) in &additions {
        device.effective_map.push(Mapping {
            context: (*context).into(),
            action: (*action).into(),
            binding: binding.clone(),
        });
    }
    let effective = EffectiveMap::from_persisted(device, None).unwrap();
    for (_, action, _) in &additions {
        let GlyphResult::Resolved(glyph) = effective
            .resolve(&ShellAction::Custom((*action).into()))
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
        .resolve(&ShellAction::Custom("SystemMenu".into()))
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
fn json_store_migrates_v1_safe_return_on_load_and_next_save() {
    let dir =
        std::env::temp_dir().join(format!("pf-input-map-v1-migration-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("remaps.json");
    let mut legacy = map(A133).mappings().to_vec();
    legacy
        .iter_mut()
        .find(|mapping| mapping.action == "SystemMenu")
        .unwrap()
        .action = "SafeReturn".into();
    let document = serde_json::json!({
        "schema_version": 1,
        "devices": {
            "a133": legacy,
            "other-device": [{
                "context": "global",
                "action": "SafeReturn",
                "binding": {"shape": "single_press", "controls": ["home"]}
            }]
        }
    });
    fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
    let mut store = JsonRemapStore::at(&path);

    let loaded = store.load("a133").unwrap().unwrap();
    assert!(loaded.iter().any(|mapping| mapping.action == "SystemMenu"));
    assert!(!loaded.iter().any(|mapping| mapping.action == "SafeReturn"));

    store.save("a133", &loaded).unwrap();
    let saved: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["schema_version"], SCHEMA_VERSION);
    assert_eq!(saved["devices"]["other-device"][0]["action"], "SystemMenu");
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
    let original = br#"{"schema_version":3,"devices":{"future-device":[]}}"#;
    fs::write(&path, original).unwrap();
    let mut store = JsonRemapStore::at(&path);

    assert_eq!(
        store.save("a133", map(A133).mappings()),
        Err(MapError::UnsupportedVersion {
            found: 3,
            supported: SCHEMA_VERSION,
        })
    );
    assert_eq!(fs::read(&path).unwrap(), original);
    let _ = fs::remove_dir_all(dir);
}

// --- contract v3: device classes, keyboard/pointer contexts, stick + on-screen shapes ---

#[test]
fn v3_contracts_resolve_every_device_class_from_the_shipped_profile() {
    assert_eq!(contract(A133_V3).device_class.as_deref(), Some("two_stick"));
    let two_stick = map(A133_V3);
    assert_eq!(
        two_stick.binding("osk", "Type"),
        Some(&Binding::single("east"))
    );
    assert_eq!(two_stick.binding("osk", "Shift"), Some(&hold("l2", 250)));
    assert_eq!(
        two_stick.binding("osk", "Symbols"),
        Some(&Binding::single("r2"))
    );
    assert_eq!(
        two_stick.binding("osk", "SuggestPick"),
        Some(&stick_axis("rstick", Axis::Horizontal))
    );
    assert_eq!(
        two_stick.binding("osk", "SuggestList"),
        Some(&stick_direction("rstick", Direction::Down))
    );
    assert_eq!(two_stick.binding("osk", "EditPad"), Some(&on_screen()));
    assert_eq!(
        two_stick.binding("osk.edit", "WordLeft"),
        Some(&Binding::single("l1"))
    );
    assert_eq!(
        two_stick.binding("osk.edit", "Done"),
        Some(&Binding::single("start"))
    );
    assert_eq!(
        two_stick.binding("pointer", "LeftClick"),
        Some(&Binding::single("east"))
    );
    assert_eq!(two_stick.binding("pointer", "Slow"), Some(&hold("l2", 250)));
    assert_eq!(two_stick.binding("pointer", "ScrollMode"), None);

    let no_stick = map(NO_STICK_V3);
    assert_eq!(no_stick.binding("osk", "Shift"), Some(&hold("l1", 250)));
    assert_eq!(no_stick.binding("osk", "CursorLeft"), Some(&on_screen()));
    assert_eq!(
        no_stick.binding("osk", "SuggestPick"),
        Some(&Binding::single("dpad_up"))
    );
    assert_eq!(no_stick.binding("osk", "SuggestList"), None);
    assert_eq!(no_stick.binding("pointer", "Slow"), None);
    assert_eq!(
        no_stick.binding("pointer", "Tab"),
        Some(&Binding::single("select"))
    );

    let minimal = map(MINIMAL_V3);
    for action in [
        "Shift",
        "Symbols",
        "CursorLeft",
        "CursorRight",
        "Commit",
        "NextLayout",
    ] {
        assert_eq!(
            minimal.binding("osk", action),
            Some(&on_screen()),
            "{action}"
        );
    }
    assert_eq!(minimal.binding("osk.edit", "SelectAll"), Some(&on_screen()));
    assert_eq!(minimal.binding("pointer", "Back"), None);

    // Rule P2: the required-physical keyboard actions keep a physical binding in every class.
    for fixture in [A133_V3, NO_STICK_V3, MINIMAL_V3] {
        let effective = map(fixture);
        for action in ["Type", "Close", "Backspace", "Space", "SuggestPick"] {
            let binding = effective
                .binding("osk", action)
                .expect("action must resolve");
            assert_ne!(binding.shape, BindingShape::OnScreen, "{action}");
        }
    }

    // Adjudication D4-M8: l3/r3 exist exactly where the device has stick clicks.
    assert!(contract(A523_V3).controls().contains("l3"));
    assert!(contract(A523_V3).controls().contains("r3"));
    assert!(!contract(A133_V3).controls().contains("l3"));
}

#[test]
fn osk_num_inherits_only_the_four_shared_keyboard_actions() {
    let effective = map(A133_V3);
    for action in ["Type", "Close", "Backspace", "Commit"] {
        assert_eq!(
            effective.binding("osk.num", action),
            effective.binding("osk", action)
        );
        assert!(effective.binding("osk.num", action).is_some(), "{action}");
    }
    for action in ["Shift", "Symbols", "SuggestPick"] {
        assert_eq!(effective.binding("osk.num", action), None, "{action}");
    }
    // Negative control: osk.num is an inherited context and never carries its own mappings.
    let inherited = mutated(A133_V3, |document| {
        mapping_mut(document, "osk", "Type")["context"] = serde_json::json!("osk.num");
    });
    assert!(matches!(
        DeviceContract::parse_json(&inherited),
        Err(MapError::InvalidContract(_))
    ));
}

#[test]
fn contract_accepts_versions_two_and_three_and_rejects_everything_else() {
    // Positive controls: the frozen v2 fixtures still parse next to the new v3 ones.
    assert_eq!(contract(A133).schema_version, 2);
    assert_eq!(contract(A133_V3).schema_version, 3);

    let v4 = mutated(A133_V3, |document| {
        document["schema_version"] = serde_json::json!(4);
    });
    assert!(matches!(
        DeviceContract::parse_json(&v4),
        Err(MapError::InvalidContract(_))
    ));
    let missing_class = mutated(A133_V3, |document| {
        document.as_object_mut().unwrap().remove("device_class");
    });
    assert!(matches!(
        DeviceContract::parse_json(&missing_class),
        Err(MapError::InvalidContract(_))
    ));
    let class_on_v2 = mutated(A133, |document| {
        document["device_class"] = serde_json::json!("two_stick");
    });
    assert!(matches!(
        DeviceContract::parse_json(&class_on_v2),
        Err(MapError::InvalidContract(_))
    ));
    let unknown_class = mutated(A133_V3, |document| {
        document["device_class"] = serde_json::json!("three_stick");
    });
    assert!(matches!(
        DeviceContract::parse_json(&unknown_class),
        Err(MapError::InvalidContract(_))
    ));
}

#[test]
fn contexts_are_ruled_and_actions_must_live_in_their_own_context() {
    let unknown_context = mutated(A133_V3, |document| {
        mapping_mut(document, "osk", "Type")["context"] = serde_json::json!("front-room");
    });
    assert!(matches!(
        DeviceContract::parse_json(&unknown_context),
        Err(MapError::InvalidContract(_))
    ));
    let mismatched_context = mutated(A133_V3, |document| {
        mapping_mut(document, "osk", "Type")["context"] = serde_json::json!("osk.edit");
    });
    assert!(matches!(
        DeviceContract::parse_json(&mismatched_context),
        Err(MapError::InvalidContract(_))
    ));
    let unknown_action = mutated(A133_V3, |document| {
        mapping_mut(document, "osk", "Type")["action"] = serde_json::json!("SelfDestruct");
    });
    assert!(matches!(
        DeviceContract::parse_json(&unknown_action),
        Err(MapError::InvalidContract(_))
    ));
    // Positive control: the untouched v3 document parses.
    assert_eq!(contract(A133_V3).schema_version, 3);
}

#[test]
fn stick_and_on_screen_shapes_validate_and_reject_bad_fields() {
    assert!(stick_axis("rstick", Axis::Horizontal).validate().is_ok());
    assert!(stick_direction("lstick", Direction::Up).validate().is_ok());
    assert!(on_screen().validate().is_ok());
    assert_eq!(
        stick_axis("rstick", Axis::Horizontal).id(),
        "stick_axis(rstick;horizontal)"
    );
    assert_eq!(
        stick_direction("rstick", Direction::Down).id(),
        "stick_direction(rstick;down)"
    );
    assert_eq!(on_screen().id(), "on_screen");

    // Negative controls: every malformed stick/on-screen binding is refused.
    assert!(Binding {
        shape: BindingShape::StickAxis,
        controls: vec!["lstick".into(), "rstick".into()],
        ..stick_axis("rstick", Axis::Horizontal)
    }
    .validate()
    .is_err());
    assert!(stick_axis("dpad_up", Axis::Horizontal).validate().is_err());
    assert!(Binding {
        shape: BindingShape::StickAxis,
        axis: None,
        ..stick_axis("rstick", Axis::Horizontal)
    }
    .validate()
    .is_err());
    assert!(Binding {
        shape: BindingShape::StickAxis,
        direction: Some(Direction::Up),
        axis: None,
        ..stick_axis("rstick", Axis::Horizontal)
    }
    .validate()
    .is_err());
    assert!(Binding {
        shape: BindingShape::StickDirection,
        axis: Some(Axis::Vertical),
        direction: None,
        ..stick_direction("rstick", Direction::Down)
    }
    .validate()
    .is_err());
    // Unknown axis/direction words never deserialise (typed enums own the vocabulary).
    assert!(serde_json::from_str::<Binding>(
        r#"{"shape":"stick_axis","controls":["rstick"],"axis":"diagonal"}"#
    )
    .is_err());
    assert!(serde_json::from_str::<Binding>(
        r#"{"shape":"stick_direction","controls":["rstick"],"direction":"sideways"}"#
    )
    .is_err());
    assert!(Binding {
        shape: BindingShape::OnScreen,
        controls: vec!["east".into()],
        ..on_screen()
    }
    .validate()
    .is_err());
    assert!(Binding {
        min_duration_ms: Some(250),
        ..on_screen()
    }
    .validate()
    .is_err());
    assert!(Binding {
        min_duration_ms: Some(250),
        ..stick_axis("rstick", Axis::Horizontal)
    }
    .validate()
    .is_err());
    // Button shapes refuse axis/direction, and single_press still requires one control.
    assert!(Binding {
        axis: Some(Axis::Horizontal),
        ..Binding::single("east")
    }
    .validate()
    .is_err());
    assert!(Binding {
        shape: BindingShape::SinglePress,
        controls: Vec::new(),
        max_interval_ms: None,
        min_duration_ms: None,
        axis: None,
        direction: None,
    }
    .validate()
    .is_err());
}

#[test]
fn menu_controls_and_ruled_menu_bindings_are_reserved_from_remaps() {
    // a133 ships SystemMenu as single_press guide: the guide control is reserved from every
    // other action, in every context (spec §4.1: a keyboard action may not use Menu).
    let mut engine = RemapEngine::new(map(A133_V3), MemoryStore::default());
    assert!(matches!(
        engine.begin("osk", "Commit", Binding::single("guide")),
        Err(MapError::MenuReserved(_))
    ));
    // Ruled SystemMenu alternatives stay reserved on every device, even when they are not
    // the shipped menu binding (mirrors the contract validator's reservation rule).
    assert!(matches!(
        engine.begin("shell", "Quick", double_press(&["guide"], 500)),
        Err(MapError::MenuReserved(_))
    ));
    assert!(matches!(
        engine.begin("shell", "Quick", chord("select", "start")),
        Err(MapError::MenuReserved(_))
    ));
    // Positive control: an unreserved control remaps fine.
    assert!(engine
        .begin("osk", "Symbols", Binding::single("l1"))
        .is_ok());
    engine.revert().unwrap();

    // A chord menu binding does not reserve its individual controls: the buttonless fixture
    // ships global/Start on `start` next to the select+start menu chord.
    let mut chord_engine = RemapEngine::new(map(BUTTONLESS), MemoryStore::default());
    assert!(chord_engine
        .begin("shell", "Activate", Binding::single("select"))
        .is_ok());
}

#[test]
fn required_physical_keyboard_actions_refuse_on_screen_candidates() {
    let mut engine = RemapEngine::new(map(A133_V3), MemoryStore::default());
    for action in ["Type", "Close", "Backspace", "Space", "SuggestPick"] {
        assert_eq!(
            engine.begin("osk", action, on_screen()),
            Err(MapError::RequiredPhysicalBinding(action.into())),
            "{action}"
        );
    }
    // Positive control: a non-required action may go on-screen-only, because the on-screen
    // key exists (§4.1: removal of the last physical binding of Shift is allowed).
    assert!(engine.begin("osk", "Shift", on_screen()).is_ok());
    engine.revert().unwrap();
    // Negative control: on_screen is refused outside the keyboard contexts.
    assert!(matches!(
        engine.begin("shell", "Quick", on_screen()),
        Err(MapError::InvalidBinding(_))
    ));
}

#[test]
fn glyphs_prefer_the_shell_context_and_resolve_keyboard_actions() {
    // Make osk.edit/Back distinguishable from shell/Back so context priority is observable.
    let document = mutated(A133_V3, |value| {
        mapping_mut(value, "osk.edit", "Back")["binding"] =
            serde_json::json!({"shape": "single_press", "controls": ["west"]});
    });
    let effective = map(&document);
    let GlyphResult::Resolved(back) = effective.resolve(&ShellAction::Back).unwrap() else {
        panic!("Back did not resolve")
    };
    assert_eq!(back.binding_id, "single_press(south)");
    assert_eq!(back.printed_label, "B");
    let GlyphResult::Resolved(typed) = effective
        .resolve(&ShellAction::Custom("Type".into()))
        .unwrap()
    else {
        panic!("Type did not resolve")
    };
    assert_eq!(typed.binding_id, "single_press(east)");
    assert_eq!(
        effective
            .resolve(&ShellAction::Custom("Nope".into()))
            .unwrap(),
        GlyphResult::UnsupportedAction
    );
}

#[test]
fn glyphs_resolve_stick_and_on_screen_shapes() {
    let effective = map(A133_V3);
    let GlyphResult::Resolved(suggest) = effective
        .resolve(&ShellAction::Custom("SuggestPick".into()))
        .unwrap()
    else {
        panic!("SuggestPick did not resolve")
    };
    assert_eq!(suggest.binding_id, "stick_axis(rstick;horizontal)");
    let GlyphResult::Resolved(edit_pad) = effective
        .resolve(&ShellAction::Custom("EditPad".into()))
        .unwrap()
    else {
        panic!("EditPad did not resolve")
    };
    assert_eq!(edit_pad.binding_id, "on_screen");
    assert_eq!(edit_pad.printed_label, "");
    assert_eq!(edit_pad.source_fallback, "");
}

#[test]
fn persisted_v2_era_documents_seed_the_shipped_v3_class_actions() {
    // A v2-era persisted document carries only the four shell/global pairs. Loading it
    // against a v3 contract must keep the user's remaps and seed every new device-class
    // action from the shipped profile instead of stranding it.
    let mut legacy = map(A133).mappings().to_vec();
    legacy
        .iter_mut()
        .find(|mapping| mapping.action == "Activate")
        .unwrap()
        .binding = Binding::single("west");
    let mut store = MemoryStore::default();
    store.save("a133", &legacy).unwrap();

    let effective = EffectiveMap::load(contract(A133_V3), &store).unwrap();
    assert_eq!(
        effective.binding("shell", "Activate"),
        Some(&Binding::single("west"))
    );
    assert_eq!(
        effective.binding("osk", "Type"),
        Some(&Binding::single("east"))
    );
    assert_eq!(
        effective.binding("pointer", "LeftClick"),
        Some(&Binding::single("east"))
    );

    // Negative control: a persisted pair that is not in the shipped map marks the document
    // untrusted; the whole document is discarded and re-resolved to shipped.
    let mut poisoned = legacy.clone();
    poisoned.push(Mapping {
        context: "front-room".into(),
        action: "Quick".into(),
        binding: Binding::single("north"),
    });
    let mut store = MemoryStore::default();
    store.save("a133", &poisoned).unwrap();
    let mut effective = EffectiveMap::load(contract(A133_V3), &store).unwrap();
    assert_eq!(
        effective.binding("shell", "Activate"),
        Some(&Binding::single("east"))
    );
    assert!(
        matches!(effective.next_event(), Some(MapEvent::BindingReResolved { action, .. }) if action == "Activate")
    );
}

#[test]
fn system_store_reads_the_system_path_first_and_migrates_the_legacy_shell_document_once() {
    let dir =
        std::env::temp_dir().join(format!("pf-input-map-system-store-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let system = dir.join("var/lib/pocketforge/input/remaps.json");
    let legacy = dir.join("state/remaps.json");
    fs::create_dir_all(legacy.parent().unwrap()).unwrap();

    // A v1 legacy document from the old shell state dir, still using the SafeReturn name.
    let mut legacy_mappings = map(A133).mappings().to_vec();
    legacy_mappings
        .iter_mut()
        .find(|mapping| mapping.action == "SystemMenu")
        .unwrap()
        .action = "SafeReturn".into();
    legacy_mappings
        .iter_mut()
        .find(|mapping| mapping.action == "Activate")
        .unwrap()
        .binding = Binding::single("west");
    let document = serde_json::json!({
        "schema_version": 1,
        "devices": { "a133": legacy_mappings }
    });
    fs::write(&legacy, serde_json::to_vec(&document).unwrap()).unwrap();
    let legacy_before = fs::read(&legacy).unwrap();

    let mut store = SystemRemapStore::at(&system).with_legacy(&legacy);
    let loaded = store.load("a133").unwrap().unwrap();
    assert!(loaded.iter().any(|mapping| mapping.action == "SystemMenu"));
    assert!(!loaded.iter().any(|mapping| mapping.action == "SafeReturn"));
    assert!(loaded
        .iter()
        .any(|mapping| mapping.action == "Activate" && mapping.binding == Binding::single("west")));
    assert!(!system.exists(), "load must not create the system document");
    assert_eq!(
        fs::read(&legacy).unwrap(),
        legacy_before,
        "the legacy document must never be mutated"
    );

    assert_eq!(
        store.migrate().unwrap(),
        MigrationStatus::MigratedFromLegacy
    );
    assert!(system.exists());
    assert_eq!(store.migrate().unwrap(), MigrationStatus::AlreadyPresent);
    assert_eq!(fs::read(&legacy).unwrap(), legacy_before);

    // Once the system document exists it wins, even while the legacy document disagrees.
    let v3_mappings = map(A133_V3).mappings().to_vec();
    store.save("a133", &v3_mappings).unwrap();
    assert_eq!(store.load("a133").unwrap().unwrap(), v3_mappings);
    assert_eq!(fs::read(&legacy).unwrap(), legacy_before);

    // Negative controls: no legacy document means nothing to migrate ...
    let mut empty = SystemRemapStore::at(dir.join("empty/remaps.json"));
    assert_eq!(empty.load("a133").unwrap(), None);
    assert_eq!(empty.migrate().unwrap(), MigrationStatus::NoLegacyDocument);
    // ... and a future-version legacy document is refused, not silently dropped.
    fs::write(&legacy, br#"{"schema_version":9,"devices":{}}"#).unwrap();
    let future = SystemRemapStore::at(dir.join("other/remaps.json")).with_legacy(&legacy);
    assert!(matches!(
        future.load("a133"),
        Err(MapError::UnsupportedVersion { found: 9, .. })
    ));
    let _ = fs::remove_dir_all(dir);
}

// --- review round 1 (runtime#119): version-gated vocabulary + persisted fail-closed ---

#[test]
fn v2_contracts_reject_every_v3_only_construct() {
    // Positive control: the frozen v2 document parses.
    assert_eq!(contract(A133).schema_version, 2);

    // A v3-only position is unknown at v2.
    let dpad = mutated(A133, |document| {
        document["physical_controls"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "position": "dpad_up",
                "fallback_glyph": {"source": "pocketforge", "id": "pf-dpad-up"}
            }));
    });
    assert!(matches!(
        DeviceContract::parse_json(&dpad),
        Err(MapError::InvalidContract(_))
    ));

    // A v3-only context/action row is rejected at v2.
    let osk_row = mutated(A133, |document| {
        document["effective_map"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "context": "osk",
                "action": "Type",
                "binding": {"shape": "single_press", "controls": ["east"]}
            }));
    });
    assert!(matches!(
        DeviceContract::parse_json(&osk_row),
        Err(MapError::InvalidContract(_))
    ));

    // v3-only binding shapes are rejected at v2, before any absent-control reasoning.
    let on_screen_doc = mutated(A133, |document| {
        mapping_mut(document, "shell", "Activate")["binding"] =
            serde_json::json!({"shape": "on_screen"});
    });
    assert!(matches!(
        DeviceContract::parse_json(&on_screen_doc),
        Err(MapError::InvalidBinding(_))
    ));
    let stick = mutated(A133, |document| {
        mapping_mut(document, "shell", "Activate")["binding"] = serde_json::json!({
            "shape": "stick_axis",
            "controls": ["lstick"],
            "axis": "horizontal"
        });
    });
    assert!(matches!(
        DeviceContract::parse_json(&stick),
        Err(MapError::InvalidBinding(_))
    ));

    // The same gate applies to interactive remap candidates on a v2 map ...
    let mut engine = RemapEngine::new(map(A133), MemoryStore::default());
    assert!(matches!(
        engine.begin("shell", "Activate", on_screen()),
        Err(MapError::InvalidBinding(_))
    ));
    // ... while the v3 map accepts the identical candidate shape (positive control:
    // required_physical_keyboard_actions_refuse_on_screen_candidates passes Shift).
    assert_eq!(map(A133_V3).contract_version(), 3);
}

#[test]
fn persisted_policy_violations_discard_the_whole_document() {
    let shipped = contract(A133_V3).effective_map.clone();
    let case = |name: &str, mutate: &dyn Fn(&mut Vec<Mapping>)| {
        let mut persisted = shipped.clone();
        mutate(&mut persisted);
        let mut store = MemoryStore::default();
        store.save("a133", &persisted).unwrap();
        // The violation must never abort startup (fail-open) nor become effective:
        // the whole document is discarded and re-resolved to shipped, with honest events.
        let mut effective = EffectiveMap::load(contract(A133_V3), &store)
            .unwrap_or_else(|error| panic!("{name}: load must not fail, got {error:?}"));
        assert_eq!(effective.mappings(), shipped.as_slice(), "{name}");
        assert!(
            matches!(
                effective.next_event(),
                Some(MapEvent::BindingReResolved { .. })
            ),
            "{name}: discard must be reported"
        );
    };

    case("p2-on-screen", &|persisted| {
        persisted
            .iter_mut()
            .find(|m| m.context == "osk" && m.action == "Type")
            .unwrap()
            .binding = on_screen();
    });
    case("on-screen-outside-keyboard", &|persisted| {
        persisted
            .iter_mut()
            .find(|m| m.context == "shell" && m.action == "Quick")
            .unwrap()
            .binding = on_screen();
    });
    case("menu-reserved-control", &|persisted| {
        persisted
            .iter_mut()
            .find(|m| m.context == "osk" && m.action == "Commit")
            .unwrap()
            .binding = Binding::single("guide");
    });
    case("ruled-menu-binding", &|persisted| {
        persisted
            .iter_mut()
            .find(|m| m.context == "shell" && m.action == "Quick")
            .unwrap()
            .binding = double_press(&["guide"], 500);
    });
    case("duplicate-pair", &|persisted| {
        let mut duplicate = persisted
            .iter()
            .find(|m| m.context == "shell" && m.action == "Quick")
            .unwrap()
            .clone();
        duplicate.binding = Binding::single("west");
        persisted.push(duplicate);
    });
    case("malformed-binding", &|persisted| {
        persisted
            .iter_mut()
            .find(|m| m.context == "osk" && m.action == "Symbols")
            .unwrap()
            .binding = Binding {
            shape: BindingShape::Hold,
            controls: vec!["r2".into()],
            max_interval_ms: None,
            min_duration_ms: None,
            axis: None,
            direction: None,
        };
    });

    // Positive control: legal persisted remaps survive the same load path.
    let mut persisted = shipped.clone();
    persisted
        .iter_mut()
        .find(|m| m.context == "shell" && m.action == "Quick")
        .unwrap()
        .binding = Binding::single("l1");
    persisted
        .iter_mut()
        .find(|m| m.context == "osk" && m.action == "Shift")
        .unwrap()
        .binding = on_screen();
    let mut store = MemoryStore::default();
    store.save("a133", &persisted).unwrap();
    let mut effective = EffectiveMap::load(contract(A133_V3), &store).unwrap();
    assert_eq!(
        effective.binding("shell", "Quick"),
        Some(&Binding::single("l1"))
    );
    assert_eq!(effective.binding("osk", "Shift"), Some(&on_screen()));
    assert_eq!(effective.next_event(), None);
}
