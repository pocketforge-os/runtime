# URL handoff: `OpenUrl` and `ReturnToCaller` in the session authority

Bead tsp-ght0z, design of record tsp-mc9m.41.996.5 (owner decisions A–D, 2026-10-11). Apps never
reach the session authority. The input epic's SDK front identifies each app once and forwards
these two verbs over a **private socket**; the trusted shell may also open URLs on its own socket.

## Identity: one implementation

`crates/pf-peer-identity` is the single peer-identity implementation (socket-bound `SO_PEERCRED`
and `SO_PEERGROUPS`; `SO_PEERPIDFD`, `pidfd_open` on 5.15, an opened `/proc/<pid>` directory on
4.9; cgroup reads bracketed by liveness checks). `pf-prefsd` classifies preference writers with it
and the authority uses it for both of the following; nothing forks it.

- **The front itself** is authenticated at accept: peer uid equals `--front-user` and the peer's
  systemd unit equals `--front-unit`. Any other peer is closed without a frame and logged
  `front_refused reason=peer_identity`.
- **No transitive trust.** Every front request carries the caller's process handle over
  `SCM_RIGHTS`: a pidfd, or on the A133/4.9 kernel (no pidfds) the front's opened `/proc/<pid>`
  directory. The authority classifies the handle from the kernel's own description of the
  descriptor, reads its cgroup, and requires `pf-app@<id>.service`. The derived id must equal the
  front's `app_id` claim; a mismatch, a missing handle or a non-app handle answers `denied` and
  logs `url_refused reason=identity_mismatch|identity_unverified|missing_claim`.

## Wire (versioned, minimal)

Front socket: one `pf-wire` frame per request, JSON `{"version": 1, "method": ..., ...}`
(`FrontEnvelope`), with the process handle attached to the first `sendmsg`. Only `open_url` and
`return_to_caller` are accepted; other verbs and other versions answer `error`.

| Verb | Fields | Where |
|---|---|---|
| `open_url` | `app_id` (front only), `url`, `flags` | front and shell sockets |
| `return_to_caller` | `app_id`, `url?` (reserved for auth sessions) | front socket only |

Flags: `AUTH_SESSION = 1` (reserved, refused everywhere in v1), `CAPTIVE_PORTAL = 2` (accepted only
from the shell socket; an app can never open a portal-mode session). Unknown bits are refused.

Results (`result` field): `launched{session_id}`, `delivered`, `no_handler`, `invalid_url`,
`denied`, `busy`, `rate_limited`, `restored`, `caller_gone`, plus the existing `error{message}`.
`no_handler` is what drives an app's QR fallback. Every refusal is logged with its reason; the
reason is never sent to the caller beyond the typed result.

## Policy (check order for a front `OpenUrl`)

1. front peer authenticated (connection), envelope version, verb;
2. app id re-derived from the handle and compared with the claim;
3. rate limit: 3 per 10 s per caller (the shell counts as one caller);
4. flags;
5. URL: `http`/`https` only (any case), an authority with a host, printable ASCII, at most 8 KiB;
   `file:`, `data:`, `javascript:` and everything else are `invalid_url`;
6. the caller's manifest declares the `open_url` capability (owner decision B);
7. the caller is the foreground app (`denied` otherwise); the slot is free and the phase is
   `running` (`busy` otherwise);
8. handler resolution through `UrlHandlerResolver` (the daemon's `--url-handler` until pf-prefsd's
   default-handler key, tsp-mv7zn); an uninstalled handler is `no_handler`;
9. `SessionSystem::open_url(UrlHandoff)`. The URL is a field of the handoff. No command template
   has a URL token, so no argv is ever derived from it.

## The depth-1 return slot

A successful `OpenUrl` records `ReturnSlot { caller, handler_session_id, handler_item_id }` in the
persisted state. It never stacks: a second `OpenUrl` while a slot exists is `busy`.

- **App caller** (owner decision A): the caller keeps running behind the handler. The handler gets
  a history entry. `ReturnToCaller` from the handler restores the caller through
  `SessionSystem::restore_caller` and clears the slot; the handler keeps running in the background
  (which is what makes a later `delivered` possible). If the handler exits on its own, the
  authority restores the caller itself. If the backgrounded caller is reaped (memory policy,
  tsp-z8vav) or crashes, its history entry is closed, the handler is promoted to the foreground
  session and the slot remembers `caller_gone`; `ReturnToCaller` then answers `caller_gone`, a
  normal outcome, and the browser's own exit runs the usual shell-restoration ladder. No Terminal
  event is published for the caller, because a Terminal means the shell is back in front.
- **Shell caller** (captive portal, tsp-7s0ti): the handler is a normal foreground session.
  `ReturnToCaller` means Home and goes through the protected return path.
- A protected `SafeReturn` with a slot stops the handler first, then runs the ladder on the phase
  session.

## Foreground switch

The switch to the handler and back goes through the trusted session path (G2.2 tsp-op5a.440.3,
G2.3 tsp-op5a.440.4, G2.4 tsp-op5a.440.5), never the `GAMESCOPECTRL_BASELAYER_*` atoms. Until
those land, `CommandSystem::open_url` and `restore_caller` refuse with a typed error and the daemon
has no `--front-socket` in its unit; the authority's behaviour is covered hermetically through the
`SessionSystem` trait.

## Daemon flags

```text
--front-socket PATH --front-user NAME --front-unit UNIT   # the private SDK-front socket
--url-handler ITEM_ID                                      # the pf-app that opens http(s) URLs
```
