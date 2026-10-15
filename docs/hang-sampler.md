# Hang sampler contract

`pf-hang-sampler` is the runtime half of problem-report-v1 hang sampling. Image integration owns
its dedicated user, socket group and unit-scoped `CAP_SYS_PTRACE`; no other service receives that
capability.

## Listener activation

Production starts the daemon with `--systemd-socket`. The daemon implements the native
`sd_listen_fds` environment protocol without linking libsystemd: `LISTEN_PID` must equal its PID,
`LISTEN_FDS` must be exactly one, and descriptor 3 must report `AF_UNIX`, `SOCK_SEQPACKET`, and a
listening state through `getsockopt`. It removes every `LISTEN_*` variable before validation and
marks the adopted descriptor close-on-exec. Any missing, malformed, mismatched, wrong-type, or
non-listening activation input is a typed startup failure; production never falls back to creating
its own socket. The accepted listener remains the direct systemd-created socket, so connection
authentication continues to use each caller's unchanged `SO_PEERCRED`.

`--socket PATH` is a separate, mutually exclusive mode retained for tests and development. Image
units must not use it: only socket activation lets systemd create the production endpoint as
`0660 root:pf-hang-trigger` while the daemon runs as the unprivileged `pf-hang-sampler` user.

## Trigger API

The closed JSON request is one `AF_UNIX/SOCK_SEQPACKET` packet. Requests that establish a target
(`suspect_no_reply` and `prepare`) carry exactly one process handle with `SCM_RIGHTS`; recovery,
discard and force-stop requests carry none.

The wire types, five-minute preparation limit and outbound client live in the dependency-light
`pf-hang-trigger` crate. It depends only on `libc`, `serde` and `serde_json`; callers such as the
session authority do not link the privileged sampler, ptrace worker, report encoder or unwinder.
CI enforces that `pf-session-authority`'s normal dependency tree contains none of
`pf-hang-sampler`, `framehop`, `object` or `gimli`. `pf-hang-sampler` consumes the same trigger
crate to decode requests, so there is one serialized wire definition.

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

Closing pre-forks a worker and gives it the configured grace period plus three seconds of slack to
retain its owned unwind tables and await ForceStop. Production configuration caps grace at five
minutes; prepared state and workers are also bounded to 128 session/item keys. Expiry, replacement,
discard, or daemon death kills/reaps the worker without ever attaching to the app. On ForceStop the
ready worker verifies start time and the complete file-backed mapping identity captured at
preparation. Mapping drift (including `dlopen`, `dlclose`, remap, or `exec`) discards the attempt
instead of applying stale unwind data. It then arms a monotonic
POSIX timer that sends it uncatchable `SIGKILL` at 300 ms, then uses `PTRACE_SEIZE`,
`PTRACE_INTERRUPT`, and `PTRACE_DETACH` without `PTRACE_O_EXITKILL`. Linux therefore detaches and
resumes a live target if the worker crashes or its deadline fires. A concurrently issued authority
kill still kills a ptrace-stopped target. Every timeout, exit, partial read, malformed unwind
record or worker failure yields an empty attempt and cannot affect termination.

Detach is signal-transparent. `waitpid(..., __WALL)` distinguishes the
`PTRACE_EVENT_STOP` produced by interrupt/group-stop from a signal-delivery-stop. The former
detaches with signal zero; the latter passes `WSTOPSIG(status)` as `PTRACE_DETACH` data so the
kernel re-delivers the held signal. The same remembered signal is used by normal, unwind-error,
and guard-drop detach paths.

The worker-deadline case relies on verified behavior in both release kernel pins, not an
assumption. A signal-delivery stop stores the original signal in `exit_code`; explicit detach
overwrites that field with its data argument, while `exit_ptrace()` calls `__ptrace_detach()`
without overwriting it. Unlink wakes the tracee, whose signal path consumes the preserved value.
See A133 7.x `07bb60764cbc64b5a5d760c34709f07f8259e985`
[`kernel/ptrace.c:151-178`](https://github.com/pocketforge-os/kernel-sunxi-7.x/blob/07bb60764cbc64b5a5d760c34709f07f8259e985/kernel/ptrace.c#L151-L178),
[`kernel/ptrace.c:589-629`](https://github.com/pocketforge-os/kernel-sunxi-7.x/blob/07bb60764cbc64b5a5d760c34709f07f8259e985/kernel/ptrace.c#L589-L629), and
[`kernel/signal.c:2413-2508`](https://github.com/pocketforge-os/kernel-sunxi-7.x/blob/07bb60764cbc64b5a5d760c34709f07f8259e985/kernel/signal.c#L2413-L2508); and A523 5.15
`e63004b0e9478de722571d97cd0e853194ffb9a2`
[`kernel/ptrace.c:132-170`](https://github.com/pocketforge-os/kernel-sunxi-5.15/blob/e63004b0e9478de722571d97cd0e853194ffb9a2/kernel/ptrace.c#L132-L170),
[`kernel/ptrace.c:588-628`](https://github.com/pocketforge-os/kernel-sunxi-5.15/blob/e63004b0e9478de722571d97cd0e853194ffb9a2/kernel/ptrace.c#L588-L628), and
[`kernel/signal.c:2273-2603`](https://github.com/pocketforge-os/kernel-sunxi-5.15/blob/e63004b0e9478de722571d97cd0e853194ffb9a2/kernel/signal.c#L2273-L2603).

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
