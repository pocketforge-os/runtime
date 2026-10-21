use pf_render::Rasterizer;
use pf_scene::{Node, NodeId, Role};
use pf_volume_keys::toast::{
    frame_metrics, scene, ToastModel, ToastState, TOAST_HEIGHT, TOAST_WIDTH,
};
use pf_volumed::Output;
use std::time::Duration;

fn level(percent: u8, muted: bool) -> ToastModel {
    ToastModel::Level {
        percent,
        output: Output::Speaker,
        muted,
    }
}

fn find<'a>(node: &'a Node, id: &str) -> Option<&'a Node> {
    if node.id.as_str() == id {
        return Some(node);
    }
    node.children.iter().find_map(|child| find(child, id))
}

fn text_of(model: &ToastModel) -> String {
    let scene = scene(model).unwrap();
    find(scene.root(), "volume-toast/label")
        .unwrap()
        .accessible_label
        .clone()
}

#[test]
fn every_state_has_a_word_not_only_a_symbol() {
    assert_eq!(text_of(&level(60, false)), "Speaker 60");
    assert_eq!(
        text_of(&ToastModel::Level {
            percent: 40,
            output: Output::Headphones,
            muted: false
        }),
        "Headphones 40"
    );
    assert_eq!(text_of(&level(60, true)), "Muted");
    assert_eq!(text_of(&level(0, false)), "Speaker 0");
    assert!(text_of(&ToastModel::Error {
        message: "write refused".into()
    })
    .starts_with("Volume unavailable"));
}

#[test]
fn bar_fill_width_tracks_the_percentage_and_is_empty_when_muted() {
    let width = |model: &ToastModel| {
        find(scene(model).unwrap().root(), "volume-toast/fill")
            .unwrap()
            .bounds
            .width
    };
    assert_eq!(width(&level(0, false)), 0.0);
    assert!((width(&level(50, false)) - width(&level(100, false)) / 2.0).abs() < 0.5);
    assert_eq!(
        width(&level(80, true)),
        0.0,
        "a muted toast shows an empty bar"
    );
    assert!(matches!(
        find(
            scene(&level(10, false)).unwrap().root(),
            "volume-toast/fill"
        )
        .unwrap()
        .role,
        Role::Group
    ));
    let _ = NodeId::new("x");
}

#[test]
fn rasterized_frames_differ_by_level_and_by_state_headlessly() {
    let mut raster = Rasterizer::new();
    let mut frame = |model: ToastModel| {
        raster
            .render(&scene(&model).unwrap(), frame_metrics())
            .unwrap()
    };
    let (low, high, muted, err) = (
        frame(level(10, false)),
        frame(level(90, false)),
        frame(level(90, true)),
        frame(ToastModel::Error {
            message: "x".into(),
        }),
    );
    assert_eq!(
        (low.width, low.height),
        (TOAST_WIDTH as u32, TOAST_HEIGHT as u32)
    );
    assert_ne!(low.rgba, high.rgba);
    assert_ne!(high.rgba, muted.rgba);
    assert_ne!(muted.rgba, err.rgba);
    let again = frame(level(10, false));
    assert_eq!(low.rgba, again.rgba, "rendering is deterministic");
}

#[test]
fn the_toast_lives_1500_ms_after_the_last_change_and_a_new_one_replaces_it_in_place() {
    let mut state = ToastState::default();
    assert!(state.visible(Duration::ZERO).is_none());
    state.show(level(60, false), Duration::from_millis(100));
    assert!(state.visible(Duration::from_millis(1599)).is_some());
    assert!(state.visible(Duration::from_millis(1600)).is_none());
    state.show(level(65, false), Duration::from_millis(1000));
    assert_eq!(
        state.visible(Duration::from_millis(2499)),
        Some(&level(65, false))
    );
    assert!(state.visible(Duration::from_millis(2500)).is_none());
}
