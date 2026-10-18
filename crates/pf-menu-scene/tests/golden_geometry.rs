use pf_menu::{
    ApplyResult, Bind, ChoiceOption, Cond, Confirm, DateTimeFields, EngineOptions, Item, ItemId,
    ItemKind, Menu, MenuEffect, MenuEngine, MenuInput, MenuValue, Msg, Page, PageId, PrefsSchemas,
    Present, Section, TextPurpose, FORMAT_V1,
};
use pf_menu_scene::{lower, PanelFacts, RenderPreferences, ResolvedMsg, TextScale};
use pf_ports::MonotonicTime;
use pf_scene::{
    resolve_layout, Bounds, Insets, LayoutCache, Node, Orientation, Role, SurfaceMetrics,
    TextAlign, TextMeasure, TypeRole,
};
use serde::Serialize;
use std::path::PathBuf;

fn item(id: &str, kind: ItemKind) -> Item {
    Item::new(id, Msg::text(id), kind)
}

fn menu() -> Menu {
    let bind = |id: &str| Bind::Host(id.to_string());
    let mut text = item(
        "text",
        ItemKind::Text {
            bind: bind("text"),
            purpose: TextPurpose::Name,
            max_chars: 20,
            secret: false,
        },
    );
    text.subtitle = Some(Msg::text("A two-line explanatory subtitle"));
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
        id: "golden".into(),
        root: PageId("root".into()),
        pages: vec![
            Page {
                id: PageId("root".into()),
                title: Msg::text("All menu controls"),
                tabs: vec![
                    PageId("root".into()),
                    PageId("other".into()),
                    PageId("last".into()),
                ],
                primary: None,
                sections: vec![Section {
                    header: Some(Msg::text("Kinds and states")),
                    footer: Some(Msg::text("Controller-friendly menu semantics")),
                    items: vec![
                        item(
                            "action",
                            ItemKind::Action {
                                confirm: Some(Confirm {
                                    title: Msg::text("Confirm action?"),
                                    body: Some(Msg::text("This cannot be undone.")),
                                    confirm_label: Msg::text("Continue"),
                                    destructive: true,
                                }),
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
                                options: ["One", "Two"]
                                    .into_iter()
                                    .map(|value| ChoiceOption {
                                        value: value.to_lowercase(),
                                        label: Msg::text(value),
                                        detail: None,
                                    })
                                    .collect(),
                                present: Present::Picker,
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
                    ],
                }],
            },
            Page {
                id: PageId("other".into()),
                title: Msg::text("More controls"),
                tabs: vec![
                    PageId("root".into()),
                    PageId("other".into()),
                    PageId("last".into()),
                ],
                primary: None,
                sections: vec![Section {
                    header: Some(Msg::text("Kinds and states, continued")),
                    footer: None,
                    items: vec![
                        item(
                            "slider",
                            ItemKind::Slider {
                                bind: bind("slider"),
                                min: 0,
                                max: 100,
                                step: 5,
                            },
                        ),
                        text,
                        item(
                            "link",
                            ItemKind::Link {
                                page: PageId("root".into()),
                            },
                        ),
                        item(
                            "progress",
                            ItemKind::Progress {
                                bind: bind("progress"),
                                indeterminate: false,
                            },
                        ),
                    ],
                }],
            },
            Page {
                id: PageId("last".into()),
                title: Msg::text("Remaining controls"),
                tabs: vec![
                    PageId("root".into()),
                    PageId("other".into()),
                    PageId("last".into()),
                ],
                primary: None,
                sections: vec![Section {
                    header: Some(Msg::text("Kinds and states, final")),
                    footer: None,
                    items: vec![
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
        ],
    }
}

fn reflow_engine() -> MenuEngine {
    let bind = |id: &str| Bind::Host(id.to_string());
    let mut choice = item(
        "language",
        ItemKind::Choice {
            bind: bind("language"),
            options: vec![ChoiceOption {
                value: "automatic".into(),
                label: Msg::text("Automatic regional language selection"),
                detail: None,
            }],
            present: Present::Picker,
        },
    );
    choice.label = Msg::text("Interface and spoken language");
    choice.subtitle = Some(Msg::text("Applies after the next system restart"));
    let menu = Menu {
        format: FORMAT_V1.into(),
        id: "reflow".into(),
        root: PageId("root".into()),
        pages: vec![Page {
            id: PageId("root".into()),
            title: Msg::text("Accessibility and language"),
            tabs: Vec::new(),
            primary: None,
            sections: vec![Section {
                header: Some(Msg::text("Readable at every supported scale")),
                footer: None,
                items: vec![
                    choice,
                    item(
                        "screen_reader",
                        ItemKind::Toggle {
                            bind: bind("reader"),
                        },
                    ),
                    item(
                        "apply",
                        ItemKind::Action {
                            confirm: None,
                            stay_open: true,
                        },
                    ),
                ],
            }],
        }],
    };
    let mut engine = MenuEngine::new(menu, &PrefsSchemas, EngineOptions::default()).unwrap();
    let choice = engine.item_index(&ItemId("language".into())).unwrap();
    engine.set_value(choice, MenuValue::Choice(0)).unwrap();
    engine
}

fn engine() -> MenuEngine {
    let mut engine = MenuEngine::new(menu(), &PrefsSchemas, EngineOptions::default()).unwrap();
    for (id, value) in [
        ("toggle", MenuValue::Bool(true)),
        ("choice", MenuValue::Choice(1)),
        ("stepper", MenuValue::Int(4)),
        ("slider", MenuValue::Int(65)),
        ("progress", MenuValue::Int(40)),
    ] {
        let index = engine.item_index(&ItemId(id.into())).unwrap();
        engine.set_value(index, value).unwrap();
    }
    engine
}

fn metrics(width: f32, height: f32) -> SurfaceMetrics {
    SurfaceMetrics {
        logical_width: width,
        logical_height: height,
        scale: 1.0,
        safe_insets: Insets::default(),
        orientation: Orientation::Landscape,
    }
}

struct DeterministicMeasure;

impl TextMeasure for DeterministicMeasure {
    fn measure(
        &self,
        text: &str,
        role: TypeRole,
        _align: TextAlign,
        scale: f32,
        max_width: Option<f32>,
        line_height: Option<f32>,
    ) -> (f32, f32) {
        let font = match role {
            TypeRole::Hero => 40.0,
            TypeRole::Title => 28.0,
            TypeRole::H1 => 32.0,
            TypeRole::Body | TypeRole::Label => 20.0,
            TypeRole::Caption => 16.0,
            TypeRole::Eyebrow => 14.0,
            TypeRole::Plate => 20.0,
        } * scale;
        let natural = text.chars().count() as f32 * font * 0.52;
        let width = max_width.map_or(natural, |limit| natural.min(limit));
        let lines = max_width
            .filter(|limit| *limit > 0.0)
            .map_or(1.0, |limit| (natural / limit).ceil().max(1.0));
        (width, lines * font * line_height.unwrap_or(1.35))
    }
}

#[derive(Serialize)]
struct Geometry {
    name: String,
    focused: Option<String>,
    nodes: Vec<NodeGeometry>,
}

#[derive(Serialize)]
struct NodeGeometry {
    id: String,
    role: String,
    bounds: [i32; 4],
    radius: i32,
    state: String,
    value: Option<String>,
}

fn geometry(
    name: &str,
    engine: &MenuEngine,
    surface: SurfaceMetrics,
    prefs: RenderPreferences,
) -> Geometry {
    let panel = if surface.logical_width == 1024.0 {
        PanelFacts::diagonal(3.2)
    } else {
        PanelFacts::diagonal(5.0)
    };
    let lowered = lower(engine, &ResolvedMsg, panel, surface, prefs).unwrap();
    let mut root = lowered.scene.root().clone();
    resolve_layout(
        &mut root,
        surface,
        prefs.text_scale.factor(),
        &DeterministicMeasure,
        &mut LayoutCache::default(),
    );
    assert_inside_surface(&root, surface, prefs, name);
    assert_rows_do_not_overlap(&root, name);
    let mut nodes = Vec::new();
    collect(&root, &mut nodes);
    Geometry {
        name: name.to_string(),
        focused: lowered.scene.focused().map(|id| id.as_str().to_string()),
        nodes,
    }
}

fn assert_inside_surface(
    node: &Node,
    surface: SurfaceMetrics,
    prefs: RenderPreferences,
    name: &str,
) {
    let bounds = node.bounds;
    assert!(
        bounds.x >= -0.5
            && bounds.y >= -0.5
            && bounds.width >= 0.0
            && bounds.height >= 0.0
            && bounds.x + bounds.width <= surface.logical_width + 0.5
            && bounds.y + bounds.height <= surface.logical_height + 0.5,
        "{name}: {} escaped the surface: {:?}",
        node.id.as_str(),
        bounds,
    );
    if node.role == Role::Text && !node.accessible_label.is_empty() {
        let scale = if node.fixed_paint_scale {
            1.0
        } else {
            prefs.text_scale.factor()
        };
        let (_, required_height) = DeterministicMeasure.measure(
            &node.accessible_label,
            node.type_role,
            node.text_align,
            scale,
            Some(bounds.width),
            node.line_height,
        );
        assert!(
            required_height <= bounds.height + 0.5,
            "{name}: {} clips text (needs {required_height}, has {})",
            node.id.as_str(),
            bounds.height,
        );
    }
    for child in &node.children {
        assert_inside_surface(child, surface, prefs, name);
    }
}

fn assert_rows_do_not_overlap(root: &Node, name: &str) {
    fn rows<'a>(node: &'a Node, found: &mut Vec<&'a Node>) {
        let id = node.id.as_str();
        if id
            .strip_prefix("menu/item/")
            .is_some_and(|rest| !rest.contains('/'))
        {
            found.push(node);
        }
        for child in &node.children {
            rows(child, found);
        }
    }
    let mut found = Vec::new();
    rows(root, &mut found);
    found.sort_by(|left, right| left.bounds.y.total_cmp(&right.bounds.y));
    for pair in found.windows(2) {
        assert!(
            pair[0].bounds.y + pair[0].bounds.height <= pair[1].bounds.y + 0.5,
            "{name}: rows {} and {} overlap",
            pair[0].id.as_str(),
            pair[1].id.as_str(),
        );
    }
}

fn collect(node: &Node, out: &mut Vec<NodeGeometry>) {
    let state = &node.state;
    out.push(NodeGeometry {
        id: node.id.as_str().to_string(),
        role: format!("{:?}", node.role),
        bounds: rounded(node.bounds),
        radius: node.corner_radius.round() as i32,
        state: format!(
            "f{} p{} d{} s{} u{} x{} c{} e{} busy{} scrim{}",
            u8::from(state.focused),
            u8::from(state.pressed),
            u8::from(state.disabled),
            u8::from(state.selected),
            u8::from(state.unavailable),
            u8::from(state.destructive),
            u8::from(state.checked),
            u8::from(state.expanded),
            u8::from(state.pending),
            u8::from(state.scrimmed),
        ),
        value: state.value_text.clone(),
    });
    for child in &node.children {
        collect(child, out);
    }
}

fn rounded(bounds: Bounds) -> [i32; 4] {
    [
        bounds.x.round() as i32,
        bounds.y.round() as i32,
        bounds.width.round() as i32,
        bounds.height.round() as i32,
    ]
}

fn suite(surface: SurfaceMetrics) -> Vec<Geometry> {
    let base = engine();
    let mut other = engine();
    other.input(MenuInput::TabNext, MonotonicTime::ZERO);
    let mut last = engine();
    last.input(MenuInput::TabNext, MonotonicTime::ZERO);
    last.input(MenuInput::TabNext, MonotonicTime::ZERO);
    let mut pending = engine();
    pending.input(MenuInput::Down, MonotonicTime::ZERO);
    let effects = pending.input(MenuInput::Activate, MonotonicTime::ZERO);
    let request = effects
        .iter()
        .find_map(|effect| match effect {
            MenuEffect::SetValue { request, .. } => Some(*request),
            _ => None,
        })
        .unwrap();
    let mut unavailable = engine();
    unavailable.input(MenuInput::Down, MonotonicTime::ZERO);
    let effects = unavailable.input(MenuInput::Activate, MonotonicTime::ZERO);
    let rejected = effects
        .iter()
        .find_map(|effect| match effect {
            MenuEffect::SetValue { request, .. } => Some(*request),
            _ => None,
        })
        .unwrap();
    unavailable.input(
        MenuInput::Applied {
            request: rejected,
            result: ApplyResult::Rejected,
        },
        MonotonicTime::from_nanos(1),
    );
    let mut confirm = engine();
    confirm.input(MenuInput::Activate, MonotonicTime::ZERO);
    let mut picker = engine();
    picker.input(MenuInput::Down, MonotonicTime::ZERO);
    picker.input(MenuInput::Down, MonotonicTime::ZERO);
    picker.input(MenuInput::Activate, MonotonicTime::ZERO);
    assert_ne!(request.0, u64::MAX);
    vec![
        geometry("base-root", &base, surface, RenderPreferences::default()),
        geometry("base-other", &other, surface, RenderPreferences::default()),
        geometry("base-last", &last, surface, RenderPreferences::default()),
        geometry("pending", &pending, surface, RenderPreferences::default()),
        geometry(
            "unavailable",
            &unavailable,
            surface,
            RenderPreferences::default(),
        ),
        geometry("confirm", &confirm, surface, RenderPreferences::default()),
        geometry("picker", &picker, surface, RenderPreferences::default()),
    ]
}

fn assert_golden(name: &str, value: &impl Serialize) {
    let actual = format!("{}\n", serde_json::to_string_pretty(value).unwrap());
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("goldens")
        .join(name);
    if std::env::var_os("UPDATE_GOLDENS").is_some() {
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "read {}: {error}; run with UPDATE_GOLDENS=1",
            path.display()
        )
    });
    assert_eq!(actual, expected, "golden {} changed", path.display());
}

#[test]
fn every_kind_and_state_matches_both_surface_goldens() {
    assert_golden("kinds-states-1280x720.json", &suite(metrics(1280.0, 720.0)));
    assert_golden("kinds-states-1024x768.json", &suite(metrics(1024.0, 768.0)));
}

#[test]
fn accessibility_reflow_matches_200_percent_and_pseudo_locale_goldens() {
    let engine = reflow_engine();
    assert_golden(
        "reflow-200-percent.json",
        &geometry(
            "text-200",
            &engine,
            metrics(1280.0, 720.0),
            RenderPreferences {
                text_scale: TextScale::Percent200,
                ..RenderPreferences::default()
            },
        ),
    );
    assert_golden(
        "reflow-pseudo-30.json",
        &geometry(
            "pseudo-30",
            &engine,
            metrics(1280.0, 720.0),
            RenderPreferences {
                pseudo_locale: true,
                ..RenderPreferences::default()
            },
        ),
    );
}
