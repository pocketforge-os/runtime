use super::*;
use pf_text_raster as raster;
use tiny_skia::Pixmap;

struct Provider {
    fonts: raster::FontSet,
}

impl Provider {
    fn new() -> Self {
        Self {
            fonts: raster::pocketforge_default_fonts(),
        }
    }
}

fn native(style: &TextStyle) -> raster::ResolvedTextStyle {
    raster::ResolvedTextStyle {
        family: style.family.clone(),
        size_px: style.size_px,
        weight: style.weight,
        line_height: style.line_height,
        tracking_em: style.tracking_em,
    }
}

fn factor(scale: TextScale) -> f32 {
    match scale {
        TextScale::Reflow(value) => value,
        TextScale::Fixed(_) => 1.0,
    }
}

impl TextSubstrate for Provider {
    fn fresh(&self) -> Self {
        Self::new()
    }

    fn measure(
        &mut self,
        style: &TextStyle,
        text: &str,
        scale: TextScale,
        max_width: Option<f32>,
    ) -> (f32, f32) {
        raster::measure(&self.fonts, &native(style), text, factor(scale), max_width)
    }

    fn rasterize(
        &mut self,
        style: &TextStyle,
        text: &str,
        scale: TextScale,
        max_width: f32,
        align: TextAlign,
        clip: (f32, f32, f32, f32),
    ) -> Raster {
        let applied = factor(scale);
        let layout = raster::measure(&self.fonts, &native(style), text, applied, Some(max_width));
        let width = 512;
        let height = 1024;
        let mut pixmap = Pixmap::new(width, height).unwrap();
        let mut fonts = self.fonts.font_system();
        let mut glyphs = raster::SwashCache::new();
        raster::draw_text(
            &mut pixmap,
            &mut fonts,
            &mut glyphs,
            raster::TextDraw {
                text,
                x: 0.0,
                y: 0.0,
                width: max_width,
                height: height as f32,
                style: &native(style),
                scale: applied,
                align: match align {
                    TextAlign::Start => raster::TextAlign::Start,
                    TextAlign::Center => raster::TextAlign::Center,
                },
                clip,
                color: [255, 255, 255, 255],
            },
        );
        let rgba = pixmap
            .pixels()
            .iter()
            .flat_map(|pixel| {
                let c = pixel.demultiply();
                [c.red(), c.green(), c.blue(), c.alpha()]
            })
            .collect();
        Raster {
            width,
            height,
            rgba,
            layout_width: layout.0,
            layout_height: layout.1,
        }
    }

    fn glyph_cache_key(&mut self, style: &TextStyle, glyph: char) -> Vec<u8> {
        use cosmic_text_tracking as text;
        let mut fonts = self.fonts.font_system();
        let mut buffer = text::Buffer::new(
            &mut fonts,
            text::Metrics::new(style.size_px, style.size_px * style.line_height),
        );
        let glyph_text = glyph.to_string();
        buffer.set_text(
            &mut fonts,
            &glyph_text,
            &text::Attrs::new()
                .family(text::Family::Name(&style.family))
                .weight(text::Weight(style.weight))
                .letter_spacing(style.tracking_em),
            text::Shaping::Advanced,
            None,
        );
        buffer.shape_until_scroll(&mut fonts, false);
        let key = buffer.layout_runs().next().unwrap().glyphs[0]
            .physical((0.0, 0.0), 1.0)
            .cache_key;
        format!("{key:?}").into_bytes()
    }
}

fn style() -> TextStyle {
    TextStyle {
        family: "Manrope".into(),
        size_px: 18.0,
        weight: 400,
        line_height: 1.2,
        tracking_em: 0.0,
    }
}

#[test]
fn determinism() {
    assert_determinism(&Provider::new(), &style(), "repeatable text", 180.0).unwrap();
}
#[test]
fn measure_paint_height_agreement() {
    let mut p = Provider::new();
    let a = style();
    let mut b = a.clone();
    b.line_height = 1.65;
    assert_measure_paint_height_agreement(
        &mut p,
        &a,
        &b,
        "text that wraps across several lines",
        100.0,
    )
    .unwrap();
}
#[test]
fn measure_paint_width_agreement() {
    assert_measure_paint_width_agreement(&mut Provider::new(), &style(), "center and start", 180.0)
        .unwrap();
}
#[test]
fn text_scale_reflow() {
    assert_text_scale_reflow(
        &mut Provider::new(),
        &style(),
        "text that reflows into more lines",
        130.0,
        1.5,
    )
    .unwrap();
}
#[test]
fn fixed_paint_scale_exactness() {
    assert_fixed_paint_scale_exactness(&mut Provider::new(), &style(), "fixed", 180.0).unwrap();
}
#[test]
fn weight_ink_monotonicity() {
    let a = style();
    let mut b = a.clone();
    b.weight = 700;
    assert_weight_ink_monotonicity(&mut Provider::new(), &a, &b, "Weight", 180.0).unwrap();
}
#[test]
fn glyph_cache_key_completeness() {
    let a = style();
    let mut b = a.clone();
    b.weight = 700;
    let mut c = a.clone();
    c.size_px = 24.0;
    assert_glyph_cache_key_completeness(&mut Provider::new(), &[a, b, c], 'A').unwrap();
}
#[test]
fn tracking_additivity() {
    let a = style();
    let mut b = a.clone();
    b.tracking_em = 0.08;
    assert_tracking_additivity(&mut Provider::new(), &a, &b, "TRACK").unwrap();
}
#[test]
fn wrap_clip_containment() {
    assert_wrap_clip_containment(
        &mut Provider::new(),
        &style(),
        "many words must wrap and remain clipped",
        96.0,
        (3.0, 4.0, 80.0, 55.0),
    )
    .unwrap();
}
