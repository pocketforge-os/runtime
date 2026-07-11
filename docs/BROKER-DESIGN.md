# PocketForge capability broker — design + threat model (`tsp-e1b.3`)

> **Status:** v0 of the broker daemon core. This documents the architecture, the audited trust
> surface, the default-deny posture, the `app.toml` launch contract, and — honestly — **what v0
> enforces vs. what is deferred to the Phase-2 substrate**. The implementation is
> `crates/pf-broker`; the cooperative loopback it supersedes is `crates/pf-broker-ref`.

## 1. Role + position

The broker is the platform-side holder of authority. Apps hold **no ambient `/dev/*` access**;
they call the `pocketforge` facade (`pf.acquire::<Cap>()`), which speaks the PFW1 wire
(`wire/WIRE-PROTOCOL.md`) to the broker. The broker owns the real device fds (`/dev/input`,
`pwm-vibrator`, IIO, GNSS, audio), decides every request against policy, and vends back either a
result or one of the four typed errors. It runs in the **M1.D supervisor's privileged context**
(`tsp-iuz.3`, currently PAUSED); when M1.D resumes, the broker is the component the supervisor
launches alongside each app.

```
   app (any language)                          broker (this crate)
   ├─ libpocketforge ── PFW1 (AF_UNIX) ───────▶ serve_enforcing  ──┐
   │   pf.acquire::<Imu>()                       SO_PEERCRED check  │
   │                                             EnforcingBackend ──┤── ceiling (app.toml use=[])
   └─ input hot path ◀── SCM_RIGHTS fd ──────── (.6 input broker)   │── default-deny (inner)
                                                                    │── per-cap quota
                                                  InProcessBackend ◀┘── descriptor presence/consent
                                                  (owns the device fds)
```

The broker is deliberately **small + audited** — it is new privileged code on the trust path, so
its surface is intentionally tiny: parse a manifest, validate it, gate a fixed set of operations.

## 2. The `app.toml` launch contract (the CapDL-style authority graph)

> **Protection tiers + the trust model** (normal/dangerous/signature capability tiers, the
> blessed-binary app exemption, the signature-tier launch reject, and the entropy-ungated rationale)
> are specified normatively in [`PERMISSION-MODEL.md`](PERMISSION-MODEL.md) (E3 / `infra-102`). This
> section covers the validator mechanics; that doc covers the *policy* it enforces.

```toml
[app]
id  = "com.example.hwprobe"
use = ["input", "vibration", "imu?", "location:approximate", "egress:steampowered.com", "entropy"]
```

`use = [...]` is a **static authority graph** — the CEILING of what the app may ever acquire. The
broker validates it **before the app runs** (`AppManifest::validate` against the device
descriptor) and refuses to launch on any violation:

| reject reason            | example                          |
|--------------------------|----------------------------------|
| unknown capability       | `"telepathy"`                    |
| duplicate capability     | `"input", "input"`               |
| bad modifier             | `"location:teleport"`            |
| undescriptored **required** hardware cap | `"imu"` on the a133 (no IMU) |

**Vocabulary (v0; co-designed with E3 / `infra-102` when filed):**
- `cap?` — **optional**: the app handles a runtime `HardwareAbsent`, so it is allowed even when
  the device can't back it. This reconciles the ceiling with the cross-device graceful
  degradation `.4` proved: `imu` *required* on the a133 is rejected (over-broad), but `imu?` is
  accepted and returns `HardwareAbsent` at runtime. **"Cannot back" means the platform has no
  such capability TYPE (unknown) OR a *required* hardware cap the device lacks — never a
  known-but-optional one.**
- `cap:modifier` — a scope. `location:{approximate|precise}` (privacy fuzzing tier, E3 owns the
  full policy); `egress:<host>` (the destination host for the network-send capability).

`egress` is a platform capability (network *send*), not a descriptor hardware row — it is the
anti-exfiltration half of `location` (`.4`: location-read ≠ location-send). It is declarable and
host-scoped; the broker records the allowed hosts.

## 3. Runtime enforcement (`EnforcingBackend`)

The validated manifest is wrapped around the inner v0 backend as a `Backend` impl, so the **same
`.2` wire server serves it unchanged** and an out-of-process app (E6) gets identical semantics to
the in-process backend except where the manifest/quotas legitimately tighten them — the
backend-swap, now enforcing. Per request:

1. **Ceiling** — a capability not in `use=[]` is `PolicyBlocked` (`acquire`/`get`/`set`) or
   `Denied` (`query`, so authority isn't leaked). Undeclared haptics is a cosmetic `NoopSuppressed`.
2. **Default-deny** — preserved from the inner backend: privacy caps (`location`/`gnss`) stay
   `ConsentDenied` until E3 consent grants them.
3. **Entropy is the ungated exception** — `entropy` is auto-granted with **no ceiling, no consent,
   no quota**. Rationale: the OS CSPRNG is **non-exhaustible** — one app reading `/dev/urandom`
   does not deplete it for another, and it carries no privacy payload — so gating it buys no
   security and only adds friction. This is the single deliberate hole in default-deny, documented
   so it is a decision, not an oversight.
4. **Per-capability quota** — a dangerous cap (`location`, `gnss`, `egress`) is rate-capped via
   the session `QuotaLedger`; exhaustion is `PolicyBlocked`. As of `tsp-ht0p.4` (merged), the
   ledger is a wall-clock **token bucket**: tier-default `(capacity, refill_per_sec)` per cap
   (`location`/`gnss` = 60 burst @ 1/sec; `egress` op count = 16 burst @ 0.25/sec; every other
   cap is UNGATED — so `entropy` above is *structurally* never rate-limited, not just
   short-circuited). A `Clock` trait + `ManualClock` make refill testable without sleeps. The
   **egress byte ledger + per-host log** — `pocketforge::managers::egress_log::EgressLog` — is
   persistent (JSONL, same dialect as `.3`'s AppOps ledger; separate directory), inspectable via
   `pf-permissions egress`. `EgressManager::with_accounting` refuses a send to an UNDECLARED
   host (typed `PolicyBlocked` + `refused` row) without spending an op token. All three are
   **cooperative accounting** (Q1 ruling: v1 = contract, kernel/netns enforcement is the
   follow-on tracked in [EGRESS-ENFORCEMENT-SEAM.md](EGRESS-ENFORCEMENT-SEAM.md)).

## 4. Peer-credential check (`SO_PEERCRED`)

At accept time the broker reads the connecting peer's kernel-attested `SO_PEERCRED`
(pid/uid/gid — unforgeable by the peer) and refuses any peer whose uid ≠ the app's expected uid.
In the substrate deployment the supervisor bind-mounts a **per-app socket** into the app's
namespace and sets the expected uid; `SO_PEERCRED` defends against a *different* local user
connecting to a socket it can see. v0 validates the mechanism on the shared socket.

## 5. Threat model — what v0 enforces vs. what is substrate-gated (R-A, honest)

**v0 enforces (cooperatively, over the socket), proven device-free on modelmaker:**
- the authority graph (manifest ceiling) — an app cannot acquire what it did not declare;
- default-deny on privacy caps;
- per-capability quotas on the dangerous tier;
- peer-uid via `SO_PEERCRED`;
- the launch gate refuses a malformed/over-broad manifest before the app runs.

**v0 does NOT enforce (and does not pretend to):**
- **confinement of a process that ignores the socket.** Today an app linked as `gamer` with the
  `input`/`video`/`render` groups can still `open("/dev/input/eventN")` directly — the broker
  cannot stop it, because there are **no namespaces/seccomp/cgroups** (the owned kernel M2.B-E is
  unbuilt and M1.D is paused). The single exception is **INPUT**, where `.6`'s `EVIOCGRAB` is a
  real kernel-enforced boundary today.
- **routing fds into a real app namespace.** v0 routes handles over the socket to a co-located
  client (proving the protocol + policy); putting those fds into a *isolated* app netns/mntns is
  the substrate-gated follow-on leg. It is **named here, not papered over**, and is the
  hardware/substrate-gated portion of this child (owned kernel M2.B-E + resumed M1.D supervisor).

When the substrate lands, the broker gains real teeth with **no app-facing API change** — that is
the entire point of the backend-swap design: apps already talk to the broker socket.

## 6. Audited surface

The whole trust surface is: the PFW1 codec (`pf-wire`, zero-dep, reimplementable), the manifest
parser/validator (`manifest.rs`), the enforcement gate (`enforce.rs`), and the accept/peercred
loop (`serve.rs`). No dynamic plugin loading, no `unsafe` beyond the three thin `libc` calls in
`serve.rs` (`SO_PEERCRED`) and the daemon's signal handler. Everything else is the inner v0
backend (the `.2`/`.4` cooperative facade) which the broker only ever *tightens*, never loosens.
