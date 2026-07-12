# PocketForge runtime — ABI + wire STABILITY policy (frozen v1, `tsp-e1b.5`)

> The libretro track record is the bar: a public contract that is **versioned and never silently
> broken**. This document freezes the `libpocketforge` C ABI and the PFW1 wire protocol at **v1**
> and states the compatibility, deprecation, and enforcement rules. It is the contract E8
> (`infra-107`) packages + distributes and an app pins.

## 1. What is frozen at v1

Two surfaces, each with a stable version:

| surface | version | source of truth | enforced by |
|---------|---------|-----------------|-------------|
| **C ABI** (`libpocketforge.{so,a}`) | `v1` | `include/pocketforge.h` + `abi/libpocketforge.v1.abi` | `abi/check-abi.sh` |
| **PFW1 wire** (broker socket) | `WIRE_VERSION = 1` | `wire/WIRE-PROTOCOL.md` | `crates/pf-wire/tests/frozen_contract.rs` |

The C ABI integer enums and the wire enums are the **same numbers** (one taxonomy, two
surfaces). Frozen values:

- **Status / `PF_*` acquire codes:** `Ok/PF_OK=0`, `Unsupported=1`, `PolicyBlocked=2`,
  `ConsentDenied=3`, `HardwareAbsent=4`.
- **Permission / `pf_query`:** `Granted=0`, `Denied=1`, `Prompt=2`.
- **RumbleStatus / `pf_rumble_pulse`:** `Fired=0`, `NoopAbsent=1`, `NoopSuppressed=2`.
- **`Op`:** `IsPresent=1`, `IsGranted=2`, `Query=3`, `Acquire=4`, `GetCapability=5`,
  `SetCapability=6`, `RumblePulse=7`, `GetPose=8`, `SetPose=9`.
- **Framing:** big-endian `u32` length prefix; `MAX_FRAME = 65536`. Body is protobuf-wire
  (varint + len-delimited), unknown fields skipped.
- **Pose payload:** exactly 9× IEEE-754 `binary64` little-endian = 72 bytes, order
  `yaw,pitch,roll,x,y,z,wx,wy,wz`.
- **C symbols (13):** `pf_connect`, `pf_connect_descriptor`, `pf_free`, `pf_has_capability`,
  `pf_is_present`, `pf_is_granted`, `pf_query`, `pf_acquire`, `pf_acquire_input_fd`,
  `pf_rumble_pulse`, `pf_entropy_fill`, `pf_wire_version`, `pf_strerror` (full list:
  `abi/libpocketforge.v1.abi`). `pf_acquire_input_fd` was ADDED additively (`tsp-e1b.10`, the
  input event fd handoff) — a new symbol, so soname + `WIRE_VERSION` are UNCHANGED and no `Op`
  was added (the broker path reuses `Acquire("input")` + `SCM_RIGHTS`, wire §4.1).

## 2. Compatibility rules (semver of the contract)

Versioning is **semver over the contract**, independent of the crate version:

- **Additive / MINOR (no version basename change):**
  - a NEW wire field number (old peers skip it — `wire/WIRE-PROTOCOL.md` §3);
  - a NEW `Op`/`Status` value (peers reject *unknown* enums rather than guess — so this is
    additive only for the side that learns it; document it);
  - a NEW exported `pf_*` C symbol (old consumers are unaffected).
  Append additive C symbols to `abi/libpocketforge.v1.abi` in the same change.
- **Breaking / MAJOR (explicit, never silent):**
  - removing/renaming a frozen C symbol, or changing a frozen C signature;
  - renumbering/removing a wire enum value, changing the framing, or changing the pose layout;
  - changing the meaning of an existing field.
  A break **bumps `WIRE_VERSION`** *and* the socket-path basename (`broker.sock` →
  `broker.v2.sock`) and ships a new `abi/libpocketforge.v2.abi` — the soname major bumps
  (`libpocketforge.so.1` → `.so.2`). v1 and v2 may coexist; nothing silently changes under a peer.

## 3. Deprecation discipline

A frozen item is never deleted in place. To retire one: (1) mark it deprecated in the header +
`WIRE-PROTOCOL.md` with the replacement and the earliest major that may remove it; (2) keep it
working for the whole current major; (3) remove it only at the next major bump. The guard keeps it
in the golden file until then.

## 4. Enforcement (CI guards)

```sh
cargo test -p pf-wire --test frozen_contract        # wire: enum discriminants + golden encodings
bash abi/check-abi.sh                                # C ABI: every frozen symbol still exported
bash ctest/run.sh                                    # the header links + behaves (gcc + staticlib)
```

`abi/check-abi.sh` FAILS on a missing/renamed frozen symbol (a break) and notes new additive
symbols. `frozen_contract.rs` FAILS on any enum renumber, `WIRE_VERSION`/`MAX_FRAME` change, or a
changed canonical message encoding. Together they make "never silently broken" a build gate, not a
promise.

## 5. Provenance gap (named, not papered over — R8; PER-FAMILY as of E8 `tsp-ziac.1`)

The contract above is frozen; build provenance is **per-family**, not one blanket claim. The old
"interim `sync-build-sources.sh` rsync of dirty working trees" flow this section once described is
**RETIRED** — replaced by the hermetic `pf build` from committed `platform.lock` refs. The honest,
current position:

- **`pocketforge/a133-powervr`** — SHA-pinned **AND reproducible-from-clean** (`tsp-1dl.4.5`,
  `tsp-cv7.6.1` closed): a third party CAN rebuild a bit-identical a133 Platform from committed
  refs.
- **`pocketforge/a523-mali`** — SHA-pinned, **NOT yet reproducible** (`tsp-jet` open; blob→IPFS
  `tsp-iby` open; no owned a523 SDL fork yet). An ABI-freeze claim for a523 must cite this.

Freezing the *contract* does not, by itself, make a given family's *build* reproducible. The
authoritative per-family posture is machine-checked in the `platform` repo
(`abi/platform-abi.json` `reproducible`/`lock_state`; `docs/PLATFORM-ABI-CONTRACT.md` §5). See
`docs/RUNTIME-SDK-SPLIT.md` §4 and the `mission-control` repo's `.claude/rules/provenance.md`.
