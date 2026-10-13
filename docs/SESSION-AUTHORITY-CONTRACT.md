# PocketForge session-authority RPC contract

This is the contract of record for the durable session-authority socket
(`pf-session-authorityd`, implemented by `crates/pf-session-authority`). It is a control-plane
boundary separate from the compositor session publication in `SESSION-SOCKET-CONTRACT.md` and
from the frozen `pf-wire` broker wire (PFW1). Frames are `pf-wire` (big-endian u32 length +
body); bodies are JSON payloads defined here.

## Contract versions

| Version | Constant | Meaning |
| --- | --- | --- |
| 1 | `SESSION_RPC_CONTRACT_V1 = 1` | Original vocabulary. No `contract` key anywhere on the wire. |
| 2 | `SESSION_RPC_CONTRACT_V2 = 2` | Adds the close-progress events the system menu needs (every-menu-spec §9.6) and the `contract` negotiation key. Current: `SESSION_RPC_CONTRACT_CURRENT = 2`. |

Negotiation is per `events` request:

* A request without `contract` is v1. A v1 request serializes byte-identically to the
  pre-negotiation shape; `contract` is skipped when it equals 1.
* The `events` response echoes the negotiated `contract`, omitted when it is 1, so v1
  exchanges are byte-identical in both directions.
* A v2 client talking to a v1 daemon works: the old daemon ignores the unknown request key and
  answers with the v1 vocabulary and no `contract` key, which the client reads as v1.
* A v1 client talking to a v2 daemon works: v2-only events are filtered at delivery
  (cursor-safe; sequences and compaction are untouched), so a v1 stream never carries an
  event tag an old decoder would reject.
* `contract` greater than `SESSION_RPC_CONTRACT_CURRENT` is refused with
  `{"result":"error","message":"unsupported session rpc contract <n>"}`. Refusing an unknown
  future contract is deliberate: guessing at semantics it does not know is worse than a
  loud refusal.

## Methods

All requests are `{"method": "<name>", ...}`:

| Method | Request | Response |
| --- | --- | --- |
| `launch` | `{item_id}` | `accepted {session_id}` / `rejected_busy` / `item_unavailable` |
| `safe_return` | — | `ok`. Intakes a durable safe-return while `starting`, `running` or `stop_failed`; ignored in every other phase. |
| `events` | `{client_id, contract?}` | `events {events: [[sequence, event]...], contract?}` |
| `acknowledge` | `{client_id, sequence}` | `ok`, or `error` when `sequence` was never published |
| `history` | — | `history {entries: [HistoryEntry...]}` |
| `observe` | `{observation: {kind, ...}}` | `ok` |
| `tick` | — | `ok` |
| `orientation` | `{version: 1, after_serial?, wait_ms?}` | `orientation {version: 1, state}`. Returns the complete current snapshot; when `after_serial` equals the current serial it waits for a semantic change or at most 30 seconds. |

Orientation waits run on an independently bounded watch lane and never enter the lifecycle
request channel. Filling all orientation waiter permits therefore does not consume or delay the
existing `safe_return` capacity. Gamescope is authenticated as a read-only compositor peer;
the broker may read orientation and call `safe_return`, while application/front peers cannot
read it.

The state reports the final `output` quarter turn and its `whole_body` and `screen_only`
components. Only `whole_body` drives gamepad remapping. User preferences are the pf-prefs keys
`auto_rotate` (boolean, default `true`) and `user_rotation`
(`normal|left|upsidedown|right`, default `normal`). Any active `SW_ROTATE_LOCK` or
`auto_rotate=false` locks whole-body rotation to `user_rotation`; sensorless devices also use
`user_rotation`. An eligible accelerometer wins only while unlocked.

App declarations constrain compatible presentation rather than rotating physical output. A
180-degree difference within the same aspect class is `fill`: an app declaring only `landscape`
stays in the device's native-landscape orientation when the user holds the device flipped, so it
appears upside-down to that user by design. This matches Android
`SCREEN_ORIENTATION_LANDSCAPE`, not `sensorLandscape`; apps that want both directions declare
`landscape_flipped` too. Portrait/landscape aspect mismatches use `letterbox`.

Events are delivered per `client_id` from a durable cursor: each event carries a monotonic
`sequence`, clients acknowledge the last sequence they processed, and acknowledged prefixes are
compacted from the pending queue. Delivery is at-least-once; consumers must treat events as
idempotent state assertions, not commands.

## Event vocabulary

`{"event": "<tag>", ...}`, tags snake_case:

| Tag | Contract | Meaning |
| --- | --- | --- |
| `starting` | v1 | Session intent accepted; unit start invoked. |
| `running` | v1 | App unit observed active. |
| `stopping_gracefully` | v2 | Graceful stop accepted (`systemctl stop --no-block`). Menu row: "Closing…". |
| `force_stopping` | v2 | Grace window expired or the stop request itself failed; `enforce_termination` was invoked. `{session_id, reason}` with `reason` in `stop_timeout` / `stop_request_failed`. Menu row: "Force closing…", detail "didn't stop in time" for `stop_timeout`. |
| `observation_complete` | v1 | Restoration ladder finished; terminal receipt follows immediately. |
| `returned` | v1 | Session ended with receipt `Returned` (`{session_id}`). |
| `forced_close` | v1 | Session ended after an escalated close (`{session_id}`). This is §9.6's "Returned with escalated flag": the menu closes to the shell and toasts "‹App› was force-closed". |
| `crash` | v1 | Session ended as a crash (`{session_id, summary}`). |
| `stop_failed` | v2 | Every close attempt failed (`{session_id, reason}`, reason shaped `app_exit_failed: <detail>`). Durable and retryable: the phase stays `stop_failed` and a further `safe_return` retries the whole close sequence. Menu row: "Close ‹App›" with detail "Couldn't close. Hold ‹Menu› 2 s"; activation retries. |
| `recovery_required` | v1 | Non-close failure (ladder/system). Terminal until operator recovery (`{session_id, reason}`). |

Starting/Running observations of already-ended sessions are filtered at delivery and never
reach a fresh cursor; close-progress filtering uses the same cursor-safe mechanism.

## Phases

The durable phase (persisted in the authority state file) is one of: `idle`, `starting`,
`running`, `stopping_gracefully`, `force_stopping`, `stop_failed`, `restoring`,
`recovery_required`. `Phase::StopFailed` is the only retryable failure phase; its JSON shape is
pinned by tests:

```json
{"StopFailed":{"session_id":"session-1","item_id":"org.example.game","reason":"app_exit_failed: refused"}}
```

Persisted-state compatibility: v1 state files decode unchanged under v2 (new variants are
additive; `pending` entries keep their externally tagged shapes). A v1 daemon reading a v2
state file that contains close-progress events fails closed as corrupt, exactly as it does for
any unknown state; downgrading mid-close is not supported.

## Close model and escalation

The authority escalates by itself; the menu shows progress only and never takes a second
decision (every-menu-spec Q3). One close sequence:

1. `safe_return` → `systemctl stop --no-block pf-app@<item>.service` →
   `stopping_gracefully`. Non-blocking so the authority keeps ticking while the unit stops;
   the unit's own `TimeoutStopSec` (2 s in the image) remains the backstop for the process
   group.
2. If systemd reports the unit inactive during the grace window, the close completes as
   `returned` — even when the unit result is `timeout` (systemd's own SIGKILL beat the
   authority; no second kill is sent).
3. If the grace period (`--grace-period-ms`, default `DEFAULT_GRACE_PERIOD` = 1500 ms)
   expires first, the authority escalates: `force_stopping {reason: stop_timeout}` after
   `systemctl kill --kill-who=all`. Budget: 1500 ms grace + ≤300 ms hang-sampler + kill spawn
   must stay inside the unit's 2 s backstop. A daemon restart loses the monotonic deadline; a
   reopened `stopping_gracefully` session re-reads the unit before acting: still active →
   immediate escalation (`stop_timeout`), already inactive → the close completes as `returned`
   with no second kill and no sampler run, unknown state → typed `systemd_state_unknown`
   recovery.
4. If the stop request itself failed in step 1, escalation is immediate with
   `force_stopping {reason: stop_request_failed}`.
5. If the kill fails, the close is `stop_failed` (durable, logged, retryable) — never a
   silent wedge and never a terminal `recovery_required`.
6. Unit inactive after escalation → restoration ladder → `forced_close`.

The service loop wakes at armed deadlines instead of waiting for the next periodic tick
(`Authority::time_until_next_deadline`), so escalation lands at the deadline, not up to a tick
interval late.

### Escalation hook

`Authority::with_escalation_hook` attaches a sampler (the crash collector's hang sampler,
`cr-collect`) that runs synchronously immediately before every close-path
`enforce_termination`, with `EscalationContext { session_id, item_id, reason }`. The hook must
self-bound (the design budget is 300 ms); the authority adds no timeout of its own. With no
hook attached, escalation is a single branch — pinned by a timing test with a sleeping-hook
negative control. The hook does not fire when systemd beats the deadline or when the unit is
already inactive across a daemon restart (no kill is sent in either case) or outside close
paths.
