# pf-input-decode — A133 gamepad MCU decoder (`tsp-ozbp.9`)

The raw-source input layer for the A133 (TrimUI Smart Pro). Our owned image ships **no gamepad
input device at all**: the pad is an external MCU that streams frames over two RX-only UARTs, and
nothing in our stack read them. This daemon reads those UARTs, decodes the frames, and presents
**one standard evdev gamepad** via `/dev/uinput` — the node every consumer above
(`pf-input-broker`, `pf-input-collect`, `pf-hwprobe`, `tsp-bwrg`, games) expects.

```text
  MCU ── ttyS3 (right) ─┐                        ┌─ pf-input-broker (grab + descriptor remap)
  MCU ── ttyS4 (left)  ─┴─▶ pf-input-decode ─▶ /dev/input/eventN
                             (THIS crate)         └─ pf-input-collect / games (read directly)
```

It sits **below** `pf-input-broker` and depends on nothing from it — this crate *creates* the node
the broker grabs.

## Wire protocol (ground truth `tsp-ozbp.2`)

Two RX-only UARTs, **19200 8N1**, each streaming an 8-byte frame **continuously at ~48 fps** (not
push-on-change):

```text
  byte0  byte1  byte2      byte3 byte4   byte5 byte6   byte7
  0xFF   0x01   <BTNmask>  Xhi   Xlo     Yhi   Ylo     0xFE
```

Sticks are 12-bit: `X = (Xhi<<8)|Xlo`, `Y = (Yhi<<8)|Ylo` (masked to `0..=4095`).

| `byte2` bit | ttyS3 = **RIGHT** | ttyS4 = **LEFT** |
|-------------|-------------------|------------------|
| `0x01` | R1 → `BTN_TR` | L1 → `BTN_TL` |
| `0x02` | R2 → `BTN_TR2` | L2 → `BTN_TL2` |
| `0x04` | X → `BTN_X` | D-up → `ABS_HAT0Y = -1` |
| `0x08` | Y → `BTN_Y` | D-left → `ABS_HAT0X = -1` |
| `0x10` | A → `BTN_A` | D-right → `ABS_HAT0X = +1` |
| `0x20` | B → `BTN_B` | D-down → `ABS_HAT0Y = +1` |
| `0x40` | Select → `BTN_SELECT` | (unused on the base unit; Pro-S only) |
| `0x80` | Start → `BTN_START` | Menu → `BTN_MODE` |
| stick | right → `ABS_RX` / `ABS_RY` | left → `ABS_X` / `ABS_Y` |

### Mapping decisions

- **L2/R2 are physically binary → buttons** (`BTN_TL2`/`BTN_TR2`), matching `pf-input-broker`'s
  `semantics="binary"` mapping. Both layers agree so the `tsp-bwrg` validation gate sees one
  consistent story.
- **Face buttons emit their like-named code** (A→`BTN_A`, B→`BTN_B`, X→`BTN_X`, Y→`BTN_Y`) — the
  exact wire codes the a133 descriptor declares. We are fresh owned source, so we do **not**
  replicate the vendor X360 driver's west↔north code quirk; the descriptor's positional remap is
  `pf-input-broker`'s job, not ours.
- **Menu → `BTN_MODE`** — the base unit's single guide/menu key (the descriptor's `id="guide"`).
- **D-pad is a hat** (`ABS_HAT0X`/`ABS_HAT0Y`), per the descriptor's `id="dpad" kind="hat"`.
- **Sticks report the honest raw 12-bit range** (`0..=4095`, no fuzz/flat). Centring, deadzone,
  and scaling to a signed range is the calibration layer's job (`tsp-bwrg`) — we do not pre-cook
  it here.
- **One device, not two.** A gamepad is one device and consumers expect one node with both
  sticks; the two UART reader threads feed a single uinput device.
- **Identity** is the `"TRIMUI Player1"` / USB `045e:028e` X360 compatibility identity, so the
  device is a drop-in for the descriptor `match` rule and the shipped `gamecontrollerdb` entry. A
  compatibility identity — not vendor code; the whole decoder is owned Rust.
- **`event0`/LRADC VOL±** is a different subsystem and is never touched.

## Layout

| file | purpose |
|------|---------|
| `src/frame.rs` | resynchronising 8-byte frame scanner (pure) |
| `src/decode.rs` | per-UART control map → evdev events (pure; the delicate part) |
| `src/codes.rs` | evdev code constants (the a133 vocabulary only) |
| `src/ioc.rs` | self-contained uinput ioctl encoding (`libc::Ioctl`, musl-safe) |
| `src/uinput.rs` | the virtual-device sink |
| `src/serial.rs` | 19200 8N1 raw tty open |
| `src/bin/pf-input-decode.rs` | the daemon: two reader threads → one uinput device |
| `tests/synthetic.rs` | every-control parity baseline (no kernel) |
| `systemd/pf-input-decode.service` | the cold-boot unit the image installs + enables |

The frame scanner and decoder are pure logic and fully unit-tested against synthetic frames — no
kernel or `/dev/uinput` needed for the test suite.

## Cold-boot

The daemon is started by `systemd/pf-input-decode.service` (installed + enabled by the image). The
pad MCU is powered at boot by the committed DT gpio-hog (`tsp-ozbp.8`); until that lands the UARTs
are silent and the decoder simply waits, so a device flashed before the hog shows the node but no
events. The on-device cold-boot acceptance for this bead is gated on `tsp-ozbp.8` and validated
through the brokered device lane (`tsp-e1b-coord`).
