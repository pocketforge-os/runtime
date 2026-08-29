# F02 renderer/text selection — PASS

**Verdict: PASS; named winner: source-owned `cosmic-text` + swash + `tiny-skia`.** It
is the only candidate that meets the current raw-fbdev/no-KMS host requirement while
keeping semantic scene and shell logic outside a toolkit. SDL3 + SDL3_ttf is rejected
for the production floor because its maintained abstraction does not expose the needed
fbdev pan path; it remains a plausible future Wayland/window host adapter.

These results are **offscreen x86_64 measurements and arm64 build estimates, not A133
performance claims. A133 measurement is pending.** No device or lab host was accessed.

## §12.3 matrix

| Criterion | Source-owned cosmic-text/swash + tiny-skia | SDL3 + SDL3_ttf bounded toolkit |
|---|---|---|
| Deterministic headless output | PASS: two fresh vendored-only 1280x720 runs have identical SHA-256 `dc753c…ec43c` | Feasible with software surface, but backend/version inputs add control surface |
| Image toolchain / Rust 1.85 / build | PASS: pure Rust crate; locked release build reproduced with Rust 1.85.0 (the locked graph's true floor) | C/C++ library and SDL build integration; absent on proof host |
| Licensed committed fonts, UTF-8 shaping/fallback | PASS with honest limit: an empty font database is loaded only with committed OFL Manrope/Fraunces; Cosmic advanced shaping; unsupported scripts/emoji render tofu | HarfBuzz-backed shaping is capable; same committed coverage limitation |
| 200% reflow + seven states | PASS: committed 200% frame and all seven labelled component states | Technically feasible; would duplicate this scene/layout work in toolkit widgets unless restricted to raster host |
| Offscreen + arm64 estimates / later instrumentation | PASS: offscreen executable, cross-build recipe; exact A133 plan below | Offscreen feasible; larger native dependency/cross-build surface |
| Offscreen host | PASS: direct pixmap | PASS: software surface |
| raw fbdev + `FBIOPAN_DISPLAY`, no KMS | PASS by design: pixmap bytes are host-neutral and can be copied/panned by a thin FrameHost | FAIL today: SDL renderer cannot provide the required sunxifb pan contract without custom backend code |
| Future Wayland shm/client | PASS: copy pixmap into wl_shm buffer | PASS: SDL Wayland backend |
| Maintainability / no shell logic in toolkit | PASS: retained semantic scene remains above rasterizer | RISK: widgets invite toolkit-owned focus/layout/state; strict canvas-only usage removes most benefit |

## Fixtures and determinism

| Artifact | SHA-256 |
|---|---|
| `evidence/home-1280x720.png` | `dc753c6ab60bbad54f9e59318053c6fc85f2287c00fc7ec1db96a1d53f1ec43c` |
| `evidence/home-1024x600.png` | `12847e7cc28a9df8055d164af767e6efafea8e8ae6400a9d1d76d99f65608b80` |
| `evidence/home-1280x720-200.png` | `40a4f78cdf9d4ced10492f750aec09ca851d4bdd0abdecc19ca8a1e3c57e6ab1` |

The layout derives all horizontal dimensions from width. At 200%, Cosmic Text wraps
inside the same constraints; this intentionally exposes clipping pressure rather than
shrinking text. The mixed fixture includes Latin accents, Arabic, Devanagari, Japanese,
Hangul, and emoji. The font database starts empty and loads only the two vendored files,
so unsupported Arabic/Devanagari/CJK/Hangul/emoji glyphs render tofu rather than leaking
host fonts. Coverage outside the vendored files is **not claimed**.

## Resource estimates and D-1 / D-2

Proof host: x86_64 Linux, optimized release build. Ten cold render processes completed
in 0.10 s total (~10 ms/render including process/font load/PNG encode); maximum RSS was
12,672 KiB. The isolated benchmark high-water RSS was 24,376 KiB (two source frames,
output, shadow, and two page buffers coexist).

**D-1 two-buffer 1280x720 crossfade:** 20 full frames per alpha step: alpha 0/64/128/
192/255 measured 2.919/2.947/2.958/2.824/2.835 ms per frame respectively. This is a
memory-resident blend estimate, excluding presentation. Estimated ceiling is ~338–354
fps on this x86 host; it does not imply the A133 rate.

**D-2 shelf glide:** the retained shadow + alternating two-page accumulated-union copy
measured 0.050/0.037/0.060/0.089/0.138 ms for current damage bands 16/64/160/320/720 px
(steady accumulated maxima 32/128/320/640/720 px). Thus the copy-only viewport-width
damage rate exceeds 7,200 fps even at full height on this host; actual shelf glide is
paint-bound. The conservative representative full-frame source paint estimate is the
~10 ms end-to-end render above (~100 fps including font load and PNG encode). These two
numbers bracket sustained shelf glide; F03 should cache shaping/glyphs and measure its
real scene rather than treating either bound as a product budget. `bench-x86_64.txt`
contains raw output.

## Exact pending A133 instrumentation plan

Through the sanctioned automation only: build the locked arm64 release artifact on
modelmaker; acquire the `tsp-base` labgrid place; stage it by the named automation
workflow; invoke the spike benchmark under `perf stat -r 20 -e task-clock,cycles,
instructions,cache-misses` and `/usr/bin/time -v`; capture concise stdout via the
automation log collector. Run D-1 at alpha 0,64,128,192,255 for at least 600 frames.
Run D-2 bands 16,64,160,320,720 for at least 1,800 alternating-page frames, adding
per-page checksums after each cold seed and the mixed scroll + independent-damage case.
Record governor, artifact SHA-256, median/p95 ms/frame, fps, per-core CPU, max RSS, and
bytes copied/frame. Repeat the full-shadow-copy baseline. A later device bead must name
the concrete automation entrypoint available then; these are payload commands, not
authorization to bypass it.

## Selection consequences

F03 should consume only the selection: semantic scene above a narrow raster interface;
Cosmic Text owns shaping/layout, swash glyph rasterization/cache, tiny-skia owns CPU
primitives, and FrameHosts own offscreen/raw-fbdev/Wayland presentation. It must add a
committed CJK fallback before claiming CJK coverage. No production wiring is in this spike.
