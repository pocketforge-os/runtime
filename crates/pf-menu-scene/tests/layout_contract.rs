use pf_menu::{
    Bind, EngineOptions, Item, ItemKind, Menu, MenuEngine, MenuInput, Msg, Page, PageId,
    PrefsSchemas, Section, FORMAT_V1,
};
use pf_menu_scene::{
    classify_size, layout_mode, lower, LayoutMode, PanelFacts, RenderPreferences, ResolvedMsg,
    SizeClass, TextScale, Tokens,
};
use pf_ports::MonotonicTime;
use pf_scene::{Insets, Node, Orientation, SurfaceMetrics};

fn metrics(width: f32, height: f32) -> SurfaceMetrics {
    SurfaceMetrics {
        logical_width: width,
        logical_height: height,
        scale: 1.0,
        safe_insets: Insets::default(),
        orientation: Orientation::Landscape,
    }
}

#[test]
fn size_class_comes_from_panel_facts_and_fails_closed() {
    assert_eq!(
        classify_size(PanelFacts::diagonal(3.2)).unwrap(),
        SizeClass::Compact
    );
    assert_eq!(
        classify_size(PanelFacts::active_mm(110.7, 62.3)).unwrap(),
        SizeClass::Regular
    );
    assert_eq!(
        classify_size(PanelFacts::diagonal(5.5)).unwrap(),
        SizeClass::Large
    );

    for invalid in [
        PanelFacts::default(),
        PanelFacts::diagonal(0.0),
        PanelFacts::diagonal(f32::NAN),
        PanelFacts::active_mm(0.0, 48.8),
    ] {
        assert!(classify_size(invalid).is_err(), "accepted {invalid:?}");
    }
}

#[test]
fn compact_four_by_three_uses_adjudicated_scale_and_target_floor() {
    let tokens = Tokens::for_surface(SizeClass::Compact, metrics(1024.0, 768.0));
    assert_eq!(tokens.reference_scale, 1.2);
    assert_eq!(tokens.target, 58.0);
    assert_eq!(tokens.row, 67.0);
    assert_eq!(tokens.safe_margin, 29.0);
    assert!(tokens.target >= 48.0);
    assert!((tokens.row / tokens.reference_scale - 56.0).abs() < 0.5);

    let reference = Tokens::for_surface(SizeClass::Regular, metrics(1280.0, 720.0));
    assert_eq!(reference.reference_scale, 1.0);
    assert_eq!(reference.target, 48.0);
    assert_eq!(reference.row, 56.0);
}

#[test]
fn four_by_three_compact_and_large_text_use_tab_strip() {
    assert_eq!(
        layout_mode(
            SizeClass::Compact,
            metrics(1024.0, 768.0),
            TextScale::Percent100
        ),
        LayoutMode::TabStrip
    );
    assert_eq!(
        layout_mode(
            SizeClass::Regular,
            metrics(1280.0, 720.0),
            TextScale::Percent150
        ),
        LayoutMode::TabStrip
    );
    assert_eq!(
        layout_mode(
            SizeClass::Regular,
            metrics(1280.0, 720.0),
            TextScale::Percent125
        ),
        LayoutMode::WideRail
    );
}

fn system_menu(ids: &[&str]) -> MenuEngine {
    let items = ids
        .iter()
        .map(|id| {
            let kind = if matches!(*id, "wifi" | "bt" | "airplane") {
                ItemKind::Toggle {
                    bind: Bind::Host((*id).to_string()),
                }
            } else {
                ItemKind::Action {
                    confirm: None,
                    stay_open: true,
                }
            };
            Item::new(id, Msg::text(id), kind)
        })
        .collect();
    MenuEngine::new(
        Menu {
            format: FORMAT_V1.into(),
            id: "system".into(),
            root: PageId("system".into()),
            pages: vec![Page {
                id: PageId("system".into()),
                title: Msg::text("System"),
                tabs: vec![],
                primary: None,
                sections: vec![Section {
                    header: None,
                    footer: None,
                    items,
                }],
            }],
        },
        &PrefsSchemas,
        EngineOptions::default(),
    )
    .unwrap()
}

fn has_node(node: &Node, id: &str) -> bool {
    node.id.as_str() == id || node.children.iter().any(|child| has_node(child, id))
}

#[test]
fn connections_fold_is_computed_from_content_band_on_every_size_class() {
    let mut overflowing = system_menu(&[
        "one", "two", "three", "four", "five", "six", "wifi", "bt", "airplane", "ten", "eleven",
        "twelve",
    ]);
    let regular = lower(
        &overflowing,
        &ResolvedMsg,
        PanelFacts::diagonal(5.0),
        metrics(1280.0, 720.0),
        RenderPreferences::default(),
    )
    .unwrap();
    assert!(regular.metadata.connections_folded);
    assert!(has_node(regular.scene.root(), "menu/connections"));
    assert!(!has_node(regular.scene.root(), "menu/item/wifi"));

    for _ in 0..6 {
        overflowing.input(MenuInput::Down, MonotonicTime::ZERO);
    }
    let expanded = lower(
        &overflowing,
        &ResolvedMsg,
        PanelFacts::diagonal(5.0),
        metrics(1280.0, 720.0),
        RenderPreferences::default(),
    )
    .unwrap();
    assert!(has_node(expanded.scene.root(), "menu/item/wifi"));
    assert_eq!(expanded.scene.focused().unwrap().as_str(), "menu/item/wifi");

    let fitting = system_menu(&["one", "wifi", "bt", "airplane", "five"]);
    let compact = lower(
        &fitting,
        &ResolvedMsg,
        PanelFacts::diagonal(3.2),
        metrics(1024.0, 768.0),
        RenderPreferences::default(),
    )
    .unwrap();
    assert!(!compact.metadata.connections_folded);
    assert!(has_node(compact.scene.root(), "menu/item/wifi"));
}
