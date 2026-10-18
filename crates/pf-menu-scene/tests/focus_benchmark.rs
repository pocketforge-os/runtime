use pf_menu::{
    Cond, EngineOptions, Item, ItemKind, Menu, MenuEngine, MenuInput, Msg, Page, PageId,
    PrefsSchemas, Section, FORMAT_V1,
};
use pf_menu_scene::{MenuSceneCache, PanelFacts, RenderPreferences, ResolvedMsg, UpdateKind};
use pf_ports::MonotonicTime;
use pf_render::Rasterizer;
use pf_scene::{resolve_layout, Insets, LayoutCache, NodeId, Orientation, Scene, SurfaceMetrics};
use std::time::Instant;

fn engine() -> MenuEngine {
    let items = (0..6)
        .map(|index| {
            let id = format!("row{index}");
            let label = format!("Menu row {index}");
            let mut item = Item::new(
                &id,
                Msg::text(&label),
                ItemKind::Action {
                    confirm: None,
                    stay_open: true,
                },
            );
            item.visible = Cond::Always;
            item
        })
        .collect();
    MenuEngine::new(
        Menu {
            format: FORMAT_V1.into(),
            id: "focus-benchmark".into(),
            root: PageId("root".into()),
            pages: vec![Page {
                id: PageId("root".into()),
                title: Msg::text("Focus benchmark"),
                tabs: Vec::new(),
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

fn metrics() -> SurfaceMetrics {
    SurfaceMetrics {
        logical_width: 1280.0,
        logical_height: 720.0,
        scale: 1.0,
        safe_insets: Insets::default(),
        orientation: Orientation::Landscape,
    }
}

fn laid_out_scene(
    cache: &MenuSceneCache,
    layout: &mut LayoutCache,
    rasterizer: &Rasterizer,
) -> Scene {
    let lowered = cache.lowered().unwrap();
    let mut root = lowered.scene.root().clone();
    resolve_layout(&mut root, metrics(), 1.0, rasterizer, layout);
    Scene::new_with_external_focus(
        root,
        NodeId::new("menu/item/row0").unwrap(),
        lowered.scene.focused().cloned(),
    )
    .unwrap()
}

/// Laptop-only, hermetic offscreen evidence. It never creates a window or injects input.
#[test]
#[ignore = "run explicitly for laptop focus-move evidence"]
fn focus_move_offscreen_benchmark() {
    const ITERATIONS: usize = 240;
    let mut engine = engine();
    let mut scenes = MenuSceneCache::default();
    let mut layouts = LayoutCache::default();
    let mut rasterizer = Rasterizer::new();
    assert_eq!(
        scenes
            .update(
                &engine,
                &ResolvedMsg,
                0,
                PanelFacts::diagonal(5.0),
                metrics(),
                RenderPreferences::default(),
            )
            .unwrap(),
        UpdateKind::Rebuilt
    );
    let initial = laid_out_scene(&scenes, &mut layouts, &rasterizer);
    rasterizer.render(&initial, metrics()).unwrap();

    let mut elapsed = Vec::with_capacity(ITERATIONS);
    let mut max_damage_area = 0_u64;
    for iteration in 0..ITERATIONS {
        let started = Instant::now();
        engine.input(
            MenuInput::Down,
            MonotonicTime::from_nanos(iteration as u64 + 1),
        );
        assert_eq!(
            scenes
                .update(
                    &engine,
                    &ResolvedMsg,
                    0,
                    PanelFacts::diagonal(5.0),
                    metrics(),
                    RenderPreferences::default(),
                )
                .unwrap(),
            UpdateKind::FocusOnly
        );
        let scene = laid_out_scene(&scenes, &mut layouts, &rasterizer);
        let frame = rasterizer.render(&scene, metrics()).unwrap();
        elapsed.push(started.elapsed().as_nanos() as u64);
        let damage = frame
            .damage
            .expect("a focus move damages old and new rings");
        let area = u64::from(damage.width) * u64::from(damage.height);
        max_damage_area = max_damage_area.max(area);
        assert!(
            damage.width < frame.width || damage.height < frame.height,
            "focus-only update damaged the full surface: {damage:?}"
        );
    }

    elapsed.sort_unstable();
    let percentile = |percent: usize| elapsed[(elapsed.len() * percent).div_ceil(100) - 1];
    println!(
        "MENUSCENE_BENCH iterations={ITERATIONS} p50_us={} p95_us={} max_us={} max_damage_px={max_damage_area}",
        percentile(50) / 1_000,
        percentile(95) / 1_000,
        elapsed[elapsed.len() - 1] / 1_000,
    );
}
