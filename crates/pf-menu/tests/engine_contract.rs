use pf_menu::{
    ApplyRequest, ApplyResult, ApplyState, Bind, ChoiceOption, Cond, Confirm, EngineOptions,
    Feedback, Item, ItemId, ItemKind, Menu, MenuEffect, MenuEngine, MenuInput, MenuValue, Msg,
    Overlay, Page, PageId, PrefsSchemas, Present, RevertReason, Section, FORMAT_V1,
};
use pf_ports::MonotonicTime;

fn item(id: &str, kind: ItemKind) -> Item {
    Item::new(id, Msg::text(id), kind)
}

fn menu() -> Menu {
    let mut disabled = item(
        "disabled",
        ItemKind::Action {
            confirm: None,
            stay_open: true,
        },
    );
    disabled.enabled = Cond::Never;
    Menu {
        format: FORMAT_V1.into(),
        id: "engine-contract".into(),
        root: PageId("root".into()),
        pages: vec![Page {
            id: PageId("root".into()),
            title: Msg::text("Root"),
            tabs: vec![],
            primary: Some(ItemId("primary".into())),
            sections: vec![Section {
                header: None,
                footer: None,
                items: vec![
                    item(
                        "toggle",
                        ItemKind::Toggle {
                            bind: Bind::Host("toggle".into()),
                        },
                    ),
                    disabled,
                    item(
                        "confirm",
                        ItemKind::Action {
                            confirm: Some(Confirm {
                                title: Msg::text("Confirm?"),
                                body: None,
                                confirm_label: Msg::text("Do it"),
                                destructive: true,
                            }),
                            stay_open: true,
                        },
                    ),
                    item(
                        "primary",
                        ItemKind::Action {
                            confirm: None,
                            stay_open: true,
                        },
                    ),
                    item(
                        "choice",
                        ItemKind::Choice {
                            bind: Bind::Host("choice".into()),
                            options: ["one", "two", "three"]
                                .into_iter()
                                .map(|value| ChoiceOption {
                                    value: value.into(),
                                    label: Msg::text(value),
                                    detail: None,
                                })
                                .collect(),
                            present: Present::Picker,
                        },
                    ),
                ],
            }],
        }],
    }
}

fn request(effects: &[MenuEffect]) -> ApplyRequest {
    effects
        .iter()
        .find_map(|effect| match effect {
            MenuEffect::SetValue { request, .. } => Some(*request),
            _ => None,
        })
        .expect("write effect")
}

#[test]
fn apply_lifecycle_commits_and_reverts_every_failure_mode() {
    let mut engine = MenuEngine::new(menu(), &PrefsSchemas, EngineOptions::default()).unwrap();
    let toggle = engine.item_index(&ItemId("toggle".into())).unwrap();
    engine.set_value(toggle, MenuValue::Bool(false)).unwrap();

    let effects = engine.input(MenuInput::Activate, MonotonicTime::ZERO);
    let first = request(effects);
    assert!(effects.iter().any(|effect| matches!(effect,
        MenuEffect::SetValue { request, item, value: MenuValue::Bool(true) }
            if *request == first && *item == toggle
    )));
    assert!(matches!(
        engine.apply_state(toggle),
        ApplyState::Pending { .. }
    ));
    engine.input(
        MenuInput::Applied {
            request: first,
            result: ApplyResult::Committed(MenuValue::Bool(true)),
        },
        MonotonicTime::from_nanos(1),
    );
    assert_eq!(engine.apply_state(toggle), ApplyState::Committed);

    let second = request(engine.input(MenuInput::Activate, MonotonicTime::from_nanos(2)));
    engine.input(
        MenuInput::Applied {
            request: second,
            result: ApplyResult::Rejected,
        },
        MonotonicTime::from_nanos(3),
    );
    assert_eq!(engine.value(toggle), Some(MenuValue::Bool(true)));
    assert_eq!(
        engine.apply_state(toggle),
        ApplyState::Reverted(RevertReason::Rejected)
    );

    let third = request(engine.input(MenuInput::Activate, MonotonicTime::from_nanos(4)));
    engine.input(
        MenuInput::Applied {
            request: third,
            result: ApplyResult::Committed(MenuValue::Int(7)),
        },
        MonotonicTime::from_nanos(5),
    );
    assert_eq!(engine.value(toggle), Some(MenuValue::Bool(true)));
    assert_eq!(
        engine.apply_state(toggle),
        ApplyState::Reverted(RevertReason::InvalidValue)
    );

    engine.input(MenuInput::Activate, MonotonicTime::from_nanos(6));
    engine.input(MenuInput::Tick, MonotonicTime::from_nanos(1_000_000_006));
    assert_eq!(engine.value(toggle), Some(MenuValue::Bool(true)));
    assert_eq!(
        engine.apply_state(toggle),
        ApplyState::Reverted(RevertReason::Timeout)
    );
}

#[test]
fn stale_apply_replies_cannot_overwrite_newer_writes_or_follow_an_item_index() {
    let mut engine = MenuEngine::new(menu(), &PrefsSchemas, EngineOptions::default()).unwrap();
    let toggle = engine.item_index(&ItemId("toggle".into())).unwrap();
    engine.set_value(toggle, MenuValue::Bool(false)).unwrap();

    let older = request(engine.input(MenuInput::Activate, MonotonicTime::ZERO));
    let newer = request(engine.input(MenuInput::Activate, MonotonicTime::from_nanos(1)));
    engine.input(
        MenuInput::Applied {
            request: older,
            result: ApplyResult::Rejected,
        },
        MonotonicTime::from_nanos(2),
    );
    assert_eq!(engine.value(toggle), Some(MenuValue::Bool(false)));
    assert!(matches!(
        engine.apply_state(toggle),
        ApplyState::Pending { .. }
    ));

    engine
        .edit(&PrefsSchemas, |menu| {
            menu.pages[0].sections[0].items.insert(
                0,
                item(
                    "inserted",
                    ItemKind::Action {
                        confirm: None,
                        stay_open: true,
                    },
                ),
            );
        })
        .unwrap();
    let moved = engine.item_index(&ItemId("toggle".into())).unwrap();
    assert_ne!(moved, toggle);
    engine.input(
        MenuInput::Applied {
            request: newer,
            result: ApplyResult::Committed(MenuValue::Bool(false)),
        },
        MonotonicTime::from_nanos(3),
    );
    assert_eq!(engine.value(moved), Some(MenuValue::Bool(false)));
    assert_eq!(engine.apply_state(moved), ApplyState::Committed);

    let invalidated = request(engine.input(MenuInput::Activate, MonotonicTime::from_nanos(4)));
    engine
        .edit(&PrefsSchemas, |menu| {
            let ItemKind::Toggle { bind } = &mut menu.pages[0].sections[0].items[1].kind else {
                unreachable!()
            };
            *bind = Bind::Host("replacement-toggle".into());
        })
        .unwrap();
    assert_eq!(engine.value(moved), Some(MenuValue::Bool(false)));
    assert_eq!(engine.apply_state(moved), ApplyState::Idle);
    assert!(engine
        .input(
            MenuInput::Applied {
                request: invalidated,
                result: ApplyResult::Committed(MenuValue::Bool(true)),
            },
            MonotonicTime::from_nanos(5),
        )
        .is_empty());
    assert_eq!(engine.value(moved), Some(MenuValue::Bool(false)));
}

#[test]
fn edit_revalidates_confirm_and_picker_overlays() {
    let mut engine = MenuEngine::new(menu(), &PrefsSchemas, EngineOptions::default()).unwrap();
    for _ in 0..4 {
        engine.input(MenuInput::Down, MonotonicTime::ZERO);
    }
    let choice = engine.focus().unwrap();
    engine.set_value(choice, MenuValue::Choice(2)).unwrap();
    engine.input(MenuInput::Activate, MonotonicTime::ZERO);
    assert_eq!(
        engine.overlay(),
        Some(Overlay::Picker {
            item: choice,
            cursor: 2
        })
    );

    engine
        .edit(&PrefsSchemas, |menu| {
            let ItemKind::Choice { options, .. } = &mut menu.pages[0].sections[0].items[4].kind
            else {
                unreachable!()
            };
            options.truncate(1);
        })
        .unwrap();
    let choice = engine.item_index(&ItemId("choice".into())).unwrap();
    assert_eq!(
        engine.overlay(),
        Some(Overlay::Picker {
            item: choice,
            cursor: 0
        })
    );

    engine
        .edit(&PrefsSchemas, |menu| {
            menu.pages[0].sections[0].items[4].kind = ItemKind::Action {
                confirm: None,
                stay_open: true,
            };
        })
        .unwrap();
    assert_eq!(engine.overlay(), None);

    let confirm = engine.item_index(&ItemId("confirm".into())).unwrap();
    while engine.focus() != Some(confirm) {
        engine.input(MenuInput::Up, MonotonicTime::ZERO);
    }
    engine.input(MenuInput::Activate, MonotonicTime::ZERO);
    assert!(matches!(engine.overlay(), Some(Overlay::Confirm { .. })));
    engine
        .edit(&PrefsSchemas, |menu| {
            menu.pages[0].sections[0].items[2].kind = ItemKind::Action {
                confirm: None,
                stay_open: true,
            };
        })
        .unwrap();
    assert_eq!(engine.overlay(), None);
}

#[test]
fn confirm_starts_on_cancel_and_disabled_rows_are_focusable_but_inert() {
    let mut engine = MenuEngine::new(menu(), &PrefsSchemas, EngineOptions::default()).unwrap();
    engine.input(MenuInput::Down, MonotonicTime::ZERO);
    assert_eq!(engine.item(engine.focus().unwrap()).id.0, "disabled");
    assert_eq!(
        engine.input(MenuInput::Activate, MonotonicTime::ZERO),
        &[MenuEffect::Feedback(Feedback::Error), MenuEffect::Redraw]
    );

    engine.input(MenuInput::Down, MonotonicTime::ZERO);
    engine.input(MenuInput::Activate, MonotonicTime::ZERO);
    let confirm = engine.focus().unwrap();
    assert_eq!(
        engine.overlay(),
        Some(Overlay::Confirm {
            item: confirm,
            confirm_focused: false,
        })
    );
    let cancelled = engine.input(MenuInput::Activate, MonotonicTime::ZERO);
    assert!(!cancelled
        .iter()
        .any(|effect| matches!(effect, MenuEffect::Invoke { .. })));

    engine.input(MenuInput::Activate, MonotonicTime::ZERO);
    engine.input(MenuInput::Down, MonotonicTime::ZERO);
    let confirmed = engine.input(MenuInput::Activate, MonotonicTime::ZERO);
    assert!(confirmed.contains(&MenuEffect::Invoke { item: confirm }));
}

#[test]
fn start_invokes_the_visible_primary_action() {
    let mut engine = MenuEngine::new(menu(), &PrefsSchemas, EngineOptions::default()).unwrap();
    let primary = engine.item_index(&ItemId("primary".into())).unwrap();
    assert!(engine
        .input(MenuInput::Primary, MonotonicTime::ZERO)
        .contains(&MenuEffect::Invoke { item: primary }));
}
