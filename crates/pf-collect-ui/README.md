# pf-collect-ui — guided-collection on-panel UI (tsp-bwrg.3)

The on-device front end for the headless guided-collection engine
(`pf-input-collect`, tsp-bwrg.2). It renders a canonical gamepad **face** on the device's own
screen, highlights the control the engine is prompting for, shows the prompt + progress, and drives
the engine's `Collector` state machine (record → commit → advance/back → emit) to a candidate
`capabilities.toml`.

**Render host:** a new minimal **pure-fbdev** software renderer, in-process linking the engine —
libc-only, static `aarch64-unknown-linux-musl`, single OTA binary, GPU-independent, sidesteps
tsp-osr. Full rationale (incl. why not pf-hwprobe, and the SDL fb-down fallback) in
[`docs/RENDER_HOST_DECISION.md`](docs/RENDER_HOST_DECISION.md).

## Design

A backend-agnostic `Canvas` (in-memory RGB buffer + 2D primitives + a hand-authored 5×7 font) is
filled by the SAME draw code whether the sink is:

- **`/dev/fb0`** (`fbdev::FbDev`) — on-panel, letterboxed to the real resolution, or
- **a PPM file** (`dump::dump_frames`) — off-device, for headless screenshot validation.

So the exact pixels the panel shows are validated headless before the device is ever touched.

- `canvas` — the RGB buffer + primitives + PPM export.
- `font` — the 5×7 bitmap font (owned, public-domain, no vendored table).
- `face` — the generic-gamepad hotspot layout keyed to the engine plan's ids.
- `render` — frame composition (title, progress, face + highlight, prompt, status).
- `fbdev` — the `/dev/fb0` sink (mmap + ioctl geometry/format + letterbox blit).
- `wizard` — the drive loops: `drive_live` (engine-parity pump vs a live evdev node) and
  `drive_demo` (auto-advancing synthetic pass — the on-panel proof that needs no live pad).
- `dump` — headless PPM frame dump for `/screen-check` render validation.

## Build

Host tests + native build (build host = modelmaker):

```sh
cargo test -p pf-collect-ui
cargo clippy -p pf-collect-ui --all-targets -- -D warnings
```

Static aarch64 binary for the device (the OTA artifact):

```sh
rustup target add aarch64-unknown-linux-musl
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-gnu-gcc
cargo build -p pf-collect-ui --release --target aarch64-unknown-linux-musl
# => target/aarch64-unknown-linux-musl/release/pf-collect-ui  (ELF aarch64, statically linked)
```

## Run

```sh
# Headless render validation — one PPM per control (no device):
pf-collect-ui --dump-dir /tmp/frames

# On-panel DEMO — synthesize a press per control so the full sequence renders on the panel with
# NO live pad decoder (this bead's on-panel proof; real collection is tsp-bwrg.6):
pf-collect-ui --mode demo --fb /dev/fb0 --out /tmp/candidate.toml

# On-panel REAL collection against a live evdev node:
pf-collect-ui --mode live --source /dev/input/eventN --id newpad --manufacturer Acme --model Pad \
              --fb /dev/fb0 --out /tmp/candidate.toml
```

## On-device deploy (OTA, ephemeral)

`/dev/fb0` is single-owner (the menu / boot-animator hold it via systemd `Conflicts=`/`After=`), so
the wizard ships a unit that conflicts with the sibling fb owners. Deploy via the OTA loop from a
session holding a **brokered** `tsp-base` window (never acquire the place yourself):

```sh
pf-app-deploy.sh target/aarch64-unknown-linux-musl/release/pf-collect-ui \
    --name pf-collect-ui --unit crates/pf-collect-ui/deploy/pf-collect-ui.service --restart
```

The binary is **ephemeral** — a reflash wipes `/opt/pocketforge/bin`, so re-deploy per session; do
not assume persistence.
