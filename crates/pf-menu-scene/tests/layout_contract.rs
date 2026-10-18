use pf_menu_scene::{
    classify_size, layout_mode, LayoutMode, PanelFacts, SizeClass, TextScale, Tokens,
};
use pf_scene::{Insets, Orientation, SurfaceMetrics};

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
