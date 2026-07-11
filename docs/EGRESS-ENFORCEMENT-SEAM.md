# Egress enforcement seam — from cooperative accounting to real teeth (`tsp-ht0p.4` follow-on)

**Status:** DESIGN (paper). Implementation is deliberately OUT OF SCOPE for `tsp-ht0p.4` per the
owner Q1 ruling (recorded on the parent epic `tsp-ht0p`, 2026-07-11): *"v1 = accounting +
declaration + quota CONTRACT; enforcement teeth are the Phase-2 follow-on."* This document is
what a future substrate-era worker picks up cold; the follow-on bead cited in §7 tracks the
build.

**Cross-refs:** [`PERMISSION-MODEL.md`](PERMISSION-MODEL.md) §4 (the tier-level classification
that keys this seam); [`BROKER-DESIGN.md`](BROKER-DESIGN.md) §3 item 4 (the "finer per-op-rate
token bucket over wall time, and the egress byte ledger" that `.4` delivered); the merged
`pf_broker::tier::Tier` (`Dangerous` = `egress:<specific-host>`; `Signature` = raw/wildcard
egress); the merged `pocketforge::managers::{QuotaLedger, EgressLog}` (the accounting layer this
seam upgrades to enforcement).

## 1. What is missing today (R-A honest)

The runtime ships a **contract-level** egress model — declaration + consent-portal + per-host
byte ledger + refusal of undeclared hosts. Every one of those decisions is a **cooperative
accountant**: an app that ignores the manager and calls into the kernel directly still opens a
socket. The gap is explicitly stated in [`BROKER-DESIGN.md`](BROKER-DESIGN.md) and in
[`PERMISSION-MODEL.md`](PERMISSION-MODEL.md) §1 (R-A: "contract now, enforce later"). This
document names how the gap closes and what has to land first.

Concretely — v1 (merged) vs post-Phase-2 (this seam):

| Concern | v1 (merged, cooperative) | Post-Phase-2 (this seam) |
|---|---|---|
| Declared-host allowlist | `EgressManager` refuses undeclared and logs a `refused` row | Kernel drops the packet before it leaves the netns |
| Op-rate cap | `QuotaLedger` wall-clock token bucket, `PolicyBlocked` on drain | Per-cgroup connection-rate limit + tc/sfq egress shaper |
| Byte accounting | `EgressLog` JSONL per app | Kernel counter (nftables/conntrack accounting) + userspace mirror |
| Bypass by ignoring the manager | Possible (app calls libc directly) | Impossible (no route to the outside world exists in the netns) |

## 2. Substrate prerequisites (the T6 flag + M2.B–E gate)

Two facts of the current substrate make v1 enforcement impossible in the honest sense:

1. **T6 — no nftables egress control on device (still true).** The DUT kernel does not expose a
   post-routing hook we can install a per-app egress filter into. The kernel-fork (`kernel-tsp`)
   is the target substrate.
2. **M2.B–E container substrate is unbuilt.** The kernel + rootfs pieces that let the supervisor
   place each app in its own netns + cgroup — `CONFIG_USER_NS`, `CONFIG_NET_NS`, cgroup v2 with
   the `misc`/`net_cls` controllers, an `nsenter`-capable launcher — do not ship yet. See
   `pocketforge-plan.md` §3.6–3.7 for the Phase-2 milestones this seam waits on.

The follow-on bead below is `depends_on` both. It is *not* blocked by anything the runtime crate
owns — the runtime side is code-complete at `.4` merge.

## 3. Design — the shape enforcement takes when the substrate lands

**One netns per app + a brokered nftables/socket-filter policy keyed by the manifest's
`use = [ "egress:<host>", … ]` allowlist.** The broker is the only process with `CAP_NET_ADMIN`;
each app is launched into a pre-created netns whose default policy is DROP with an allow-list
generated from its validated manifest.

Concretely, when the supervisor launches an app the broker:

1. Reads the app's `ValidatedManifest::egress_hosts()` — the exact set already committed by `.2`.
2. Creates (or reuses) a per-app netns: `pf-app-<app_id>`. The netns has an internal veth pair
   whose upstream side lives in the broker's routing namespace.
3. Installs an nftables ruleset in the app's namespace whose default `filter output` policy is
   DROP, with `accept` rules for each declared host (resolved to the current IPv4/IPv6 set at
   launch; DNS-name changes over the app's lifetime are the substrate's re-resolve concern,
   tracked separately). Raw/broad egress from an untrusted app never reaches here — the
   `.2` `Signature`-tier launch reject fires first (`Violation::SignatureTierRequiresTrust`).
4. Attaches a cgroup-v2 counter (`bpf_skb_output` cgroup socket filter) whose per-flow byte
   counter is the enforcement mirror of the accounting-layer `EgressLog`. On overflow (`.4`'s
   per-host byte cap the manifest may carry) the socket filter drops.

**Vocabulary reuse.** No new capability tokens; no new manifest verbs. The seam consumes what
already ships: `Tier::Dangerous` = per-app netns; `Tier::Signature` = launch reject; the
`egress:<host>` list is the allowlist verbatim. A change to the accounted rate becomes a change
to the tier-default `BucketConfig` in `pocketforge::managers::QuotaLedger::default_config_for`
(no seam re-plumb).

**Broker responsibility split.**

- The **supervisor** (paused M1.D) owns netns lifecycle: create on launch, tear down on exit.
  The current `SupervisorAsk` seam grows a `ProvisionEgress(app_id, hosts) → Result<netns_id>`
  entry when M1.D unpauses.
- The **broker** owns the ruleset install + cgroup counter mirror. It stays the trust-path
  process; the app never sees a socket except through its own libc, which is now confined.
- The **runtime** — this crate — needs *no* net-new type once the seam lands. The
  `EgressManager` accounting path becomes a lightweight mirror: what the kernel enforces is what
  the log records. That is the desired invariant: post-substrate, an app that misbehaves
  encounters the same `PolicyBlocked` shape it did in v1, only now with kernel-level teeth
  behind it.

## 4. Byte accounting — the userspace mirror stays

The v1 [`EgressLog`](../crates/pocketforge/src/managers/egress_log.rs) does NOT go away on the
enforcement transition. It is upgraded from the accountant to the audit + operator-facing view:

- `pf-permissions egress` keeps working as-is (per-`(app × host)` byte rollup + refusal counts).
- The kernel counter is the source of truth for enforcement; the log is the source of truth for
  operator inspection + longitudinal analysis.
- Discrepancies (kernel counter > log byte total) are the diagnostic the enforcement layer
  surfaces: an app bypassing the cooperative accountant now shows up as a mirror gap. Today it
  is silent.

## 5. What tests + evidence the follow-on bead ships

- **Per-app netns exists and is default-DROP.** Container-substrate spike (M2.B) proves the
  netns lifecycle on the target kernel.
- **Declared-host `send` succeeds; undeclared `send` is a kernel drop.** End-to-end, on a real
  DUT once M2.B–E land: an app declares `egress:tile.example`, sends to that host, gets bytes
  through; the same app tries `evil.example`, gets `EACCES`/`EPERM` at `connect(2)`. The v1
  cooperative test (`step3_3_egress_undeclared_host_refused_and_logged_declared_host_accounted`)
  is the regression floor; the enforcement bead adds the kernel-level version.
- **Bypass attempts fail.** A synthetic misbehaving app that calls `socket(2)` directly, without
  going through `EgressManager`, is dropped identically. This is the *point* of the seam: the
  cooperative accountant becomes irrelevant to correctness, only to observability.
- **The per-host byte cap** (`.4` follow-on) is enforced by the cgroup socket filter, not the
  userspace log.

## 6. Explicit R-A boundary (state it everywhere it might be read)

- **v1 (merged, `.4`):** wall-clock token bucket + byte ledger + per-host log + undeclared-host
  refusal are **COOPERATIVE ACCOUNTING**. A linked app can bypass them by ignoring the manager.
  The point of shipping v1 is to hold the CONTRACT stable — the API a well-behaved app sees does
  not change when enforcement lands, and the audit log immediately backfills history when a
  hostile app arrives.
- **Post-Phase-2 (this seam):** the same API, the same accounting, plus kernel drops for the
  bypass path. Correctness stops depending on cooperation.

## 7. Follow-on bead

Filed by `tsp-ht0p.4` on merge as **`tsp-wufc`** (*Egress enforcement — per-app netns +
brokered nftables teeth (Phase-2 substrate; follows tsp-ht0p.4)*). Depends on the M2.B–E
container substrate; references this document + the merged accounting layer + the
`pf_broker::tier` classification. Also cross-references `tsp-rlis` / `tsp-wd9g` (the `.5`
follow-ons) where the app-delivery + ceiling-diff flows touch egress declaration; those beads
own the DELIVERY side, this seam owns the RUNTIME side — they compose, they do not duplicate.

**Delivery vs enforcement scope split (verified 2026-07-11):**

- `tsp-rlis` (B: signed app.toml delivery + atomic install) — owns *how* a new manifest gets
  onto the device with a signed trust chain. It does not build the netns or the nftables
  ruleset.
- `tsp-wd9g` (C: re-permission ceiling-diff + ledger invalidation) — owns *what happens to
  existing grants* when a re-installed manifest widens or narrows `use = […]`. It invokes the
  `AppOpsLedger::revoke_orphans` helper `.3` already ships; it does not build enforcement.
- **This follow-on (new bead) — the runtime-side enforcement teeth.** Distinct concern; no
  duplication.
