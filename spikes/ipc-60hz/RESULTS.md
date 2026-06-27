# SPIKE-1 results — broker IPC overhead @ 60 Hz (`tsp-e1b.1`)

**Verdict (TL;DR):** **INPUT → shared-fd** (`uinput`+`EVIOCGRAB`, R-B, child `.6`);
**every other capability → per-event RPC is fine.** A broker round-trip is ~25× more
expensive than a shared-fd `read()` and, crucially, couples the broker's scheduling tail
(ms-scale `max`) to the render loop — so the 60 Hz hot path stays off it. Per-event RPC has
**ample** frame-budget headroom for all low-rate caps even after A53 scaling. This
**quantitatively confirms** R-B's pre-answer; it does **not** reshape the epic.

## Measured (off-device, x86 — a LOWER BOUND)

Host: AMD Ryzen Threadripper PRO 3955WX (16C/32T, Zen2), gcc 13.3.0 — see `baseline/host.txt`.
200k iters, warmup 11k; `req=24B resp=8B event=24B`. Frame budget @60 Hz = **16,666,667 ns**.

| measurement | placement | mean | p50 | p99 | p999 | max |
|---|---|--:|--:|--:|--:|--:|
| **rpc_roundtrip** | default (cross-core) | 12,301 | 13,165 | 16,001 | 20,218 | 111,021 |
| **rpc_roundtrip** | same-core (`taskset -c 3`) | 14,248 | 14,087 | 17,323 | 20,329 | 48,613 |
| **rpc_roundtrip** | default, 1M iters | 12,845 | 13,676 | 16,401 | 20,509 | 1,371,175 |
| **sharedfd_read** | default (cross-core) | 536 | 481 | 1,673 | 3,095 | 29,456 |
| **sharedfd_read** | default, 1M iters | 552 | 491 | 1,714 | 2,766 | 44,184 |

(ns. Raw JSON + logs in `baseline/`.)

**Reading it:**
- A broker hop (RPC round-trip) ≈ **12–14 µs** typical, **~16–20 µs** p99/p999 on x86.
- A shared-fd `read()` ≈ **0.5 µs** typical, **~1.7–3 µs** p99/p999 — **~25× cheaper** per event.
- The RPC `max` reaches **ms-scale** (1.37 ms in the 1M run): scheduler-preemption outliers.
  Coupling that tail to a per-event input path would inject visible **frame-time jitter**.
- Cross-core (default) beats same-core: the A133 has 4 cores, so the ~12–13 µs default figure
  is the representative one (broker and app run concurrently).

## A53-scaled estimate (the A133 is the gated authority)

The A133 is 4×Cortex-A53 (in-order, small caches) @ ≤2.0 GHz; the round-trip is dominated by
2 context switches + ~4 syscalls + 2 socket-buffer copies. A53 syscall+ctx-switch latency runs
roughly **5–8× a Zen2 server core**; using a conservative **×6**:

| | x86 p50 | A53-est p50 (×6) | A53-est p99 (×6) |
|---|--:|--:|--:|
| rpc_roundtrip | ~13 µs | **~78 µs** | **~100 µs** |
| sharedfd_read | ~0.5 µs | **~3 µs** | **~10 µs** |

Frame budget @60 Hz = 16,667 µs. At A53-est ~100 µs p99 per RPC:

| events/frame, per-event RPC | A53-est cost/frame | % of 16.667 ms budget |
|--:|--:|--:|
| 1 | ~0.1 ms | 0.6% |
| 10 | ~1.0 ms | 6% |
| 60 | ~6.0 ms | 36% — concerning |
| 200 (analog flood) | ~20 ms | **>100% — blows it** |

Shared-fd at A53-est ~10 µs p99 stays under ~2 ms even at 200 events/frame (~12% budget),
**and** removes the broker from the per-event critical path (no jitter coupling).

## Go / no-go

1. **INPUT → SHARED-FD (confirmed, R-B / child `.6`).** Not because typical gamepad rates blow
   the budget — they don't (a few events/frame ≈ <1% budget) — but because the shared-fd read is
   ~25× cheaper **and** keeps the broker's ms-scale scheduling tail off the render loop, **and**
   it is the transparent-enforcement path anyway (`uinput`+`EVIOCGRAB`). Worst-case high-rate
   analog input on A53 is the only regime where per-event RPC would threaten the budget, and the
   jitter argument settles it regardless.
2. **LOW-RATE caps (vibration, sensors, location, audio, settings, entropy) → per-event RPC is
   fine.** These fire at most a few calls/sec; at A53-est ~100 µs p99 that is <0.001% of the
   frame budget. **Threshold:** per-event RPC is acceptable for any capability under
   **~1,000 calls/sec on A53** (~16 RPC/frame ≈ 10% budget). Every non-input cap is orders of
   magnitude below that; input alone exceeds it in the worst case.

→ **Fold into `.2`'s wire design:** the wire protocol carries low-rate request/reply per-event
RPC for all caps; the INPUT capability is delivered as a **handed/shared fd** (the app reads the
`uinput`-re-emitted device directly — child `.6`), **not** call-per-sample RPC. No epic reshape.

## Honesty limits

- The x86 numbers are a **lower bound**; the **×6 A53 figure is an estimate**, not a measurement.
  The authoritative A133 4×A53 number is **HARDWARE-GATED** (owner return, run `bench` through
  `pocketforge-automation`; compare to this baseline). Filed as a gated follow-on leg — its
  absence does not block the off-device go/no-go.
- `bench.c` measures the raw socket round-trip; protobuf encode/decode of a tiny message adds a
  few hundred ns of **userspace** work — negligible vs the syscall round-trip, and identical on
  both paths' request side.
- Outlier `max` values are real scheduler preemption (no RT priority here); on-device the
  supervisor may pin/prioritize the broker, which would tighten the tail but not change the verdict.
