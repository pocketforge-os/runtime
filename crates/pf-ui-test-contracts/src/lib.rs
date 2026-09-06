//! Reusable, framework-neutral conformance contracts for text measurement and rasterization.

use std::fmt;

#[derive(Clone, Debug)]
pub struct TextStyle {
    pub family: String,
    pub size_px: f32,
    pub weight: u16,
    pub line_height: f32,
    pub tracking_em: f32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextAlign {
    Start,
    Center,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TextScale {
    Reflow(f32),
    Fixed(f32),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Raster {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// Geometry used by the painter's layout pass, before clipping.
    pub layout_width: f32,
    pub layout_height: f32,
}

impl Raster {
    pub fn ink(&self) -> u64 {
        self.rgba.chunks_exact(4).map(|pixel| pixel[3] as u64).sum()
    }

    pub fn ink_bounds(&self) -> Option<(u32, u32, u32, u32)> {
        let mut bounds = (self.width, self.height, 0, 0);
        let mut found = false;
        for (index, pixel) in self.rgba.chunks_exact(4).enumerate() {
            if pixel[3] == 0 {
                continue;
            }
            let x = index as u32 % self.width;
            let y = index as u32 / self.width;
            bounds = (
                bounds.0.min(x),
                bounds.1.min(y),
                bounds.2.max(x + 1),
                bounds.3.max(y + 1),
            );
            found = true;
        }
        found.then_some(bounds)
    }
}

pub trait TextSubstrate: Sized {
    fn fresh(&self) -> Self;
    fn measure(
        &mut self,
        style: &TextStyle,
        text: &str,
        scale: TextScale,
        max_width: Option<f32>,
    ) -> (f32, f32);
    fn rasterize(
        &mut self,
        style: &TextStyle,
        text: &str,
        scale: TextScale,
        max_width: f32,
        align: TextAlign,
        clip: (f32, f32, f32, f32),
    ) -> Raster;
    /// Stable identity of the actual glyph-cache entry selected for this styled glyph.
    fn glyph_cache_key(&mut self, style: &TextStyle, glyph: char) -> Vec<u8>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContractError {
    contract: &'static str,
    detail: String,
}

impl ContractError {
    fn new(contract: &'static str, detail: impl Into<String>) -> Self {
        Self {
            contract,
            detail: detail.into(),
        }
    }
    pub fn contract(&self) -> &'static str {
        self.contract
    }
}

impl fmt::Display for ContractError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.contract, self.detail)
    }
}

impl std::error::Error for ContractError {}
pub type ContractResult = Result<(), ContractError>;
const EPSILON: f32 = 0.01;

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() <= EPSILON
}

pub fn assert_determinism<S: TextSubstrate>(
    prototype: &S,
    style: &TextStyle,
    text: &str,
    max_width: f32,
) -> ContractResult {
    let mut a = prototype.fresh();
    let mut b = prototype.fresh();
    let ra = a.rasterize(
        style,
        text,
        TextScale::Reflow(1.0),
        max_width,
        TextAlign::Start,
        (0.0, 0.0, max_width, 512.0),
    );
    let rb = b.rasterize(
        style,
        text,
        TextScale::Reflow(1.0),
        max_width,
        TextAlign::Start,
        (0.0, 0.0, max_width, 512.0),
    );
    (ra == rb).then_some(()).ok_or_else(|| {
        ContractError::new(
            "determinism",
            "fresh instances produced different raster output",
        )
    })
}

pub fn assert_measure_paint_height_agreement<S: TextSubstrate>(
    substrate: &mut S,
    default: &TextStyle,
    custom: &TextStyle,
    text: &str,
    max_width: f32,
) -> ContractResult {
    for style in [default, custom] {
        let measured = substrate.measure(style, text, TextScale::Reflow(1.0), Some(max_width));
        let painted = substrate.rasterize(
            style,
            text,
            TextScale::Reflow(1.0),
            max_width,
            TextAlign::Start,
            (0.0, 0.0, max_width, 1024.0),
        );
        if !close(measured.1, painted.layout_height) {
            return Err(ContractError::new(
                "measure_paint_height_agreement",
                format!(
                    "measured {} but painted {}",
                    measured.1, painted.layout_height
                ),
            ));
        }
    }
    Ok(())
}

pub fn assert_measure_paint_width_agreement<S: TextSubstrate>(
    substrate: &mut S,
    style: &TextStyle,
    text: &str,
    max_width: f32,
) -> ContractResult {
    let measured = substrate.measure(style, text, TextScale::Reflow(1.0), Some(max_width));
    for align in [TextAlign::Start, TextAlign::Center] {
        let painted = substrate.rasterize(
            style,
            text,
            TextScale::Reflow(1.0),
            max_width,
            align,
            (0.0, 0.0, max_width, 512.0),
        );
        if !close(measured.0, painted.layout_width) {
            return Err(ContractError::new(
                "measure_paint_width_agreement",
                format!(
                    "measured {} but painted {}",
                    measured.0, painted.layout_width
                ),
            ));
        }
    }
    Ok(())
}

pub fn assert_text_scale_reflow<S: TextSubstrate>(
    substrate: &mut S,
    style: &TextStyle,
    text: &str,
    max_width: f32,
    larger_scale: f32,
) -> ContractResult {
    let normal = substrate.rasterize(
        style,
        text,
        TextScale::Reflow(1.0),
        max_width,
        TextAlign::Start,
        (0.0, 0.0, max_width, 1024.0),
    );
    let large = substrate.rasterize(
        style,
        text,
        TextScale::Reflow(larger_scale),
        max_width,
        TextAlign::Start,
        (0.0, 0.0, max_width, 1024.0),
    );
    if large.layout_height <= normal.layout_height
        || large.ink() <= normal.ink()
        || large.rgba == normal.rgba
    {
        return Err(ContractError::new(
            "text_scale_reflow",
            "larger scale did not reshape into taller, heavier ink",
        ));
    }
    Ok(())
}

pub fn assert_fixed_paint_scale_exactness<S: TextSubstrate>(
    substrate: &mut S,
    style: &TextStyle,
    text: &str,
    max_width: f32,
) -> ContractResult {
    let a = substrate.rasterize(
        style,
        text,
        TextScale::Fixed(1.0),
        max_width,
        TextAlign::Start,
        (0.0, 0.0, max_width, 512.0),
    );
    let b = substrate.rasterize(
        style,
        text,
        TextScale::Fixed(2.0),
        max_width,
        TextAlign::Start,
        (0.0, 0.0, max_width, 512.0),
    );
    (a == b).then_some(()).ok_or_else(|| {
        ContractError::new(
            "fixed_paint_scale_exactness",
            "fixed-scale ink was not pixel-identical",
        )
    })
}

pub fn assert_weight_ink_monotonicity<S: TextSubstrate>(
    substrate: &mut S,
    light: &TextStyle,
    heavy: &TextStyle,
    text: &str,
    max_width: f32,
) -> ContractResult {
    let light_ink = substrate
        .rasterize(
            light,
            text,
            TextScale::Fixed(1.0),
            max_width,
            TextAlign::Start,
            (0.0, 0.0, max_width, 512.0),
        )
        .ink();
    let heavy_ink = substrate
        .rasterize(
            heavy,
            text,
            TextScale::Fixed(1.0),
            max_width,
            TextAlign::Start,
            (0.0, 0.0, max_width, 512.0),
        )
        .ink();
    (heavy_ink > light_ink).then_some(()).ok_or_else(|| {
        ContractError::new(
            "weight_ink_monotonicity",
            format!("heavy ink {heavy_ink} not strictly greater than light ink {light_ink}"),
        )
    })
}

pub fn assert_glyph_cache_key_completeness<S: TextSubstrate>(
    substrate: &mut S,
    styles: &[TextStyle],
    glyph: char,
) -> ContractResult {
    let mut keys = Vec::new();
    for style in styles {
        keys.push(substrate.glyph_cache_key(style, glyph));
    }
    for i in 0..keys.len() {
        for j in i + 1..keys.len() {
            let bitmap_differs = styles[i].family != styles[j].family
                || styles[i].size_px != styles[j].size_px
                || styles[i].weight != styles[j].weight;
            if bitmap_differs && keys[i] == keys[j] {
                return Err(ContractError::new(
                    "glyph_cache_key_completeness",
                    format!(
                        "bitmap-affecting styles {i} and {j} share a glyph-cache key (under-keyed)"
                    ),
                ));
            }
        }
    }
    Ok(())
}

pub fn assert_tracking_additivity<S: TextSubstrate>(
    substrate: &mut S,
    natural: &TextStyle,
    tracked: &TextStyle,
    text: &str,
) -> ContractResult {
    let natural_width = substrate
        .measure(natural, text, TextScale::Fixed(1.0), None)
        .0;
    let tracked_width = substrate
        .measure(tracked, text, TextScale::Fixed(1.0), None)
        .0;
    let expected =
        natural_width + tracked.tracking_em * tracked.size_px * text.chars().count() as f32;
    if !close(tracked_width, expected) {
        return Err(ContractError::new(
            "tracking_additivity",
            format!("tracked {tracked_width}, expected {expected}"),
        ));
    }
    let mut doubled = tracked.clone();
    doubled.size_px *= 2.0;
    let doubled_width = substrate
        .measure(&doubled, text, TextScale::Fixed(1.0), None)
        .0;
    if !close(doubled_width, tracked_width * 2.0) {
        return Err(ContractError::new(
            "tracking_additivity",
            "width was not linear in size",
        ));
    }
    Ok(())
}

pub fn assert_wrap_clip_containment<S: TextSubstrate>(
    substrate: &mut S,
    style: &TextStyle,
    text: &str,
    max_width: f32,
    clip: (f32, f32, f32, f32),
) -> ContractResult {
    let raster = substrate.rasterize(
        style,
        text,
        TextScale::Fixed(1.0),
        max_width,
        TextAlign::Start,
        clip,
    );
    if raster.layout_width > max_width + EPSILON {
        return Err(ContractError::new(
            "wrap_clip_containment",
            "layout exceeded max width",
        ));
    }
    if let Some((x0, y0, x1, y1)) = raster.ink_bounds() {
        let clip_left = clip.0.floor() as i32;
        let clip_top = clip.1.floor() as i32;
        let clip_right = (clip.0 + clip.2).ceil() as i32;
        let clip_bottom = (clip.1 + clip.3).ceil() as i32;
        if (x0 as i32) < clip_left
            || (y0 as i32) < clip_top
            || ((x1 - 1) as i32) >= clip_right
            || ((y1 - 1) as i32) >= clip_bottom
        {
            return Err(ContractError::new(
                "wrap_clip_containment",
                "ink escaped clip box",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
