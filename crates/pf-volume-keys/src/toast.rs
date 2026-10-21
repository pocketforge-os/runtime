//! The system volume toast (every-menu-spec 11.5): glyph + bar + a word, drawn into a buffer.

use pf_scene::{
    Bounds, Insets, Node, NodeId, Orientation, Role, Scene, SceneError, SurfaceMetrics,
};
use pf_volumed::Output;
use std::time::Duration;

pub const TOAST_WIDTH: f32 = 320.0;
pub const TOAST_HEIGHT: f32 = 64.0;
/// The toast stays 1.5 s after the last change.
pub const TOAST_LIFETIME: Duration = Duration::from_millis(1500);
const BAR_X: f32 = 16.0;
const BAR_Y: f32 = 44.0;
const BAR_WIDTH: f32 = TOAST_WIDTH - 2.0 * BAR_X;
const BAR_HEIGHT: f32 = 8.0;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToastModel {
    Level {
        percent: u8,
        output: Output,
        muted: bool,
    },
    Error {
        message: String,
    },
}

pub fn frame_metrics() -> SurfaceMetrics {
    SurfaceMetrics {
        logical_width: TOAST_WIDTH,
        logical_height: TOAST_HEIGHT,
        scale: 1.0,
        safe_insets: Insets::default(),
        orientation: Orientation::Landscape,
    }
}

fn label(model: &ToastModel) -> String {
    match model {
        ToastModel::Level { muted: true, .. } => String::from("Muted"),
        ToastModel::Level {
            percent,
            output: Output::Speaker,
            ..
        } => format!("Speaker {percent}"),
        ToastModel::Level {
            percent,
            output: Output::Headphones,
            ..
        } => format!("Headphones {percent}"),
        ToastModel::Error { message } => format!("Volume unavailable: {message}"),
    }
}

/// Build the toast scene. Every state carries a word; the bar is empty when muted or in error.
pub fn scene(model: &ToastModel) -> Result<Scene, SceneError> {
    let (fill_width, fill_token) = match model {
        ToastModel::Level { muted: true, .. } => (0.0, "--color-status-attention"),
        ToastModel::Level { percent, .. } => (
            BAR_WIDTH * f32::from(*percent) / 100.0,
            "--color-status-ready",
        ),
        ToastModel::Error { .. } => (0.0, "--color-status-stopped"),
    };
    let id = |name: &str| NodeId::new(format!("volume-toast{name}")).expect("static id");
    let text = Node::new(
        id("/label"),
        Role::Text,
        label(model),
        Bounds::new(16.0, 10.0, TOAST_WIDTH - 32.0, 28.0),
        "--color-transparent",
    );
    let track = Node::new(
        id("/track"),
        Role::Group,
        "",
        Bounds::new(BAR_X, BAR_Y, BAR_WIDTH, BAR_HEIGHT),
        "--color-surface-sunken",
    )
    .with_corner_radius(BAR_HEIGHT / 2.0);
    let fill = Node::new(
        id("/fill"),
        Role::Group,
        "",
        Bounds::new(BAR_X, BAR_Y, fill_width, BAR_HEIGHT),
        fill_token,
    )
    .with_corner_radius(BAR_HEIGHT / 2.0);
    let root = Node::new(
        id(""),
        Role::Group,
        "Volume",
        Bounds::new(0.0, 0.0, TOAST_WIDTH, TOAST_HEIGHT),
        "--color-surface-overlay",
    )
    .with_corner_radius(12.0)
    .with_children(vec![text, track, fill]);
    Scene::new(root, id(""))
}

/// Visibility window: the newest toast replaces the old one in place and expires 1.5 s later.
#[derive(Default)]
pub struct ToastState {
    current: Option<(ToastModel, Duration)>,
}

impl ToastState {
    pub fn show(&mut self, model: ToastModel, now: Duration) {
        self.current = Some((model, now + TOAST_LIFETIME));
    }

    pub fn visible(&self, now: Duration) -> Option<&ToastModel> {
        self.current
            .as_ref()
            .filter(|(_, until)| now < *until)
            .map(|(model, _)| model)
    }
}
