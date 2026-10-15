# Hang sampler contract

`pf-hang-sampler` is the runtime half of problem-report-v1 hang sampling. Image integration owns
its dedicated user, socket group and unit-scoped `CAP_SYS_PTRACE`; no other service receives that
capability.

## Trigger API

The closed JSON request is one `AF_UNIX/SOCK_SEQPACKET` packet. Requests that establish a target
(`suspect_no_reply` and `prepare`) carry exactly one process handle with `SCM_RIGHTS`; recovery,
discard and force-stop requests carry none.

- `suspect_no_reply { target, hang_nonce }` comes only from `pf-input-broker.service`, after the
  router's two-second foreground-app probe expires. This is unrelated to the menu provider's
  500 ms ping / one-second health timeout.
- `recovered { target, hang_nonce }` comes only from the same router. It installs a tombstone and
  deletes an already-completed provisional sample; a racing worker cannot recreate it.
- `prepare { target, session_id }` comes only from `pf-session-authorityd.service`. At Closing the
  authority submits a bounded `try_send` before it requests the graceful stop. The receiving
  thread resolves `MainPID`, pins its pidfd/start time and asks the sampler to validate the app
  cgroup, snapshot `/proc/<pid>/maps`, load build IDs/unwind sections, and pre-fork a worker. This
  read-only phase never uses ptrace and cannot stop the app.
- `force_stop { item_id, session_id, reason }` comes only from the authority. At escalation the
  authority performs one bounded `try_send` and immediately invokes termination; it never waits
  for target lookup, the sampler socket, ptrace or unwind. The sampler can act only on the pinned
  preparation with the same session and item identity. Missing or expired preparation is an empty
  sample; a changed process identity drops the preparation and produces no report.
- `discard_prepared { item_id, session_id }` comes only from the authority when the app exits in
  the grace window. It removes the preparation and produces no report.

Caller identity comes from `SO_PEERCRED` plus the exact leaf systemd unit derived by
`pf-peer-identity`; request data never declares the caller. A process handle is accepted only
when its pinned PID/start time and claimed unit agree and its exact cgroup is a valid
`pf-app@<id>.service` leaf immediately below `system-pf\x2dapp.slice`. Consequently OSK, fcitx5,
the broker, menu and every other input-process unit are refused by identity rather than process
name.

## Worker and unwind

Closing pre-forks a worker and gives it five seconds (the 1500 ms default grace plus slack) to
retain its owned unwind tables and await ForceStop. Prepared state and workers are bounded to 128
session/item keys; expiry, replacement, discard, or daemon death kills/reaps the worker without
ever attaching to the app. On ForceStop the ready worker verifies start time, arms a monotonic
POSIX timer that sends it uncatchable `SIGKILL` at 300 ms, then uses `PTRACE_SEIZE`,
`PTRACE_INTERRUPT`, and `PTRACE_DETACH` without `PTRACE_O_EXITKILL`. Linux therefore detaches and
resumes a live target if the worker crashes or its deadline fires. A concurrently issued authority
kill still kills a ptrace-stopped target. Every timeout, exit, partial read, malformed unwind
record or worker failure yields an empty attempt and cannot affect termination.

A daemon restart over a lost graceful deadline has no Closing notice in the new daemon lifetime,
so it sends ForceStop without doing a new preparation and kills immediately. A matching prepared
worker left in the still-running sampler may be used; otherwise the result is empty. This is the
only intentionally unprepared authority path. A kill may also win the remaining race after a
successful preparation; that attempt is empty rather than delaying the close.

The unwinder pins `framehop 0.15.0` and `object 0.37.3` under the workspace's Rust 1.85 MSRV.
Framehop consumes ELF `.eh_frame_hdr` / `.eh_frame` / `.debug_frame` and falls back to frame
pointers on x86_64 and aarch64. Stripped symbols do not matter: output is only an allowlisted GNU
build ID plus module-relative offset. A module outside the allowlist is `[null, null]`. There is no
heuristic stack scan.

At most two successful samples contribute to a hang. Empty attempts are not represented by a
non-positive sample count. `encode_hang_report` validates and canonically encodes the result
through `pf-problem-report`; a recovery produces no payload. The in-memory episode ledger retains
at most 128 distinct episode keys, evicting the oldest completed samples, recovery tombstones, and
finalization markers together so long-running service memory remains bounded.

## Data minimization

Paths, registers and stack bytes exist only in worker memory. They are never included in the wire
protocol, audit lines, report values or test output. Audit lines use a fixed result vocabulary.
The sampler never creates a core or copies application memory beyond the individual words needed
by framehop.
