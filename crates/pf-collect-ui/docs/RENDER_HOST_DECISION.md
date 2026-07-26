# Render-host decision — pf-collect-ui (tsp-bwrg.3)

**Decision: the FACE is the real device's 3D model, CONSUMED from the platform `.scad`->skin chain's
committed, drift-gated assets — rendered by a pure-fbdev software sink.** No hand-drawn face, no
SDL/GPU, no re-generation of the model here.

This records the "make + justify the render-host decision" the bead requires. It has two layers: the
**face source** (what the device looks like) and the **render sink** (how pixels reach the panel).

## Face source: CONSUME the gated `.scad`->skin assets (owner-directed 2026-07-26)

The owner directed that the on-panel face be **the real device's 3D model**, not a hand-drawn
diagram ("move away from these hand drawings"). That model already exists and is fully tooled in
`pocketforge-os/platform`:

- `platform/device-models/trimui-smart-pro/trimui-smart-pro.scad` — the canonical 1:1 device model,
  purpose-built for input highlighting (`HIGHLIGHT` param lights any control red; `CONTROL_IDS` map
  1:1 to the engine plan). `render.py` renders it (via OpenSCAD) into skin assets.
- The committed, gated skin assets for the a133: `platform/skins/a133/{body,body_lit,body_top,body_lit_top}.png`
  + `platform/devices/a133/capabilities.toml` `[skin]` / `[skin.parts]` (14 control rects) /
  `[skin.views.top]` + `[skin.views.top.parts]` (the top-edge shoulders/triggers).

**This app CONSUMES those assets; it does not generate them and does not invoke `render.py`.** It is
the pf-hwprobe highlight model: draw the neutral `body`, then blit the active control's crop from the
`lit` atlas over its `[skin.parts]` rect. Top-edge controls (`l1`/`r1`/`ltrig`/`rtrig`) use the
`[skin.views.top]` atlas so the trigger — a mere sliver from the front camera — is prominent (this
app is the **first runtime consumer of `[skin.views.*]`**). Rects are read from the descriptor,
never hardcoded (`skin.rs`).

### Why consume, not generate (one source of truth)

The `.scad -> body/lit/[skin.parts]` chain is guarded by a **merged, enforcing CI drift gate**
(`tsp-65jc.7`, `platform/device-models/check-skin-drift.py`): it asserts the committed skin PNGs +
rects stay in lockstep with the `.scad` (via `skins/<dev>/model-render.json` sha manifest + the
`[skin.parts]` table). Building a *second* generator (a parallel `render.py` in runtime) is exactly
the drift that gate exists to catch. Consuming the already-gated assets means there is **one
generator** (platform's `render.py`) and **one source of truth**; runtime is a read path.

### Auto-rebuild is inherent — but the deploy/bake must ship the CURRENT skin

Because the app reads the gated assets rather than embedding a copy, a `.scad` geometry fix
propagates automatically: platform's gated chain regenerates `body/lit/parts`, and this app reads the
latest — **provided the deploy/bake ships the CURRENT platform skin, not a stale pinned copy.** That
is the one link that makes "auto-rebuild" real end to end:

- **OTA deploy** (`pf-app-deploy.sh`): ship the current `platform/skins/<dev>/` + descriptor
  alongside the binary and point `--descriptor`/`--skin-root` at them.
- **Image bake** (tsp-bwrg.7): stage the current platform skin into the image; do not freeze an old
  copy.

(The fully-automatic cross-repo `platform .scad change -> runtime` CI is deliberately out of scope
for this bead — the reproducible one-source-of-truth consumption above satisfies the owner's
auto-rebuild intent; a standalone cross-lane bead tracks the automatic trigger.)

## Render sink: pure-fbdev software (GPU-independent), NOT SDL / live 3D

The composited device image (RGB) is blitted to `/dev/fb0` by a **libc-only software fbdev sink**
(`fbdev.rs`), color-keying the model's solid background so the device floats on the dark UI. Decode
is pure-Rust (`png`) + `toml`/`serde` for the descriptor — so the on-panel binary is a single static
`aarch64-unknown-linux-musl` artifact with **no SDL, no GPU, no PowerVR** dependency.

- It **runs during bring-up before the GPU stack** — the whole point of a bring-up/calibration tool.
- It **sidesteps tsp-osr** entirely (tsp-osr is an SDL-on-sunxifb window-creation segfault; with no
  SDL window there is nothing to segfault). The tsp-osr pin
  (`pf-hwprobe/src/main.c:876-878`) records that on sunxifb *every* `SDL_Renderer` is the PowerVR
  GLES2 path, so an SDL renderer would hard-couple this tool to the fragile GPU blob.
- **Live / interactive 3D on-device** (rendering the `.scad` model live via GLES) was considered and
  **deferred** (owner-ratified 2026-07-26): pre-rendered now, full 3D later — filed as **tsp-bwrg.11**
  (blocked on the GPU re-validated on a stable kernel + tsp-osr; the current kernel's `disp`
  regression makes live-GLES non-viable regardless). Pre-rendered consumption is the ratified path.

A backend-agnostic `Canvas` fills the SAME pixels whether the sink is `/dev/fb0` (on-panel) or a PPM
file (`dump`, off-device) — so the render is validated headless by the screenshot reviewer before the
device is ever touched.

## Why NOT pf-hwprobe as the renderer

pf-hwprobe (`pocketforge-os/pf-hwprobe`, C/SDL3) is the descriptor-driven **verify** UI (job #3, epic
`tsp-fr2n` / `tsp-wbd6`) and is architecturally a sim-harness **slave**: `main()` opens external
`req`/`resp` FIFOs and its whole loop is an external-command server
(`pf-hwprobe/src/main.c:1025-1078`) — the wrong shape for an autonomous prompt sequence. This app
therefore has its own autonomous loop and its own libc-only fbdev sink, but **reuses pf-hwprobe's
skin-asset consumption model** (neutral body + lit-atlas crop over the `[skin.parts]` rect), so both
consume the one gated source of truth. pf-hwprobe's own on-panel SDL proof stays `tsp-fr2n.8`.

## fb0 single-owner note (on-device leg)

`/dev/fb0` is single-owner on the image: the boot-animator / `pocketforge-menu` hold it, arbitrated by
systemd `Conflicts=`/`After=`. `deploy/pf-collect-ui.service` `Conflicts=` the sibling fb owner so
systemd stops the menu before the wizard takes fb0, and restores it after. Headless PPM render
validation needs none of this.
