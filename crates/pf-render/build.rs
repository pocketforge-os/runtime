use serde_json::Value;
use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=../pf-theme/vendor/package/tokens.json");
    let input = fs::read_to_string("../pf-theme/vendor/package/tokens.json").unwrap();
    let json: Value = serde_json::from_str(&input).unwrap();
    let bases = &json["bases"];
    let mut code = String::from("&[\n");
    for (base, variant) in [
        ("dark", "Dusk"),
        ("light", "Day"),
        ("high-contrast", "HighContrast"),
    ] {
        for (key, elevation) in [
            ("--elev-1", "Elev1"),
            ("--elev-2", "Elev2"),
            ("--elev-focus", "Focus"),
        ] {
            let raw = bases[base][key].as_str().unwrap();
            let (offset_x, offset_y, blur, spread, color) = parse_shadow(raw);
            let margin = if color[3] == 0 {
                0
            } else {
                (blur + spread + offset_x.abs().max(offset_y.abs())).ceil() as usize
            };
            let side = margin * 2 + 3;
            let bytes = bake(side, margin, offset_x, offset_y, blur, spread, color);
            code.push_str(&format!("ShadowAsset {{ base: ThemeBase::{variant}, elevation: Elevation::{elevation}, side: {side}, margin: {margin}, rgba: &{:?} }},\n", bytes));
        }
    }
    code.push(']');
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("shadow_assets.rs");
    fs::write(out, code).unwrap();
}

fn parse_shadow(raw: &str) -> (f32, f32, f32, f32, [u8; 4]) {
    if raw == "none" {
        return (0.0, 0.0, 0.0, 0.0, [0; 4]);
    }
    let rgba_at = raw.find("rgba(").unwrap();
    let lengths: Vec<f32> = raw[..rgba_at]
        .split_whitespace()
        .map(|v| v.trim_end_matches("px").parse().unwrap())
        .collect();
    let channels: Vec<f32> = raw[rgba_at + 5..]
        .trim_end_matches(')')
        .split(',')
        .map(|v| v.trim().parse().unwrap())
        .collect();
    let color = [
        channels[0] as u8,
        channels[1] as u8,
        channels[2] as u8,
        (channels[3] * 255.0).round() as u8,
    ];
    (
        lengths[0],
        lengths[1],
        lengths[2],
        *lengths.get(3).unwrap_or(&0.0),
        color,
    )
}

fn bake(
    side: usize,
    margin: usize,
    ox: f32,
    oy: f32,
    blur: f32,
    spread: f32,
    color: [u8; 4],
) -> Vec<u8> {
    if color[3] == 0 {
        return vec![0; side * side * 4];
    }
    let mut mask = vec![0.0f32; side * side];
    let grow = spread.round() as isize;
    let left = margin as isize + ox.round() as isize - grow;
    let top = margin as isize + oy.round() as isize - grow;
    let right = margin as isize + 3 + ox.round() as isize + grow;
    let bottom = margin as isize + 3 + oy.round() as isize + grow;
    for y in top.max(0)..bottom.min(side as isize) {
        for x in left.max(0)..right.min(side as isize) {
            mask[y as usize * side + x as usize] = 1.0;
        }
    }
    if blur > 0.0 {
        let radius = blur.ceil() as isize;
        let sigma = blur / 2.0;
        let kernel: Vec<f32> = (-radius..=radius)
            .map(|x| (-(x * x) as f32 / (2.0 * sigma * sigma)).exp())
            .collect();
        let sum: f32 = kernel.iter().sum();
        let kernel: Vec<f32> = kernel.into_iter().map(|v| v / sum).collect();
        let mut tmp = vec![0.0; mask.len()];
        for y in 0..side {
            for x in 0..side {
                tmp[y * side + x] = (-radius..=radius)
                    .filter_map(|d| {
                        usize::try_from(x as isize + d)
                            .ok()
                            .filter(|p| *p < side)
                            .map(|p| mask[y * side + p] * kernel[(d + radius) as usize])
                    })
                    .sum();
            }
        }
        for y in 0..side {
            for x in 0..side {
                mask[y * side + x] = (-radius..=radius)
                    .filter_map(|d| {
                        usize::try_from(y as isize + d)
                            .ok()
                            .filter(|p| *p < side)
                            .map(|p| tmp[p * side + x] * kernel[(d + radius) as usize])
                    })
                    .sum();
            }
        }
    }
    let mut out = Vec::with_capacity(side * side * 4);
    for alpha in mask {
        out.extend_from_slice(&[
            color[0],
            color[1],
            color[2],
            (alpha * color[3] as f32).round() as u8,
        ]);
    }
    out
}
