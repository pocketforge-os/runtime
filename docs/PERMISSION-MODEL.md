# PocketForge permission model — protection tiers, trust classes, and the launch contract (`tsp-ht0p.2`, E3 / infra-102)

> **Status:** the normative tier/policy model for LOCKED decision #3 (`infra-102`), as refined by
> the owner-approved R-A / R-C / R-G refinements. This document is the **contract**; it formalizes
> the *implicit* flat-list policy that shipped in the merged v0 broker
> (`crates/pf-broker/{manifest.rs,enforce.rs}`, `crates/pocketforge/src/backend.rs`) into an
> **explicit, documented** tier vocabulary. It is deliberately honest about what v1 *enforces* vs.
> what is *contracted now and enforced post-Phase-2* (R-A). Implementation:
> `crates/pf-broker/src/tier.rs` + the `LaunchTrust`/`BlessedRegistration` types in `manifest.rs`.

## 1. R-A — contract now, enforce later (read this first)

The v0 capability facade is an **in-process library** (no namespaces/seccomp/broker on the vendor
4.9 kernel — that substrate is Phase-2, M2.B–E, plus the paused M1.D supervisor). So v1 delivers the
**policy MODEL** — the tier vocabulary, the `use=[]` manifest schema, the launch-time validator, the
AppOps ledger store, the consent UX, and egress accounting — proven in the E5 simulator and honored
**cooperatively**. It does **not** yet confine a process that bypasses the broker socket. The one
exception with real v0 teeth is **input** (E2's `uinput`+`EVIOCGRAB` broker, R-B).

Two consequences bind every claim in this document:

- **"Declared" is the CEILING; the ledger is the live "allowed-now" SUBSET.** The manifest `use=[]`
  graph bounds what an app may *ever* acquire; the AppOps ledger (`.3`) is the revocable subset
  actually granted right now. A tier decision here changes what the *contract* permits; the runtime
  *teeth* land where each `.N` child says below.
- Every tier statement is split **"contract (v1)"** vs **"enforcement (post-Phase-2)"**. Where the
  runtime path is not yet wired, this doc names the child that wires it — it is not silently implied.

## 2. The four named postures (R-C vocabulary)

There are **three capability tiers** (a property of a *capability*, optionally with its scope) plus
**one app-trust exemption** (a property of an *app*). R-C calls the exemption a "4th tier"; we
reconcile that with the epic's "three protection tiers only" by classifying *capabilities* into
three tiers and the *app's trust* separately — exactly the move the epic already made for "AppOps is
the ledger mechanism, not a tier." Modeling "blessed" as a capability tier would be a category error
(it describes the subject, not the resource).

| posture | applies to | v1 contract | runtime teeth |
|---------|-----------|-------------|---------------|
| **Normal** | capability | auto-grant once declared: no consent, no quota | ceiling only (merged v0) |
| **Dangerous** | capability | default-deny + runtime consent + per-capability quota | consent/default-deny via `.3`'s generic dangerous-tier flow; quota/ledger via `.4` |
| **Signature** | capability | first-party-only; a non-first-party, non-blessed app is a **launch REJECT** | launch-time validator (this bead) |
| **Blessed-binary** | app (exemption) | an enumerated, first-party-**signed** broad-grant that clears Signature-tier entries for one enrolled app | launch-time validator (this bead); confinement is cgroup/ns/seccomp only (§6) |

Implementation factoring: `Tier { Normal, Dangerous, Signature }` (in `tier.rs`) × `AppTrust`
(`LaunchTrust { first_party, blessed }` in `manifest.rs`).

## 3. The capability vocabulary, classified

Grounded in the merged `enforce.rs` behavior so the model **formalizes what already ships** rather
than drifting it: `Dangerous` == the merged `DEFAULT_DENY`/`QUOTA_CAPS` set; `entropy` == the merged
`UNGATED` exception; everything else that auto-grants == `Normal`. The one deliberate, documented
change is egress (§4).

| capability | tier | notes |
|------------|------|-------|
| `input` | Normal | platform cap; the one capability with real v0 enforcement (E2 `EVIOCGRAB`, R-B) |
| `vibration` / `rumble` | Normal | cosmetic actuator; unified no-op shape when absent |
| `leds` | Normal | cosmetic actuator |
| `audio` | Normal | **playback only** — see §3.1 (mic/capture is a future Dangerous cap, not in today's vocabulary) |
| `settings` | Normal | local user settings |
| `entropy` | Normal (**ungated**) | auto-granted **even when undeclared**; no ceiling, no consent, no quota — §5 |
| `imu` / `accelerometer` / `gyroscope` / `magnetometer` | Normal | motion sensors; **future-dangerous candidate** — see §3.1 |
| `location` | Dangerous | default-deny + consent + quota; presence derives from a gnss/gps sensor row |
| `gnss` | Dangerous | as `location`; the descriptor `kind` gap is `tsp-9sx.6` (see §3.2) |
| `egress:<specific-host>` | Dangerous | a declared network-send destination — §4 |
| `egress:0.0.0.0/0` / `::/0` / `*` / unscoped `egress` | Signature | raw/arbitrary egress — first-party-only — §4 |

### 3.1 Two documented judgment calls (motion sensors + audio)

These two are classified **Normal to match the merged auto-grant behavior**, and flagged here rather
than silently decided:

- **Motion sensors (`imu`/`accelerometer`/`gyroscope`/`magnetometer`) are Normal today, but are a
  known future *Dangerous* candidate.** High-rate inertial data is a real side channel (keystroke /
  PIN inference, gait identification). The merged v0 auto-grants them (not in `DEFAULT_DENY`), so the
  tier model matches that to avoid behavior drift; a future revision that adds a sampling-rate gate
  or a consent prompt for high-rate motion access should promote them (or a `motion:high-rate`
  scope) to Dangerous under the §7 future-cap rule.
- **`audio` is Normal as PLAYBACK only.** Microphone/capture is a genuinely Dangerous capability, but
  there is **no `mic`/capture capability in today's vocabulary** and no descriptor row backs one. A
  future capture capability is born Dangerous (§7); it depends on the E1 capability-descriptor
  gaining a microphone/audio-input row (an E1 descriptor gap, adjacent to `tsp-9sx.6`).

### 3.2 `location`/`gnss` and `tsp-9sx.6` (reference, not solved here)

`location`/`gnss` presence derives from a gnss/gps **sensor row** in the E1 capability descriptor.
The descriptor schema does **not** yet model the GNSS *kind* (constellation / fix-type) — that is
`tsp-9sx.6` (open, E1-side, claimed by a sibling E3 worker). This bead **references** it: the tier
model treats `location`/`gnss` as one Dangerous class and does not depend on the unmodeled `kind`.
Do not solve `tsp-9sx.6` here.

## 4. Egress — a deliberate, documented tightening vs merged v0

Egress is the **privacy act**: consented `location`-read plus an auto-granted send is exactly the
"did the dev silently collect + exfiltrate?" worry LOCKED #3 exists to answer (location-read ≠
location-send). LOCKED #3 default-denies **all** privacy capabilities, and egress is one. Therefore:

- **`egress:<specific-host>` is Dangerous** — default-deny + consent + its own ledger line/quota at
  the **contract** level. This is a **deliberate tightening vs merged v0**, which auto-granted a
  declared egress host under ceiling+quota. It is the formalization this bead exists for, **not**
  silent drift. (No real v1 consumer regresses: `pf-hwprobe` declares no egress; Steam Link is
  blessed-exempt; Poolsuite is first-party native and does not broker. No existing test asserted
  runtime auto-grant on a declared egress host — verified against `crates/pf-broker/tests/broker.rs`,
  which never acquires/queries `egress`.)
- **`egress:0.0.0.0/0`, `::/0`, `*`, or unscoped `egress` is Signature** — raw/arbitrary egress is
  first-party-only; a non-first-party, non-blessed app that declares it is a launch REJECT.

**Convergence (where the runtime teeth land):** the **contract** is stated in `.2` (this bead, via
the `Tier` classification). The **consent / default-deny** path for a declared egress host arrives
with **`.3`** — the *generic* dangerous-tier flow (a dangerous-tier acquire with no standing grant →
`Prompt`, driven by the tier classification, **not** egress-special code). The **egress-specific
teeth** — wall-clock token bucket, byte ledger, per-host log, and refusal of a send to an *undeclared*
host — are **`.4`**'s egress-as-capability leg. So egress is *not* wholesale-deferred to `.4`: `.3`
gives it consent via the generic tier path; `.4` adds the byte-level accounting. The kernel/netns
egress *enforcement* (nftables/socket-filter keyed by the `use=` host allowlist) is substrate-gated
(Q1 ruled v1 = accounting/declaration, cooperative; enforcement is a Phase-2 follow-on bead).

**Status update (`.4` landed, 2026-07-11).** All four `.4` accounting teeth are on `main`:

- **Wall-clock token buckets** — `pocketforge::managers::QuotaLedger` now runs on a fake-clockable
  token-bucket (`Clock` trait + `SystemClock`/`ManualClock`; `TokenBucket` refills at the
  tier-default rate over wall time). Defaults: `location`/`gnss` = 60 burst @ 1 tok/sec; `egress`
  (op count) = 16 burst @ 0.25 tok/sec ≈ 15 ops/min; every Normal-tier capability is UNGATED so
  `entropy` is *structurally* never rate-limited (§5). Throttling surfaces as
  `CapError::PolicyBlocked` — a typed error, no exit-code guessing.
- **Egress byte ledger + per-host log** — `pocketforge::managers::EgressLog` is a persistent
  JSONL-shaped store at `$PF_EGRESS_LOG_DIR` (else `$XDG_STATE_HOME/pocketforge/egress/`), same
  tab-separated / backslash-escaped dialect as `.3`'s AppOps ledger. Every declared-host `send`
  writes a `send` row; every undeclared-host attempt writes a `refused` row *without* spending an
  op token.
- **Undeclared-host refusal** — `EgressManager::with_accounting(quotas, EgressManager::accounting(
  app_id, manifest_hosts, log))` binds the declared-host set to the manager. Any `send` to a host
  outside that set returns `PolicyBlocked` (accounting-level, R-A honest) and logs the refusal.
- **`pf-permissions egress`** — new subcommand: per-`(app × host)` byte rollup + refusal counts
  read from the persistent log. Same CLI shape as `pf-permissions inspect` for the AppOps ledger.

None of the four is kernel-level enforcement (an app that ignores the accountant and calls into
the kernel directly still sends). The kernel/netns enforcement seam that closes that gap is the
follow-on bead — see [EGRESS-ENFORCEMENT-SEAM.md](EGRESS-ENFORCEMENT-SEAM.md) (per the Q1 ruling).

## 5. Entropy — Normal-tier, UNGATED (with an honest scope note)

`entropy` is **Normal-tier and deliberately ungated**: auto-granted even when *undeclared*, with no
ceiling, no consent, and no quota (`enforce.rs` `const UNGATED = "entropy"`). Rationale, stated so it
is a decision and not an oversight:

- **Non-exhaustibility (the DRAIN answer).** A modern CSPRNG `/dev/urandom` never blocks and cannot
  be "drained" — one app reading it does not deplete it for another, and it carries no privacy
  payload. Only the long-deprecated blocking `/dev/random` pool ever had exhaustion semantics, and
  that behavior is gone in our kernel line. Gating entropy for exhaustion buys no security and only
  adds friction. Cited: briefing §A (Option A: "entropy left ungated") and §D part 3 (the challenge
  "Is entropy truly ungated acceptable given the anti-entropy-drain ask?").

- **SCOPE NOTE (R-G) — drain ≠ boot-seed quality.** The non-exhaustibility rationale addresses
  *drain only*, NOT boot-**seed** quality. On a headless handheld the CRNG may be *unseeded* at first
  crypto use; `/dev/urandom` never blocks and can return pre-seed bytes ("Mining your Ps and Qs"
  class). Our own early crypto is largely safe — minisign/cosign **verify** consume no RNG, and TLS
  `[fetch]` runs after `network.target` so the pool is realistically seeded by then — so the residual
  is a **community app pulling entropy very early**. The mitigation is a substrate property (a
  well-seeded CRNG), not a capability gate: **entropy stays ungated**; the seed-quality question is
  tracked as a low-severity substrate item, probed below.

### 5.1 R-G CRNG seed-timing probe — findings (device-free, per the owner Q2 ruling)

Device-free source/config read of `pocketforge-os/kernel-tsp` @ `18c239a` (kernel `4.9.191`; A133 =
`sun50iw10p1`; defconfig `arch/arm64/configs/pocketforge_tsp_defconfig`; DT
`arch/arm64/boot/dts/sunxi/pocketforge_tsp.dts`).

| seed source | status on A133/4.9 | evidence |
|-------------|--------------------|----------|
| hardware RNG (`/dev/hwrng`) | **absent** — subsystem compiled out | `# CONFIG_HW_RANDOM is not set` (defconfig:247) |
| sun8i/sunxi CE **TRNG** | **dark** — CE block exists + DT node `ce@1904000` is enabled, but the `sunxi-ss` driver (`CONFIG_CRYPTO_DEV_SUNXI`) is **not built**, so nothing binds the TRNG | DT `pocketforge_tsp.dts:4911`; `CONFIG_CRYPTO_DEV_SUNXI` absent (Kconfig-default `n`) |
| jitterentropy | **not built** | `CONFIG_CRYPTO_JITTERENTROPY` absent from defconfig |
| bootloader → kernel seed | **no path** — `RANDOM_TRUST_CPU`/`RANDOM_TRUST_BOOTLOADER` do not exist in 4.9; no `/chosen/rng-seed`, no `add_bootloader_randomness()` | zero grep hits across the tree; `chosen{}` has only `bootargs`/`initrd` |
| eFuse SID unique-id | present but **non-credited** (`add_device_randomness` — diversifies, does not flip `crng_init`) | `CONFIG_NVMEM_SUN50I_SID=y`; `drivers/nvmem/sun50i_sid.c:89` |
| generic IRQ/input/disk timing | **the only credited path** — `crng_init` flips to "done" only at ≥128 credited bits from these | `drivers/char/random.c:722,891` |
| `/dev/urandom` pre-seed behavior | **never blocks; warn-and-serve** — returns CRNG output even when `crng_init < 2` (only a rate-limited "uninitialized urandom read" printk) | `urandom_read()`, `drivers/char/random.c:1788` |

**Verdict — this is a REAL early-boot seed-quality gap, but NOT a drain/exhaustion vector, so the
`entropy` tier is UNCHANGED (ungated).** The gap is specifically *boot-seed quality*: on a headless
unit, a process that reads `/dev/urandom` *very early* (e.g. first-boot host-key / crypto-token
generation, before much IRQ/network activity has accrued credited entropy) can receive low-entropy
pre-seed bytes ("Mining your Ps and Qs" class), because there is no hwrng/TRNG/jitter/bootloader seed
to reach `crng_init` fast. This is a **substrate** property (fix = a real early seed source), not a
capability-policy property — gating the `entropy` *capability* would not help (the vulnerable read is
the app's own crypto, not another app draining a pool). Per the owner Q2 ruling, the escalation
trigger is a real **exhaustion** vector; this is a seed-timing caveat, so it is **documented here, not
escalated**, and a **follow-on kernel-hardening item** is recommended (enable the CE TRNG driver
and/or `CONFIG_CRYPTO_JITTERENTROPY` on `kernel-tsp`, and/or seed early from the eFuse SID with
credit) — tracked separately from this bead (it is `kernel-tsp` substrate scope, not runtime E3).

> **A523/5.15 is the near-non-issue** the refinement anticipated: it ships the CE (`ce@3040000`) +
> jitterentropy, so the early-seed window is far narrower there. The gap above is A133/4.9-specific.
>
> Not verified (out of device-free scope): actual first-boot credited-entropy *timing* vs. when a
> specific service first reads `/dev/urandom` — that needs a live serial boot trace (`/serial-review`),
> deferred to the recommended hardening item.

## 6. Blessed-binary exemption — the audited posture + threat-model caveat

A **blessed binary** is a closed app that cannot be transparently brokered. The flagship is **Steam
Link**: a closed Valve binary that never calls the broker and is *both* a uinput consumer *and* a
uinput producer, so the `EVIOCGRAB` interposition pattern would break it (EBUSY / double-input /
enumeration loop). Rather than force it through the broker, the platform grants it an **enumerated,
signed broad-grant** and confines it differently.

**Model (`BlessedRegistration` + `LaunchTrust::blessed`):** the registration is a platform-side,
enumerated record `{ app_id, sha256, grants[] }`. At launch it clears `Signature`-tier entries —
**but only** for the matching `app_id`, and **only** the enumerated `grants` (a broad-egress grant it
does not list is still rejected). It is the validator's canonical exemption case.

**Threat-model caveat (state it, don't paper over it):**

- **Confinement of a blessed binary is cgroup / namespace / seccomp only — NOT capability-brokered.**
  We do not mediate its individual capability calls; we sandbox the process. Input is coarse
  **fd-passing** (the broker opens `event3` and hands the fd in — no grab); it keeps its own
  `/dev/uinput`; egress is coarse netns/nftables (fine-grained is vacuous for a streaming client);
  `fb0` stays the supervisor single-writer handoff.
- **Trust = first-party enrollment + signed-fetch SHA.** The blessed *registration itself is
  first-party-signed*, and it is keyed to the signed-fetch `sha256` the supervisor verified for the
  bundle. A blessed grant is therefore only as good as (a) the first-party signature on the
  registration and (b) the content-hash match on the fetched binary — both are the supervisor's to
  verify (§8), not the app's to assert.

**Which apps go through the broker (v1 reality):** **Steam Link = blessed-exempt** (does not broker);
**Poolsuite FM / TV = first-party native, self-trusted** (does not broker); **`pf-hwprobe` = the
first and only genuine broker consumer in v1.** So the full apparatus has *no untrusted subject*
until the first community app — which is exactly why the model is built now, ahead of need.

## 7. Normative future-capability rule (encodes LOCKED #3 forward)

**Any new capability that reads user-environment data (camera, microphone, precise or high-rate
sensors) or moves data off-device is born Dangerous-or-higher. A `Normal` classification for such a
capability requires explicit written justification** (as entropy has in §5). This rule is how the
tier model stays honest as the vocabulary grows: the default for a new privacy/exfiltration-relevant
capability is default-deny, never auto-grant.

## 8. Where `LaunchTrust` comes from (INPUT seam — NOT implemented in `.2`)

`LaunchTrust` is an **input** the supervisor computes *before* validation; this bead does **not**
implement signature verification. The provenance seam:

- **`app.toml.sig` minisign verify against the first-party `release.d` key directory → `first_party`.**
- **Blessed-binary registry lookup (enrolled `app_id` + signed-fetch `sha256` match) → `blessed`.**
- **Neither → `UNTRUSTED`** (the safe floor; `AppManifest::validate` uses it).

The verification machinery is owned by the existing two-signature trust model (`app.toml.sig`
minisign + `oci.sig` cosign) and by `.5`'s fielded-delivery trust-chain design; `.2` only defines the
type they populate and the validator that consumes it.

## 9. Launch-time validator — the reject table (extends `BROKER-DESIGN.md` §2)

`AppManifest::validate[_with_trust]` collects **all** violations (not just the first):

| reject reason | example | since |
|---------------|---------|-------|
| unknown capability | `"telepathy"` | v0 |
| duplicate capability | `"input", "input"` | v0 |
| bad modifier | `"location:teleport"` | v0 |
| undescriptored **required** hardware cap | `"imu"` on the a133 (no IMU) | v0 |
| **signature-tier without trust** | `"egress:0.0.0.0/0"` from a non-first-party, non-blessed app | **this bead** |

See `crates/pf-broker/tests/broker.rs` for the executable proof of every row (the STEP-1 well-formed
+ broad-egress-reject pair and the blessed-binary positive/negative pairs).

---

### Cross-references
- `docs/BROKER-DESIGN.md` — the broker daemon, the launch contract, and the v0-vs-substrate threat model.
- `STABILITY.md` — the frozen v1 wire/ABI (this model is additive: `Tier`/`Violation`/`LaunchTrust`
  are Rust-crate types, not wire enums or C symbols — no wire/ABI surface changes).
- Briefing `.planning/app-runtime-simulator-research-briefing.md` — §A Option A (permissions/consent,
  entropy ungated), §D parts 3–4 (challenges + risks), R-A / R-C / R-G refinements.
- `tsp-9sx.6` — the E1 GNSS `kind` descriptor gap (referenced, not solved here).
