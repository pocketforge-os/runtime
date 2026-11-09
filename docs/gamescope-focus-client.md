# Gamescope-side `compositor.sock` focus client (design)

Status: design only, no code. Bead `tsp-9c5171be44169385a02d`. Contract: `docs/input-ipc.md`
§1, §5, §6 and §9 and `crates/pf-input-ipc/src/lib.rs` (`pf_input_ipc::compositor`). This note
adds no new message; it says who sends the existing ones, from where in gamescope, and how the
work lands. All runtime citations are at `origin/main` 6368912. Gamescope citations are at the
platform pin `pocketforge-os/gamescope` `cf4475a7` (`pocketforge-linear`, platform `4832ce8`
`platform.lock:877-879`), read through the GitHub API, unless marked `gamescope-rust@5fb8dce4`.

## Evidence

* Device window `2b9733a7`, run r4 (`/home/matt/recovery/gfx5/mmc0-exp/r31w16-r4-evidence/push-exec-4.stdout`),
  runtime 63689128. The broker logs `compositor-sock=/run/pocketforge/input/compositor.sock`
  (stdout lines 28, 30), so it listens. `ss_peers_pre_play lines=0` (line 9) and
  `ss_peers_post lines=0` (line 26): nothing ever connected.
* The journal has no `rumble: compositor_register` and no `rumble: focus` line (0 matches).
  Every one of the six App Pad plays p0..p5 (lines 10-16) logs
  `ff_upload ... focused=false` and then `ff_play ... result=dropped_unfocused` (lines 32-47, 6 matches).
* Why: `Forwarder::new` starts unfocused (`crates/pf-input-broker/src/ff.rs:95-109`). `play` drops
  every play while unfocused (`ff.rs:186-188`). The only thing that sets `focused = true` is
  `focus_gained` (`ff.rs:112-114`), and that is reached only from `RumbleControl::focus_update`
  (`ff.rs:282-290`), which only `CompositorAuthority::rumble_focus` calls with
  `gained = matches!(surface, FocusedSurface::App { .. })` (`crates/pf-input-broker/src/session_ipc.rs:91-103`).
  `rumble_focus` runs only on a `state_snapshot` or `focus` frame (`session_ipc.rs:146-163`).
* No sender: the runtime#197 worker audited runtime, image, platform and `gamescope-rust@5fb8dce4`
  and found no producer of `CompositorToRouter::{Register, Focus, StateSnapshot}`.
  `docs/input-ipc.md:221-224` calls the client a **new gamescope facility**. I re-checked only
  two things. `gamescope-rust` has no `compositor.sock` string and no `wayland_security` file at
  5fb8dce4 (its `origin` is `ValveSoftware/gamescope`, and pin `cf4475a7` is not in that checkout).
  At `cf4475a7`, `src/wayland_security.hpp` has a role-less `TrustedClientRegistry` and a
  `TrustedClientBroker`, but `wlserver.cpp`, `steamcompmgr.cpp` and `wayland_security.*` contain no `compositor.sock` or `/run/pocketforge/input` string (grep, 0 matches); other files at the pin were not searched.
* Positive control: the direct gpio-vibrator plays d0..d3 (lines 18-22) were audible on the C270
  mic (z=12, per the work order; the spectrogram is in the evidence directory and I did not
  re-score it). So the motor, driver and mic work. The only missing piece is the focus bridge.

## Protocol

The client speaks `pf_input_ipc` major 1 (`lib.rs:29`). The transport is `AF_UNIX SOCK_STREAM`
with pf-wire framing: a big-endian `u32` length, then a JSON body of at most `MAX_FRAME` = 64 KiB
(`lib.rs:32`, `docs/input-ipc.md:29-32`). Every frame carries `"version":1,"type":...`.

1. **Register.** Connect to `sockets::COMPOSITOR_SOCKET` (`lib.rs:53`) and send `{"type":"register"}`
   with no fd. The router refuses any other first frame, and a register frame that carries a
   descriptor (`session_ipc.rs:355-366`).
2. **Registered.** Read `registered{epoch}` (`lib.rs:669-671`). The epoch is nonzero. Treat it as
   opaque. The runtime mints it from a counter (`session_ipc.rs:69,106`), but the contract
   promises an unpredictable value (`docs/input-ipc.md:83-85`). Register also forces rumble
   `focus_lost` (`session_ipc.rs:114-116`), so a new epoch always starts with the motor off.
3. **StateSnapshot.** Send exactly one `state_snapshot` (`lib.rs:582-587,621-624`) before any delta:
   * `focus`: the current `FocusedSurface` (`lib.rs:537-541`). Use the classification under
     *Gamescope integration point*, plus `text_input` and `secure_entry` (see open questions for v1 values).
   * `overlays`: keyboard, menu and toast, each named exactly once. Otherwise the snapshot is
     `IncompleteSnapshot` (`lib.rs:784-807`). In v1 all three are `unmapped` until the
     role-aware registry lands.
   * `power`: gamescope's current Power level and last sequence. Sending these never fires a
     power action (`lib.rs:573-578`).
   The router applies the snapshot and calls `rumble_focus` (`session_ipc.rs:146-153`). If the
   snapshot names `App`, the motor is armed straight away.
4. **Focus deltas.** Send `focus{epoch,surface,text_input,secure_entry}` (`lib.rs:627-632`) in the
   same frame tick as the change (`lib.rs:625`). Send it only when the triple changes. The router
   refuses deltas with the wrong epoch or deltas sent before the snapshot (`lib.rs:773-818`).
5. **Other deltas** (`overlay`, `physical_input`, `protected_power`) come in later beads. The
   focus client must not send them before the snapshot.
6. **Ping.** Answer every `ping{epoch,seq}` with `pong` (`lib.rs:659-662`). Today the router never
   sends one (there is no `Ping` writer in `session_ipc.rs`), so liveness is EOF only.
7. **Router EOF / broker restart.** Drop the epoch. Destroy every router-minted trusted
   `wl_client` and keep consuming protected Power (`docs/input-ipc.md:310-315`). Reconnect with
   backoff, 100 ms doubling to 2 s (`lib.rs:72-74`). After the new `registered`, send a fresh full
   snapshot. Never replay an old-epoch delta, request id or snapshot. The broker cannot keep
   rumble focus across its own restart, because `Forwarder` is rebuilt unfocused (`ff.rs:95-109`).
   The new snapshot re-arms it.

## Gamescope integration point

* **Where focus lives.** `steamcompmgr` picks focus in `determine_and_apply_focus`
  (`src/steamcompmgr.cpp:4211`; xdg path `:4190`). It hands the keyboard surface to wlserver
  under `wlserver_lock` (`:4400-4415`, call at `:4411`; second call site `:5569`).
  `wlserver_keyboardfocus` (`src/wlserver.cpp:2695-2718`) is the single choke point that stores
  `wlserver.kb_focus_surface` (`:2718`). Hook the client there, and also at surface destroy,
  where `kb_focus_surface` is nulled without going through keyboardfocus (`src/wlserver.cpp:750-751`).
  That second site must emit `surface: none`. Same structure at `gamescope-rust@5fb8dce4`:
  `wlserver.cpp:2346-2369`, `:606-607`; `steamcompmgr.cpp:4154`.
* **Surface → `FocusedSurface`.** Use the surface's owning `wl_client`, never a title or an
  app-chosen id:
  * `Trusted{role}`: the client is in the trusted registry and bound to `role`. Today's registry
    has no role (`src/wayland_security.hpp`, struct `Entry`; `docs/input-ipc.md:107-110`), so
    v1 can never emit `Trusted`. Registry members that have no role are reported as `none`.
  * `App{app_id}`: a public-socket client. pf-apps reach gamescope via
    `/run/pocketforge/session/wayland-0`, a symlink to gamescope's named socket
    (image `rootfs-overlay/usr/lib/pocketforge/pf-session-publish:111,118`;
    `rootfs-overlay/etc/systemd/system/pf-app@.service:22-24`). To map the client, get its pid
    from `wl_client_get_credentials` (already used at `src/wlserver.cpp:2135`), pin the process
    with a pidfd, read `/proc/<pid>/cgroup`, and take the leaf `pf-app@<id>.service`. That is
    the same rule as runtime `pf_peer_identity::app_id_from_unit` and its leaf check
    (`crates/pf-peer-identity/src/lib.rs:369-415`). Gamescope is C++, so this is a port, and it
    shares one fixture file with the Rust rule.
  * `None`: anything else. That covers no surface, gamescope's own clients, and Xwayland-backed
    windows: their `wl_client` is Xwayland itself, and the X11 pid (`steamcompmgr.cpp:5204`) is
    client-asserted. This fails closed: no app focus means no rumble.
* **Which process owns the socket.** The gamescope process (`pocketforge-gamescope.service`,
  `User=pf-gamescope`, image `rootfs-overlay/etc/systemd/system/pocketforge-gamescope.service:16-18,35`).
  Make it a non-blocking fd on the wlserver event loop with `wl_event_loop_add_fd`, the way
  `src/wayland_security.cpp:119` does. All I/O then happens on the wlserver thread, and the
  steamcompmgr thread only queues the focus triple while it holds `wlserver_lock`. The router
  pins Compositor to uid `pf-gamescope`, group `pf-input-compositor` and the exact unit
  `pocketforge-gamescope.service` (`lib.rs:53-56`; `crates/pf-input-broker/src/bin/pf-input-broker.rs:386-391`;
  `session_ipc.rs:784-795`). Two different processes can therefore never both claim the role.
* **Permissions.** `/run/pocketforge/input` is `0750 root:pf-input` (image
  `rootfs-overlay/usr/lib/tmpfiles.d/pf-input.conf:2`). The socket is `0660` with the role group
  (`lib.rs:50`). Image `rootfs-overlay/usr/lib/sysusers.d/pf-input.conf:2-7` makes `pf-gamescope`
  a member of `pf-input` and `pf-input-compositor`. The unit's `SupplementaryGroups=` names
  `pf-compositor-input` (`pocketforge-gamescope.service:18`), which is a **different** group: it
  is the evdev group from `73-pocketforge-compositor-input.rules:6-9`. The two router groups
  therefore depend on initgroups from `User=`. I have not verified this on a device.
  `/proc/<gamescope>/status Groups:` must contain both gids.
* **Unit order.** The gamescope unit already orders `After=pf-input-broker.service`
  (`pocketforge-gamescope.service:3`). The client must still tolerate a broker that is absent or
  late (see *Failure modes*). The broker unit passes `--compositor-sock`
  (image `pf-input-broker.service.d/10-app-session.conf:31`).

## Failure modes

* **Broker absent.** Connect fails with ENOENT or ECONNREFUSED: back off 100 ms up to 2 s and keep
  compositing. Rumble stays `dropped_unfocused` (safe). Log once per state change, not on every retry.
* **Broker restart.** EOF: the client clears its epoch. The new broker's `Forwarder` starts
  unfocused (`ff.rs:95-109`) and its register forces `focus_lost` (`session_ipc.rs:114-116`).
  The motor stays off until the new snapshot names `App`.
* **Gamescope crash or exit ⇒ focus_lost.** **Gap in runtime today.** `CompositorAuthority::disconnect`
  (`session_ipc.rs:214-230`) clears the session and signals but never calls `rumble.focus_lost()`,
  so a motor that is running keeps its effects mapped and stays focused until the next register.
  The runtime bead below fixes this, and its test must come first.
* **Malformed frame ⇒ stale focus.** `decode(...)?` (`session_ipc.rs:382`) returns without
  `authority.disconnect(epoch)`. That differs from the read-error paths (`:372-379`) and from the
  contract's "drop the frame and log the type" (`docs/input-ipc.md:35-38`). Today a bad frame
  can leave `current_epoch` and rumble focus stale. Same runtime bead.
* **Stale focus inside gamescope.** The surface is destroyed without a keyboardfocus call
  (`wlserver.cpp:750-751`), or the pf-app process exits while its surface is still mapped. Emit
  `focus{none}` from the destroy path. Also note that the App Pad reader closing already calls
  `reader_closed` → `stop_all` (`ff.rs:236-241`).
* **App id mismatch.** `rumble_focus` arms the motor for *any* `App` (`session_ipc.rs:102`), not
  only the app that owns the App Pad. See open questions.
* **Wrong epoch, pre-snapshot delta or incomplete snapshot.** The router refuses it and then
  disconnects (`session_ipc.rs:385-389`). The client reconnects and replays.

## Work split

Tests first in every repo. Each bead lands its failing test in the same PR as the fix.

1. **runtime (producer contract + broker fixes)**:
   a. Test that compositor EOF, a malformed frame and an apply error each end in
      `rumble: focus ... action=lost`, the motor stopped, and no `forwarded` play afterwards.
      Then make `disconnect` call `rumble.focus_lost()` and route decode errors through `disconnect`.
   b. Golden fixtures for the producer: canonical `register`, `state_snapshot` (three unmapped
      overlays) and `focus` bodies under `crates/pf-input-ipc/tests/`, so gamescope's C++ encoder
      can be byte-checked against them.
   c. A shared cgroup-leaf → app_id fixture table for `app_id_from_unit`.
2. **gamescope (`pocketforge-os/gamescope`, `pocketforge-linear`)**: the client module (event-loop
   fd, framing, epoch state, backoff) plus the hooks at `wlserver.cpp:2695-2718` and `:750-751`
   and the surface classifier. Test first: a fake router socketpair that drives
   register/snapshot/focus/EOF/reconnect, plus the fixture tables from 1b and 1c.
3. **image**: bump the gamescope producer witness to the new head. Assert the gamescope gids on
   a booted image (test: `/proc/<pid>/status` groups include `pf-input` and `pf-input-compositor`).
   Add a session test where `fake-gamescope` connects and the broker logs `compositor_register`
   (`tests/session-authority-systemd/` already has `fake-gamescope`).
4. **platform**: pin `platform.lock` last, with runtime, gamescope and image together.

## Device acceptance

All in one device window, recorded on the C270 mic, with the spectrogram scored per play window:

* **App Pad plays → motor line.** A pf-app with focus plays through its App Pad. The journal shows
  `compositor_register`, then `focus surface=app action=gained`, then `ff_play ... result=forwarded`.
  Each play window shows the motor line on the spectrogram. A **motor-off control** in the same
  window (idle span plus a play while focus is `none`) shows no line, and the direct-vibrator
  positive control repeats d0..d3.
* **stop-on-focus-loss.** Start a long effect, then move focus away (system menu, or kill the
  app surface). Expect `action=lost` and silence on the spectrogram within one frame window.
  Repeat by restarting gamescope mid-play (exercises the `disconnect` fix) and by restarting the
  broker.
* **SDL-originated rumble.** The app itself calls `SDL_RumbleGamepad` (not a test injector), and
  the journal ties `ff_upload`/`ff_play` to that app's App Pad. Whether the target app runs
  native Wayland or Xwayland must be recorded, because Xwayland maps to `none` in v1.
* **In-app button/stick delivery.** While the app holds focus, buttons and both sticks reach the
  app (app-side log). Menu still returns, and nothing reaches the app while the destination is
  `quarantined`.

### Could not verify

* That `pf-gamescope` really holds the `pf-input*` gids at runtime. Nor whether SDL3 in the target
  pf-app picks Wayland or X11.
* The C270 z=12 score, which I took from the work order and did not re-score.
* Behaviour at the newest `pocketforge-linear` head `3d085131`, past pin `cf4475a7`.

### Open questions (owner / gpu-16)

* Must `focus` gain require `app_id` == the App Pad owner's app id, or is any `App` enough
  (`session_ipc.rs:102`)?
* v1 values for `text_input` and `secure_entry`: `{false, normal}` and `false`, or fail-closed
  `secure_entry=true` until text-input-v3 tracking lands?
* Xwayland pf-apps: is `none` (no rumble) acceptable for v1, or is a trusted X11 pid path needed?
* Should the router start sending `ping`? Without it, a wedged gamescope that never reaches EOF
  keeps focus.
