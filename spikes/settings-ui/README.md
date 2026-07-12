# SPIKE — gamepad-navigable settings UI (`tsp-xubv.3`, E4.3)

**A minimal full-screen, gamepad-only settings UI** for the E4 accessibility /
user-preference service (epic `tsp-xubv`, infra-103). It lists the E4 preferences
(`reduceMotion` / `hapticsEnabled` / `monoAudio` toggles + a `brightness` scalar row),
**writes through the merged `.1` settings-authority path** (`pf_prefs::PrefsStore::apply`
— never a private file poke), and proves that a toggle flips **live behavior** via `.2`'s
`PrefsDidChange` observer: a running, already-subscribed app sees its next `pulse()`
no-op the instant you turn Haptics off — **no restart**.

**Prototype-grade by design** (the M1.D supervisor `tsp-iuz.3` is paused): the deliverable
is the working UI + the decided idiom + the store→observer liveness proof, **not**
supervisor/MainUI integration — the same scope boundary E3's consent spike held.

## The idiom is REUSED, not reinvented

This UI shares ONE navigation grammar with the consent dialog
([`../consent-ui/DESIGN.md`](../consent-ui/DESIGN.md) §2), **rotated** from a horizontal
button row onto a vertical row list. [`DESIGN.md`](DESIGN.md) §1 is a rule-by-rule
mapping: focus travels the primary axis (⇧/⇩ here) with **non-wrap guards**; **A** =
the single commit; **B** = back to the safe state; **default focus = the reading-start
anchor** (top row); **focus visual = thick outline + brightened fill, never color alone**;
the same X/Y/stick/trigger/menu **no-op** set. No second grammar is introduced.

## What lands

- [`DESIGN.md`](DESIGN.md) — **the primary deliverable**: the decided interaction model,
  the §1 idiom-reuse mapping (which consent §2 rules apply and how), honest-absent
  rendering (§3), the liveness model (§4), scope non-goals (§5), the queued on-panel
  hardware gate (§6), and the renderer-recipe pin (§7).
- [`settings-render.c`](settings-render.c) — the C prototype. Renders the settings list
  at any `(profile × values × focus)` combination to a PPM via the pinned tsp-osr-safe
  **software** recipe (offscreen `SDL_CreateSoftwareRenderer` + the on-window
  `SDL_CreateRenderer(win,"software")` pin). Honest-absent Haptics row on the a133.
- [`driver.py`](driver.py) — a pure-Python state machine encoding the DESIGN.md §1 nav
  grammar; drives a 16-scenario matrix across **both** device profiles, dumps per-state
  PNGs + a render transcript, and emits an **authority-write intent transcript** (the
  seam to the liveness harness — the nav grammar and the store→observer proof are joined
  by one shared transcript).
- [`liveness/`](liveness) — a **detached** Rust crate (own `[workspace]`, path-deps on the
  merged `crates/pf-prefs` + `crates/pocketforge`; **zero** edits to either) that drives the
  REAL store + observer: the headline "toggle flips a running app, no restart" proof (both
  the external-write→`reload_prefs` path and the control-plane `set_preference` path), store
  round-trip, the brightness scalar (Q3 contract-only), and the a133 honest-absent leg.
- [`run-settings-ui.sh`](run-settings-ui.sh) — end-to-end on modelmaker: build → render +
  nav matrix → nav-grammar assert → liveness → per-PNG `review-screen.sh` verification.
- [`baseline/`](baseline) — PNG evidence + `RESULTS.md` (post-run).
- [`font8x13.h`](font8x13.h), [`ppm2png.py`](ppm2png.py) — copied verbatim from
  `../consent-ui/` (which copied them from `sim/`) so the spike is self-contained.
  Provenance headers name the upstream file; re-copy on upstream change, don't hand-edit.

## Run (on modelmaker)

```bash
# assumes sim/fb/build-sdl3-render.sh has produced /home/mm/sim-build/sdl3-render
ssh mm@10.0.40.90 'cd /home/mm/runtime/spikes/settings-ui && ./run-settings-ui.sh'
```

`make build/settings-render` (prefers the pinned `libSDL3.a`; falls back to
`pkg-config sdl3`), `python3 driver.py`, the nav-grammar assert, `cargo run` in
`liveness/`, then each PNG → `review-screen.sh` (opencode `--file` path — pixels never
`Read` into any context; see the `tsp-visual-inspection` memory).

## What this proves — and the HONESTY CONTRACT

**Proves (logical layer):**
- the DESIGN.md §1 navigation grammar (reused from consent §2) covers the settings state
  space, screen-verified across both profiles including the a133 honest-absent row;
- the UI's writes ride the **authority path** (`PrefsStore::apply`), persist, and are
  read back by an independent handle (round-trip);
- **liveness**: a toggle flips a running subscribed app's next primitive with **no
  restart** — via the external-write→`reload_prefs` seam AND the control-plane seam;
- the brightness scalar round-trips store + observer (Q3 contract-only);
- absence (a133, no motor) and suppression (a523, haptics off) collapse to the SAME
  silent no-op at the primitive, distinguished only by the frozen diagnostic discriminant.

**Does NOT prove** (stays the flash → serial → webcam hardware gate's sole authority,
queued at epic level with explicit owner OK — DESIGN.md §6):
- the on-panel PowerVR/dc_sunxi/DE2.0/fb0 render path (E6/C2's separate pin);
- real-panel legibility / gamepad ergonomics on the physical device;
- a real a523 buzz killed/restored by the panel UI; the a133 Haptics row honest-absent
  **on the panel** — the queued on-panel gate. A follow-on bead is filed + linked at close.
- integration with the M1.D supervisor (`tsp-iuz.3`, paused).

## Sibling coordination

- **Parent:** `tsp-xubv` (E4 Accessibility / User-Preference Service, infra-103).
- **Builds on:** `tsp-xubv.1` (store + `apply`), `tsp-xubv.2` (observer +
  `subscribe_preference`/`reload_prefs`) — both merged; consumed, never modified.
- **Parallel sibling:** `tsp-xubv.4` (sim E2E matrix) — DISJOINT surface
  (`crates/pocketforge/tests/` + the sim repo).
- **Related:** `tsp-xubv.5` (brightness hardware apply leg — hardware-gated follow-on);
  `tsp-an4` (E5 sim — the pinned recipe); `tsp-9sx` (E1 descriptors — presence);
  `tsp-osr` (the on-panel render pin, consumed not solved).
