# SPIKE-2 — consent-UI-on-gamepad interaction model

**Bead:** `tsp-ht0p.1` (E3 SPIKE-2, epic `tsp-ht0p`, infra-102).
**Status:** decision doc + prototype. Bindings for the sibling implementation bead `tsp-ht0p.3` (runtime consent flow + AppOps ledger portal).
**Owner ruling this doc binds to (parent epic Q4, 2026-07-11):** v0 consent = LAUNCH / APP-SWITCH BOUNDARIES ONLY. Mid-run consent (over a running frame) is deferred post-substrate. This spike prototypes the dialog + navigation boundary-agnostically, and this doc records the three mid-run options with a recommendation.

---

## 1. What the dialog says

### Prompt shape

```
{APP_NAME} wants to use {RESOURCE}

[ Deny ]  [ Allow once ]  [ Allow always ]
```

- **Prompt line** — "`{APP_NAME}` wants to use `{RESOURCE}`". Reasoning:
  - Uses the verb *wants*, not *is requesting* or *has requested*, so the user reads it as an intent (declinable) not a fait accompli.
  - Puts the app name FIRST (subject) — the user has to identify which app before caring about the resource. This matches how gamepad users triage stacked UI: read left, decide, don't scan.
  - Uses the RESOURCE in caps (LOCATION / CAMERA / MICROPHONE / EGRESS / …) as the salient noun. The Latin case difference makes it stand out from the sentence and pattern-matches "system-level" UX conventions (Android's per-permission dialogs also emphasize the resource noun).
- **Optional purpose string** (v0.5, not required): a second line with the manifest-declared purpose (`use = [ "location", purpose = "show nearby weather" ]`). If the manifest supplies one, it is rendered as a smaller line below the prompt. If not, it is omitted. This spike does not require the purpose line; the seam contract §4 carries an optional `purpose` field so `.3` can start emitting it whenever the schema lands (`.2` KEYSTONE).
- **NO app-icon in v0.** Icon rendering pulls in signed-artifact provenance (which is a separate epic — E4/tsp-6ky, tsp-ozbp) and a font/glyph pipeline the supervisor does not yet own. Adding it now is out-of-scope. Anchored solely on `APP_NAME` (from the signed manifest identity block).

### Grant-scope buttons

Three, in **fixed left-to-right order**: `[ Deny ]  [ Allow once ]  [ Allow always ]`.

- **`Deny`** = no grant for this operation, this session. Ledger records a deny event. On the NEXT operation attempt (`.3` behavior, not this spike), the dialog re-prompts — a `Deny` is *not* remembered ("Don't ask again" is deliberately absent in v0; users who want the app not to ask them just uninstall / don't grant).
- **`Allow once`** = a single-use grant for the operation that triggered the ask. The AppOps ledger row is written with `scope=once`, marked `consumed=false`; the next capability call consumes it and marks `consumed=true`. Subsequent calls re-prompt.
- **`Allow always`** = persistent grant for the `use=[]` entry that triggered the ask. Ledger row is `scope=always`, revocable via the settings UI (`.3` — not this spike). Persists across app restarts and reboots (backed by the ledger store).

**Order rationale — decreasing risk right-to-left:** left is safest, right is most permissive. The user's default *reading* motion is left-to-right; the leftmost button being `Deny` means the user reads "Deny" first — a small psychological reinforcement of the least-privilege default. This is also the order Android used before its 2020 mid-run consent revamp; it maps cleanly to the muscle memory of anyone who has already met Android's runtime prompts.

### What's absent from v0

Documented so we notice if a later change tries to add them without discussion:

- **No "Don't ask again" checkbox.** Deny remembers as a single deny event, not a persistent "muted" state. A hostile app cannot exhaust the user into flipping a "don't ask" toggle it can then hide behind.
- **No timeout / auto-dismiss.** The dialog persists until the user decides. Rationale: auto-dismissing to a *default* is either a silent grant (unacceptable) or a silent deny (fine but confusing — the user opens the app tomorrow and can't remember why it doesn't work). Boundary consent is not a hostile modal (the app is at launch or app-switch, nothing is being interrupted) so persistence is fine.
- **No icon, no app screenshot, no scary red banner.** v0 is text-only. UX polish (icon, brand color, danger accent) rides on top later without changing the interaction model.

---

## 2. Gamepad interaction model

### Physical inputs used

- **Dpad ⇦ / ⇨** — move focus left / right across the three buttons.
- **A (south)** — commit the currently focused button. This is the single confirmatory input.
- **B (east)** — cancel. Under selection-IS-grant semantics (LOCKED decision §3, refined §R-A), *cancelling* is not a null-op; it collapses to the least-privilege outcome, i.e. **Deny**. The ledger row written is identical to what pressing A on `Deny` would produce, with an `input=B` marker in the audit tail for accountability.
- **Dpad ⇧ / ⇩** — **no-op in v0**. All three buttons live on one horizontal row. Reserved for later flow extension (e.g. an expandable "Details" panel; see §5 deferred).

### Inputs deliberately NOT used

- **Face buttons other than A/B (X/west, Y/north)** — no-op. Rationale: only two decisive semantics — commit and cancel — so we bind them to the two universally-recognized buttons. Overloading X or Y with "Allow always" shortcut, etc. is tempting but silently breaks the model (a user pressing X hoping to grant is instead confused; a user pressing X hoping to skip risks granting).
- **Analog sticks** — no-op. Consent is a discrete choice; sticks would require deadband handling, direction↔focus mapping, and drift tolerance for zero gain.
- **Triggers L1/L2/R1/R2** — no-op. Same reason.
- **Home / menu / start / select** — no-op **at this layer**. The supervisor may swallow home at a higher layer to prevent the app from being backgrounded during consent (see §3 fb-writer discussion), but that is *supervisor* policy, not consent-UI input semantics.

### Focus / default / focus visual

- **Default focus (initial) = `Deny`.** Least-privilege default: a user who spams A through consent dialogs (habit from other UIs, or accidental) grants nothing. This is the safest failure mode when the user is not paying attention. Documented on-screen through the visual focus ring, not through any "recommended" language on the button itself (the button labels stay symmetric).
- **Focus visual:** the focused button has a **thick outline** (~4 px at 1280×720) and a **brightened fill** vs the two unfocused buttons. The focus ring color is high-contrast on both light and dark rendered backgrounds. We do NOT rely on color alone: the outline width is a shape difference too, so a color-blind or low-vision user still sees it. (v0.5 could add a small ▶ prefix on the focused button label for further belt-and-suspenders; not required.)
- **Focus wrap:** dpad-right from `[Allow always]` **does NOT wrap** back to `Deny`. Dpad-left from `Deny` **does NOT wrap** to `[Allow always]`. Rationale: wrap would let a user overshoot, wrap into `[Allow always]`, and grant accidentally. Non-wrap makes the physical layout match the mental model: focus can only travel between fixed endpoints, and to reach `Allow always` you must intentionally traverse the whole row.

### Commit semantics

- **A on focused button = irrevocable grant/deny event for this ask.** The ledger row is written before the dialog is dismissed and before control returns to the app. If the ledger write fails, the dialog stays up with an error banner (spike §5 deferred). The app is *never* told "consent was granted" before the ledger has stored it.
- **After commit, dialog dismisses; supervisor draws the app's next frame** (or the launcher, if the ask was at app-switch). The commit action IS the answer to the seam call — see §4.
- **B on any focused state = irrevocable deny event.** No confirmation. Rationale: cancelling is safe; adding a confirmation would encourage the user to just press A to escape, which is the opposite of what we want on an "am I being asked for something?" screen.

### Multi-ask sequencing (not this spike — noted for `.3`)

If a launch requires two capabilities (e.g. `use = ["location", "egress:api.weather.com"]`), the manifest validator (`.2` KEYSTONE) MAY batch them into one consent screen with a two-line summary OR present them as sequential asks. `.3` decides which. This spike prototypes one-ask-at-a-time; the seam contract in §4 is scalar-per-ask, wrappable by `.3` into either flow.

---

## 3. Single-fb-writer reconciliation (Q4 ruling + deferred options)

The panel has ONE framebuffer writer. The E5 sim proves the tsp-osr-safe recipe off-hardware; the E6/C2 worker will pin the on-panel PowerVR/EGL equivalent. Neither pin removes the *systems* fact that if an app owns fb0 and needs consent for a mid-run operation, *someone* has to reclaim fb0 to draw the dialog.

### v0 answer — owner Q4 ruling — LAUNCH / APP-SWITCH boundary only

The v0 supervisor answers this by refusing to raise mid-run consent at all. Consent asks land at moments the **supervisor already owns fb0**:

- **App launch** — the supervisor is drawing the launch splash; the app has not yet been handed fb0. Any `use=[]` capability the manifest declares and the ledger has not granted is prompted for HERE, in a batched sequence if there are multiple, before the app is even started. This is the primary flow.
- **App-switch** — the user backs out of App A into the launcher, then opens App B; between A being suspended and B being resumed, the supervisor holds fb0 again. If B has an unsatisfied capability, prompt HERE.

**Consequence for `.3`:** the manifest validator MUST enforce `use=[]` at launch-time (which is already the case in the merged `pf-broker/manifest.rs` — the E3 co-design hook), and the runtime MUST call the supervisor-ask seam BEFORE handing fb0 to the app, not on the first capability call. If the first capability call *does* reach `pf-broker/enforce.rs` without a ledger row, that is a runtime bug in `.3`, not a mid-run consent trigger — it must return `EPERM` and log, not prompt.

**What this loses:** the app cannot *runtime-discover* a capability need. E.g. a game that offers optional in-game voice chat cannot ask for microphone only when the user pressed "start voice chat"; instead the manifest must declare microphone up front, and the user grants (or denies) it at launch. Post-Phase-2, this restriction lifts.

### Deferred: mid-run reconciliation options (post-substrate, ordered by preference)

The M1.D supervisor (`tsp-iuz.3`) is paused; when it lands it can drive any of these. This section is the design *record* so `.3` reviewers don't need to re-derive it.

**Recommended: (B) Pause the app.** When a mid-run consent is needed, the supervisor issues a SIGSTOP to the app process (or the equivalent scheduler-suspend on the Phase-2 substrate), takes fb0 back, draws the consent dialog on a clean background of the *supervisor's* choosing (dark neutral fill; app's last frame is NOT shown), commits, releases fb0, and SIGCONTs the app.

- Pros: no compositor needed (still one fb writer at a time). The app cannot fingerprint or capture the consent dialog because its process is stopped. The app's last frame does not blend with a system-modal (which would be user-confusing). The supervisor's dialog visually looks the same in every context (launch, switch, mid-run) — one dialog, one story.
- Cons: the app pauses visibly (a "frozen" frame is briefly visible; SIGSTOP is not fully instantaneous but is fast). Anything mid-transaction in the app (network I/O, running audio) continues *around* the process pause with backpressure the app must handle; this is a small runtime-contract addition (idempotent operations, no assumption of steady frame delivery).

**(A) Draw over the last frame** — the supervisor blits the consent dialog on top of the app's last-drawn frame, effectively pasting the modal onto whatever pixels were showing.
- Pros: no visible pause; the transition feels lightweight.
- Cons: no compositor means the *app* must not draw over the modal, which means we still have to suspend the app's render loop somehow (either SIGSTOP or a runtime-cooperative "please stop drawing" — the latter is trivially bypassable, so we're back to SIGSTOP). And the app's last frame under the dialog can be visually noisy or, worse, a screenshot the user did not intend to share (e.g. banking, private chat). This is real UX and privacy harm.

**(C) Modal overlay via compositor** — a Phase-2 compositor (e.g. Wayland-derived, or a bespoke pf-compositor) manages fb0, blends the app's plane with a system-modal plane on top.
- Pros: the "right" answer long-term. The app keeps drawing (into an offscreen), the modal blends cleanly, no pause visible. Enables progress bars, background audio during consent, everything a mature OS does.
- Cons: a whole compositor. This is a Phase-2 substrate item that dwarfs E3's scope. Not recommended for v1 (would blow the M1.E ship-date) — noted here so a future maintainer knows the road ends here.

**Recommendation: (B) at v1.5 when the supervisor lands. (C) at v3+ substrate work.** The v0 boundary-only rule stays in force until (B) is proven.

---

## 4. The supervisor-ask seam (contract handed to `.3`)

The interaction model above is FRONT half; `.3` implements the BACK half (broker → ledger → supervisor-drawn dialog → ledger commit → answer). This section is the wire contract in between. Written down here — concrete fields, not prose — so `.3` can copy it into `pf-broker` without another round-trip.

### Seam name

Working name: `pf.broker.ConsentAsk`. Actual name/placement in `pf-broker` is `.3`'s call; the *shape* below is the deliverable.

### Request — broker → supervisor

Sent when the broker (`enforce.rs`, at launch-time OR at app-switch — never mid-run in v0) determines a `use=[]` entry is not covered by an active ledger grant.

Wire representation is `.3`'s (JSON or bincode or the existing pf-broker binary format). The *fields* — all mandatory unless marked optional — are:

| field | type | description |
|---|---|---|
| `ask_id` | u64 | Broker-assigned monotonic per-boot id. Used to correlate the response back to the pending broker call. |
| `app_id` | string | Signed-manifest identity (`identity.package` from the manifest) — e.g. `com.example.weather`. |
| `app_name` | string | Human-readable app name (`identity.display_name`) — the string rendered in the prompt. |
| `resource` | enum | The capability being asked for, from the closed set `.2` defines (`LOCATION`, `LOCATION_APPROXIMATE`, `CAMERA`, `MICROPHONE`, `EGRESS`, ...). String on the wire is fine; the supervisor renders it (mapping in supervisor code, not manifest). |
| `resource_arg` | string, optional | For parameterized capabilities: the egress host (`api.weather.com`), the camera id, etc. Rendered as a secondary line in the dialog if present. |
| `purpose` | string, optional | Purpose string declared in the manifest for this `use=[]` entry (`purpose = "show nearby weather"`). Rendered as a small line below the prompt if present. |
| `default_focus` | enum | `deny` \| `allow_once` \| `allow_always`. Fixed at `deny` in v0. Reserved for future policy tuning (e.g. an org fleet policy might set `allow_once` as default for a signature-tier app). |
| `allowed_scopes` | []enum | Subset of `{deny, allow_once, allow_always}`. In v0 always `[deny, allow_once, allow_always]`. Reserved so a `dangerous`-tier capability could restrict to `[deny, allow_once]` (no persistent grant). The supervisor draws only the buttons in this set. |
| `ask_context` | enum | `launch` \| `app_switch`. Used by the supervisor to decide the *background* (launch splash vs launcher context) and by the audit trail. |

### Response — supervisor → broker

Sent when the user commits (A on a focused button, or B).

| field | type | description |
|---|---|---|
| `ask_id` | u64 | Correlates to the request. |
| `decision` | enum | `deny` \| `allow_once` \| `allow_always`. `B` maps to `deny`. |
| `input` | enum | `A_on_deny` \| `A_on_allow_once` \| `A_on_allow_always` \| `B_cancel`. For audit: recovers the user's actual gesture even after `decision` collapses `B` onto `deny`. |
| `elapsed_ms` | u64 | Wall-time between request emission and response commit. For audit / behavioral analytics; the ledger MAY store it but does not act on it. |
| `supervisor_note` | string, optional | If the supervisor recorded anything context-specific (e.g. accessibility mode, org policy override) — free-form; ledger stores verbatim. |

### Invariants

- The response is BINDING — the broker treats it as the source of truth for the grant. The broker writes the ledger row *from* the response, not from the dialog state.
- Timeouts: v0 does NOT time out the ask. If the supervisor crashes or dies mid-ask, the broker times out its own wait (recommended `.3` default: no timeout at launch — the launcher blocks; at app-switch, TBD by `.3` — probably no timeout there either, treat as `deny` if the user backs all the way out).
- Idempotency: an `ask_id` is answered EXACTLY once. Re-sending a request with the same `ask_id` is a broker bug; the supervisor MAY refuse it.
- Ordering: multiple asks queue in the broker; the supervisor processes them serially. A launch with 3 unsatisfied capabilities produces 3 sequential asks, not one batched dialog — `.3` may add batching later, atop the same seam, by folding N asks into one supervisor-side flow. The seam here does NOT support batched asks in v0.
- No side-channel to the app: the app process is not signaled until AFTER the response and ledger row land. Neither the ask nor the response is visible to the app in any form other than the eventual `EPERM` / operation-succeeds delta.

### Non-goals for the seam

- Revocation — handled by `.3`'s settings-UI path, which writes ledger rows directly (no supervisor-ask needed; the settings screen IS the UI).
- Grant enumeration — `.3`'s AppOps ledger has read/list APIs; not the ConsentAsk seam's job.
- Fleet policy push — a signature-tier app that a fleet admin pre-approves has its ledger rows written by admin tooling; no ask fires.

---

## 5. Deferred / noted for follow-up

- **Purpose line pipeline.** `.2` KEYSTONE decides whether `purpose = "…"` is a per-`use[]` field or a separate manifest section. Seam field `purpose` is ready; supervisor draws if present.
- **Error path when the ledger write fails after commit.** The dialog should stay up with a "System error — please try again" banner and re-attempt. `.3` owns the retry semantics; the seam response is already committed by that point, so the retry is broker-internal.
- **Multi-capability launch flow.** `.3`'s choice (sequential asks vs one batched dialog) — spike does not force it.
- **Accessibility.** No screen-reader on a headless-panel gamepad UI in v0. High-contrast mode + haptic tick on focus move are natural extensions; not this spike.
- **Localization.** Prompt strings and button labels are English-only in v0. `.3` MAY plumb through a locale hint; the seam does not carry one today (add via an `additional_fields` extension in a later wire version — recall wire/ABI is FROZEN at v1 per `runtime/STABILITY.md`; extensions are additive).
- **Hardware gate — on-panel render + navigation.** Queued at epic level for tonight's DUT harness restoration + explicit owner OK. NOT a blocker for this spike close. Evidence path when it fires: `capture-screen.sh --cam tsp` → `review-screen.sh` (the epic and bead already record this).

---

## 6. Renderer recipe pin

This spike consumes the tsp-osr-safe recipe from `pocketforge-os/sim` (bead `tsp-an4`), specifically:

- `sim/fb/README.md` §"tsp-osr — pinned, not tripped" — the two safe paths: `SDL_CreateSoftwareRenderer(surface)` (readback / headless) and non-`OPENGL` window + `SDL_CreateRenderer(win, "software")` (on-window).
- `sim/fb/build-sdl3-render.sh` — the SDL3 static-lib build with `VIDEO + RENDER + SOFTWARE`, all GPU/windowing backends OFF. Same lib artifact this spike links against on modelmaker.
- `sim/fb/fb-render.c` — reference implementation of the offscreen path (memfd → `SDL_CreateSurfaceFrom` → `SDL_CreateSoftwareRenderer`).

**We do NOT re-derive the recipe.** If the recipe evolves (say, tsp-osr is closed with a different pin), sim owns the change; this spike updates only its build script/comments to consume the new pin. The on-panel PowerVR / EGL recipe is E6/C2's to pin separately (bead `tsp-fr2n` epic; a parallel coordinator is running); when it lands, `.3`'s supervisor integration will link against it — not this spike.

This spike's prototype ALSO calls `pin_tsp_osr_recipe(W, H)` on startup (the same pinning function `sim/fb/fb-render.c` performs), so a regression that breaks the safe on-window path is caught the moment this spike runs.

---

## 7. What this spike deliberately does NOT do

- Does not integrate with the M1.D supervisor (`tsp-iuz.3` is paused; that's the M1.D bead's job).
- Does not integrate with `pf-broker` — no code lands in `crates/pf-broker/`; the seam contract §4 is the handoff to `.3`. This spike may (per E3xE6 agreement) add a small ADDITIVE helper to `sim/control/control_surface.py` if the sim-driven verification needs a consent-dialog assertion primitive; it MUST NOT edit the merged wire/ABI.
- Does not draw on real hardware. On-panel render is the queued epic-level gate.
- Does not decide `.2`'s protection tiers or `use=[]` schema — that's `.2` KEYSTONE.
- Does not build the AppOps ledger — that's `.3`.

---

## 8. Sources

- Parent epic bead `tsp-ht0p` (owner rulings comments, esp. Q4).
- `.planning/infra/infra-102-permission-consent-model.md` — the .1 section + REFINEMENTS banner.
- `.planning/app-runtime-simulator-research-briefing.md` — R-A ("contract now, enforce later"), R-C (blessed-binary exemption), §A.2 (consent UX brief).
- `pocketforge-os/sim/fb/README.md` — the pinned tsp-osr-safe recipe.
- `pocketforge-os/runtime/crates/pf-broker/manifest.rs` and `enforce.rs` — the merged v0 `use=[]` validator and enforcement seams the seam contract §4 threads into.
- `pocketforge-os/runtime/STABILITY.md` §"wire/ABI FROZEN at v1, additive=minor" — the constraint the seam contract respects.
