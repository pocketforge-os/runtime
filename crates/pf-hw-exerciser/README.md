# pf-hw-exerciser

Device-facing hardware ground-truth exercisers for the **A523 (TrimUI Smart Pro S)** — the
HARDWARE GATE of `tsp-e1b.4`. The `.4` per-capability managers (`vibration` / `sensors` +
`physical_model`) were verified only against the E5 sim; this binary drives the **real silicon**
over SSH on the stock-booted device (vendor kernel 5.15.147).

It is an **`itest`-tier** tool — a workspace member so it can reuse OUR
`pocketforge::physical_model` math, but **not part of the shipped runtime**.

## Subcommands

| cmd | what it does |
| --- | --- |
| `probe` | Inventory `/dev/input/event*`, `/sys/bus/iio/devices`, `/sys/class/leds`; report whether `qmi8658` (IMU) + `mmc5603` (mag) actually BIND as IIO on stock (cross-feeds SPIKE-0 / `tsp-9sx.1`). |
| `rumble [--node N] [--strong M] [--weak M] [--ms MS] [--count C] [--gap MS] [--list]` | Find the `FF_RUMBLE`-capable evdev node, upload an effect, play it `C`× (default strong=weak=0xFFFF, 500 ms, 3×). Owner confirms the motor fires. |
| `imu [--secs S] [--hz HZ] [--mount a,b,c,d,e,f,g,h,i]` | Dump accel+gyro at ~`HZ` Hz: RAW counts \| chip-frame SI \| device frame (mount matrix applied via `physical_model::apply_mount` — **OUR** math). Prints a flat-test gravity verdict. |
| `led [--only SUBSTR] [--on-ms MS] [--gap-ms MS] [--repeat N] [--list]` | Blink each `/sys/class/leds` node in sequence with stdout markers; owner maps node→physical LED. |

Most subcommands need **root** on the device (evdev rw, LED brightness writes).

## Build — fully static aarch64 (musl)

The stock userland is BusyBox; assume nothing about its libc → link fully static. Built on
**modelmaker** (`mm@10.0.40.90`) with the musl std + the gnu cross-gcc as the linker driver:

```bash
rustup target add aarch64-unknown-linux-musl
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-gnu-gcc
cargo build --release --target aarch64-unknown-linux-musl -p pf-hw-exerciser
# => target/aarch64-unknown-linux-musl/release/pf-hw-exerciser  (ELF aarch64, statically linked)
```

Smoke-test under `qemu-tsp` (qemu-user runs against the host fs, so `probe`/`led`/`imu`
exercise the sysfs-walking + graceful-empty paths without a device):

```bash
~/qemu-tsp/build/qemu-tsp/qemu-aarch64 <bin> probe
~/qemu-tsp/build/qemu-tsp/qemu-aarch64 <bin> --help
```

The evdev ioctl numbers (`EVIOCSFF=0x40304580`, `EVIOCRMFF`, `EVIOCGBIT`, `EVIOCGNAME`) and
`sizeof(struct ff_effect)=48` are verified byte-identical against the kernel `<linux/input.h>`
macros via a C oracle compiled with the same cross-gcc (see the bead transcript).

## Deploy + run on the device (phase 2, coordinator-gated)

```bash
scp target/aarch64-unknown-linux-musl/release/pf-hw-exerciser root@192.168.86.225:/tmp/
ssh root@192.168.86.225 /tmp/pf-hw-exerciser probe
# then rumble / imu / led per the bead's owner-confirmation steps
```
