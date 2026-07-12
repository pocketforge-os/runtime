# SPIKE — gamepad-navigable settings UI interaction model

**Bead:** `tsp-xubv.3` (E4.3, epic `tsp-xubv`, infra-103).
**Status:** decision doc + prototype + liveness proof. Prototype-grade by design —
the M1.D supervisor (`tsp-iuz.3`) is paused; the deliverable is the working UI +
the decided idiom + the store→observer liveness proof, **not** supervisor/MainUI
integration (the same scope boundary E3's consent spike `tsp-ht0p.1` held).

**Owner rulings this doc binds to** (parent epic `tsp-xubv`, batched 2026-07-11):
- **Q1 WRITER** — the on-panel settings UI writes through the **same** authority
  path the `pf-settings` CLI uses (`pf_prefs::PrefsStore::apply`), never a private
  file poke. Supervisor/MainUI integration is post-M1.D.
- **Q2 STORE** — a single current-state JSON document at `$PF_PREFS_DIR` /
  `$XDG_STATE_HOME/pocketforge/prefs.json`, schema-validated, atomic temp+rename.
- **Q3 BRIGHTNESS** — CONTRACT-ONLY in v1: the brightness row **adjusts + persists +
  is observed**, with **no sysfs apply leg** (a133 has no `/sys/class/backlight`;
  the hardware leg is the hardware-gated follow-on `tsp-xubv.5`).
- **Q4 R-A** — preferences are read-only to apps **by contract, cooperatively
  honored** (permanent); the settings UI is the user's authority surface, not an app
  API.

---

## 1. The idiom is REUSED, not reinvented

This UI shares ONE navigation grammar with the consent dialog
(`spikes/consent-ui/DESIGN.md` §2). The consent dialog is a **horizontal row of
three buttons**; the settings screen is a **vertical list of preference rows**.
The grammar is identical — it is only **rotated onto the layout's primary axis**.
No second navigation grammar is introduced. The table below maps every §2 rule to
its settings-screen application; the C prototype and the Python driver implement
exactly this mapping.

| consent-ui DESIGN.md §2 rule | consent (horizontal) | settings (vertical) — this spike |
|---|---|---|
| **Focus travels the layout's primary axis** | Dpad ⇦/⇨ move focus across the 3 buttons | **Dpad ⇧/⇩** move focus up/down the row list (the axis is rotated; the rule is the same) |
| **Focus wrap guards** ("does NOT wrap"; §2 *Focus wrap*) | ⇦ from Deny / ⇨ from Allow-always are clamped | **⇧ from the top row / ⇩ from the bottom row are clamped** — same non-wrap guard, same overshoot-safety rationale |
| **A (south) = the single confirmatory input** (§2 *Commit semantics*) | A commits the focused button; ledger row written before dismiss | **A commits the focused boolean row's toggle** — the write rides `PrefsStore::apply` (persisted) before the frame updates |
| **B (east) = cancel/back to the safe state** (§2 *Commit semantics*) | B collapses a *pending ask* to the least-privilege Deny | **B exits the settings screen** (back to the caller). There is no pending ask to collapse — each toggle is already committed on A — so B is a plain, safe "leave"; it is the same "cancel = return to the safe caller state" rule with nothing to collapse |
| **Default focus = the reading-start anchor** (§2 *Focus / default*) | Default = Deny = leftmost = reading-start (also least-privilege) | **Default = the top row** = reading-start anchor. No settings row is "dangerous", so there is no least-privilege bias to encode — the reading-start half of the rule is what carries over |
| **Focus visual = thick outline + brightened fill, never color alone** (§2 *Focus visual*) | ~4–6 px outline + brighter fill on the focused button | **Identical**: the focused row draws a thick outline (reused `outline_thick`) + a brightened row fill; shape difference, not color alone (low-vision / color-blind safe) |
| **The orthogonal dpad axis** | ⇧/⇩ are **no-op** in consent (§2 *Physical inputs*) | On the **brightness scalar row only**, the orthogonal axis **Dpad ⇦/⇨** adjusts the value ±`STEP`, clamped to `[0,100]` (the same non-wrap clamp philosophy). On boolean rows ⇦/⇨ are no-op. This is the natural, consistent use of the axis consent reserved |
| **Inputs deliberately NOT used** (§2 *Inputs deliberately NOT used*) | X/Y, analog sticks, triggers, home/menu/start/select = no-op | **Identical** — same set, same rationale (two decisive semantics only: commit + back) |
| **Terminal-after-commit** (§2 *Multi-ask sequencing* / driver.py) | after commit the dialog dismisses; later inputs no-op | Settings has no single terminal commit (it is a live editor); instead **each A/adjust is individually committed-and-persisted**, and **B is the terminal "leave"** — after B the screen is dismissed and further inputs no-op |

**Divergences, stated honestly** (so a reviewer sees they are deliberate, not drift):
1. **Axis rotation** — consent's primary axis is horizontal, settings' is vertical.
   The grammar (move-focus / non-wrap / A-commit / B-back) is unchanged.
2. **B has nothing to collapse** — consent's B is a *decision on a pending grant*
   (collapse → Deny); settings has no pending grant (toggles commit on A), so B is a
   plain exit. Same "cancel returns to the safe caller state" invariant.
3. **Live editor, not a one-shot modal** — consent commits once then dismisses;
   settings commits per-row and dismisses only on B. This is why the scalar row can
   use the orthogonal ⇦/⇨ axis that consent left reserved.

---

## 2. What the screen says

```
                         Settings

  Reduce motion                                   [ OFF ]
  Haptics                                         [ ON  ]
  Mono audio                                      [ OFF ]
  Brightness                            [####------]  40

  Up/Down: move    A: toggle    L/R: adjust    B: back
```

- **Title** — `Settings`, centered, scale-5 bitmap font (VLM-legible at 1280×720).
- **Rows** — one per schema key (`pf_prefs::SCHEMA` order: `reduceMotion`,
  `hapticsEnabled`, `monoAudio`, `brightness`). Left-aligned human label + a
  right-aligned value widget:
  - **Boolean rows** → a pill reading `[ ON ]` / `[ OFF ]` (bright fill when ON, dim
    when OFF — shape + text, not color alone).
  - **Brightness row** → a 10-cell bar `[####------]` plus the numeric `0..100` — the
    Q3 contract-only scalar.
- **Focused row** — thick outline (`outline_thick`, reused verbatim from
  `consent-render.c`) + brightened row band.
- **Hint line** — the input legend, mirroring consent's hint line.

## 3. Honest-absent rendering (the unification's presence half)

The `hapticsEnabled` row's **presence** is read from the **E1 capabilities
descriptor**, never fabricated (epic invariant; `tsp-9sx`). On a device whose
descriptor has **no rumble actuator** (the base **a133** — its descriptor omits the
`rumble` row), the Haptics row renders **honest-absent**:

- drawn **greyed** (dim label, the value widget replaced by `— unavailable`),
- **skipped in focus traversal** — an absent capability is not a focus stop, because
  there is nothing to toggle. ⇧/⇩ move past it; A can never land on it.

This is the **presence half** of the E4 unification ("suppression ≡ absence"): on the
a133 the row is *absent* (no motor); on the a523 with `hapticsEnabled=false` the row
is *present but off* (suppressed). The **panel** shows the honest difference; the
**app-visible primitive** collapses both to the same silent no-op (proved in §4 and in
`.2`'s merged unification test). The settings UI never invents a toggle for hardware
that isn't there.

## 4. Liveness — the store writes flip live behavior (no restart)

The renderer (§2) proves the *shape*; the **`liveness/` Rust harness** proves the
*behavior* — that a toggle in this UI flips a **running, already-subscribed** app
without restarting it, exactly the owner's "turn it off mid-session" case. It builds
on the merged `.1` store + `.2` observer (`crates/pf-prefs`, `crates/pocketforge`),
touching **zero** of their files, via a **detached** crate (its own `[workspace]`
table + path deps — no root `Cargo.toml`/lock churn).

The harness models the settings UI as a **separate process from the backend** (which
it is in this prototype: C/Python render vs Rust facade), so it exercises **both**
write paths the `.2` honesty rider ratified:

1. **External-process write → reload seam (the prototype's true path).** The UI writes
   through `PrefsStore::apply("hapticsEnabled", Bool(false))` — the *same* authority
   seam the `pf-settings` CLI uses (Q1). A running app subscribed via
   `subscribe_preference("hapticsEnabled")` sees nothing until the host calls
   `reload_prefs()` (the v0 supervisor-file-watch stand-in — its docstring literally
   names "the `.3` UI, running in a separate process"). On reload: `PrefsDidChange`
   fires **and** the app's next `vibration().pulse()` flips `Fired → NoopSuppressed`,
   **no restart**. Toggle back → `NoopSuppressed → Fired`.
2. **Control-plane write (the supervisor-integrated future).** When the supervisor
   later owns both the UI and the backend in one process, the write is
   `set_preference(...)`, which fires the observer **directly**. The harness asserts
   this path too so the seam is proven ahead of that integration.

The harness also asserts:
- **store round-trip** — the value persists to disk and an independent handle reads it
  back (what `pf-settings get` sees);
- **brightness scalar** — a `Scalar` adjust round-trips store + observer (Q3
  contract-only: no hardware effect asserted, by design);
- **a133 honest-absent** — the same toggle sequence on an a133-shaped backend leaves
  the pulse at `NoopAbsent` (the row was never a focus stop; the descriptor has no
  motor), the primitive collapsing absence and suppression to one silent no-op.

Every assertion prints a machine-readable line; any failure exits non-zero.

## 5. What this spike deliberately does NOT do

- **No supervisor / MainUI integration** — `tsp-iuz.3` is paused. Prototype-grade.
- **No edits to `crates/*`, `docs/*`, or `spikes/consent-ui/*`** — this bead's whole
  surface is NEW files under `spikes/settings-ui/`. The store + observer are consumed
  as merged (`.1`/`.2`), never modified.
- **No on-panel render** — the on-panel PowerVR/dc_sunxi recipe is E6/C2's scope. This
  spike consumes the sim's **software** tsp-osr-safe recipe (`SDL_CreateRenderer(win,
  "software")`, non-OPENGL window), never re-derives it.
- **No brightness hardware apply** — Q3 contract-only; the sysfs leg is `tsp-xubv.5`.
- **No new wire/ABI surface** — nothing added to `pf-wire` or `abi/`; the harness uses
  only the merged public Rust API.

## 6. ON-PANEL confirmation — a QUEUED HARDWARE GATE

Rendering + navigating this UI on the real panels (a real **a523** buzz killed and
restored by toggling Haptics on the panel; the **a133** Haptics row honest-absent on
the panel) is a **queued hardware gate requiring the owner's explicit OK** — it is
**never a close blocker** (epic constraint window; matches the `tsp-xubv.5` /
`tsp-ht0p.1` pattern). A follow-on bead is filed + linked at close. Evidence path when
it fires: `capture-screen.sh --cam {tsp|tsp-s}` → `review-screen.sh`.

## 7. Renderer recipe pin

Consumes the tsp-osr-safe recipe from `pocketforge-os/sim` (bead `tsp-an4`), same as
`consent-render.c`: the offscreen `SDL_CreateSurfaceFrom` → `SDL_CreateSoftwareRenderer`
path (headless PPM dump) plus the on-window non-`OPENGL` + `SDL_CreateRenderer(win,
"software")` pin (`pin_tsp_osr_recipe`, called on startup so an on-window-recipe
regression is caught immediately). **We do NOT re-derive the recipe** — sim owns it; a
pin change updates only this spike's build script/comments. The on-panel EGL/PowerVR
recipe is E6/C2's separate pin.

## 8. Sources

- Parent epic `tsp-xubv` — OWNER RULINGS (Q1–Q4) + `.2 DESIGN RATIFIED` (the
  `reload_prefs` seam + honesty rider) comments.
- `spikes/consent-ui/DESIGN.md` §2 — the navigation grammar this doc reuses.
- `crates/pf-prefs/src/{schema,store,prefs}.rs` — the schema (row list, brightness
  range), the `apply` authority seam, the store.
- `crates/pocketforge/src/backends/inproc.rs` — `subscribe_preference`,
  `set_preference`, `reload_prefs`; `crates/pocketforge/tests/prefs_change_event.rs`
  — the observer + primitive-honoring template this spike's harness mirrors.
- `crates/pocketforge/tests/fixtures/{a133,a523}-capabilities.toml` — the E1
  descriptors (a133 omits `rumble`) the honest-absent leg reads.
- `pocketforge-os/sim/fb/README.md` — the pinned tsp-osr-safe recipe.
- `runtime/STABILITY.md` — the frozen-v1 constraint the harness respects (public API
  only; no wire/ABI touch).
</content>
