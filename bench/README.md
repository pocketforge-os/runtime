# On-silicon A133 latency benches (`tsp-e1b.7`)

The **hardware leg** of the E2 runtime epic. Two static aarch64 binaries produce the
authoritative on-A133 numbers the off-device models in `tsp-e1b.1` (SPIKE-1) and `tsp-e1b.6`
(input broker) named as hardware-gated:

| binary | source | measures |
| --- | --- | --- |
| `ipcbench` | [`../spikes/ipc-60hz/bench.c`](../spikes/ipc-60hz/bench.c) (`tsp-e1b.1`) | per-event broker RPC round-trip (AF_UNIX) vs shared-fd `read()`, p50/p95/p99 |
| `inputlat` | [`input-latency/inputlat.c`](input-latency/inputlat.c) (`tsp-e1b.7`, mirrors `tsp-e1b.6`) | EVIOCGRAB→uinput re-emit interposition latency per event, p50/p95/p99 + 60 Hz jitter |

Both are **fully static** (`aarch64 musl-static`, no runtime deps) so they run on **stock CrossMix**
with nothing installed — the DUT harness is down, so this bead runs over **SSH on the stock OS**
(vendor 4.9 kernel; `UINPUT`/`EVDEV` are `=y`). See the bead for the owner-ratified deviation.

## Build (on modelmaker)

```sh
make            # -> build/ipcbench, build/inputlat  (aarch64 musl-static)
file build/*    # must report: ELF 64-bit … ARM aarch64 … statically linked
```

Smoke-test under `qemu-tsp`/binfmt on modelmaker before shipping (`ipcbench` needs no privilege;
`inputlat` needs `/dev/uinput` + root, so its full run is a device/qemu-root step — the build +
`--help`-less start is the smoke check, the real numbers come from the device).

## Run on the A133 (stock, over SSH)

```sh
# governor -> performance on all 4 A53 cores (record + restore afterwards)
for c in /sys/devices/system/cpu/cpu[0-3]/cpufreq/scaling_governor; do cat $c; echo performance > $c; done

/tmp/ipcbench  200000 > /tmp/ipc.json  2> /tmp/ipc.log     # IPC round-trip + shared-fd read
/tmp/inputlat  20000  > /tmp/inp.json  2> /tmp/inp.log      # broker re-emit path (needs root/uinput)

# restore the governor to its recorded prior value
```

`ipcbench` prints one JSON object per measurement (`rpc_roundtrip`, `sharedfd_read`) plus a
human stats line on stderr; `inputlat` prints `reemit_burst` + `reemit_60hz`. Record the p50/p95/p99
tables in [`RESULTS.md`](RESULTS.md) and cross-post the verdict to `tsp-e1b.1`/`tsp-e1b.6`, each
caveated **"measured on stock CrossMix, vendor 4.9 kernel, performance governor"**.

## Honesty limits

- `inputlat` re-implements the broker's grab→re-emit **hot loop** in C; it exercises the identical
  kernel primitives (uinput source + `EVIOCGRAB` + uinput re-emit) the silicon latency depends on,
  but does **not** run the Rust `pf-input-broker` daemon (whose *functional* enforcement was proven
  under qemu-tsp in `tsp-e1b.6`). It measures the **kernel-path** interposition cost, which is what
  the ~0.15 ms/event R-B claim is about.
- Stock CrossMix runs the same vendor 4.9 kernel **class** as our fork, so silicon/driver numbers
  are valid with the recorded caveat; a fork rebuild could shift constant factors slightly.
