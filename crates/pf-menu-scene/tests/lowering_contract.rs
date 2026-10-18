use pf_menu::{
    Bind, ChoiceOption, Cond, DateTimeFields, EngineOptions, Item, ItemId, ItemKind, Menu,
    MenuEngine, MenuValue, Msg, Page, PageId, PrefsSchemas, Present, Section, TextPurpose,
    FORMAT_V1,
};
use pf_menu_scene::{lower, PanelFacts, RenderPreferences, ResolvedMsg, SizeClass, ThemeMode};
use pf_scene::{Insets, Node, Orientation, Role, SurfaceMetrics};

fn item(id: &str, kind: ItemKind) -> Item {
    Item::new(id, Msg::text(id), kind)
}

fn all_kinds_menu() -> Menu {
    let bind = |id: &str| Bind::Host(id.to_string());
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
        id: "lowering".into(),
        root: PageId("root".into()),
        pages: vec![
            Page {
                id: PageId("root".into()),
                title: Msg::text("Root"),
                tabs: vec![PageId("root".into()), PageId("other".into())],
                primary: None,
                sections: vec![Section {
                    header: Some(Msg::text("Kinds")),
                    footer: Some(Msg::text("End")),
                    items: vec![
                        item(
                            "action",
                            ItemKind::Action {
                                confirm: None,
                                stay_open: true,
                            },
                        ),
                        item(
                            "toggle",
                            ItemKind::Toggle {
                                bind: bind("toggle"),
                            },
                        ),
                        item(
                            "choice",
                            ItemKind::Choice {
                                bind: bind("choice"),
                                options: vec![ChoiceOption {
                                    value: "one".into(),
                                    label: Msg::text("One"),
                                    detail: None,
                                }],
                                present: Present::Inline,
                            },
                        ),
                        item(
                            "stepper",
                            ItemKind::Stepper {
                                bind: bind("stepper"),
                                min: 0,
                                max: 10,
                                step: 1,
                                unit: Some(Msg::text("ms")),
                            },
                        ),
                        item(
                            "info",
                            ItemKind::Info {
                                value: Some(Msg::text("Ready")),
                                bind: None,
                            },
                        ),
                        item(
                            "slider",
                            ItemKind::Slider {
                                bind: bind("slider"),
                                min: 0,
                                max: 100,
                                step: 5,
                            },
                        ),
                        item(
                            "text",
                            ItemKind::Text {
                                bind: bind("text"),
                                purpose: TextPurpose::Name,
                                max_chars: 20,
                                secret: false,
                            },
                        ),
                        item(
                            "link",
                            ItemKind::Link {
                                page: PageId("other".into()),
                            },
                        ),
                        item(
                            "progress",
                            ItemKind::Progress {
                                bind: bind("progress"),
                                indeterminate: false,
                            },
                        ),
                        item(
                            "binding",
                            ItemKind::Binding {
                                context: "game".into(),
                                action: "jump".into(),
                            },
                        ),
                        item(
                            "datetime",
                            ItemKind::DateTime {
                                bind: bind("datetime"),
                                fields: DateTimeFields::DateTime,
                            },
                        ),
                        item(
                            "custom",
                            ItemKind::Custom {
                                renderer: "preview".into(),
                            },
                        ),
                        disabled,
                    ],
                }],
            },
            Page {
                id: PageId("other".into()),
                title: Msg::text("Other"),
                tabs: vec![PageId("root".into()), PageId("other".into())],
                primary: None,
                sections: vec![Section::default()],
            },
        ],
    }
}

fn metrics() -> SurfaceMetrics {
    SurfaceMetrics {
        logical_width: 1280.0,
        logical_height: 720.0,
        scale: 1.0,
        safe_insets: Insets::default(),
        orientation: Orientation::Landscape,
    }
}

fn find<'a>(node: &'a Node, id: &str) -> &'a Node {
    if node.id.as_str() == id {
        return node;
    }
    node.children
        .iter()
        .find_map(|child| find_optional(child, id))
        .unwrap_or_else(|| panic!("missing node {id}"))
}

fn find_optional<'a>(node: &'a Node, id: &str) -> Option<&'a Node> {
    (node.id.as_str() == id).then_some(node).or_else(|| {
        node.children
            .iter()
            .find_map(|child| find_optional(child, id))
    })
}

#[test]
fn lowers_every_kind_to_stable_semantics_and_engine_owned_focus() {
    let mut engine =
        MenuEngine::new(all_kinds_menu(), &PrefsSchemas, EngineOptions::default()).unwrap();
    engine
        .set_value(
            engine.item_index(&ItemId("toggle".into())).unwrap(),
            MenuValue::Bool(true),
        )
        .unwrap();
    engine
        .set_value(
            engine.item_index(&ItemId("choice".into())).unwrap(),
            MenuValue::Choice(0),
        )
        .unwrap();

    let lowered = lower(
        &engine,
        &ResolvedMsg,
        PanelFacts::diagonal(5.0),
        metrics(),
        RenderPreferences::default(),
    )
    .unwrap();
    assert_eq!(lowered.metadata.size_class, SizeClass::Regular);
    assert_eq!(lowered.metadata.theme, ThemeMode::Dusk);
    let root = lowered.scene.root();
    for (id, role) in [
        ("action", Role::Button),
        ("toggle", Role::Toggle),
        ("choice", Role::Choice),
        ("stepper", Role::Slider),
        ("info", Role::Text),
        ("slider", Role::Slider),
        ("text", Role::TextField),
        ("link", Role::Link),
        ("progress", Role::Progress),
        ("binding", Role::Button),
        ("datetime", Role::Choice),
        ("custom", Role::Group),
    ] {
        assert_eq!(find(root, &format!("menu/item/{id}")).role, role, "{id}");
    }
    let toggle = find(root, "menu/item/toggle");
    assert_eq!(toggle.state.value_text.as_deref(), Some("On"));
    assert!(toggle.state.checked);
    assert_eq!(toggle.accessible_label, "toggle, On");
    assert_eq!(
        find(root, "menu/item/choice").state.value_text.as_deref(),
        Some("One")
    );
    assert!(find(root, "menu/item/disabled").state.disabled);
    assert_eq!(
        lowered.scene.focused().unwrap().as_str(),
        "menu/item/action"
    );
    assert_eq!(find(root, "menu/tab/root").role, Role::Tab);
    assert_eq!(find(root, "menu/section/0/separator").role, Role::Separator);
}

#[test]
fn preferences_are_typed_metadata_not_ad_hoc_style_strings() {
    let engine =
        MenuEngine::new(all_kinds_menu(), &PrefsSchemas, EngineOptions::default()).unwrap();
    let preferences = RenderPreferences {
        theme: ThemeMode::Day,
        high_contrast: true,
        reduce_motion: true,
        ..RenderPreferences::default()
    };
    let lowered = lower(
        &engine,
        &ResolvedMsg,
        PanelFacts::diagonal(5.0),
        metrics(),
        preferences,
    )
    .unwrap();
    assert_eq!(lowered.metadata.theme, ThemeMode::Day);
    assert!(lowered.metadata.high_contrast);
    assert_eq!(lowered.metadata.transition_ms, 0);
}
