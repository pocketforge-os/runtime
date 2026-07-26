# pf-input-collect — guided-collection engine (tsp-bwrg.2)

The **headless data heart** of the input bring-up / calibration app (epic `tsp-bwrg`, job #2:
*guided ground-truth collection*). With **no pre-filled descriptor** it prompts for each control
in turn, reads the raw `/dev/input/eventN` node **directly** (post-decoder), records the raw evdev
event(s) per prompt, classifies binary-vs-analog triggers from **observed** behaviour, and emits a
candidate `capabilities.toml` for a brand-new device.

The on-panel wizard UI is a **separate** crate (`tsp-bwrg.3`) that *drives* this engine;
full live-pad validation is `tsp-bwrg.6`. This crate proves the engine **headless** against a
synthetic a133-shaped stream.

## Why it can't go through `pf-input-broker`

`pf_input_broker::InputBroker::start_with()` requires a `Descriptor` to construct. Guided
collection is the mode that runs **before** a descriptor exists, so it has nothing to build a
broker from. It is a passive reader of the raw node — exactly as
`platform/regression/caps/evdev-probe.py --watch` is. The evdev ioctls it needs
(`EVIOCGNAME`/`EVIOCGID`/`EVIOCGABS` + `poll`/`read`) are carried self-contained in `source.rs`,
typed as `libc::Ioctl` (the `pf-hw-exerciser` precedent), so the binary cross-compiles to a
static `aarch64-unknown-linux-musl` and stays decoupled from in-flight `pf-input-broker` edits.

## CLI

```
pf-input-collect --source <node> --id <id> --manufacturer <mfr> --model <model> [--out <file>]
```

| flag | meaning |
|------|---------|
| `--source <node>`    | raw evdev node to read (e.g. `/dev/input/event3`) |
| `--id <id>`          | `identity.id` for the new device (lowercase alnum, e.g. `a133`) |
| `--manufacturer`     | `identity.manufacturer` |
| `--model`            | `identity.model` |
| `--out <file>`       | write candidate TOML here (default: stdout) |

Prompts go to **stderr**, so with `--out` unset stdout carries only the emitted TOML. Validate the
result on any host with Python 3.11+:

```
cp candidate.toml platform/devices/<id>/capabilities.toml && pf caps validate <id>
```

(A brand-new device also needs a sibling `profile.toml` for the full `caps.py validate` device-id
join — that is the build-integration step, a later bead. The **schema** + input semantics pass
immediately.)

## Programmatic API — what `tsp-bwrg.3` (the UI) drives

The UI owns its own event pump (it is rendering the skin and reading the node anyway) and steps
the engine with a small pull-model API:

```rust
use pf_input_collect::{Collector, DeviceMeta, default_gamepad_plan};
use pf_input_collect::source::{EvdevSource, EventSource};

let mut src = EvdevSource::open("/dev/input/event3")?;
let mut c   = Collector::new(default_gamepad_plan());
let meta    = DeviceMeta { id: "newdev".into(), manufacturer: "Acme".into(), model: "Pad".into() };

while let Some(spec) = c.current().cloned() {
    ui.show_prompt(c.prompt().unwrap(), c.position());   // "[3/16] west — Press the LEFT face button"
    loop {
        let events = src.poll(std::time::Duration::from_millis(50))?;   // UI pumps its own events
        c.record(&events);                                             // feed them to the engine
        if ui.user_confirmed_or_settled() { break; }                   // UI decides when done
    }
    match c.commit_current(&mut src)? {                                // finalize (reads EVIOCGABS)
        pf_input_collect::CommitOutcome::Captured(rec) => ui.show(rec),
        pf_input_collect::CommitOutcome::Skipped       => ui.show_skipped(),
    }
    c.advance();                                                       // or c.back() to redo
}
let candidate = c.emit(&mut src, &meta)?;    // -> pf_input_collect::Capabilities
std::fs::write("candidate.toml", candidate.to_toml())?;
```

Key methods on `Collector`:

- `current()` / `prompt()` / `position()` — the control being asked for + `(idx, total)`.
- `record(&[RawEvent])` — feed observed events for the current control.
- `commit_current(&mut dyn EventSource)` → `CommitOutcome::{Captured, Skipped}` — finalize; an
  `optional` control with no activity is **skipped** (row omitted, never fabricated).
- `advance()` / `back()` — move on / redo the previous control.
- `clear_working()` — discard the current control's buffer (retry a press).
- `recorded(id)` — the capture for a control id, for progress rendering.
- `emit(&mut dyn EventSource, &DeviceMeta)` → `Capabilities` — build the candidate.

The headless CLI runs the **same** logic via `collect::run(&mut collector, &mut src, &meta, &cfg,
&mut out)`; the unit tests drive `collect::run` over a `ScriptedSource` (a synthetic stream, no
kernel / no `/dev/uinput`) — see `tests/a133_synthetic.rs`.

## Classification: binary vs analog triggers

The a133 `L2`/`R2` are a **binary switch on an analog wire** — the driver reports `ABS_Z`/`ABS_RZ`
over `0..255`, but only the endpoints ever fire. The engine records the declared range (via
`EVIOCGABS`) **and** tracks the observed values (jitter-deduped, the same delta
`evdev-probe.py` uses); if only endpoint values are seen → `semantics="binary"`, if intermediate
travel is observed → `semantics="analog"`. The `range` field keeps the analog wire range either
way; `semantics` carries the physical truth the broker/shim consumes.

## Test / build

```
cargo test  -p pf-input-collect                 # native: unit + synthetic-a133 acceptance
# optional real caps.py validation runs automatically when a platform checkout is discoverable
# (sibling ../platform, or $PF_PLATFORM_DIR); it is self-contained and skips gracefully in CI.

# static device binary (pf-hw-exerciser recipe):
rustup target add aarch64-unknown-linux-musl
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-gnu-gcc
cargo build --release -p pf-input-collect --target aarch64-unknown-linux-musl
```
