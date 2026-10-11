# pf-menu

`pf-menu` is PocketForge's data model and controller focus engine for Settings, the system
menu, keyboard panels, and declared app menus. The platform JSON contract is
`schemas/menu-page.schema.json`.

## Frozen item-kind numbers

These values are the source for the SDK `PfMenuItemKind` ABI. Existing values are never
renumbered; later kinds are appended.

| Number | pf-menu kind | SDK v1 name |
|---:|---|---|
| 0 | `action` | `PF_MENU_ACTION` |
| 1 | `toggle` | `PF_MENU_TOGGLE` |
| 2 | `choice` | `PF_MENU_CHOICE` |
| 3 | `stepper` | `PF_MENU_RANGE` |
| 4 | `info` | `PF_MENU_INFO` |
| 5 | `slider` | system-only |
| 6 | `text` | system-only |
| 7 | `link` | system-only |
| 8 | `progress` | system-only |
| 9 | `binding` | system-only |
| 10 | `date_time` | system-only |
| 11 | `custom` | system-only |

SDK `RANGE` deliberately maps to `stepper`: a bounded integer adjusted with left/right and
shown with a unit. `slider` is a distinct system-only presentation in v1. Consumers can import
`pf_menu::kind::SDK_V1` and call `check_numbering` in their contract tests.

## Reducer contract

`MenuEngine::input` returns an engine-owned effect slice that remains valid until the next
input. Construction and model edits may allocate; the input path does not. Bound values are
shown optimistically and remain pending until the host commits them, rejects them, or the
configured echo deadline expires.
