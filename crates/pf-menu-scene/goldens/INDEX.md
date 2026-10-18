# pf-menu-scene geometry goldens

These small JSON fixtures record semantic roles, state, radii, and Taffy-resolved geometry. They
deliberately omit glyph pixels: font raster changes may alter shapes, while the layout contract's
spacing, targets, state, and radii remain reviewable and byte-compared.

| Golden | Coverage |
|---|---|
| `kinds-states-1280x720.json` | Every item kind; focus, selected, disabled, pending, unavailable, destructive, confirm, and picker states at 16:9 |
| `kinds-states-1024x768.json` | The same matrix at compact 4:3 and the adjudicated 1.2 device-pixel scale |
| `reflow-200-percent.json` | 200% text-scale structural breakpoint and wrapping |
| `reflow-pseudo-30.json` | Deterministic 30% pseudo-locale expansion |

Regenerate intentionally with `UPDATE_GOLDENS=1 cargo test -p pf-menu-scene --test golden_geometry`
inside the pinned runtime CI container, then inspect the diff.
