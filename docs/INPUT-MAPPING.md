# Input mapping: contexts, remaps and the router-owned store

`crates/pf-input-map` turns a device's frozen `shell-input-contract` document into the
effective map the shell, Settings, pf-osk, the system pointer and the input router all
consult. This file is the normative runtime-side description; the contract data itself is
owned by the platform repo (`shell-input-contract/`, schema v3) and the product rules live
in the input-layer design (every-menu-spec §4.1/§4.2/§4.3.1).

## Contract versions

The crate accepts contract schema versions 2 and 3.

- v3 adds `device_class` (`two_stick`, `no_stick`, `minimal`), the keyboard and pointer
  contexts, the stick/D-pad positions (`l3`, `r3`, `dpad_*`, `lstick`, `rstick`) and the
  `stick_axis`, `stick_direction` and `on_screen` binding shapes.
- The device-class defaults table (`shell-input-contract/fixtures/device-class-defaults.json`
  in the platform repo) is the shipped per-class binding table and the source the
  Settings "Reset to default" action restores.

## Contexts and actions

Every mapping names a context and an action; both are ruled vocabularies
(`known_contexts()`), and each action lives only in its canonical contexts:

| Context | Meaning |
| --- | --- |
| `shell` | shell navigation (Activate, Back, Move.*, Quick) |
| `search` | search surfaces (Search.open/submit/cancel) |
| `global` | session-wide (SystemMenu, Start) |
| `osk` | on-screen keyboard, letter + symbol pages (spec §4.1) |
| `osk.edit` | on-screen keyboard edit pad (spec §4.1) |
| `osk.num` | number/phone/PIN pads — inherited, never mapped |
| `pointer` | system pointer buttons (spec §4.3.1) |

`osk.num` carries no rows of its own: `EffectiveMap::binding("osk.num", …)` resolves
`Type`, `Close`, `Backspace` and `Commit` through `osk`.

Contexts that are never live together do not conflict: `osk`/`osk.edit`/`pointer` bindings
may reuse controls that `shell` bindings use. Glyph resolution for context-less consumers
(`ShellAction`) follows a fixed priority — `shell`, `global`, `search`, `osk`, `osk.edit`,
`pointer` — so the shell's own binding always wins when an action name exists in several
contexts (e.g. `Back`).

## Protection rules

- `SystemMenu` can never be rebound (`ProtectedActionImmutable`); a persisted document
  that tries is re-resolved to the shipped binding with a one-time honest
  `BindingReResolved` event.
- The Menu control and every ruled SystemMenu alternative binding are reserved from all
  other actions in all contexts (`MenuReserved`). A keyboard action may not use Menu
  (spec §4.1). A chord menu (e.g. select+start) does not reserve its individual controls.
- Rule P2 (spec §4.2): `osk/{Type,Close,Backspace,Space,SuggestPick}` must keep a physical
  binding; an `on_screen` candidate for them is refused (`RequiredPhysicalBinding`).
  Removing the last physical binding of the other keyboard actions is allowed exactly
  because the on-screen key always exists.
- `on_screen` is only valid in the keyboard contexts (`osk`, `osk.edit`).
- Protected reachability is keyed by (context, action) pairs — `shell/Activate`,
  `shell/Back`, `global/SystemMenu`, `global/Start` — because v3 legitimately ships `Back`
  in three contexts at once.

## Remap transactions and persistence (X16)

Remaps stay transactional: `RemapEngine::begin` previews, `confirm` commits (persistence
is the commit point), `timeout`/`interrupt`/`revert` roll back without touching the map.
Interrupts — the Menu button, sleep, Settings losing focus — call `interrupt` and change
nothing (spec §4.2).

The remap document is router-owned and lives at the system path:

```
/var/lib/pocketforge/input/remaps.json        (SYSTEM_REMAP_PATH)
```

keyed per device identity. The legacy location was the shell's per-session state dir
(`<state-dir>/remaps.json`, default `./state/remaps.json`). `SystemRemapStore` bridges the
two:

- reads prefer the system document; while it does not exist, a legacy document is read
  and migrated **in memory** (including the v1 `SafeReturn` → `SystemMenu` rename);
- the legacy file is never mutated — retiring it belongs to the router cutover
  (tsp-ihne1.7);
- saves always go to the system path, atomically (temp file + rename);
- `SystemRemapStore::migrate()` performs the explicit idempotent startup migration the
  router runs once and reports `MigratedFromLegacy` / `AlreadyPresent` /
  `NoLegacyDocument`.

Persisted documents are untrusted startup input. A document containing a (context,
action) pair the shipped contract does not know, or violating the complete-map
invariants, is discarded whole and re-resolved to the shipped map with honest events; a
document from an older contract generation keeps every user remap it has and is
backfilled from the shipped profile for the actions it predates, so nothing is stranded.

## X19: how user remaps meet app pointer claims

The user's remap is a **position translation** that is applied *before* an app's
`pf_pointer_config.by_position` claims are resolved. When an app asks for `east`, it gets
"the user's Activate control", not the physical east button: if the user swapped Activate
to west, the app's `east` claim follows to west. An app's own controls win only for the
buttons the app actually claims; Accept on a text field always opens the keyboard
whatever `pointer/LeftClick` is bound to. Remap edits apply live at commit: the router
re-resolves the running app's `by_position` claims through the new translation
(adjudication D2-G7). SDK-side documentation of `by_position` must state this precedence;
aligning the SDK's `by_position` position set with the contract's 20 positions (including
`l3`/`r3`, adjudication D4-M8) belongs to the pointer SDK work, not to this crate.
