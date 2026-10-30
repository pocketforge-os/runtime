# Physical power key policy

`tsp-mc9m.60.29.8.1` assigns short PEK presses to `pf-powerd` and long presses to
systemd-logind. The daemon opens the one `axp20x-pek` evdev node advertising
KEY_POWER without grabbing it. Logind continues to see the same edges. There is
no `handle-power-key` inhibitor and no PMIC register write.

A complete press/release shorter than five seconds queues one coalesced menu
request. Kernel CLOCK_MONOTONIC timestamps classify the press even if the reader
is delayed. Repeats do not extend the hold; unmatched releases, a backward
clock, SYN_DROPPED and reconnects cannot create a menu request from a stale hold.
Discovery refuses missing or ambiguous PMIC sources and retries after one second.
Other keyboard and virtual pad power keys are outside this board-specific intake.

The existing power socket admits only the gamer uid in
`pf-shell-selected.service`, before reading a request. `menu_poll` returns
`{"result":"menu","request_id":null}` or a daemon-incarnation/sequence ID.
`menu_ack` with that ID returns `{"result":"menu_ack","acknowledged":true}`
and clears only the matching pending request. A newer physical press supersedes
the prior request; stale ACKs return false. The bounded state contains one request,
not an unbounded queue. Polling and ACK cannot invoke the power executor.

The shell polls asynchronously and presents its existing Quick actions sheet
with inert initial focus. Navigation is required to select an action; the normal
power confirmation remains required. A new physical press cancels an existing
power confirmation rather than accepting it. ACK follows a successful frame
presentation. Lost ACKs retry without repeatedly reopening the sheet; a shell
crash before ACK permits redelivery to the restarted shell. A powerd restart
clears its volatile request and changes incarnation, so an old ACK cannot consume
a new request. Both services retain their existing restart-on-failure policy.

A foreground app is never closed, terminated, switched away from, or asked to
suspend by a short press. The selected framebuffer shell has no non-destructive
power overlay over a foreground app, so its request waits until the shell is ready
again. First-run, recovery and transition screens similarly defer presentation.
This is an explicit remaining parent `.8` behavior gap; active-shell acceptance
is `.8.1`. No claim of stock short-action parity or working suspend is made.

The paired image installs `60-pocketforge-power-key.conf` with
`HandlePowerKey=ignore` and `HandlePowerKeyLongPress=poweroff` whenever powerd is
installed. This leaves graceful long-press shutdown available with shell or
powerd absent. Build 58's preserved rootfs identifies systemd
`252.39-1~deb12u2`; its logind binary includes the long-press configuration and
message strings. The matching [systemd-stable v252.39 source](https://github.com/systemd/systemd-stable/blob/v252.39/src/login/logind-button.c)
sets LONG_PRESS_DURATION to five seconds and starts its own monotonic timer when
short and long actions differ. This is a userspace threshold, not a claim about
PMIC forced-off timing. Clean-room `POWER-SEQUENCING.md` R-E1/R-E3 assigns action
policy to userspace and records vendor hardware timing intent separately.

Hermetic checks: `cargo test -p pf-powerd --offline --locked` covers event
sequences, source selection, bounded pending/ACK races, shell restart semantics,
wrong peer admission and framed Unix socket requests with a recording executor.
Launcher tests cover presentation-before-ACK, retries, foreground deferral and
Quick actions confirmation safety. Image wiring/overlay tests check installation.
Physical KEY_POWER count, camera-observed UI, same BootID/SSH liveness, and graceful
long-press shutdown still require a coordinator-owned whole-image device test.
