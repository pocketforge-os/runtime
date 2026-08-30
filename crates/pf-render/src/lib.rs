//! Deterministic scene rasterization on the ruled Cosmic Text/Swash/tiny-skia stack.
//!
//! This crate exposes only the minimal raster palette seam. Unifying these colors with
//! the full `pf-theme` token set is intentionally left to a follow-on change.

use cosmic_text::{fontdb, Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, SwashCache};
use pf_scene::{Bounds, ImageFit, ImageSource, Node, NodeContent, Scene, SurfaceMetrics};
use std::collections::{HashMap, VecDeque};
use std::io::Cursor;
use tiny_skia::{
    Color as SkColor, FilterQuality, Mask, Paint, Pixmap, PixmapPaint, Rect, Transform,
};

const MANROPE: &[u8] =
    include_bytes!("../../../spikes/render-text/fonts/manrope/Manrope[wght].ttf");
const FRAUNCES: &[u8] =
    include_bytes!("../../../spikes/render-text/fonts/fraunces/Fraunces[SOFT,WONK,opsz,wght].ttf");
const CJK: &[u8] = include_bytes!("../fonts/NotoSansCJK-Regular.ttc");
/// Maximum decoded PNG dimensions accepted by the rasterizer (8 megapixels).
pub const MAX_IMAGE_PIXELS: u64 = 8_000_000;
const IMAGE_CACHE_CAPACITY: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Palette {
    pub background: (u8, u8, u8),
    pub text: (u8, u8, u8),
    pub node_default: (u8, u8, u8),
    pub node_focused: (u8, u8, u8),
    pub node_selected: (u8, u8, u8),
    pub node_disabled: (u8, u8, u8),
}

impl Palette {
    pub const fn standard() -> Self {
        Self {
            background: (13, 17, 23),
            text: (244, 234, 220),
            node_default: (26, 36, 48),
            node_focused: (36, 65, 95),
            node_selected: (44, 58, 72),
            node_disabled: (32, 36, 42),
        }
    }

    pub const fn high_contrast() -> Self {
        Self {
            background: (0, 0, 0),
            text: (255, 255, 255),
            node_default: (0, 0, 0),
            node_focused: (0, 102, 204),
            node_selected: (80, 80, 80),
            node_disabled: (20, 20, 20),
        }
    }
}

impl Default for Palette {
    fn default() -> Self {
        Self::standard()
    }
}

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
    /// Recoverable image failures encountered while producing this frame.
    pub notes: Vec<RenderNote>,
}

/// A typed, non-fatal condition for which the embedder may choose a fallback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RenderNote {
    ImageDecodeFailed {
        source_id: String,
    },
    ImageTooLarge {
        source_id: String,
        width: u32,
        height: u32,
        max_pixels: u64,
    },
}

/// Long-lived rasterizer. Cosmic Text's shaping state and Swash's glyph images are retained.
pub struct Rasterizer {
    fonts: FontSystem,
    glyphs: SwashCache,
    previous: Vec<NodeSnapshot>,
    images: ImageCache,
    palette: Palette,
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
    content: ContentSnapshot,
}

#[derive(Clone, PartialEq)]
enum ContentSnapshot {
    Label,
    Image { source_id: String, fit: ImageFit },
}

#[derive(Default)]
struct ImageCache {
    decoded: HashMap<String, Pixmap>,
    insertion_order: VecDeque<String>,
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
            images: ImageCache::default(),
            palette: Palette::standard(),
        }
    }

    pub fn set_palette(&mut self, palette: Palette) {
        self.palette = palette;
    }

    pub fn render(
        &mut self,
        scene: &Scene,
        metrics: SurfaceMetrics,
    ) -> Result<RasterFrame, RenderError> {
        let width = physical(metrics.logical_width, metrics.scale)?;
        let height = physical(metrics.logical_height, metrics.scale)?;
        let mut pixmap = Pixmap::new(width, height).ok_or(RenderError::InvalidSurface)?;
        let background = self.palette.background;
        pixmap.fill(SkColor::from_rgba8(
            background.0,
            background.1,
            background.2,
            255,
        ));
        let mut notes = Vec::new();
        let mut context = DrawContext {
            fonts: &mut self.fonts,
            glyphs: &mut self.glyphs,
            images: &mut self.images,
            notes: &mut notes,
            palette: self.palette,
        };
        draw_node(&mut pixmap, &mut context, scene.root(), metrics.scale);
        let mut current = Vec::new();
        collect(scene.root(), None, 0, &mut current);
        let damage = damage(&self.previous, &current, metrics.scale, width, height);
        self.previous = current;
        Ok(RasterFrame {
            width,
            height,
            rgba: pixmap.data().to_vec(),
            damage,
            notes,
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
        content: match &node.content {
            NodeContent::Label => ContentSnapshot::Label,
            NodeContent::Image { source, fit } => ContentSnapshot::Image {
                source_id: source.id.clone(),
                fit: *fit,
            },
        },
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

struct DrawContext<'a> {
    fonts: &'a mut FontSystem,
    glyphs: &'a mut SwashCache,
    images: &'a mut ImageCache,
    notes: &'a mut Vec<RenderNote>,
    palette: Palette,
}

fn draw_node(pm: &mut Pixmap, context: &mut DrawContext<'_>, node: &Node, scale: f32) {
    let b = node.bounds;
    let color = if node.state.focused {
        context.palette.node_focused
    } else if node.state.disabled {
        context.palette.node_disabled
    } else if node.state.selected {
        context.palette.node_selected
    } else {
        context.palette.node_default
    };
    if let Some(rect) = Rect::from_xywh(b.x * scale, b.y * scale, b.width * scale, b.height * scale)
    {
        let mut paint = Paint::default();
        paint.set_color_rgba8(color.0, color.1, color.2, 255);
        pm.fill_rect(rect, &paint, Transform::identity(), None);
    }
    match &node.content {
        NodeContent::Label => draw_text(
            pm,
            context.fonts,
            context.glyphs,
            TextDraw {
                text: &node.accessible_label,
                x: (b.x + 6.0) * scale,
                y: (b.y + 5.0) * scale,
                width: (b.width - 12.0).max(1.0) * scale,
                height: (b.height - 5.0).max(0.0) * scale,
                size: 15.0 * scale,
                clip: (b.x * scale, b.y * scale, b.width * scale, b.height * scale),
                color: context.palette.text,
            },
        ),
        NodeContent::Image { source, fit } => {
            if let Err(note) = draw_image(pm, context.images, source, *fit, b, scale) {
                context.notes.push(note);
            }
        }
    }
    for child in &node.children {
        draw_node(pm, context, child, scale);
    }
}

fn draw_image(
    target: &mut Pixmap,
    cache: &mut ImageCache,
    source: &ImageSource,
    fit: ImageFit,
    bounds: Bounds,
    surface_scale: f32,
) -> Result<(), RenderNote> {
    let image = cache.get_or_decode(source)?;
    let target_width = bounds.width * surface_scale;
    let target_height = bounds.height * surface_scale;
    if target_width <= 0.0 || target_height <= 0.0 {
        return Ok(());
    }
    let scale_x = target_width / image.width() as f32;
    let scale_y = target_height / image.height() as f32;
    let image_scale = match fit {
        ImageFit::Cover => scale_x.max(scale_y),
        ImageFit::Contain => scale_x.min(scale_y),
    };
    let draw_width = image.width() as f32 * image_scale;
    let draw_height = image.height() as f32 * image_scale;
    let x = bounds.x * surface_scale + (target_width - draw_width) / 2.0;
    let y = bounds.y * surface_scale + (target_height - draw_height) / 2.0;

    let mut mask_data = vec![0; target.width() as usize * target.height() as usize];
    let left = (bounds.x * surface_scale).floor().max(0.0) as u32;
    let top = (bounds.y * surface_scale).floor().max(0.0) as u32;
    let right = ((bounds.x + bounds.width) * surface_scale)
        .ceil()
        .clamp(0.0, target.width() as f32) as u32;
    let bottom = ((bounds.y + bounds.height) * surface_scale)
        .ceil()
        .clamp(0.0, target.height() as f32) as u32;
    for row in top.min(target.height())..bottom {
        let start = (row * target.width() + left.min(target.width())) as usize;
        let end = (row * target.width() + right) as usize;
        mask_data[start..end].fill(255);
    }
    let mask_size = tiny_skia::IntSize::from_wh(target.width(), target.height())
        .expect("pixmap has valid dimensions");
    let mask = Mask::from_vec(mask_data, mask_size).expect("mask matches target dimensions");
    let paint = PixmapPaint {
        quality: FilterQuality::Bilinear,
        ..PixmapPaint::default()
    };
    let transform = Transform::from_scale(image_scale, image_scale).post_translate(x, y);
    target.draw_pixmap(0, 0, image.as_ref(), &paint, transform, Some(&mask));
    Ok(())
}

impl ImageCache {
    fn get_or_decode(&mut self, source: &ImageSource) -> Result<&Pixmap, RenderNote> {
        if !self.decoded.contains_key(&source.id) {
            let pixmap = decode_png(source)?;
            if self.decoded.len() == IMAGE_CACHE_CAPACITY {
                if let Some(evicted) = self.insertion_order.pop_front() {
                    self.decoded.remove(&evicted);
                }
            }
            self.insertion_order.push_back(source.id.clone());
            self.decoded.insert(source.id.clone(), pixmap);
        }
        Ok(self
            .decoded
            .get(&source.id)
            .expect("just inserted or present"))
    }
}

fn decode_png(source: &ImageSource) -> Result<Pixmap, RenderNote> {
    let failed = || RenderNote::ImageDecodeFailed {
        source_id: source.id.clone(),
    };
    let bytes = source.bytes.as_ref();
    if bytes.len() < 24 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" || &bytes[12..16] != b"IHDR" {
        return Err(failed());
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().expect("four-byte width"));
    let height = u32::from_be_bytes(bytes[20..24].try_into().expect("four-byte height"));
    let pixels = u64::from(width) * u64::from(height);
    if pixels > MAX_IMAGE_PIXELS {
        return Err(RenderNote::ImageTooLarge {
            source_id: source.id.clone(),
            width,
            height,
            max_pixels: MAX_IMAGE_PIXELS,
        });
    }
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    decoder.set_limits(png::Limits {
        bytes: MAX_IMAGE_PIXELS as usize * 4,
    });
    let mut reader = decoder.read_info().map_err(|_| failed())?;
    let info = reader.info();
    debug_assert_eq!((info.width, info.height), (width, height));
    let mut decoded = vec![0; reader.output_buffer_size()];
    let output = reader.next_frame(&mut decoded).map_err(|_| failed())?;
    let bytes = &decoded[..output.buffer_size()];
    let mut rgba = Vec::with_capacity(pixels as usize * 4);
    match output.color_type {
        png::ColorType::Rgba => rgba.extend_from_slice(bytes),
        png::ColorType::Rgb => bytes
            .chunks_exact(3)
            .for_each(|p| rgba.extend_from_slice(&[p[0], p[1], p[2], 255])),
        png::ColorType::Grayscale => bytes
            .iter()
            .for_each(|&v| rgba.extend_from_slice(&[v, v, v, 255])),
        png::ColorType::GrayscaleAlpha => bytes
            .chunks_exact(2)
            .for_each(|p| rgba.extend_from_slice(&[p[0], p[0], p[0], p[1]])),
        png::ColorType::Indexed => return Err(failed()),
    }
    let mut pixmap = Pixmap::new(output.width, output.height).ok_or_else(failed)?;
    for (source, target) in rgba.chunks_exact(4).zip(pixmap.pixels_mut()) {
        *target =
            tiny_skia::ColorU8::from_rgba(source[0], source[1], source[2], source[3]).premultiply();
    }
    Ok(pixmap)
}

struct TextDraw<'a> {
    text: &'a str,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    size: f32,
    clip: (f32, f32, f32, f32),
    color: (u8, u8, u8),
}

fn draw_text(pm: &mut Pixmap, fonts: &mut FontSystem, glyphs: &mut SwashCache, draw: TextDraw<'_>) {
    let mut buffer = Buffer::new(fonts, Metrics::new(draw.size, draw.size * 1.25));
    buffer.set_size(fonts, Some(draw.width), Some(draw.height));
    buffer.set_text(
        fonts,
        draw.text,
        Attrs::new().family(Family::Name("Manrope")),
        Shaping::Advanced,
    );
    buffer.shape_until_scroll(fonts, false);
    buffer.draw(
        fonts,
        glyphs,
        Color::rgb(draw.color.0, draw.color.1, draw.color.2),
        |gx, gy, gw, gh, color| {
            let alpha = color.a() as u32;
            let pixmap_width = pm.width() as usize;
            for yy in 0..gh as i32 {
                for xx in 0..gw as i32 {
                    let px = gx + xx + draw.x as i32;
                    let py = gy + yy + draw.y as i32;
                    if px < draw.clip.0.floor() as i32
                        || py < draw.clip.1.floor() as i32
                        || px >= (draw.clip.0 + draw.clip.2).ceil() as i32
                        || py >= (draw.clip.1 + draw.clip.3).ceil() as i32
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
    use pf_scene::{ImageSource, Insets, NodeAction, NodeId, Orientation, Role};
    use std::sync::Arc;

    const IMAGE_PNG: &[u8] = include_bytes!("../../../spikes/consent-ui/baseline/s01-initial.png");
    const CORRUPT_PNG: &[u8] = include_bytes!("../tests/fixtures/corrupt.png");
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
    fn standard_palette_is_the_default_and_high_contrast_changes_pixels() {
        let scene = fixture("Palette");
        let default = Rasterizer::new().render(&scene, metrics()).unwrap();
        let mut explicit_standard = Rasterizer::new();
        explicit_standard.set_palette(Palette::standard());
        assert_eq!(
            default.rgba,
            explicit_standard.render(&scene, metrics()).unwrap().rgba
        );

        let mut high_contrast = Rasterizer::new();
        high_contrast.set_palette(Palette::high_contrast());
        let frame = high_contrast.render(&scene, metrics()).unwrap();
        assert_eq!(&frame.rgba[..4], &[0, 0, 0, 255]);
        assert!(frame
            .rgba
            .chunks_exact(4)
            .any(|pixel| pixel == [255, 255, 255, 255]));
    }

    fn image_fixture(bytes: &'static [u8], fit: ImageFit) -> Scene {
        let root = Node::new(
            NodeId::new("root").unwrap(),
            Role::Button,
            "cover art",
            Bounds::new(17.0, 11.0, 173.0, 129.0),
            "art",
        )
        .with_action(NodeAction::Activate)
        .with_image(
            ImageSource::new("fixture-v1", Arc::<[u8]>::from(bytes)),
            fit,
        );
        Scene::new(root, NodeId::new("root").unwrap()).unwrap()
    }

    fn frame_hash(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xcbf29ce484222325, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        })
    }

    #[test]
    fn image_frame_hash_is_deterministic_across_fresh_runs() {
        let first = Rasterizer::new()
            .render(&image_fixture(IMAGE_PNG, ImageFit::Cover), metrics())
            .unwrap();
        let second = Rasterizer::new()
            .render(&image_fixture(IMAGE_PNG, ImageFit::Cover), metrics())
            .unwrap();
        assert!(first.notes.is_empty());
        assert_eq!(frame_hash(&first.rgba), frame_hash(&second.rgba));
        assert_eq!(frame_hash(&first.rgba), 4_451_243_305_016_117_238);
    }

    #[test]
    fn corrupt_image_is_a_typed_note_and_never_draws_debug_text() {
        let frame = Rasterizer::new()
            .render(&image_fixture(CORRUPT_PNG, ImageFit::Contain), metrics())
            .unwrap();
        assert_eq!(
            frame.notes,
            vec![RenderNote::ImageDecodeFailed {
                source_id: "fixture-v1".into()
            }]
        );
        // The node's themed background remains; its semantic alt text is not painted.
        assert!(!frame
            .rgba
            .chunks_exact(4)
            .any(|p| p == [244, 234, 220, 255]));
    }

    #[test]
    fn oversized_header_is_rejected_before_pixel_allocation() {
        let mut bytes = vec![0; 24];
        bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        bytes[12..16].copy_from_slice(b"IHDR");
        bytes[16..20].copy_from_slice(&4_000u32.to_be_bytes());
        bytes[20..24].copy_from_slice(&3_000u32.to_be_bytes());
        let source = ImageSource::new("oversized", Arc::<[u8]>::from(bytes));
        assert_eq!(
            decode_png(&source).unwrap_err(),
            RenderNote::ImageTooLarge {
                source_id: "oversized".into(),
                width: 4_000,
                height: 3_000,
                max_pixels: MAX_IMAGE_PIXELS,
            }
        );
    }

    #[test]
    fn image_content_and_fit_participate_in_damage_tracking() {
        let mut rasterizer = Rasterizer::new();
        rasterizer
            .render(&image_fixture(IMAGE_PNG, ImageFit::Contain), metrics())
            .unwrap();
        let frame = rasterizer
            .render(&image_fixture(IMAGE_PNG, ImageFit::Cover), metrics())
            .unwrap();
        assert_eq!(
            frame.damage,
            Some(DamageRect {
                x: 17,
                y: 11,
                width: 173,
                height: 129,
            })
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
                if !(40..82).contains(&x) || !(30..54).contains(&y) {
                    let offset = (y * frame.width + x) as usize * 4;
                    assert_eq!(&frame.rgba[offset..offset + 4], &[13, 17, 23, 255]);
                }
            }
        }
    }
}
