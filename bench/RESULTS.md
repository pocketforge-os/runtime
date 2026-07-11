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

## On-silicon A133 tables — captured 2026-07-11

**Environment / caveat (applies to every table below):** measured on the **PocketForge image**
(owned **4.9.191** vendor-4.9 fork, Debian 12, hostname `pocketforge`), `gamer@192.168.86.132`,
over SSH — **not stock CrossMix** (the CrossMix card had no bootloader — overlay-only — so the
device was flashed with our image via the standard automation; per the coordinator this is better,
the numbers land on the kernel we ship). **CPU: 4×A53 pinned at 1.008 GHz** — the image's
`sun50i-cpufreq-nvmem` driver fails to probe (`Could not get nvmem cell: -22`), so there is **no
cpufreq/DVFS and no governor to set** (the "performance governor" step is N/A here); the cores sit
at the boot PLL (1.008 GHz, **below** the A133's ~2.0 GHz ceiling). **These numbers are therefore
CONSERVATIVE** — filed as its own defect **`tsp-9h88`** (kernel/substrate). System otherwise idle.
Binaries: static aarch64 musl, sha256 `65df627b…` (ipcbench) / `2e3bf618…` (inputlat).

### IPC (`ipcbench`, 200000 iters) — per-event cost vs the 16.667 ms frame budget

| measurement | p50 | p99 | p999 | max |
| --- | --- | --- | --- | --- |
| `rpc_roundtrip` | 134.7 µs | 170.0 µs | 218.4 µs | 667.1 µs |
| `sharedfd_read` | 3.0 µs | 55.5 µs | 88.5 µs | 600.0 µs |

_(`ipcbench` reports p50/p99/p999/max, not p95.)_

### Input path (`inputlat`, 2000 iters, 0 dropped) — EVIOCGRAB→uinput re-emit interposition per event

| measurement | p50 | p95 | p99 | p999 | max |
| --- | --- | --- | --- | --- | --- |
| `reemit_burst` | 36.5 µs | 38.9 µs | 48.3 µs | 266.6 µs | 795.6 µs |
| `reemit_60hz` | 91.3 µs | 95.3 µs | 139.5 µs | 141.7 µs | 176.6 µs |

## Verdict — CONFIRMED (no epic reshape)

- **Input = shared-fd** (broker re-emit path, `tsp-e1b.6`): interposition p50 36–91 µs / p99
  48–140 µs — matches the R-B **~0.15 ms/event** claim (paced/pessimistic ~90–140 µs < 150 µs;
  warm ~36 µs), and every percentile is <1% of the 16.667 ms frame budget. Shared-fd `read()` alone
  is p50 3.0 µs vs per-event RPC p50 134.7 µs → **~45× cheaper**, and RPC's max tail (667 µs) is the
  broker scheduling tail the shared-fd path keeps off the render loop.
- **Low-rate caps = per-event RPC fine** (`tsp-e1b.1`): even at the pinned 1.008 GHz, RPC round-trip
  **p99 170 µs ≈ 1.0% of the frame budget** — the `.1` conclusion holds with margin on real silicon.
- **vs the off-device model (`tsp-e1b.1`):** x86 RPC ~12–14 µs p50; ×6 A53 estimate ~78 µs p50 /
  ~100 µs p99. Real A133 @1.008 GHz = 135 µs p50 / 170 µs p99 — the ×6 estimate was optimistic
  because this device runs at ~half its max clock (`tsp-9h88`); scaling to ~2 GHz lands close.
  **Shape/verdict unchanged; magnitudes now pinned on real silicon.**

Cross-posted to `tsp-e1b.1` and `tsp-e1b.6`. The bead's hardware gate (owner OK on the numbers) is
relayed by the coordinator.
