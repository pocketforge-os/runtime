# Input-layer IPC contract: router ↔ gamescope ↔ pf-osk ↔ system menu (v1)

> **Status:** frozen interface for epic `tsp-ihne1` (bead `tsp-ihne1.36`). This page is the
> contract; `crates/pf-input-ipc` is its machine-checkable half (types, constants, fixtures,
> two pure state machines). A second-language peer (gamescope is C++) can be written from this
> page alone. The design it freezes is mission-control `design/input-layer/` (sections
> *Router and Menu*, *System menu*, *Security*, *How a keypress reaches an app*) with the round-7
> adjudication folded in (items 1, 14, 15, 17). Changes to this page are contract changes: bump
> the major for incompatible ones, record the rest in the PR and the bead.

Four parties share the built-in controller and the screen:

| Party | Process | Owns |
|---|---|---|
| **Router** | `pf-input-broker`, root, session-wide | the `EVIOCGRAB` on the pad, one virtual device per destination, Menu, modality, the one return path |
| **Compositor** | gamescope (PocketForge fork), session user | real keyboards/mice/touch (libinput), focus, trusted overlays, `secure_entry` |
| **Keyboard** | pf-osk, session user, system-launched | the keyboard surfaces |
| **Menu** | the system-menu provider, session user, resident, system-launched | the sheet, its items, its heartbeat |

The router is the hub. It is the only listener; the other three connect to it and reconnect
after EOF. Nothing on these sockets is the pad stream: the pad reaches the keyboard and the menu
as an evdev file descriptor handed over `SCM_RIGHTS`, exactly like the app's `Acquire("input")`.

## 1. Transport

* `AF_UNIX` `SOCK_STREAM`. Framing is pf-wire's: `len: u32` big-endian, then `len` bytes.
  `len` ≤ 65536 (`MAX_FRAME`); a larger prefix closes the connection without allocating.
* The body is one JSON object. It always carries `"version"` (the schema **major**, currently
  `1`) and `"type"` (the message name). Field order is free. Strings are UTF-8.
* **Version check first.** A receiver that does not speak the major refuses the frame as
  `UnsupportedVersion` before it reads `type`, and closes the connection (a peer on another major
  can never be understood). Within a major: unknown fields are ignored; an unknown `type` or a
  wrong field shape is `Malformed`, and the receiver drops that frame and logs the `type` only
  (never the body: bodies can carry secrets). Adding optional fields or new types is a minor
  change; changing a meaning, removing a field or making a new field mandatory is a new major.
* File descriptors travel with the frame that announces them (`registered`,
  `trusted_connection_result`) as `SCM_RIGHTS` ancillary data on the frame's first byte. A
  frame that announces an fd and arrives without one is `Malformed` and closes the connection.
* **Nothing sent here blocks the router's input pump.** The pump only flips atomics and hands
  tokens to a worker thread; all socket I/O, deadlines and fallbacks run on that thread
  (`crates/pf-input-broker/src/safe_return.rs` already works this way).

### Sockets, ownership, admission

Apps run as the same uid as the system processes, so a path alone separates nothing. Every
endpoint is gated twice: the directory and socket modes keep apps from connecting, and the
router re-checks the peer on accept with `SO_PEERCRED` (uid must be the session uid) and
`SO_PEERGROUPS` (must contain the endpoint's group), the mechanism `pf-prefsd --writer-group`
uses. Apps never carry any of these groups. The image creates the directory and groups.

| Path | Mode / owner | Who connects | Constant |
|---|---|---|---|
| `/run/pocketforge/input/` | `0750 root:pf-input` | nobody; traversal only | `sockets::INPUT_RUN_DIR` |
| `/run/pocketforge/input/compositor.sock` | `0660 root:pf-input-compositor` | gamescope | `sockets::COMPOSITOR_SOCKET` |
| `/run/pocketforge/input/keyboard.sock` | `0660 root:pf-input-keyboard` | pf-osk | `sockets::KEYBOARD_SOCKET` |
| `/run/pocketforge/input/menu.sock` | `0660 root:pf-input-menu` | system menu | `sockets::MENU_SOCKET` |

`menu.sock` is the endpoint `pf-input-broker --system-menu-sock` already serves (runtime#108);
this page fixes its path and extends its protocol within the same major.

**Registration.** The first frame on every connection is `register`. One peer per endpoint: a
second registration replaces the first after the router has released that destination (pad back
to the app, `dismiss`/`hide` sent to the old peer, its connection shut). A connection that sends
anything else first is closed.

**Reconnect.** Clients retry after EOF starting at 100 ms and doubling to 2 s
(`sockets::RECONNECT_INITIAL`, `RECONNECT_MAX`). The router restarts under systemd; the
"you are never trapped" guarantee holds through the restart because the router grabs the pad
and Menu before it serves any endpoint, and Menu with no provider is a direct return.

## 2. Timeouts (defined once: `pf_input_ipc::timeouts`)

| Constant | Value | Meaning |
|---|---|---|
| `MENU_ACK_TIMEOUT` | 250 ms | `system_menu` → `ack`. Late or invalid: the provider is dropped and the router performs the return path itself. |
| `MENU_SHOWN_TIMEOUT` | 500 ms | `ack` → `shown`. Late: `dismiss{shown_timeout}` and return. |
| `MENU_HEARTBEAT_INTERVAL` | 500 ms | `ping` cadence while the menu is open. |
| `MENU_HEALTH_TIMEOUT` | 1 s | no `pong` for this long: `dismiss{unhealthy}`, pad back to the app. |
| `MENU_HOLD_RETURN` | 2 s | Menu held this long returns to the shell from anywhere; the router measures the hardware press itself, so a provider that acks but never draws cannot trap the user. |
| `USER_PRESS_WINDOW` | 1 s | `show_modal` is admitted only within this window after a press the router routed to that app (built-in pad, trusted keyboard, pointer click or an accepted menu action for that app). One press admits one open. |
| `PONG_MAX_AGE` | 1 s | a `pong` echoing a `seq` older than this is ignored. |

`MenuSession` in the crate is the deterministic model of the first five: feed it `ack`,
`shown`, `pong`, `held` and `tick` with the elapsed time since the press and it returns the
step the router must take. The router bead (`tsp-ihne1.7`) drives the real worker from it.

## 3. Menu channel (`menu.sock`)

Provider → router:

| `type` | Fields | Meaning |
|---|---|---|
| `register` | — | first frame |
| `ack` | `action: system_menu` | ownership of the press accepted (not: drawn) |
| `shown` | — | first frame of the sheet committed to gamescope |
| `return` | — | the Return item; the router runs the one return path (`safe_return`) and answers `ack{return}` |
| `closed` | `reason: resume \| returning \| handoff` | the sheet closed by the provider's choice; pad goes back to the app |
| `secure_entry` | `active: bool` | a field on the provider's own surface is secret; relayed to gamescope as `surface_secure_entry` |
| `pong` | `seq` | echo of `ping` |

Router → provider:

| `type` | Fields | Meaning |
|---|---|---|
| `registered` | — (+ pad fd) | the read fd of the menu's private virtual pad, and the trusted Wayland client fd once §6 is in force |
| `system_menu` | `front?: {app_id, name, unit}` | a Menu press; `front` absent in the shell |
| `ack` | `action: return` | reply to `return` |
| `dismiss` | `reason: shown_timeout \| unhealthy \| hold_return \| front_changed` | hide now; the pad is already back with the app |
| `ping` | `seq` | heartbeat |

**Sequence of one open.** Press → router withholds Menu from every destination, sends
`system_menu` → `ack` ≤ 250 ms → `shown` ≤ 500 ms → router synthesises releases on the app's
device and re-points the pad to the menu → `ping`/`pong` every 500 ms → `closed` or `return` or
`dismiss` → router synthesises releases on the menu's device and re-points the pad to the app.
While open: the keyboard gets `hide{menu_open}`, the pointer freezes, the app is told the menu
opened and sees its held buttons released once. The press that closes the menu never reaches
the app. A `system_menu` while one is in flight is coalesced, not queued.

## 4. Keyboard channel (`keyboard.sock`)

pf-osk → router:

| `type` | Fields | Meaning |
|---|---|---|
| `register` | `surfaces: [docked, floating, number_pad, edit_pad, modal, strip]` | first frame; what this build can show |
| `visibility` | `surface, visible, wants_pad, rect?` | a surface appeared or went. `wants_pad` is pf-osk's half of the pad gate (§7c); the strip never wants it |
| `modal_result` | `request_id, outcome: submitted \| cancelled, text?` | result of `show_modal`; `text` only with `submitted`, never logged by anyone |
| `pong` | `seq` | |

Router → pf-osk:

| `type` | Fields | Meaning |
|---|---|---|
| `registered` | — (+ pad fd) | the keyboard's private virtual pad, and the trusted Wayland client fd once §6 is in force |
| `show_modal` | `request_id, app{app_id,name,unit}, purpose, title, initial_text, max_chars, multiline` | open the modal keyboard for the app in front (spec §12.1). The router has already applied the open guard; pf-osk draws "Typing for ‹name›" from `app`, never from the request text |
| `hide` | `reason: menu_open \| physical_keyboard \| app_lost_front \| app_exit` | hide now; `menu_open` means come back unchanged on Resume; `physical_keyboard` lets the strip stay |
| `ping` | `seq` | |

Opening for text-input apps is **not** on this channel: gamescope relays text-input-v3 to
pf-fcitx5, which asks pf-osk over their private bus. This channel carries only what the router
owns: the pad, modal requests from the SDK, and hides the router decides.

## 5. Compositor channel (`compositor.sock`)

gamescope → router:

| `type` | Fields | Meaning |
|---|---|---|
| `register` | — | first frame |
| `focus` | `surface: {kind: none \| app{app_id} \| trusted{role}}, text_input{active, purpose}, secure_entry` | sent on the frame the focus, the text-input state or `secure_entry` changes |
| `overlay` | `role: keyboard \| menu \| toast, state: mapped \| unmapped, rect?` | a trusted overlay was mapped or unmapped |
| `physical_input` | `class: keyboard \| mouse \| touch` | first event of a class and every class change, never per event; the router owns modality and answers a real keypress with `hide{physical_keyboard}` |
| `trusted_connection_result` | `request_id, result: {status: ok} (+ fd) \| {status: refused, reason}` | answer to `trusted_connection` |
| `pong` | `seq` | |

Router → gamescope:

| `type` | Fields | Meaning |
|---|---|---|
| `registered` | — | |
| `trusted_connection` | `request_id, role, unit` | the provenance API (§6) |
| `surface_secure_entry` | `role, active` | relay of a trusted surface's own flag (§3) |
| `ping` | `seq` | |

`purpose` is the text-input-v3 `content_purpose` name: `normal, alpha, digits, number, phone,
url, email, name, password, pin, date, time, datetime, terminal`.

## 6. Trusted-overlay provenance

gamescope (fork, PR #9, tsp-op5a.440.11) trusts a Wayland client by **connection provenance**:
only a `wl_client` gamescope itself created from a socketpair, and handed out through its private
broker, may bind the privileged globals. This contract extends that check with a **role**:

1. The router, on behalf of a system-launched process (a systemd unit the session started: pf-osk,
   the system menu), sends `trusted_connection{request_id, role, unit}`.
2. gamescope creates a socketpair, registers the server end as a trusted `wl_client` bound to
   exactly that role, and replies `trusted_connection_result{status: ok}` with the client end
   attached by `SCM_RIGHTS`. `unit` is recorded for diagnostics and is never an input to the trust
   decision; names, pids and client-chosen identifiers grant nothing.
3. The router forwards that fd to the process in its `registered` frame. The process opens its
   Wayland display from the inherited fd (`wl_display_connect_to_fd`), never from the named socket.
4. A layer surface is a **trusted overlay** iff its `wl_client` came through step 2 **and** the
   overlay role it requests equals the connection's role. A keyboard connection cannot create a
   menu surface; a public-socket client cannot create any. gamescope reports every map/unmap of
   a trusted overlay as `overlay` on the compositor channel.
5. One live connection per role. A second `trusted_connection` for a role already bound is
   `refused` until the router has shut the previous peer (registration replacement, §1).

gamescope's private broker today serves only its reaper; admitting the router as a second caller
is the gamescope side of this contract (`tsp-op5a.440.13`, `.5`).

## 7. `secure_entry`

One producer: **gamescope**, on the same frame as the focus change. `secure_entry` is true iff
the focused app surface's text-input-v3 purpose is `password` or `pin` **or** the focused
trusted surface has set `surface_secure_entry{active: true}` (relayed from the menu channel: the
Wi-Fi join field, QR share). The router does not compute it. It relays gamescope's last value
unchanged as `events::RouterEvent::SecureEntry` to the capture lane, which blanks the keyboard
and key highlights in screenshots, streams and replay. This replaces the design page's "the
router publishes a secure_entry signal" (adjudication item 14).

## 8. Ordering rules

* **(a) Menu first.** The router consumes every Menu transition before any routing. Menu never
  reaches the app, the keyboard or the pointer, including after a dropped-event resync.
* **(b) Releases before a switch.** Before the pad is re-pointed, the router synthesises releases
  for every held button and centred axes on the device it is leaving. No reader ever sees a
  press without its release, and no two readers ever share a stream.
* **(c) Keyboard pad gate.** The pad goes to the keyboard only when **both** pf-osk's
  `visibility{visible: true, wants_pad: true}` and gamescope's `overlay{keyboard, mapped}` have
  arrived. It returns to the app on the **first** of `visibility{wants_pad: false}` or
  `visible: false`, `overlay{keyboard, unmapped}`, `hide` being sent, or the keyboard
  connection closing. A stale `mapped` after EOF routes nothing until a fresh visibility claim.
  `PadGate` in the crate is this rule.
* **(d) Menu open/close.** §3's sequence. The menu outranks the keyboard: while the menu is
  open the pad gate answers `menu` regardless of the keyboard's halves.
* **(e) The closing press.** The press that closes the menu (Resume, B) is consumed by the
  menu's device and never replayed to the app.
* **(f) Focus and visibility between gamescope, router and pf-osk.** gamescope emits `focus`
  before `overlay` for the same frame; the router applies `focus` first (front/secret state),
  then `overlay` (pad gate), then anything from pf-osk that arrived in between. The router never
  acts on a pf-osk `visibility` for a surface gamescope has not reported mapped, except to
  release (toward the app).
* **(g) Front changes.** When the app in front exits or changes: `dismiss{front_changed}` to an
  open menu, `hide{app_exit | app_lost_front}` to the keyboard, synthetic keys still held by the
  fallback keyboard released, key repeat reset (adjudication item 17), and any pending
  `show_modal` answered `modal_result{cancelled}`.
* **(h) Modal open guard.** `show_modal` is sent only within `USER_PRESS_WINDOW` of a press the
  router routed to that app; otherwise the SDK call fails with `PF_ERR_NOT_USER_INITIATED`
  (adjudication item 15). Measured by the router; pf-osk trusts the router.

## 9. What is deliberately not here

* No CBOR. The merged broker already speaks JSON bodies in pf-wire frames, no CBOR crate is
  vendored, and the pad is an fd, not frames. The fixture set is JSON only.
* No Wayland XML rendering of the compositor channel. gamescope implements one tiny codec (§1)
  rather than a new protocol; the privileged Wayland globals it already filters stay as they are.
* No pointer or SDK-front messages; those are the pointer and SDK beads, which cite this page.
* The directory, groups and units are image work (tmpfiles, `pf-osk.service`,
  `pf-system-menu.service`); this page fixes their names.

## 10. Fixtures and tests

`crates/pf-input-ipc/fixtures/<channel>/*.json` holds one file per message (60 files); the tests
decode, re-encode and compare each one and check that the `type` set equals the published list,
so a new message without a fixture fails CI. `fixtures/refused/` holds the frames that must be
refused (majors 0 and 2, missing version, unknown type, wrong shape, non-object). The menu frames
are compared against `pf-input-broker`'s merged provider constants so the two cannot drift.
