//! Real-asset render test for the on-panel UI (`tsp-bwrg.8`, extending the tsp-bwrg.3 skin tests).
//!
//! The in-crate `#[cfg(test)]` render tests use a SYNTHETIC in-memory `SkinSet` (tiny 20x10 body,
//! hand-built parts). That covers the composite algebra but does NOT exercise the REAL
//! `SkinSet::load` path — the fs read + PNG decode + toml parse of the committed platform assets
//! (`platform/skins/a133/*.png` + `platform/devices/a133/capabilities.toml`'s [skin], [skin.parts],
//! and [skin.views.top.*] tables). A break in that path — a renamed asset, a schema shift, a lost
//! [skin.views.top.parts] entry, a wrong id->skin_part map — would land silently on-device today
//! and only surface in the on-panel gate. This test catches it in CI.
//!
//! Requires a `platform` checkout, same discovery mechanism as
//! `pf-input-collect/tests/a133_synthetic.rs::candidate_passes_real_caps_py_validate_when_platform_available`
//! (env `PF_PLATFORM_DIR`, else `../../../platform` / `../../platform` sibling of `runtime`).
//! Skips gracefully when absent so a bare `cargo test` on a lone runtime checkout still works.

use std::path::{Path, PathBuf};

use pf_collect_ui::canvas::Canvas;
use pf_collect_ui::image::load_png;
use pf_collect_ui::render::{render_frame, FrameState, CANVAS_H, CANVAS_W};
use pf_collect_ui::skin::SkinSet;

fn discover_platform() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("PF_PLATFORM_DIR") {
        let pb = PathBuf::from(p);
        if pb.join("devices/a133/capabilities.toml").is_file() {
            return Some(pb);
        }
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")); // .../runtime/crates/pf-collect-ui
    for rel in ["../../../platform", "../../platform"] {
        let pb = manifest.join(rel);
        if pb.join("devices/a133/capabilities.toml").is_file() {
            return Some(pb.canonicalize().unwrap_or(pb));
        }
    }
    None
}

/// Load the real committed a133 SkinSet — `descriptor_path` is `devices/a133/capabilities.toml`,
/// `skin_root` is the platform tree (its relative `skins/a133/body.png` paths resolve against it).
fn load_a133(platform: &Path) -> SkinSet {
    SkinSet::load(&platform.join("devices/a133/capabilities.toml"), platform)
        .expect("SkinSet::load(a133) should succeed on the committed platform assets")
}

/// `[skin.parts.btn_south]` on the a133 front atlas, hand-cited from
/// `platform/devices/a133/capabilities.toml`. If the toml shifts these, this test rightfully
/// starts asserting against stale rects — the fix is to bump the constants (with a comment),
/// not to weaken the assertion. That drift is exactly what this test exists to catch.
const BTN_SOUTH_FRONT: (i64, i64, i64, i64) = (1271, 245, 55, 56);

/// `[skin.views.top.parts.trig_l]` on the a133 top atlas.
const TRIG_L_TOP: (i64, i64, i64, i64) = (52, 255, 200, 55);

#[test]
fn real_a133_skin_loads_and_maps_engine_ids_to_skin_parts() {
    let platform = match discover_platform() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: no platform checkout found (set PF_PLATFORM_DIR to run the real-asset render test)");
            return;
        }
    };
    let skin = load_a133(&platform);

    // The engine-id -> skin_part map came out of the committed [[inputs]] table.
    assert_eq!(skin.part_for("south"), Some("btn_south"), "engine id 'south' must map to skin part 'btn_south'");
    assert_eq!(skin.part_for("ltrig"), Some("trig_l"), "engine id 'ltrig' must map to skin part 'trig_l'");

    // The atlases decoded to their real 1480x640 dimensions (the current a133 render).
    let (w, h) = skin.size();
    assert_eq!((w, h), (1480, 640), "a133 front body dimensions changed unexpectedly (got {w}x{h})");
}

#[test]
fn compose_south_blits_the_lit_atlas_over_the_btn_south_rect() {
    let platform = match discover_platform() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: no platform checkout found (set PF_PLATFORM_DIR to run the real-asset render test)");
            return;
        }
    };
    let skin = load_a133(&platform);
    let neutral = skin.compose(None);
    let active = skin.compose(Some("south"));

    // Sanity: same-dimension output (compose returns a clone of the front body).
    assert_eq!((neutral.w, neutral.h), (active.w, active.h));

    // At LEAST one pixel INSIDE the btn_south rect must differ between neutral and active — that
    // is the lit-atlas crop actually landing on the composite. If neutral == active everywhere in
    // the rect, either SkinSet::load did not read [skin.parts.btn_south] or the lit atlas was
    // wired to the same file as body (both are real regression modes).
    let (rx, ry, rw, rh) = BTN_SOUTH_FRONT;
    let mut differed = false;
    for yy in ry..ry + rh {
        for xx in rx..rx + rw {
            if neutral.get(xx as usize, yy as usize) != active.get(xx as usize, yy as usize) {
                differed = true;
                break;
            }
        }
        if differed {
            break;
        }
    }
    assert!(
        differed,
        "compose(Some(\"south\")) did not change any pixel inside the [skin.parts.btn_south] rect \
         ({rx},{ry},{rw},{rh}) — the lit atlas did not land over the neutral body"
    );

    // Every pixel OUTSIDE the rect must be identical (proving the blit is scoped to the rect).
    // Spot-check a corner far from any part rect (top-left).
    assert_eq!(neutral.get(0, 0), active.get(0, 0), "outside-rect pixel changed — blit is not rect-scoped");
}

#[test]
fn compose_ltrig_chooses_the_top_view_and_blits_top_lit_atlas() {
    let platform = match discover_platform() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: no platform checkout found (set PF_PLATFORM_DIR to run the real-asset render test)");
            return;
        }
    };
    let skin = load_a133(&platform);

    // Ground-truth atlases loaded independently — this test asserts SkinSet::compose matches them.
    let top_body = load_png(&platform.join("skins/a133/body_top.png")).expect("body_top.png decodes");
    let top_lit = load_png(&platform.join("skins/a133/body_lit_top.png")).expect("body_lit_top.png decodes");
    let front_body = load_png(&platform.join("skins/a133/body.png")).expect("body.png decodes");

    let active = skin.compose(Some("ltrig"));

    // Top view was CHOSEN: find a pixel where top_body and front_body actually differ (a corner may
    // happen to match if both atlases share the same background color there), then assert
    // `compose(Some("ltrig"))` matches top_body at that pixel — proving the top view, not the front,
    // was selected. Scan outside every part rect (front + top) so a highlight blit cannot confound.
    let (tx, ty, tw, th) = TRIG_L_TOP;
    let mut discriminator: Option<(usize, usize)> = None;
    'scan: for y in 0..top_body.h {
        for x in 0..top_body.w {
            // Skip the top trig_l rect — that region carries the lit blit, not the neutral top body.
            let inside_trig_l =
                (x as i64) >= tx && (x as i64) < tx + tw && (y as i64) >= ty && (y as i64) < ty + th;
            if inside_trig_l {
                continue;
            }
            if top_body.get(x, y) != front_body.get(x, y) {
                discriminator = Some((x, y));
                break 'scan;
            }
        }
    }
    let (dx, dy) = discriminator
        .expect("top body and front body are pixel-identical everywhere outside trig_l — the a133 top view atlas may have regressed to a copy of the front");
    assert_eq!(
        active.get(dx, dy),
        top_body.get(dx, dy),
        "compose(Some(\"ltrig\")) at ({dx},{dy}) matched neither the top view (got 0x{:06X}, expected 0x{:06X}) — top view was NOT chosen for a trig_l control (front was used instead)",
        active.get(dx, dy),
        top_body.get(dx, dy),
    );
    assert_ne!(
        active.get(dx, dy),
        front_body.get(dx, dy),
        "compose(Some(\"ltrig\")) at ({dx},{dy}) matched the FRONT body — top-view selection failed"
    );

    // Lit atlas actually landed: at least one pixel inside the top trig_l rect must match top_lit
    // (the crop that was blitted) and differ from top_body (proving a highlight occurred).
    let (rx, ry, rw, rh) = TRIG_L_TOP;
    let mut lit_landed = false;
    for yy in ry..ry + rh {
        for xx in rx..rx + rw {
            let (x, y) = (xx as usize, yy as usize);
            if top_lit.get(x, y) != top_body.get(x, y)
                && active.get(x, y) == top_lit.get(x, y)
            {
                lit_landed = true;
                break;
            }
        }
        if lit_landed {
            break;
        }
    }
    assert!(
        lit_landed,
        "compose(Some(\"ltrig\")) inside the top trig_l rect ({rx},{ry},{rw},{rh}) did not match \
         body_lit_top.png anywhere — the top lit atlas did not blit"
    );
}

#[test]
fn render_frame_composes_a_full_panel_frame_with_real_assets() {
    let platform = match discover_platform() {
        Some(p) => p,
        None => {
            eprintln!("SKIP: no platform checkout found (set PF_PLATFORM_DIR to run the real-asset render test)");
            return;
        }
    };
    let skin = load_a133(&platform);

    // A full 1280x720 canvas, one frame, ltrig prompted (exercises the top-view code path end-to-end).
    let mut c = Canvas::new(CANVAS_W as usize, CANVAS_H as usize);
    let st = FrameState {
        title: "INPUT BRING-UP",
        active_id: Some("ltrig"),
        prompt: "Squeeze the LEFT trigger.",
        index: 13,
        total: 14,
        status: "waiting",
        done: false,
    };
    render_frame(&mut c, &skin, &st);

    // The canvas got PAINTED (not left at solid BG). The BG is rgb(18,20,28); at least one pixel
    // in the middle band must differ — cheap proof that render_frame reached the device blit
    // AND the text overlay against a real-asset skin.
    let bg = pf_collect_ui::canvas::rgb(18, 20, 28);
    let painted = c.pixels().iter().filter(|&&p| p != bg).count();
    assert!(
        painted > 128,
        "render_frame emitted only {painted} non-background pixels — the real-asset skin did not composite"
    );
}
