//! CONSUME the platform `.scad`->skin chain's gated assets to render "the real device with control
//! X highlighted". This is the pf-hwprobe highlight model: a NEUTRAL `body` image + a `lit` atlas
//! (each control rendered red at its rect) + a `[skin.parts]` rect table; highlighting control X =
//! draw the neutral body, then paste the lit atlas's crop for X's rect over it. Top-edge controls
//! (`l1`/`r1`/`ltrig`/`rtrig`) use the `[skin.views.top]` atlas so the trigger — a mere sliver from
//! the front camera — is prominent (the first runtime consumer of `[skin.views.*]`).
//!
//! Nothing here GENERATES from the `.scad`; it READS the committed, drift-gated (tsp-65jc.7) PNG +
//! TOML. A `.scad` geometry fix regenerates those assets in `platform`; this app reads the latest at
//! its next deploy/bake — one source of truth, zero drift (see docs/RENDER_HOST_DECISION.md).

use std::collections::HashMap;
use std::io;
use std::path::Path;

use serde::Deserialize;

use crate::canvas::Color;
use crate::image::{load_png, Rgb};

/// A `[skin.parts]` rectangle (skin-image space).
#[derive(Deserialize, Clone, Copy, Debug)]
pub struct Rect {
    pub x: i64,
    pub y: i64,
    pub w: i64,
    pub h: i64,
}

#[derive(Deserialize)]
struct Descriptor {
    skin: SkinTable,
    #[serde(default)]
    inputs: Vec<InputRow>,
}

#[derive(Deserialize)]
struct SkinTable {
    body: String,
    lit_body: String,
    #[serde(default)]
    parts: HashMap<String, Rect>,
    #[serde(default)]
    views: HashMap<String, ViewTable>,
}

#[derive(Deserialize)]
struct ViewTable {
    body: String,
    lit_body: String,
    #[serde(default)]
    parts: HashMap<String, Rect>,
}

#[derive(Deserialize)]
struct InputRow {
    id: String,
    #[serde(default)]
    skin_part: Option<String>,
    /// The PRINTED faceplate glyph (e.g. south -> "B" on a Nintendo chassis) — rendered in the
    /// on-panel prompt, never an SDL letter derived from the position id (tsp-bwrg.6 pass #5).
    #[serde(default)]
    label: Option<String>,
}

/// One rendered view (front or top): its neutral body, its lit atlas, and its part rects.
pub struct View {
    pub body: Rgb,
    pub lit: Rgb,
    pub parts: HashMap<String, Rect>,
}

impl View {
    /// The neutral body with `part` (if present in this view) highlighted from the lit atlas.
    fn highlighted(&self, part: Option<&str>) -> Rgb {
        let mut img = self.body.clone();
        if let Some(p) = part {
            if let Some(r) = self.parts.get(p) {
                img.blit_region_from(&self.lit, r.x, r.y, r.w, r.h);
            }
        }
        img
    }
}

/// A device's consumable skin: the engine-id->skin_part map + the front (and optional top) views.
pub struct SkinSet {
    input_to_part: HashMap<String, String>,
    /// engine-id -> the PRINTED faceplate glyph (descriptor `label`), e.g. south -> "B" on a
    /// Nintendo-arranged chassis. The ONLY control-name source an on-panel prompt may render — never
    /// an internal id or an SDL letter derived from position (tsp-bwrg.6 owner pass #5).
    input_to_label: HashMap<String, String>,
    front: View,
    top: Option<View>,
    /// The device background color (a solid corner pixel) — used to color-key the device onto the
    /// dark UI so it floats rather than sitting on a card.
    pub bg: Color,
}

impl SkinSet {
    /// Load from a committed descriptor (`devices/<dev>/capabilities.toml`) + a `skin_root` the
    /// descriptor's relative skin paths (`skins/<dev>/body.png`) resolve against.
    pub fn load(descriptor_path: &Path, skin_root: &Path) -> io::Result<SkinSet> {
        let text = std::fs::read_to_string(descriptor_path)?;
        let desc: Descriptor = toml::from_str(&text)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("parsing {}: {e}", descriptor_path.display())))?;

        let mut front = View {
            body: load_png(&skin_root.join(&desc.skin.body))?,
            lit: load_png(&skin_root.join(&desc.skin.lit_body))?,
            parts: desc.skin.parts,
        };
        // Synthesize per-direction D-PAD highlight rects from the single `dpad` part, so each of the
        // four direction prompts lights ONLY its own arm (a half of the cross) — no new atlas needed
        // (tsp-bwrg.6 pass #5: "any way to just highlight the top/left/right/bottom?").
        if let Some(&d) = front.parts.get("dpad") {
            let (w2, h2, w3, h3) = (d.w / 2, d.h / 2, d.w / 3, d.h / 3);
            front.parts.insert("dpad_up".into(), Rect { x: d.x + w3, y: d.y, w: w3, h: h2 });
            front.parts.insert("dpad_down".into(), Rect { x: d.x + w3, y: d.y + h2, w: w3, h: d.h - h2 });
            front.parts.insert("dpad_left".into(), Rect { x: d.x, y: d.y + h3, w: w2, h: h3 });
            front.parts.insert("dpad_right".into(), Rect { x: d.x + w2, y: d.y + h3, w: d.w - w2, h: h3 });
        }
        let top = match desc.skin.views.get("top") {
            Some(v) => Some(View {
                body: load_png(&skin_root.join(&v.body))?,
                lit: load_png(&skin_root.join(&v.lit_body))?,
                parts: v.parts.clone(),
            }),
            None => None,
        };
        let bg = front.body.get(0, 0);
        let mut input_to_part = HashMap::new();
        let mut input_to_label = HashMap::new();
        for r in desc.inputs {
            if let Some(p) = r.skin_part {
                input_to_part.insert(r.id.clone(), p);
            }
            if let Some(l) = r.label {
                input_to_label.insert(r.id, l);
            }
        }
        Ok(SkinSet { input_to_part, input_to_label, front, top, bg })
    }

    /// In-memory constructor for tests (no disk / PNG decode). Labels default empty; add them with
    /// [`with_labels`](Self::with_labels).
    pub fn from_parts(input_to_part: HashMap<String, String>, front: View, top: Option<View>, bg: Color) -> SkinSet {
        SkinSet { input_to_part, input_to_label: HashMap::new(), front, top, bg }
    }

    /// Attach an engine-id -> faceplate-label map (test builder / overlay).
    pub fn with_labels(mut self, input_to_label: HashMap<String, String>) -> Self {
        self.input_to_label = input_to_label;
        self
    }

    /// The front body dimensions (all views share them) — for scaling into the canvas.
    pub fn size(&self) -> (usize, usize) {
        (self.front.body.w, self.front.body.h)
    }

    /// The skin_part an engine control id maps to, if any.
    pub fn part_for(&self, input_id: &str) -> Option<&str> {
        if let Some(p) = self.input_to_part.get(input_id) {
            return Some(p.as_str());
        }
        // A D-PAD direction prompt (dpad_up/down/left/right) highlights its OWN arm — the synthetic
        // per-direction sub-rect built in `load()` — falling back to the whole `dpad` part if the
        // sub-rects weren't synthesized (tsp-bwrg.6 pass #5: highlight the specific direction).
        if input_id.starts_with("dpad_") {
            if let Some((k, _)) = self.front.parts.get_key_value(input_id) {
                return Some(k.as_str());
            }
            return self.input_to_part.get("dpad").map(|s| s.as_str());
        }
        None
    }

    /// The PRINTED faceplate glyph (descriptor `label`) for an engine control id, if the descriptor
    /// gives one — what a human reads on the device. This is the ONLY control-name source an
    /// on-panel prompt may render (never an internal id, never an SDL letter derived from position).
    pub fn label_for(&self, input_id: &str) -> Option<&str> {
        self.input_to_label.get(input_id).map(|s| s.as_str())
    }

    /// Choose the best view for an engine control: the TOP view when the control's part is drawn
    /// there (top-edge shoulders/triggers), else the front. Data-driven — no hardcoded id list.
    fn view_for(&self, part: &str) -> &View {
        if let Some(top) = &self.top {
            if top.parts.contains_key(part) {
                return top;
            }
        }
        &self.front
    }

    /// The composited device image for the given active engine control id (or the neutral front
    /// body when `active` is `None`/unknown). This is what the renderer blits onto the panel.
    pub fn compose(&self, active: Option<&str>) -> Rgb {
        match active.and_then(|id| self.part_for(id)) {
            Some(part) => {
                let view = self.view_for(part);
                view.highlighted(Some(part))
            }
            None => self.front.body.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::rgb;

    fn synth_skinset() -> SkinSet {
        // A 20x10 front body (dark) with a lit atlas that is red inside btn_south's rect (5,5,4,3),
        // plus a top view with trig_l lit at (2,2,4,2). Mirrors the real asset shape at tiny size.
        let mut front_body = Rgb::new(20, 10, rgb(4, 4, 5));
        // give the very corner a distinct "bg" so bg color-key has something to detect
        front_body.set(0, 0, rgb(248, 248, 248));
        let mut front_lit = Rgb::new(20, 10, rgb(4, 4, 5));
        for y in 5..8 {
            for x in 5..9 {
                front_lit.set(x, y, rgb(200, 0, 0));
            }
        }
        let mut front_parts = HashMap::new();
        front_parts.insert("btn_south".to_string(), Rect { x: 5, y: 5, w: 4, h: 3 });
        front_parts.insert("trig_l".to_string(), Rect { x: 1, y: 1, w: 2, h: 1 }); // present front too, but top wins

        let mut top_lit = Rgb::new(20, 10, rgb(4, 4, 5));
        for y in 2..4 {
            for x in 2..6 {
                top_lit.set(x, y, rgb(0, 200, 0)); // distinct color to prove the TOP view was chosen
            }
        }
        let mut top_parts = HashMap::new();
        top_parts.insert("trig_l".to_string(), Rect { x: 2, y: 2, w: 4, h: 2 });

        let mut map = HashMap::new();
        map.insert("south".to_string(), "btn_south".to_string());
        map.insert("ltrig".to_string(), "trig_l".to_string());

        SkinSet::from_parts(
            map,
            View { body: front_body, lit: front_lit, parts: front_parts },
            Some(View { body: Rgb::new(20, 10, rgb(4, 4, 5)), lit: top_lit, parts: top_parts }),
            rgb(248, 248, 248),
        )
    }

    #[test]
    fn compose_highlights_the_active_control_from_the_lit_atlas() {
        let s = synth_skinset();
        let img = s.compose(Some("south"));
        // btn_south rect became red; a pixel outside stays neutral.
        assert_eq!(img.get(6, 6), rgb(200, 0, 0));
        assert_eq!(img.get(0, 5), rgb(4, 4, 5));
    }

    #[test]
    fn dpad_direction_prompts_resolve_to_the_single_dpad_part() {
        // The four atomic dpad direction prompts (dpad_up/down/left/right) are NOT in the
        // descriptor's id->skin_part map (which has only "dpad"), so part_for must fall back to the
        // single "dpad" part — else the dpad renders with no highlight (tsp-bwrg.6 owner pass #5).
        let mut map = HashMap::new();
        map.insert("dpad".to_string(), "dpad".to_string());
        map.insert("south".to_string(), "btn_south".to_string());
        let s = SkinSet::from_parts(
            map,
            View { body: Rgb::new(4, 4, rgb(0, 0, 0)), lit: Rgb::new(4, 4, rgb(0, 0, 0)), parts: HashMap::new() },
            None,
            rgb(0, 0, 0),
        );
        for dir in ["dpad_up", "dpad_down", "dpad_left", "dpad_right"] {
            assert_eq!(s.part_for(dir), Some("dpad"), "{dir} must highlight the dpad part");
        }
        assert_eq!(s.part_for("south"), Some("btn_south"), "a directly-mapped id still resolves");
        assert_eq!(s.part_for("nonexistent"), None, "an unrelated unknown id resolves to nothing");
    }

    #[test]
    fn dpad_direction_highlights_its_own_arm_when_a_subrect_exists() {
        // With a per-direction sub-rect present (synthesized from the whole `dpad` in load()), a
        // direction highlights its OWN arm; a direction without a sub-rect falls back to whole dpad.
        let mut parts = HashMap::new();
        parts.insert("dpad_up".to_string(), Rect { x: 0, y: 0, w: 2, h: 2 });
        let mut map = HashMap::new();
        map.insert("dpad".to_string(), "dpad".to_string());
        let s = SkinSet::from_parts(
            map,
            View { body: Rgb::new(4, 4, rgb(0, 0, 0)), lit: Rgb::new(4, 4, rgb(0, 0, 0)), parts },
            None,
            rgb(0, 0, 0),
        );
        assert_eq!(s.part_for("dpad_up"), Some("dpad_up"), "a synthesized per-direction sub-rect wins");
        assert_eq!(s.part_for("dpad_down"), Some("dpad"), "a direction with no sub-rect falls back to whole dpad");
    }

    #[test]
    fn load_synthesizes_four_dpad_direction_subrects_inside_the_dpad_rect() {
        // Pure geometry check on the synthesis: four direction rects, each inside the parent dpad.
        let mut parts = HashMap::new();
        parts.insert("dpad".to_string(), Rect { x: 100, y: 200, w: 144, h: 142 });
        // Re-run the same synthesis load() does (kept in sync with the load() body).
        let d = parts["dpad"];
        let (w2, h2, w3, h3) = (d.w / 2, d.h / 2, d.w / 3, d.h / 3);
        parts.insert("dpad_up".into(), Rect { x: d.x + w3, y: d.y, w: w3, h: h2 });
        parts.insert("dpad_down".into(), Rect { x: d.x + w3, y: d.y + h2, w: w3, h: d.h - h2 });
        parts.insert("dpad_left".into(), Rect { x: d.x, y: d.y + h3, w: w2, h: h3 });
        parts.insert("dpad_right".into(), Rect { x: d.x + w2, y: d.y + h3, w: d.w - w2, h: h3 });
        for dir in ["dpad_up", "dpad_down", "dpad_left", "dpad_right"] {
            let r = parts[dir];
            assert!(r.x >= d.x && r.y >= d.y && r.x + r.w <= d.x + d.w && r.y + r.h <= d.y + d.h,
                "{dir} rect {r:?} must lie inside the parent dpad {d:?}");
            assert!(r.w > 0 && r.h > 0, "{dir} must be non-empty");
        }
    }

    #[test]
    fn top_edge_control_uses_the_top_view() {
        let s = synth_skinset();
        let img = s.compose(Some("ltrig"));
        // ltrig's part (trig_l) is present in BOTH views; the TOP view must win -> GREEN, not red.
        assert_eq!(img.get(3, 2), rgb(0, 200, 0));
    }

    #[test]
    fn none_and_unknown_return_the_neutral_body() {
        let s = synth_skinset();
        assert_eq!(s.compose(None).get(6, 6), rgb(4, 4, 5));
        assert_eq!(s.compose(Some("nonexistent")).get(6, 6), rgb(4, 4, 5));
    }
}
