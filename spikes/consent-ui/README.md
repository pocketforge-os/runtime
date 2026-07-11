# SPIKE-2 — consent-UI-on-gamepad (`tsp-ht0p.1`)

**PROVE FIRST.** Gates the E3 consent flow's UI shape: on a gamepad-only,
single-fb-writer panel with no keyboard and no touch, can we run a legible
permission consent dialog with unambiguous nav semantics? This spike answers
it with a **decided interaction model**, a **working prototype** driven under
the E5 sim's tsp-osr-safe software recipe, and a **concrete seam contract**
handed to sibling `tsp-ht0p.3`.

## What lands

- [`DESIGN.md`](DESIGN.md) — the decided interaction model (dialog layout,
  gamepad navigation semantics, grant-scope options, v0 launch/app-switch
  boundary per owner Q4 ruling, deferred mid-run options with a recommendation)
  and the **supervisor-ask seam shape** for `.3` (§4 — concrete request/response
  fields, invariants, non-goals). This is the primary deliverable.
- [`consent-render.c`](consent-render.c) — the C prototype. Uses the pinned
  tsp-osr-safe recipe from `pocketforge-os/sim @ 74ddfbc`
  (`sim/fb/README.md`): both the offscreen `SDL_CreateSoftwareRenderer(surface)`
  path (for the PPM dump) and the on-window `SDL_CreateRenderer(win, "software")`
  pin (proves the recipe the M1.D supervisor will use). Renders the
  `{APP} wants to use {RESOURCE} — [Deny] [Allow once] [Allow always]` dialog
  at any (focus × state) combination.
- [`driver.py`](driver.py) — a pure-Python state machine that encodes DESIGN.md
  §2 verbatim (dpad-left/right focus with non-wrap; A=commit; B=cancel→Deny;
  everything else no-op) and drives the C prototype through a 12-scenario
  matrix that covers every state the DESIGN commits to.
- [`run-consent-ui.sh`](run-consent-ui.sh) — end-to-end runner on modelmaker:
  build, render the matrix, verify each PNG via `review-screen.sh` (screen-reviewer
  agent — pixels never enter the main-loop context).
- [`baseline/`](baseline/) — PNG evidence + `RESULTS.md` (post-run).
- [`font8x13.h`](font8x13.h), [`ppm2png.py`](ppm2png.py) — copied from `sim/` so
  the spike is self-contained under `runtime/`. Provenance headers name the
  upstream file; re-copy on upstream change, don't hand-edit either copy.

## Run (on modelmaker)

```bash
# assumes sim/fb/build-sdl3-render.sh has already produced /home/mm/sim-build/sdl3-render
ssh mm@10.0.40.90 'cd /home/mm/runtime/spikes/consent-ui && ./run-consent-ui.sh'
```

Under the hood: `make build/consent-render` (Makefile prefers the pinned
`libSDL3.a` in `$SDLR/x86/lib/`; falls back to `pkg-config sdl3` on hosts
without the sim build), `python3 driver.py` (renders 12 PPMs, converts to
PNG), then each PNG is routed to `review-screen.sh` (opencode agent path —
image pixels via `--file`, never `Read` — see `tsp-visual-inspection` memory).

## What this proves — and the HONESTY CONTRACT

**Proves (logical layer):** the tsp-osr-safe **software** recipe is enough to
render a legible, dpad-navigable consent dialog; the DESIGN.md §2 interaction
model exhaustively covers the state space (12 scenarios, all rendered and
screen-verified); the seam contract (§4) is concrete enough for `.3` to
implement the broker/ledger without another design round-trip.

**Does NOT prove** (stays the flash → serial → webcam hardware-gate's sole
authority, queued at epic level):
- the on-panel PowerVR/dc_sunxi/DE2.0/fb0 render path — `tsp-osr` (open), the
  E6/C2 worker's separate pin
- real-panel legibility (bitmap font at 1280×720 on the actual TFT screens)
- gamepad ergonomics on the physical device (analog stick drift, dpad diagonal
  ghosting, etc.)
- integration with the M1.D supervisor (`tsp-iuz.3`, paused) — that's `.3`+M1.D
- integration with `pf-broker` — that's `.3`, using the §4 seam contract as
  input

The last item is the intended handoff. `.3` reads DESIGN.md §4, implements the
seam in the merged `crates/pf-broker` on the additive path (STABILITY.md v1
frozen), wires the AppOps ledger, and calls into the (still-paused) supervisor
placeholder. The consent UI rendering is factored into the shared runtime piece
`.3` will land; this spike is the shape reference, not the code.

## Sibling coordination

- **Parent:** `tsp-ht0p` (E3 Permission/Consent Model, infra-102)
- **Blocks:** `tsp-ht0p.3` (runtime consent flow + AppOps ledger portal)
- **Related:**
  - `tsp-ht0p.2` KEYSTONE — protection tiers + `use=[]` schema + validator (extends
    the merged v0 `manifest.rs`); the seam §4 `resource`/`allowed_scopes` fields
    are defined in terms of the tier vocabulary `.2` lands
  - `tsp-osr` (open) — the SDL3 render segfault; this spike consumes the pin
    from `sim/fb/`, does not solve on-panel
  - `tsp-an4` (E5 sim, closed) — supplies the pinned recipe + control_surface API
  - `tsp-iuz.3` (M1.D supervisor, paused) — will DRAW consent on-panel using
    this spike's recipe + seam shape
  - `tsp-fr2n` (E6 epic, in progress via a parallel coordinator) — E6's C7
    worker will edit `sim/control/control_surface.py` after this spike; a
    claim-broadcast + second-lander-rebases etiquette is agreed
