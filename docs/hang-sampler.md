# Hang sampler contract

`pf-hang-sampler` is the runtime half of problem-report-v1 hang sampling. Image integration owns
its dedicated user, socket group and unit-scoped `CAP_SYS_PTRACE`; no other service receives that
capability.

## Trigger API

The closed JSON request is one `AF_UNIX/SOCK_SEQPACKET` packet. Sampling requests carry exactly
one target process handle with `SCM_RIGHTS`; recovery carries none.

- `suspect_no_reply { target, hang_nonce }` comes only from `pf-input-broker.service`, after the
  router's two-second foreground-app probe expires. This is unrelated to the menu provider's
  500 ms ping / one-second health timeout.
- `recovered { target, hang_nonce }` comes only from the same router. It installs a tombstone and
  deletes an already-completed provisional sample; a racing worker cannot recreate it.
- `force_stop { target, session_id, reason }` comes only from
  `pf-session-authorityd.service`. The authority submits it with bounded `try_send` and never
  waits for target lookup, the sampler socket, ptrace or unwind.

Caller identity comes from `SO_PEERCRED` plus the exact leaf systemd unit derived by
`pf-peer-identity`; request data never declares the caller. A process handle is accepted only
when its pinned PID/start time and claimed unit agree and its exact cgroup is a valid
`pf-app@<id>.service` leaf immediately below `system-pf\x2dapp.slice`. Consequently OSK, fcitx5,
the broker, menu and every other input-process unit are refused by identity rather than process
name.

## Worker and unwind

Each attempt forks a short-lived worker. Before `PTRACE_SEIZE`, it sets parent-death `SIGKILL` and
arms a monotonic POSIX timer that sends the worker uncatchable `SIGKILL` at 300 ms. The worker uses
`PTRACE_SEIZE`, `PTRACE_INTERRUPT`, and `PTRACE_DETACH` without `PTRACE_O_EXITKILL`. Linux therefore
detaches and resumes a live target if the worker crashes or its deadline fires. A concurrently
issued authority kill still kills a ptrace-stopped target. Every timeout, exit, partial read,
malformed unwind record or worker failure yields an empty attempt and cannot affect termination.

The unwinder pins `framehop 0.15.0` and `object 0.37.3` under the workspace's Rust 1.85 MSRV.
Framehop consumes ELF `.eh_frame_hdr` / `.eh_frame` / `.debug_frame` and falls back to frame
pointers on x86_64 and aarch64. Stripped symbols do not matter: output is only an allowlisted GNU
build ID plus module-relative offset. A module outside the allowlist is `[null, null]`. There is no
heuristic stack scan.

At most two successful samples contribute to a hang. Empty attempts are not represented by a
non-positive sample count. `encode_hang_report` validates and canonically encodes the result
through `pf-problem-report`; a recovery produces no payload.

## Data minimization

Paths, registers and stack bytes exist only in worker memory. They are never included in the wire
protocol, audit lines, report values or test output. Audit lines use a fixed result vocabulary.
The sampler never creates a core or copies application memory beyond the individual words needed
by framehop.
