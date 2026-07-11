# pf-prefs — the E4 preference data layer

A typed **accessibility / user-preference** schema, a persistent JSON store, a validator, a
read-API, and a persist-and-signal write seam. This is the *data layer* E4 (infra-103) is built
on. Sibling `.2` wires it into the capability backends (so the facade honors it at the primitive)
and attaches the `PrefsDidChange` observer; `.3` adds the on-panel settings UI. The v1 writer
surface is the `pf-settings` CLI (crate `pf-settings`).

> `docs/PREFERENCES.md` — the epic's cross-linked contract doc — is authored by `.2` (this bead
> is new-files-only and does not touch `docs/`). Until then, this README + the crate rustdoc are
> the contract of record.

## The contract: read-only to apps, cooperatively honored

Preferences are **READ-ONLY TO APPS by contract** (owner ruling Q4 / R-A). An app may *read* a
preference and — once `.2` lands the observer — *subscribe* to changes; it may **never write
one**. Authority to change a preference lives with the user: the `pf-settings` CLI today, the
on-panel settings UI (`.3`) and supervisor later, all through the single write path here.

This contract is **cooperative, permanently** — *"contract, cooperatively honored"*, never an
enforcement claim against a hostile app. The v0 facade is an in-process library; it proves the
contract + ergonomics + graceful missing-hardware degradation, not confinement. The one path
where a preference is enforceable against a *non-cooperative* app is the FF/rumble route through
E2's `uinput`+`EVIOCGRAB` input broker — that R-B nuance is documented where it applies (`.2`'s
integration docs), not here.

## Not a fork of the capabilities descriptor

Preferences are **user-mutable STATE**; hardware **presence** is device-fixed data owned by the
E1 capabilities descriptor. pf-prefs never duplicates or forks that descriptor. `hapticsEnabled
== false` and "this a133 has no rumble motor" are deliberately different facts that collapse to
the *same* silent no-op at the primitive — that unification is `.2`'s job; pf-prefs owns only the
preference half.

## Schema (v1)

| key              | type            | default | notes                                                        |
|------------------|-----------------|---------|--------------------------------------------------------------|
| `reduceMotion`   | bool            | `false` | Suppress non-essential cosmetic motion.                      |
| `hapticsEnabled` | bool            | `true`  | Allow haptics; off ⇒ rumble is a silent no-op at the primitive. Matches the merged in-memory default. |
| `monoAudio`      | bool            | `false` | Down-mix audio to mono.                                      |
| `brightness`     | scalar `0..=100`| `100`   | **CONTRACT-ONLY in v1** (owner ruling Q3): read + observed, **no sysfs apply leg anywhere in this epic** (a133 has no `/sys/class/backlight`; apply is a hardware-gated follow-on). |

**Extensible tail.** The schema is a `const` table (`schema::SCHEMA`); adding a preference is one
`PrefSpec` row (`key`, `PrefKind`, default, doc) — validator, defaults, and `pf-settings list`
all derive from it. **Unknown-key policy:** the explicit *set* path rejects unknown keys
(`PrefError::UnknownKey`); the tolerant *load* path **preserves** them (forward-compat — an older
reader round-trips a newer writer's key instead of dropping it).

## Store

A single current-state JSON document at `$PF_PREFS_DIR/prefs.json` (owner ruling Q2), resolving
the directory the same way `pf_broker::appops` does:

1. `$PF_PREFS_DIR` (mirrors `PF_APPOPS_DIR`; used by tests),
2. else `$XDG_STATE_HOME/pocketforge/prefs`,
3. else `$HOME/.local/state/pocketforge/prefs`.

Unlike the AppOps *ledger* (an append-only event log), preferences are fit-for-current-state: one
JSON object, rewritten whole, humanly `cat`-able and hand-editable.

- **Atomic writes** — serialize to `prefs.json.tmp.<pid>`, `fsync`, `rename(2)` over `prefs.json`.
  A crash leaves either the old or the new document, never a torn file.
- **Tolerant load** — missing file ⇒ all defaults; present file ⇒ parsed as a JSON object with
  every known key validated (a type mismatch or out-of-range scalar is a typed `PrefError`, never
  a panic); unknown keys preserved.
- Only explicitly-set keys are written (a fresh store is `{}`); every other key reads through to
  its default, and `pf-settings list` shows `default` vs `stored` per key.

## API sketch

```rust
use pf_prefs::{PrefsStore, PrefValue};

let store = PrefsStore::open_default();           // honors $PF_PREFS_DIR
let prefs = store.load()?;                          // tolerant
let _ = prefs.haptics_enabled();                    // typed read-API (facade side)

// The write authority persists-and-signals through ONE seam:
if let Some(change) = store.apply("hapticsEnabled", PrefValue::Bool(false))? {
    // `.2` fires PrefsDidChange here so a running app reacts live.
    let _ = change; // PrefChange { key, old, new }
}
```

`apply` returns `Some(PrefChange)` **iff the effective value actually moved** — that is exactly
the "fire the observer" signal `.2` attaches to, so the `PrefsDidChange` hook is a one-line graft
onto the write path, not a redesign.
