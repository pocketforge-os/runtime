# Renderer/text spike (`tsp-op5a.32`)

This is a self-contained, non-production comparison of a source-owned CPU renderer
(`cosmic-text` shaping/fallback + swash glyph rasterization + `tiny-skia` pixels) and a
bounded maintained-toolkit option (SDL3 + SDL3_ttf). It deliberately does not alter the
runtime workspace or wire a `FrameHost`.

## Reproduce

From this directory, with Rust 1.85 or newer (the minimum required by the committed
lockfile):

```sh
./run.sh
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
  cargo build --release --target aarch64-unknown-linux-gnu
file target/aarch64-unknown-linux-gnu/release/pf-render-text-spike
```

`run.sh` emits three committed fixtures, benchmark data, checks two independent PNGs
are byte-identical, and prints `PASS`. The renderer takes width and height arguments;
the 1024x600 fixture is the non-720 proof. `scale=2` exercises maximum text size and
reflow. The seven labelled state samples are default, focused, pressed, disabled,
loading, error, and empty.

The benchmark intentionally separates full-screen blend (D-1) from retained-shadow
damage copy (D-2). Times are estimates from the executing offscreen host, not A133
claims. Run a benchmark several times with the CPU governor recorded before comparing.

## Inputs and fallback honesty

The exact Manrope and Fraunces variable fonts and their OFL texts are copied from
`pocketforge-os/design/directions/quiet-console/fonts/`. Cosmic Text performs advanced
Unicode shaping. Those two flagship files do **not** contain CJK glyphs and this spike
does not silently consult system fonts, so the CJK/Hangul portion renders missing-glyph
boxes. Production must either commit an OFL CJK fallback or define intentional tofu;
the former is recommended for F03. Arabic and Devanagari exercise complex shaping but
also depend on glyph coverage in the committed set.

SDL3_ttf was evaluated as the toolkit-class candidate rather than vendored here: it
adds SDL platform/backend ownership, is not installed on the proof host, and cannot
present through the required raw-fbdev + `FBIOPAN_DISPLAY` host without a custom escape
hatch. Reusing this source-owned fixture through such a toolkit would not improve the
decisive host constraint.
