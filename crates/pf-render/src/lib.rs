//! Deterministic scene rasterization on the ruled Cosmic Text/Swash/tiny-skia stack.

use cosmic_text::{fontdb, Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, SwashCache};
use pf_scene::{Bounds, Node, Scene, SurfaceMetrics};
use tiny_skia::{Color as SkColor, Paint, Pixmap, Rect, Transform};

const MANROPE: &[u8] =
    include_bytes!("../../../spikes/render-text/fonts/manrope/Manrope[wght].ttf");
const FRAUNCES: &[u8] =
    include_bytes!("../../../spikes/render-text/fonts/fraunces/Fraunces[SOFT,WONK,opsz,wght].ttf");
const CJK: &[u8] = include_bytes!("../fonts/NotoSansCJK-Regular.ttc");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DamageRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl DamageRect {
    fn union(self, other: Self) -> Self {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let right = (self.x + self.width).max(other.x + other.width);
        let bottom = (self.y + self.height).max(other.y + other.height);
        Self {
            x,
            y,
            width: right - x,
            height: bottom - y,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RasterFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub damage: Option<DamageRect>,
}

/// Long-lived rasterizer. Cosmic Text's shaping state and Swash's glyph images are retained.
pub struct Rasterizer {
    fonts: FontSystem,
    glyphs: SwashCache,
    previous: Vec<NodeSnapshot>,
}

#[derive(Clone, PartialEq)]
struct NodeSnapshot {
    id: String,
    parent_id: Option<String>,
    sibling_index: usize,
    bounds: Bounds,
    label: String,
    focused: bool,
    disabled: bool,
    selected: bool,
}

impl Rasterizer {
    pub fn new() -> Self {
        let mut db = fontdb::Database::new();
        // Never call load_system_fonts: output must depend only on repository bytes.
        for data in [MANROPE, FRAUNCES, CJK] {
            db.load_font_data(data.to_vec());
        }
        Self {
            fonts: FontSystem::new_with_locale_and_db("en-US".into(), db),
            glyphs: SwashCache::new(),
            previous: Vec::new(),
        }
    }

    pub fn render(
        &mut self,
        scene: &Scene,
        metrics: SurfaceMetrics,
    ) -> Result<RasterFrame, RenderError> {
        let width = physical(metrics.logical_width, metrics.scale)?;
        let height = physical(metrics.logical_height, metrics.scale)?;
        let mut pixmap = Pixmap::new(width, height).ok_or(RenderError::InvalidSurface)?;
        pixmap.fill(SkColor::from_rgba8(13, 17, 23, 255));
        draw_node(
            &mut pixmap,
            &mut self.fonts,
            &mut self.glyphs,
            scene.root(),
            metrics.scale,
        );
        let mut current = Vec::new();
        collect(scene.root(), None, 0, &mut current);
        let damage = damage(&self.previous, &current, metrics.scale, width, height);
        self.previous = current;
        Ok(RasterFrame {
            width,
            height,
            rgba: pixmap.data().to_vec(),
            damage,
        })
    }
}

impl Default for Rasterizer {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderError {
    InvalidSurface,
}

fn physical(logical: f32, scale: f32) -> Result<u32, RenderError> {
    let value = logical * scale;
    if !value.is_finite() || value <= 0.0 || value > u32::MAX as f32 {
        Err(RenderError::InvalidSurface)
    } else {
        Ok(value.round() as u32)
    }
}

fn collect(
    node: &Node,
    parent_id: Option<&str>,
    sibling_index: usize,
    out: &mut Vec<NodeSnapshot>,
) {
    out.push(NodeSnapshot {
        id: node.id.as_str().into(),
        parent_id: parent_id.map(str::to_owned),
        sibling_index,
        bounds: node.bounds,
        label: node.accessible_label.clone(),
        focused: node.state.focused,
        disabled: node.state.disabled,
        selected: node.state.selected,
    });
    for (index, child) in node.children.iter().enumerate() {
        collect(child, Some(node.id.as_str()), index, out);
    }
}

fn damage(
    old: &[NodeSnapshot],
    new: &[NodeSnapshot],
    scale: f32,
    width: u32,
    height: u32,
) -> Option<DamageRect> {
    if old.is_empty() {
        return Some(DamageRect {
            x: 0,
            y: 0,
            width,
            height,
        });
    }
    let mut result = None;
    for node in old.iter().chain(new) {
        let peer = if old.iter().any(|v| std::ptr::eq(v, node)) {
            new.iter().find(|v| v.id == node.id)
        } else {
            old.iter().find(|v| v.id == node.id)
        };
        if peer != Some(node) {
            let r = bounds_rect(node.bounds, scale, width, height);
            result = Some(result.map_or(r, |d: DamageRect| d.union(r)));
        }
    }
    result
}

fn bounds_rect(b: Bounds, scale: f32, width: u32, height: u32) -> DamageRect {
    let x = (b.x * scale).floor().max(0.0) as u32;
    let y = (b.y * scale).floor().max(0.0) as u32;
    let right = ((b.x + b.width) * scale).ceil().max(0.0) as u32;
    let bottom = ((b.y + b.height) * scale).ceil().max(0.0) as u32;
    DamageRect {
        x: x.min(width),
        y: y.min(height),
        width: right.min(width).saturating_sub(x.min(width)),
        height: bottom.min(height).saturating_sub(y.min(height)),
    }
}

fn draw_node(
    pm: &mut Pixmap,
    fonts: &mut FontSystem,
    glyphs: &mut SwashCache,
    node: &Node,
    scale: f32,
) {
    let b = node.bounds;
    let color = if node.state.focused {
        (36, 65, 95)
    } else if node.state.disabled {
        (32, 36, 42)
    } else if node.state.selected {
        (44, 58, 72)
    } else {
        (26, 36, 48)
    };
    if let Some(rect) = Rect::from_xywh(b.x * scale, b.y * scale, b.width * scale, b.height * scale)
    {
        let mut paint = Paint::default();
        paint.set_color_rgba8(color.0, color.1, color.2, 255);
        pm.fill_rect(rect, &paint, Transform::identity(), None);
    }
    draw_text(
        pm,
        fonts,
        glyphs,
        &node.accessible_label,
        (b.x + 6.0) * scale,
        (b.y + 5.0) * scale,
        (b.width - 12.0).max(1.0) * scale,
        (b.height - 5.0).max(0.0) * scale,
        15.0 * scale,
        (b.x * scale, b.y * scale, b.width * scale, b.height * scale),
    );
    for child in &node.children {
        draw_node(pm, fonts, glyphs, child, scale);
    }
}

fn draw_text(
    pm: &mut Pixmap,
    fonts: &mut FontSystem,
    glyphs: &mut SwashCache,
    text: &str,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    size: f32,
    clip: (f32, f32, f32, f32),
) {
    let mut buffer = Buffer::new(fonts, Metrics::new(size, size * 1.25));
    buffer.set_size(fonts, Some(width), Some(height));
    buffer.set_text(
        fonts,
        text,
        Attrs::new().family(Family::Name("Manrope")),
        Shaping::Advanced,
    );
    buffer.shape_until_scroll(fonts, false);
    buffer.draw(
        fonts,
        glyphs,
        Color::rgb(244, 234, 220),
        |gx, gy, gw, gh, color| {
            let alpha = color.a() as u32;
            let pixmap_width = pm.width() as usize;
            for yy in 0..gh as i32 {
                for xx in 0..gw as i32 {
                    let px = gx + xx + x as i32;
                    let py = gy + yy + y as i32;
                    if px < clip.0.floor() as i32
                        || py < clip.1.floor() as i32
                        || px >= (clip.0 + clip.2).ceil() as i32
                        || py >= (clip.1 + clip.3).ceil() as i32
                        || px < 0
                        || py < 0
                        || px >= pm.width() as i32
                        || py >= pm.height() as i32
                    {
                        continue;
                    }
                    let dst = &mut pm.pixels_mut()[py as usize * pixmap_width + px as usize];
                    let old = dst.demultiply();
                    let inv = 255 - alpha;
                    *dst = tiny_skia::PremultipliedColorU8::from_rgba(
                        ((color.r() as u32 * alpha + old.red() as u32 * inv) / 255) as u8,
                        ((color.g() as u32 * alpha + old.green() as u32 * inv) / 255) as u8,
                        ((color.b() as u32 * alpha + old.blue() as u32 * inv) / 255) as u8,
                        255,
                    )
                    .unwrap();
                }
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_scene::{Insets, NodeAction, NodeId, Orientation, Role};
    fn fixture(label: &str) -> Scene {
        let root = Node::new(
            NodeId::new("root").unwrap(),
            Role::Button,
            label,
            Bounds::new(3.0, 4.0, 250.0, 80.0),
            "card",
        )
        .with_action(NodeAction::Activate);
        Scene::new(root, NodeId::new("root").unwrap()).unwrap()
    }
    fn metrics() -> SurfaceMetrics {
        SurfaceMetrics {
            logical_width: 431.0,
            logical_height: 277.0,
            scale: 1.0,
            safe_insets: Insets::default(),
            orientation: Orientation::Landscape,
        }
    }
    #[test]
    fn deterministic_between_fresh_runs() {
        assert_eq!(
            Rasterizer::new()
                .render(&fixture("再開 日本語"), metrics())
                .unwrap()
                .rgba,
            Rasterizer::new()
                .render(&fixture("再開 日本語"), metrics())
                .unwrap()
                .rgba
        );
    }
    #[test]
    fn cjk_fixture_produces_ink_and_damage_accumulates() {
        let mut r = Rasterizer::new();
        let first = r.render(&fixture("日本語"), metrics()).unwrap();
        assert!(first
            .rgba
            .chunks_exact(4)
            .any(|p| { p[0] > 80 && p[1] > 80 && p[2] > 80 && p != [13, 17, 23, 255] }));
        assert!(first.damage.is_some());
        let second = r.render(&fixture("日本語"), metrics()).unwrap();
        assert_eq!(second.damage, None);
    }

    fn overlapping_scene(order: [&str; 2]) -> Scene {
        let children = order.map(|id| {
            Node::new(
                NodeId::new(id).unwrap(),
                Role::Text,
                id,
                if id == "front" {
                    Bounds::new(20.0, 20.0, 80.0, 40.0)
                } else {
                    Bounds::new(50.0, 30.0, 80.0, 40.0)
                },
                "card",
            )
        });
        let root = Node::new(
            NodeId::new("root").unwrap(),
            Role::Button,
            "",
            Bounds::new(0.0, 0.0, 150.0, 90.0),
            "root",
        )
        .with_action(NodeAction::Activate)
        .with_children(children.into());
        Scene::new(root, NodeId::new("root").unwrap()).unwrap()
    }

    #[test]
    fn swapping_overlapping_siblings_damages_their_union() {
        let mut r = Rasterizer::new();
        r.render(&overlapping_scene(["front", "back"]), metrics())
            .unwrap();
        let frame = r
            .render(&overlapping_scene(["back", "front"]), metrics())
            .unwrap();
        assert_eq!(
            frame.damage,
            Some(DamageRect {
                x: 20,
                y: 20,
                width: 110,
                height: 50,
            })
        );
    }

    #[test]
    fn wrapping_text_is_clipped_to_node_bounds() {
        let bounds = Bounds::new(40.0, 30.0, 42.0, 24.0);
        let node = Node::new(
            NodeId::new("root").unwrap(),
            Role::Button,
            "This label wraps across far more lines than fit",
            bounds,
            "card",
        )
        .with_action(NodeAction::Activate);
        let scene = Scene::new(node, NodeId::new("root").unwrap()).unwrap();
        let frame = Rasterizer::new().render(&scene, metrics()).unwrap();
        for y in 0..frame.height {
            for x in 0..frame.width {
                if x < 40 || x >= 82 || y < 30 || y >= 54 {
                    let offset = (y * frame.width + x) as usize * 4;
                    assert_eq!(&frame.rgba[offset..offset + 4], &[13, 17, 23, 255]);
                }
            }
        }
    }
}
