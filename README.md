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
                   out-of-process broker-client backend, the reference server, the action map
  libpocketforge/  the C ABI (cdylib + staticlib) over `pocketforge` -> libpocketforge.{so,a}
  pf-broker-ref/   the reference PFW1 broker daemon (cooperative loopback; the enforcing one is .3)
wire/WIRE-PROTOCOL.md   the byte-level, reimplementable wire spec (folds in SPIKE-1's verdict)
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
