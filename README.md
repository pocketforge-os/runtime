# pocketforge-os/runtime — Runtime SDK + Capability Broker (E2)

The app-facing **runtime layer**: a `getSystemService("name") -> manager` facade (Android/portal-derived)
over a **capability broker** that holds the real `/dev/*` handles, so an app holds only routed
capability **handles** and **zero ambient authority**. One line, any language:

```rust
let pf  = pocketforge::connect();          // opens the broker socket in our namespace
let vib = pf.acquire::<Vibration>()?;      // -> a HANDLE or a typed error
let loc = pf.query::<Location>();          // side-effect-free: Granted | Denied | Prompt
```

Epic **tsp-e1b** / kickoff [`infra-101`](https://github.com/pocketforge-os/mission-control)
(`.planning/infra/infra-101-runtime-sdk-capability-broker.md`). Part of the app-runtime/simulator
track (E1 [`platform`](https://github.com/pocketforge-os/platform) descriptor →
E5 [`sim`](https://github.com/pocketforge-os/sim) → **E2 this repo** → E6 `pf-hwprobe`).

## What lives here

- `libpocketforge` — the thin client lib (Rust, exports a **C ABI** so any-language OCI apps link
  it); the **wire protocol** is language-agnostic (length-prefixed protobuf / D-Bus-lite),
  reimplementable from the committed spec.
- The **v0 in-process backend** (direct evdev/IIO/FF/sysfs under the `input`/`video`/`render`
  groups) and the **out-of-process broker daemon** — a **backend swap** behind the SAME facade,
  not an app rewrite.
- Per-capability **managers** (input/action-map, vibration, sensors, entropy, location, audio,
  settings) + the **v0 INPUT broker** (`uinput`+`EVIOCGRAB`).
- `spikes/` — de-risking spikes (e.g. `ipc-60hz/`, SPIKE-1).

## Repo layout (as of `tsp-e1b.2`)

A Cargo workspace (`Cargo.lock` IS committed — pinned deps are part of the reproducibility
ethos):

```
crates/
  pf-wire/         PFW1 wire protocol — framing + messages + codec (ZERO deps, reimplementable)
  pocketforge/     the facade: connect()/acquire()/query()/has_capability(), the four-way
                   taxonomy, the v0 in-process backend (port of the sim's broker_stub.py), the
                   out-of-process broker-client backend, the reference server, the action map,
                   the single physical_model (port of the sim's), and the per-capability managers/
  libpocketforge/  the C ABI (cdylib + staticlib) over `pocketforge` -> libpocketforge.{so,a}
  pf-broker-ref/   the reference PFW1 broker daemon (cooperative loopback; the enforcing one is .3)
  pf-input-broker/ the v0 INPUT broker (.6): EVIOCGRAB + uinput re-emit + SCM_RIGHTS fd handoff —
                   the ONE capability with REAL v0 enforcement
  pf-broker/       the ENFORCING broker daemon core (.3): app.toml use=[] launch validation +
                   default-deny + manifest ceiling + SO_PEERCRED + per-cap quota (docs/BROKER-DESIGN.md)
wire/WIRE-PROTOCOL.md   the byte-level, reimplementable wire spec (folds in SPIKE-1's verdict)
docs/BROKER-DESIGN.md   the broker architecture + threat model + what v0 enforces vs. substrate-gated
include/pocketforge.h   the hand-maintained C header (matches libpocketforge)
ctest/                  a gcc C smoke test that links the staticlib and checks the contract
crates/pocketforge/tests/fixtures/  vendored a133 + a523 capability descriptors (from E1)
```

## Build & test (on the build host, `mm@10.0.40.90`)

```sh
cargo build --workspace                       # build everything
cargo test  --workspace                       # taxonomy + backend-swap + change-event + wire
cargo clippy --workspace --all-targets -- -D warnings
bash ctest/run.sh                             # C ABI link + behavior smoke (gcc + staticlib)
```

The **backend-swap proof** lives in `crates/pocketforge/tests/backend_swap.rs`: the v0
in-process backend and the out-of-process broker (PFW1 over a real Unix socket) produce
byte-identical capability snapshots for both the a133 and a523 descriptors — the same app code,
"surviving the runtime fork." The wire spec's reimplementability is demonstrated by a tiny
from-spec client (no project code) driving `pf-broker-ref`.

## Per-capability managers (`tsp-e1b.4`)

`pf.sensors()` / `pf.vibration()` / `pf.input_manager()` / `pf.entropy()` / `pf.location()` /
`pf.egress()` / `pf.audio()` / `pf.settings()` each return a **device-agnostic object** over the
SAME `Backend` trait (so the backend swap holds for the manager layer too). Highlights:

- **sensors** — pose → device/chip-frame accelerometer (gravity reaction) + gyroscope via the
  single `physical_model` (a faithful port of the sim's), applying the descriptor `mount_matrix`.
  ABSENT on the base Pro ⇒ typed `HardwareAbsent`, never a crash.
- **vibration** — the unified no-op shape; the **E4 accessibility enforcement point lands here, at
  the primitive**: `settings().set_bool("hapticsEnabled", false)` makes a pulse `NoopSuppressed`
  via the SAME path as an absent motor.
- **location vs egress** — `location` (read) and `egress` (send) account into **separate** quota
  buckets: reading a fix never spends send budget and vice-versa (anti-exfiltration; the `.3`
  broker turns this cooperative accounting + audit log into enforcement).
- **probe seam** — each manager reconciles `descriptor` (expectation) against a `HardwareProbe`
  (ground truth): an off-hardware `DescriptorTrustProbe` trusts the descriptor; an on-silicon
  `LiveProbe` can DEMOTE a DT-but-unbound cap to `HardwareAbsent` (the authoritative reconciliation
  is the owner-gated hardware leg).

Contract proven in `crates/pocketforge/tests/managers.rs`. Honesty (R-A): these are the
cooperative v0 contract — real default-deny-vs-hostile + server-side quotas are the `.3` broker.

## The v0 INPUT broker (`tsp-e1b.6`) — the ONE real v0 enforcement

`pf-input-broker` is the exception to "cooperative v0": it is genuinely enforcing on the vendor
4.9 A133 kernel TODAY, with **no namespaces**. The daemon:

1. **`EVIOCGRAB`s** the real evdev source — the kernel then delivers that device's events ONLY to
   the broker, so a hostile app that opens the raw node reads **nothing** (the kernel-enforced
   boundary, not a cooperative promise);
2. **re-emits** a `uinput` virtual device, applying the **descriptor action-map** (canonical
   positional codes) + a **rate-limit** token bucket. The X360 driver emits `BTN_X` (0x133) for
   the physical WEST button and `BTN_Y` (0x134) for NORTH; the broker normalizes these onto
   canonical `BTN_WEST`/`BTN_NORTH`, so the app never sees the driver quirk and the Pro→Pro-S delta
   is invisible;
3. **hands the app the re-emit read fd** via `Acquire("input")` + `SCM_RIGHTS` (the out-of-band
   path `wire/WIRE-PROTOCOL.md` §4.1 reserves). The fd, not per-event RPC, is the input hot path
   (SPIKE-1 / `.1`).

```sh
pf-input-broker --source /dev/input/eventN --descriptor <caps.toml> --acquire-sock <sock>
```

**Proof** (`crates/pf-input-broker/itest/run.sh`, run as root): against the E5 sim's
descriptor-synthesized source it shows grab + remap (`BTN_X`→`BTN_WEST`) + the `SCM_RIGHTS`
handoff + a **silent grabbed source** (the app cannot bypass), on **both** x86 (native, via the
fd handoff) and **arm64 under `qemu-tsp`** (the device target). The authoritative on-silicon
shared-fd latency (~0.15 ms/event on the A133) is a HARDWARE GATE (owner OK).

### R-C: the Steam Link blessed-binary FD-pass exemption (the validator's canonical exemption)

Steam Link is BOTH an input consumer AND a `uinput` producer (it makes its own virtual controller
for the host). `EVIOCGRAB` on its input would break it (EBUSY / double-input / enumeration loop).
So Steam Link is a **blessed binary**: the broker re-emits + hands the fd **without grabbing**
(coarse FD-passing, `--no-grab` / `AcquireMode::BlessedNoGrab`), and Steam Link keeps its own
`/dev/uinput`. The grab path is for genuine broker consumers (e.g. `pf-hwprobe`); the no-grab
exemption is keyed on the consumer's identity (E3's blessed-binary tier).

## The enforcing broker daemon core (`tsp-e1b.3`)

`pf-broker` is the **default-deny daemon** that owns the device fds and vends brokered handles
over the `.2` wire. It is what turns the cooperative facade into a real broker, via three pieces
(full design + threat model: [`docs/BROKER-DESIGN.md`](docs/BROKER-DESIGN.md)):

1. **Launch-time `app.toml` validation** — the `use = [...]` CapDL-style authority graph is checked
   against the device descriptor; **unknown / duplicate / bad-modifier / undescriptored-required**
   routes are REJECTED before the app runs. `cap?` marks a capability *optional* (graceful absence
   allowed → runtime `HardwareAbsent`); `location:approximate` / `egress:<host>` are scopes.
2. **Runtime enforcement** (`EnforcingBackend`, itself a `Backend` so the `.2` server serves it
   unchanged) — the validated manifest is the **ceiling** (undeclared ⇒ `PolicyBlocked`/`Denied`),
   default-deny is preserved, dangerous caps are quota-capped, and **`entropy` is the deliberate
   ungated exception** (non-exhaustible CSPRNG).
3. **`SO_PEERCRED`** uid check at accept time.

```sh
pf-broker --validate-only --descriptor <caps.toml> --manifest <app.toml>   # launch gate
pf-broker --socket <path> --descriptor <caps.toml> --manifest <app.toml> [--peer-uid <uid>]
```

The **backend-swap, now enforcing**: `crates/pf-broker/tests/broker.rs` runs the SAME client
(`Pf::via_broker`) an app uses against this daemon and observes the ceiling + default-deny +
quotas over the socket. **Honesty (R-A):** v0 enforces the authority graph cooperatively over the
socket; it does NOT yet confine a process that ignores the socket and reaches `/dev/*` directly
(no namespaces/seccomp — except INPUT's `EVIOCGRAB`). Real fd-isolation into an app namespace is
the substrate-gated leg (owned kernel M2.B–E + paused M1.D) — named, not papered over.

## Frozen public contract + the runtime/SDK split (`tsp-e1b.5`)

The `libpocketforge` C ABI and the PFW1 wire are **version-frozen at v1** — versioned, never
silently broken (the libretro bar). Policy + the enumerated frozen surface:
[`STABILITY.md`](STABILITY.md). Two CI guards make it a build gate, not a promise:

```sh
cargo test -p pf-wire --test frozen_contract   # wire: enum discriminants + golden message bytes
bash abi/check-abi.sh                            # C ABI: every frozen pf_* symbol still exported
```

A break (renamed symbol, renumbered enum, changed framing/pose layout) FAILS these and requires an
explicit major bump (`WIRE_VERSION`++, `broker.v2.sock`, `libpocketforge.so.2`, a new
`abi/libpocketforge.v2.abi`). Additive symbols/fields are minor.

The **Platform vs SDK** seam (Flatpak-style) — what ships on device vs. what an app pins, and how an
app pins a **per-SoC family** (`sun50i-a133` 4.9/PowerVR/sunxifb vs `sun55i-a523` 5.15/Mali/kmsdrm)
— is [`docs/RUNTIME-SDK-SPLIT.md`](docs/RUNTIME-SDK-SPLIT.md), which also fixes the E2/E8 boundary
(E2 defines the contract; E8 packages it) and **names the reproducible-from-clean provenance gap**
(`tsp-cv7.4.13`/`.6`/`tsp-iby`) rather than papering over it.

## Honesty contract — contract now, enforce later (R-A)

> The v0 facade is an **in-process library** linked into the app: an app running as `gamer` with
> the `input`/`video`/`render` groups holds ambient `/dev/*` authority by definition, so v0 is
> **cooperative, not enforcing** — **except INPUT** (`uinput`+`EVIOCGRAB`, R-B), the one capability
> with real v0 enforcement on the vendor 4.9 kernel today. v0 ships the API **shape** + the
> cooperative facade + the v0 input broker. **Enforcement** of unforgeable handles, default-deny
> vs. hostile apps, and fine-grained egress is **deferred** to the out-of-process broker on the
> Phase-2 substrate (owned kernel M2.B–E + resumed M1.D supervisor). "Zero ambient authority /
> unforgeable handles" is a **post-Phase-2 target**, not a v0 claim.

The off-hardware legs (facade, managers, sim integration, backend-swap) are proven against the
[E5 simulator](https://github.com/pocketforge-os/sim) and in CI. The real-silicon legs (the SPIKE-1
authoritative A133 number, the real-namespace broker, rumble/LED/IMU/GNSS/egress) are hardware
gates that need the owner's explicit OK.

## Owner decisions (2026-06-27)

- **Repo = this** (new public repo): broker daemon + `libpocketforge` + managers + wire spec + v0
  in-process backend. The SDK/distribution side splits out later (`.5` → E8).
- **Language = Rust** for `libpocketforge` + the v0 in-process backend + the broker daemon
  (memory-safe new privileged code). Exports a **C ABI**; the wire protocol stays language-agnostic.
- **Standalone top-level epic** (matches E1/E5); the Phase-3 "Appliance Shell" parent is deferred.
