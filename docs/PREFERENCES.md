# PocketForge user & accessibility preferences (E4 — `tsp-xubv`, infra-103)

> **Status:** the normative contract for the E4 preference surface. It describes a **read-only-to-apps,
> live-observable** set of user/accessibility preferences that the capability facade honors **at the
> primitive**, and it is deliberately honest — per **R-A** — about what v0 *contracts cooperatively*
> vs. what enforces later. It **cross-references** [`PERMISSION-MODEL.md`](PERMISSION-MODEL.md) and
> never forks it: preferences are user-mutable *state* the `settings` capability (Normal-tier there)
> exposes; they are **not** a permission tier. Implementation: `crates/pf-prefs` (the data layer, `.1`),
> `crates/pocketforge/src/backends/inproc.rs` + `managers/{settings,vibration,audio}.rs` +
> `crates/pf-broker/src/enforce.rs` (this bead, `.2`), and the `pf-settings` CLI (the v1 writer).

## 1. R-A — contract now, cooperatively honored (read this first)

E4 is a **COOPERATIVE** surface and may stay so **permanently** (owner ruling Q4, 2026-07-11): the
docs and acceptance never claim enforcement against a hostile app. A preference is a **contract,
cooperatively honored** — the v0 facade is an in-process library, so an app linked with ambient
`/dev/*` authority is not *confined* by a preference; the value is honored because the primitive
reads it, not because a boundary stops the app. This mirrors [`PERMISSION-MODEL.md §1`](PERMISSION-MODEL.md#1-r-a--contract-now-enforce-later-read-this-first).

**The one enforceable exception is the FF/rumble path (R-B).** Force-feedback / rumble writes route
through E2's v0 `uinput`+`EVIOCGRAB` input broker, so `hapticsEnabled` is enforceable there even for
a non-cooperative app — the same single v0-enforceable seam the permission model names for `input`.
Every other preference is cooperative-only in v0.

## 2. The preferences (v2 schema)

The schema is data (`crates/pf-prefs/src/schema.rs`); adding a preference is one row. v2:

| key | type | default | honored where (v0) | scope note |
|-----|------|---------|--------------------|------------|
| `hapticsEnabled` | bool | `true` | **at the primitive** — the rumble path (`enforce.rs::rumble_pulse` + `managers::vibration`); off ⇒ silent no-op | the FF/rumble path is R-B-enforceable |
| `textScale` | enum (`100%`, `125%`, `150%`, `175%`, `200%`) | `100%` | **stored + observable through `PreferencePort`** | shell application lands with its consumer; until then the port reports stored-not-applied |
| `highContrast` | bool | `false` | **stored + observable through `PreferencePort`** | shell application lands with its consumer; until then the port reports stored-not-applied |
| `reduceMotion` | bool | `false` | **readable + observable flag only** — see §4 | no cosmetic-motion machinery exists; the port reports stored-not-applied |
| `reduceFlashing` | bool | `false` | **stored + observable through `PreferencePort`** | no flashing-suppression consumer exists yet; the port reports stored-not-applied |
| `monoAudio` | bool | `false` | **routing layer** — `AudioManager::output_mix()` reports `Mono` | sim-visible semantic; real DSP down-mix is post-v0 (§4) |
| `brightness` | scalar `0..=100` | `100` | **contract-only** — readable + observer fires; **NO sysfs apply leg** | owner ruling Q3; per-SoC hardware leg is a follow-on bead (§5) |

Defaults match the merged in-memory seam (`hapticsEnabled` ON, as the rumble primitive reads it) and
the accessibility-off-by-default norm (`reduceMotion`/`monoAudio` opt-in — the accessible affordance
is never surprising).

Schema v1 stores were unversioned flat JSON objects. Schema v2 writes `schemaVersion: 2`; the loader
accepts only those two forms. An explicitly versioned document other than v2 returns a typed
unsupported-version error before any save, so a newer schema can never be misread or downgraded.
continues to accept an unversioned v1 document and supplies defaults for every additive v2 key.

## 3. Read-only to apps; observable; honored at the primitive

Three properties define the surface:

- **Read-only to apps (BY CONTRACT).** An app **reads** a preference (`SettingsManager::get_bool`/
  `get_scalar`, or the typed readers `haptics_enabled()`/`reduce_motion()`/`mono_audio()`/
  `brightness()`) and **subscribes** to it (`SettingsManager::subscribe(name) -> Option<Receiver<PrefValue>>`),
  but it **never writes one**. Authority to *change* a preference lives with the user — the
  `pf-settings` CLI today (owner ruling Q1), the on-panel settings UI (`.3`) and supervisor later —
  all going through the single `pf_prefs::PrefsStore::apply()` persist-and-signal seam. The
  `SettingsManager::set_bool` method is the **in-process control plane** (tests + the sim's
  injection-as-API surface), *not* the app contract.
- **Live-observable (`PrefsDidChange`).** A running app reacts the instant a preference flips.
  `InProcessBackend::subscribe_preference(name)` returns a `Receiver<PrefValue>` (mirroring the
  Permissions-API `subscribe()` change-event in `tests/change_event.rs`) that yields the new
  effective value on **any** write path (§3.1).
- **Honored at the primitive.** `hapticsEnabled` is read AT the point of actuation — the rumble
  primitive computes `Fired` / `NoopAbsent` / `NoopSuppressed` (see §6). The app calls `pulse()`; the
  primitive no-ops if the user disabled haptics, with **zero app special-casing**.

### 3.1 The observer fires on ANY write path — including the external-process (CLI) leg

The epic acceptance is explicit: `PrefsDidChange` fires on **any** write path. Two paths in v0:

1. **Control-plane write** (same process): `set_preference_bool`/`set_preference` persist through the
   store **and fire the observer directly**.
2. **External-process write** (the `pf-settings` CLI, a shell away, or the `.3` UI in its own
   process): the CLI is exactly `parse_value` + `PrefsStore::apply` against the shared
   `$PF_PREFS_DIR/prefs.json`. A running session picks that write up — and fires its observers — when
   the host calls **`InProcessBackend::reload_prefs()`**. That is the **honest v0 stand-in** for a
   supervisor file-watch/inotify signal: it is wired to the **sim control surface now**, becomes a
   **supervisor file-watch** on the paused-M1.D supervisor, and **post-Phase-2 the out-of-process
   broker owns the store and fires natively** over the wire (§7). The reload seam is **part of** the
   any-write-path story, not an exemption from it — the `.2` unit tests
   (`tests/prefs_change_event.rs::external_cli_write_becomes_observable_via_reload`) and `.4`'s sim
   E2E exercise the CLI-write → reload → observer-fires leg explicitly.

## 4. `reduceMotion` and `monoAudio` — documented seams, honest v0 semantics

- **`reduceMotion` is a readable + observable flag with NO v0 machinery.** There is no cosmetic-motion
  animator in the v0 runtime to suppress, so this bead does **not invent one**. An app (or a future
  broker-driven animator / the `.3` UI) reads the flag and honors it cooperatively; the observer lets
  it react live. The suppression seam is *documented*, not machinery — promoting it to an actual
  motion-suppressor is a future consumer's job, additively.
- **`monoAudio` is honored on the routing layer.** `AudioManager::output_mix()` returns `OutputMix::Mono`
  when the preference is on — the **sim-visible semantic** a cooperative renderer/mixer reads. The real
  on-device DSP/ALSA channel down-mix is post-v0 and hardware-gated (R-A honesty: v0 proves the
  preference is read at the routing primitive and flips the contract, not that silicon mixes channels).

### 4.1 The `PreferencePort` adapter and applied-vs-stored truth

`crates/pf-prefs-port` adapts the existing `PrefsStore` behind the F01 `PreferencePort`. It snapshots
the store, reloads it when `next_change` is polled, and therefore observes writes made by the external
`pf-settings` process without creating a shell-local store. Submissions accept only the adapter's
configured authority (`user` for the standard constructor); app authority cannot self-promote.

The adapter currently reports every key as stored-not-applied. Although `hapticsEnabled` and
`monoAudio` have runtime consumers, this store adapter is not connected to the running backend's
apply cache and therefore cannot observe either consumer's apply/reload acknowledgement. For every
key, `stored` is the requested value, `effective` remains the schema default, and `applied = false`;
a write returns `StoredNotApplied`. A future consumer may report application only after its actual
acknowledgement is wired into this boundary. This distinction prevents persistence or store reload
alone from being presented as application.

## 5. `brightness` — contract-only in v1 (owner ruling Q3)

`brightness` is a **contract-only** scalar in v1: it is readable (`SettingsManager::brightness()`) and
the observer fires on a change, but there is **no sysfs apply leg anywhere in this epic**. The a133 has
no `/sys/class/backlight` (backlight rides `/sys/class/disp` PWM), and the path is per-SoC divergent
(a133 disp-PWM vs a523). The hardware apply leg is a **hardware-gated follow-on bead** (filed at this
bead's close and linked on the epic `tsp-xubv`), with an explicit owner return.

## 6. The no-op unification, stated honestly

Preference-**suppression** ("user disabled rumble") and missing-**hardware** ("this a133 has no motor")
collapse to the **same app-visible silent no-op**: the app's `pulse()` succeeds, the motor stays
silent, there is no error to handle, and **no app code special-cases either**. The `RumbleStatus`
enum's diagnostic distinction — `Fired=0`, `NoopAbsent=1`, `NoopSuppressed=2`, **discriminants FROZEN
at wire v1** — is *deliberate honesty* for surfaces like `pf-hwprobe`, **not** a behavioral fork. Do
not "fix" the enum. The unification is proven under IDENTICAL calling code by
`tests/prefs_change_event.rs::suppression_and_absence_are_one_silent_no_op_under_identical_code`
(a523-with-haptics-off ⇒ `NoopSuppressed`, a133-no-motor ⇒ `NoopAbsent`), and `.4`'s two CI matrix
rows build on it.

## 7. Additive-only on the frozen v1 wire/ABI — and the post-Phase-2 path

This bead adds **NO** PFW1 wire op and **NO** C-ABI symbol — the frozen surfaces
([`STABILITY.md`](STABILITY.md): the `pf-wire` `Op` enum, `abi/libpocketforge.v1.abi`) are **untouched**
(`crates/pf-wire/tests/frozen_contract.rs` and `abi/check-abi.sh` stay green unchanged). The store
integration, the `PrefsDidChange` observer, and the scalar getter are **Rust-level** additions
(`InProcessBackend` methods + two **defaulted** `Backend` trait methods `preference_scalar` /
`subscribe_preference`) — not part of the frozen contract. The v0 in-process backend is the facade
that proves the contract + observer + at-the-primitive honoring device-free; the out-of-process broker
client cannot yet read/observe preferences over the wire and says so honestly (`preference_scalar`
returns the caller's default; `subscribe_preference` returns `None`).

**Post-Phase-2 path (so the deferral is documented, not accidental):**

- When the broker goes **out-of-process**, preference **read/subscribe** ops are added to the PFW1 wire
  **ADDITIVELY** then (a new `Op` value + `frozen_contract.rs` golden in the same change, per
  `STABILITY.md §2`). The broker will own the store and fire `PrefsDidChange` natively over the socket,
  retiring the `reload_prefs()` stand-in (§3.1).
- The preference **WRITE** op stays **control-plane-scoped** — it is exposed to the authority side
  (CLI / supervisor / settings UI), **never to app sockets**. That is precisely how **read-only-to-apps
  survives the backend swap**: an app that gains a broker socket still cannot write a preference,
  because the wire never offers it a write.
- **`reduceMotion`'s C-ABI story:** no C symbol is added in v1 because there is no C consumer that reads
  preferences yet. When a real C consumer exists, a `pf_preference_*` read/subscribe symbol is added
  **additively** (appended to `abi/libpocketforge.v1.abi` in the same change, per `STABILITY.md §2`).

## 8. Store shape + writer (owner rulings Q1/Q2)

- **Writer (Q1):** the `pf-settings` CLI (`get`/`set`/`list`), modeled on `pf-permissions`. The `.3` UI
  and the supervisor later write through the **same** `PrefsStore::apply()` library path.
- **Store (Q2):** a single current-state JSON document at `$PF_PREFS_DIR/prefs.json` (else
  `$XDG_STATE_HOME/pocketforge/prefs`, else `$HOME/.local/state/pocketforge/prefs`) — schema-validated,
  atomic temp+rename, tolerant load (missing ⇒ defaults; unknown keys preserved for forward-compat).
  It **follows** the AppOps store family conventions; it does **not** fork the capabilities descriptor
  (presence is the E1 descriptor's job — `hapticsEnabled == false` and "a133 has no motor" are
  deliberately different facts that unify only at the primitive, §6).

---

### Cross-references
- [`PERMISSION-MODEL.md`](PERMISSION-MODEL.md) — the tier/trust model (`settings` is Normal-tier there);
  E4 cross-references it and never forks it. R-A framing is shared (§1 of both).
- [`STABILITY.md`](STABILITY.md) — the frozen v1 wire/ABI; §7 above explains why this bead is additive-
  trivial (no wire/ABI surface changed) and how the post-Phase-2 preference ops land additively.
- `crates/pf-prefs` — the `.1` data layer (schema + store + validator + read-API + the persist-and-
  signal `apply()` seam this bead's observer keys off).
- `crates/pf-prefs-port` — the F01 `PreferencePort` adapter, including authority-scoped writes and
  per-key applied-vs-stored reporting.
- Briefing `.planning/app-runtime-simulator-research-briefing.md` — R-A (cooperative), R-B (the FF/rumble
  enforceable exception), §A.2.
