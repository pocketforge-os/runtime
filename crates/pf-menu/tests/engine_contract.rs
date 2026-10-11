use pf_menu::{
    ApplyResult, ApplyState, Bind, Cond, Confirm, EngineOptions, Feedback, Item, ItemId, ItemKind,
    Menu, MenuEffect, MenuEngine, MenuInput, MenuValue, Msg, Overlay, Page, PageId, PrefsSchemas,
    RevertReason, Section, FORMAT_V1,
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
                ],
            }],
        }],
    }
}

#[test]
fn apply_lifecycle_commits_and_reverts_every_failure_mode() {
    let mut engine = MenuEngine::new(menu(), &PrefsSchemas, EngineOptions::default()).unwrap();
    let toggle = engine.item_index(&ItemId("toggle".into())).unwrap();
    engine.set_value(toggle, MenuValue::Bool(false)).unwrap();

    let effects = engine.input(MenuInput::Activate, MonotonicTime::ZERO);
    assert!(effects.contains(&MenuEffect::SetValue {
        item: toggle,
        value: MenuValue::Bool(true),
    }));
    assert!(matches!(
        engine.apply_state(toggle),
        ApplyState::Pending { .. }
    ));
    engine.input(
        MenuInput::Applied {
            item: toggle,
            result: ApplyResult::Committed(MenuValue::Bool(true)),
        },
        MonotonicTime::from_nanos(1),
    );
    assert_eq!(engine.apply_state(toggle), ApplyState::Committed);

    engine.input(MenuInput::Activate, MonotonicTime::from_nanos(2));
    engine.input(
        MenuInput::Applied {
            item: toggle,
            result: ApplyResult::Rejected,
        },
        MonotonicTime::from_nanos(3),
    );
    assert_eq!(engine.value(toggle), Some(MenuValue::Bool(true)));
    assert_eq!(
        engine.apply_state(toggle),
        ApplyState::Reverted(RevertReason::Rejected)
    );

    engine.input(MenuInput::Activate, MonotonicTime::from_nanos(4));
    engine.input(
        MenuInput::Applied {
            item: toggle,
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
