# On-silicon A133 latency results (`tsp-e1b.7`)

**Status:** device phase pending (DUT harness down; awaiting the coordinator's A133 stock-OS
window grant). This file holds the build/qemu proof now and the on-silicon tables once captured.

## Build + qemu proof (device-free, modelmaker — 2026-07-11)

- Toolchain: bootlin `aarch64--musl--stable-2025.08-1` (musl-static cross), staged on modelmaker
  at `/mnt/pocketforge-tmp/pf-musl-toolchain/`. `make` is warning-clean (`-Wall -Wextra -std=c11`).
- `file build/ipcbench`, `file build/inputlat`: both `ELF 64-bit LSB executable, ARM aarch64,
  version 1 (SYSV), statically linked` — no runtime deps, runnable on stock CrossMix.
- **`ipcbench`** under qemu-tsp/binfmt: **runs** (pure syscalls fully emulatable). Emulated numbers
  are not authoritative — functional smoke only.
- **`inputlat`** aarch64 under qemu-tsp: **launches + runs** to the first uinput ioctl, then stops
  at `UI_SET_EVBIT: Bad address` — qemu-user does **not** translate uinput ioctls (a known
  qemu-user limitation, not a binary defect). Its device semantics are instead validated on the
  **native x86 build** (`make native`, root + `/dev/uinput`): the full EVIOCGRAB→uinput re-emit
  round trip completes with **zero dropped events** over both passes —
  `reemit_burst` n=500 p50=8.7µs/p99=10.0µs, `reemit_60hz` n=500 p50=75.6µs/p99=193.5µs (x86,
  a functional proof — the authoritative figures come from the A133 below). This is exactly why
  the real numbers need silicon: qemu can't exercise the uinput path.

## On-silicon A133 tables (stock CrossMix, vendor 4.9 kernel, performance governor)

_Pending device window. Populated from `/tmp/ipc.json` + `/tmp/inp.json` on the DUT._

### IPC (`ipcbench`) — per-event cost vs the 16.667 ms frame budget

| measurement | p50 (ns) | p95 (ns) | p99 (ns) | p999 (ns) | max (ns) |
| --- | --- | --- | --- | --- | --- |
| `rpc_roundtrip` | | | | | |
| `sharedfd_read` | | | | | |

### Input path (`inputlat`) — EVIOCGRAB→uinput re-emit interposition per event

| measurement | p50 (ns) | p95 (ns) | p99 (ns) | p999 (ns) | max (ns) |
| --- | --- | --- | --- | --- | --- |
| `reemit_burst` | | | | | |
| `reemit_60hz` | | | | | |

## Verdict

_Pending._ Confirms or corrects the off-device model: input = shared-fd (broker re-emit path,
`tsp-e1b.6`); low-rate caps = per-event RPC acceptable (`tsp-e1b.1`). Compare the on-silicon
`rpc_roundtrip` p99 to the ×6 A53-scaled estimate (~100 µs p99) from `tsp-e1b.1`, and the
`reemit_*` p99 to the ~0.15 ms/event R-B claim.
