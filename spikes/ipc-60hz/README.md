# SPIKE-1 — broker IPC overhead @ 60 Hz (`tsp-e1b.1`)

**PROVE FIRST.** Gates E2's hot-path shape: does a broker round-trip **per input event**
blow the 16.667 ms frame budget at 60 Hz? If yes, the input hot path cannot be
call-per-sample and must collapse to a **shared evdev/uinput fd** handed into the app
(R-B: `uinput`+`EVIOCGRAB`). This spike answers it with a measured number.

## What it measures (`bench.c`)

Both costs, on one host, back to back:

- **(A) `rpc_roundtrip`** — an `AF_UNIX` `SOCK_STREAM` round-trip with a length-prefixed
  message (`u32 len` + payload, the wire shape `.2` commits to): client writes a request,
  the broker peer reads it and writes a reply, client reads the reply. **One broker hop per
  event.**
- **(B) `sharedfd_read`** — the app does `read(fd, ev, 24)` of one kernel `input_event` from
  an always-full pipe (a writer peer keeps it fed): the per-event cost the shared-fd path
  pays the **app**, with the broker's write off the app's critical path. **No round-trip.**

## Run it

```sh
make run                 # builds build/bench, writes build/results.json + build/results.log
./build/bench 1000000    # custom iteration count; JSON to stdout, stats to stderr
```

Build/run on **modelmaker** (the x86 build host) — device-free. See
[`RESULTS.md`](RESULTS.md) for the captured numbers, the A53-scaled estimate, and the
**go/no-go verdict**. The authoritative A133 4×A53 silicon number is **hardware-gated**
(owner return, through `pocketforge-automation`). Evidence: `baseline/`.
