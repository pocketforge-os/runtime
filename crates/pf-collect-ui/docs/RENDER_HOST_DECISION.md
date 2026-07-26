# Render-host decision — pf-collect-ui (tsp-bwrg.3)

**Decision: a NEW minimal renderer — a pure-fbdev (`/dev/fb0`) software renderer in Rust,
in-process linking the `pf-input-collect` engine.** SDL3 + the tsp-osr recipe-B is documented
here as the *fb-down fallback*; it is not the primary path.

This is the "make and justify the render-host decision" required by the bead. The two options the
bead named were (a) a new minimal SDL3/fbdev renderer, or (b) reuse pf-hwprobe's renderer.

## Why NOT (b): reuse pf-hwprobe

pf-hwprobe (`pocketforge-os/pf-hwprobe`, C/SDL3) is the descriptor-driven **verify** UI (job #3,
epic `tsp-fr2n` / `tsp-wbd6`). It is the wrong shape for guided collection:

- **It is architecturally a sim-harness SLAVE.** `main()` opens externally-created `req`/`resp`
  FIFOs (no `O_CREAT`) and its whole loop is an external-command server
  (`pf-hwprobe/src/main.c:1025-1078`). There is no autonomous loop and no on-device host. A guided
  prompt sequence is an *autonomous* driver; bolting a mode onto that FIFO server was explicitly
  ruled out by the bead ("do NOT silently add a mode to pf-hwprobe").
- **It is descriptor-driven.** It renders *from* a `capabilities.toml`. Guided collection has NO
  descriptor — it is *building* one. So pf-hwprobe's renderer has nothing to render from during
  collection.
- **Language / linking mismatch.** The engine (`pf-input-collect`) is a Rust rlib exporting no C
  ABI. Reusing pf-hwprobe would force a C-ABI bridge across the exact boundary a same-language
  in-process link makes trivial.
- **Its reusable widgets are for a target SKIN.** The `widget_*.c` units draw a specific device's
  skin; collection wants a *generic* controller reference (the shape `default_gamepad_plan`
  intends), not a device skin.

pf-hwprobe stays job #3 (verify); its on-panel SDL proof is owned by `tsp-fr2n.8`. Different concern.

## Why pure-fbdev (over SDL) within option (a)

1. **Fewest dependencies → it runs during bring-up.** The tsp-osr pin in
   `pf-hwprobe/src/main.c:876-878` records the load-bearing fact: on sunxifb **SDL's software
   renderer is not compiled in**, so *every* `SDL_Renderer` on-device is the PowerVR GLES2 path.
   An SDL tool is therefore hard-coupled to the fragile PowerVR blob. A bring-up / calibration tool
   must be able to run *before / independent of* the GPU stack — libc-only fbdev has no such
   coupling.
2. **Single static aarch64-musl binary.** No SDL, no dynamic GPU linkage — one
   `aarch64-unknown-linux-musl` static artifact, matching the `pf-hw-exerciser` / `pf-input-collect`
   precedent and the OTA-first "push one binary" loop (`pf-app-deploy.sh`, tsp-bwrg.4). SDL would
   drag dynamic linking against on-device SDL3 + PowerVR — the opposite of an OTA-pushed ephemeral
   binary.
3. **It sidesteps tsp-osr entirely** — tsp-osr is an SDL-on-sunxifb window-creation segfault; with
   no SDL window there is nothing to segfault. Acceptance "tsp-osr handled" is satisfied by
   avoidance, with the SDL recipe kept documented + available (below) for the fb-down case.
4. **Backend-agnostic `Canvas` → honest off-device proof.** The same draw code fills an in-memory
   `Canvas` whether the sink is `/dev/fb0` or a PPM file, so the exact pixels the panel shows are
   validated headless by the screenshot reviewer *before* the device is touched (the sim's
   recipe-C insight, generalized).

## The fb-down fallback (SDL3 + tsp-osr recipe-B)

If a target ever has the GPU up but `/dev/fb0` unavailable, the documented fallback is SDL3 with
**tsp-osr recipe-B**: create the window WITH `SDL_WINDOW_OPENGL` (so SDL core loads EGL and the
sunxifb backend creates a valid surface even on an unpatched `libsdl3-sunxifb`), then
`SDL_CreateRenderer(window, NULL)` selects GLES2 bound to that surface — no NULL-EGL deref. The
owned-source fix (recipe-A, patch `SUNXIFB_CreateWindow` to always create the EGL surface) is the
durable form. This renderer is intentionally *not* built here — it is the escape hatch, and the
`Canvas` abstraction is deliberately thin so an `SdlSink` could be added without touching the draw
code. On the A133 today `/dev/fb0` is present and the fbdev path is primary.

## fb0 single-owner note (on-device leg)

`/dev/fb0` is single-owner on the image: the boot-animator / `pocketforge-menu` hold it, arbitrated
by systemd `Conflicts=`/`After=`. The on-panel deploy (`pf-app-deploy.sh --unit`) installs
`deploy/pf-collect-ui.service`, which `Conflicts=` the sibling fb-owning units so systemd stops the
menu before the wizard takes fb0, and restores it after. Headless PPM render validation needs none
of this.
