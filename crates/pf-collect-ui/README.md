# pf-collect-ui — guided-collection on-panel UI (tsp-bwrg.3)

The on-device front end for the headless guided-collection engine
(`pf-input-collect`, tsp-bwrg.2). It renders **the real device** on the device's own screen,
highlights the control the engine is prompting for, shows the prompt + progress, and drives the
engine's `Collector` state machine (record → commit → advance/back → emit) to a candidate
`capabilities.toml`.

**The face is the real device's 3D model**, CONSUMED from the platform `.scad`->skin chain's
committed, drift-gated (tsp-65jc.7) assets — not hand-drawn, not re-generated here. Highlighting a
control is the pf-hwprobe model: draw the neutral `body`, blit the active control's crop from the
`lit` atlas over its `[skin.parts]` rect (top-edge shoulders/triggers use `[skin.views.top]`). The
render sink is pure-fbdev (libc + pure-Rust decode only → a single static aarch64-musl binary, no
SDL/GPU, sidesteps tsp-osr). Full rationale, and why not SDL / live-3D / pf-hwprobe, in
[`docs/RENDER_HOST_DECISION.md`](docs/RENDER_HOST_DECISION.md).

## Design

- `skin` — CONSUME the gated descriptor + skin PNGs (`SkinSet::load(descriptor, skin_root)`);
  `compose(active)` returns the neutral device with the active control highlighted, picking the
  front or top view per control.
- `image` — a tiny RGB type + `png` decode of the gated skin PNGs.
- `canvas` / `font` — the RGB buffer + primitives + a 5×7 font + PPM export; the SAME buffer feeds
  `/dev/fb0` on-device and a PPM off-device.
- `render` — frame composition: blit the composed device image + overlay title/progress/prompt/status.
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

Static aarch64 binary for the device (the OTA artifact — no OpenSCAD/GPU needed, it consumes the
prebuilt skin PNGs at runtime):

```sh
rustup target add aarch64-unknown-linux-musl
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-gnu-gcc
cargo build -p pf-collect-ui --release --target aarch64-unknown-linux-musl
```

## Run

The device skin is consumed from the platform assets via `--descriptor` (the device
`capabilities.toml`) + `--skin-root` (defaults to the descriptor's repo root):

```sh
# Headless render validation — one PPM per control (no device):
pf-collect-ui --dump-dir /tmp/frames --descriptor <platform>/devices/a133/capabilities.toml

# On-panel DEMO — synthesize a press per control so the full sequence renders on the panel with
# NO live pad decoder (this bead's on-panel proof; real collection is tsp-bwrg.6):
pf-collect-ui --mode demo --descriptor <platform>/devices/a133/capabilities.toml --fb /dev/fb0

# On-panel REAL collection against a live evdev node:
pf-collect-ui --mode live --source /dev/input/eventN \
              --descriptor <platform>/devices/a133/capabilities.toml \
              --id newpad --manufacturer Acme --model Pad --fb /dev/fb0 --out /tmp/candidate.toml
```

## On-device deploy (OTA, ephemeral)

`/dev/fb0` is single-owner (menu / boot-animator hold it via systemd `Conflicts=`/`After=`), so the
wizard's unit conflicts with the sibling fb owner. Deploy via the OTA loop from a session holding a
**brokered** `tsp-base` window (never acquire the place yourself). **Ship the CURRENT platform skin
alongside the binary** (not a stale copy) so a `.scad` geometry fix propagates on the next deploy —
one source of truth:

```sh
pf-app-deploy.sh target/aarch64-unknown-linux-musl/release/pf-collect-ui \
    --name pf-collect-ui --unit crates/pf-collect-ui/deploy/pf-collect-ui.service --restart
# and stage the current <platform>/skins/a133/*.png + devices/a133/capabilities.toml where the
# unit's --descriptor/--skin-root point (see the unit file).
```

The binary is **ephemeral** — a reflash wipes `/opt/pocketforge/bin`, so re-deploy per session.
