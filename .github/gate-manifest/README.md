# runtime quality-gate meta-gate (tsp-1x04)

**Do not delete this as ceremony.** It exists because this repo produced, in one
week, three CI gates that *looked* like coverage while checking nothing:

- **tsp-ozbp.16** — the `fixtures_track_platform` drift guard compared **nothing**
  (it early-returned unless an env var set nowhere was present) and reported "ok"
  on every run it ever made. *Vacuous.*
- **tsp-7rts** — there was **no clippy gate at all**; a lint failure sat in the
  tree with nothing able to report it. *Absent.*
- **tsp-ovee** — `--locked` was passed **nowhere**, so `cargo` silently rewrote
  `Cargo.lock` in the runner and went green while the lock was stale. *Absent.*

The systemic cause was not three coincidences: **nothing stated what the quality
gates were**, so a missing gate had no shape and could not be noticed. A green
tick read as "checked" whether a gate was present, vacuous, or missing — *a
check-shaped absence keeps reading as coverage*.

## What is here

| file | role |
|------|------|
| [`../quality-gates.toml`](../quality-gates.toml) | the committed **manifest**: the required gates, each with its invariant + the exact invocation that satisfies it. The single source of truth. |
| `check_gates.py` | the **meta-gate**: reads the manifest, FAILS when a required gate is absent from or neutered in the workflows. |
| `selftest.py` | the **guard-of-the-guard**: proves `check_gates.py` can go RED (and does so for the right gate), then passes clean. |
| [`../workflows/gate-manifest.yml`](../workflows/gate-manifest.yml) | runs both on every PR. **No `paths:` filter** — a gate can be removed by editing only a workflow, which a `crates/**` filter would not trigger on; a meta-gate about absence must not be filterable into absence. |

## Why this is not a naive grep (the obvious way to fail this)

Grepping the YAML for a flag string is **instance #4 of the same joke**: it passes
on a flag in a step that never runs, a job excluded by a `paths` filter, or a step
marked `continue-on-error`. So instead of *"does the text appear somewhere?"*,
`check_gates.py` asks *"is the gate in a step that would RUN on a normal PR and
COULD fail?"* — it models **reachability**:

- **Trigger** — the gate's job must fire on `pull_request` to `main`. No such
  trigger ⇒ the gate is treated as absent.
- **Paths filter** — the job's `pull_request.paths` must actually *cover* the
  paths the gate guards (`guards_paths`). A gate guarding `crates/**` whose job
  filters to `docs/**` is reported absent — it would not run on the change that
  matters. A gate whose `guards_paths = ["*"]` requires the workflow to have **no**
  paths filter at all.
- **Conditional skip** — an `if:` on the gate's job or step is treated as
  skippable (see *Modelling choices*), because the gate must not be able to skip
  on a normal PR.
- **`continue-on-error`** on the job or step ⇒ the gate "runs" but cannot fail
  the run ⇒ reported absent.
- **`|| true` / `|| :` / `; true` / `|| exit 0`** tail on the command ⇒ neutered ⇒
  reported absent.
- **Strict flags** — the *invariant-carrying* flags are part of the required
  match, so weakening them is caught: `clippy` without `-D warnings`, `test`
  without `--workspace`, any lock-consuming `cargo` without `--locked` all fail.
- **Anti-vacuity (partial)** — for the platform-backed test gate, which goes
  green-while-checking-nothing if it silently skips, the manifest additionally
  requires `PF_PLATFORM_DIR` to be set on the step **and** the `platform` repo to
  be checked out in the job. Both are verified structurally.
- **Matrix** — the prefs-E2E unification proof is meaningless with one row, so the
  `descriptor` matrix is required to still contain both `a133` and `a523`.

It is **fail-closed**: a workflow that will not parse, a missing PyYAML, or an
unreadable manifest is a *failure*, never a skip. *In CI, absent input must fail,
never skip* — a fail-open skip is how the gates this bead is about slipped through.

## The meta-gate is PROVEN able to fail

A guard you have not seen fail is not a guard. `selftest.py` constructs the broken
states and watches `check_gates.py` go red — the positive control (unmutated tree)
must pass, and **11 negative controls**, one per evasion, must each fail *for the
right gate*:

```
 ✓  POSITIVE (unmutated tree)                        PASS
 ✓  clippy step deleted entirely (ABSENT)            RED for the right reason
 ✓  --locked stripped (ABSENT flag, tsp-ovee)        RED for the right reason
 ✓  clippy marked continue-on-error (NEUTERED)       RED for the right reason
 ✓  clippy tail `|| true` (NEUTERED)                 RED for the right reason
 ✓  paths filter excludes crates/** (FILTERED)       RED for the right reason
 ✓  clippy `if:` skip (SKIPPED STEP)                 RED for the right reason
 ✓  clippy `-D warnings` removed (WEAKENED/vacuous)  RED for the right reason
 ✓  a133 matrix row dropped (silent single-row)      RED for the right reason
 ✓  PF_PLATFORM_DIR unset (VACUOUS skip)             RED for the right reason
 ✓  platform checkout removed (VACUOUS skip)         RED for the right reason
 ✓  whole runtime-tests.yml deleted (EXCLUDED JOB)   RED for the right reason
```

The selftest **derives its broken cases from the real workflows** (copy → mutate
one thing → check), rather than committing hand-written fixture workflows. That is
deliberate: a committed fixture copy is exactly the hand-copied artifact that
silently drifts from what it mirrors — the tsp-ozbp.16 defect. This selftest cannot
go stale against the real workflows because it starts from them. (It also already
earned its keep: it caught a bug in its *own* first draft, where the clippy
mutations were hitting the `cargo clippy --version` toolchain step instead of the
gate step, so five "negative" controls were silently not exercising anything —
the precise shape this whole bead is about, caught by insisting on *red for the
right reason* rather than merely *red*.)

## The honest bound — what this CANNOT catch (Acceptance Criterion 3)

An honest partial gate beats a silent overclaim. This meta-gate reads the workflow
**source** and reasons about whether a gate *would* run; it does **not** observe a
specific live run's executed-step log. Therefore:

1. **It proves reachability, not execution.** It establishes that the invocation
   sits in a step that, as written, would run on a normal PR and could fail. It
   does not fetch a completed run and confirm the step's conclusion. A neutering
   mechanism it does not model (below) could still let a green through.

2. **Residual vacuous gap.** The original `fixtures_track_platform` vacuity was a
   command that *ran* and compared nothing because of runtime state (an unset env
   var). This checker catches the two structural forms of that for the test gate
   (env unset, platform not checked out), but it cannot prove a command is
   semantically non-vacuous in general — e.g. a future test whose body asserts
   nothing, or a `cargo test` filter that matches zero tests, would still satisfy
   the token match. **Vacuity that lives inside the invoked program, not in the
   workflow wiring, is out of this gate's reach.** The mitigation is that the
   manifest requires the *strict* form of each invocation (the flags that remove
   the known vacuity) and, for the one historically-vacuous gate, the env +
   checkout that stop it skipping.

3. **Token matching is substring matching.** Required tokens must co-occur in one
   non-neutered step's `run` (shell-comment lines stripped first), which makes a
   decorative match unlikely, but a sufficiently adversarial `run` string could in
   principle contain the tokens without the intended effect. This is a bounded
   risk, not a closed one.

4. **It cannot stop the whole `gate-manifest.yml` file being deleted** while the
   checker is not running to notice. The `gate-manifest-self` manifest entry guards
   the job's *trigger and steps* from being neutered, but deleting the workflow
   removes the thing that would report the deletion. **Only making `gate-manifest`
   a required status check closes that** — which is Layer 2 below.

5. **It does not verify Layer 2 (below) at all, on purpose.** Reading the branch
   ruleset needs `admin:read` the CI token lacks; a check that *skips* when it
   cannot read its input is a fail-open skip — the exact anti-pattern. Layer 2 is
   declared-and-tracked here, never half-checked.

## Layer 2 — invoked ≠ merge-blocking (tracked in tsp-i9xs)

A gate being **invoked** in a workflow (what this meta-gate enforces) is not the
same as a **red result blocking a merge**. As of 2026-07-27 the only required
status check on `runtime`'s `main` is **`pf-pr-review / review`** (org ruleset id
**18463042**). `runtime-tests` and `prefs-e2e` **run but are not required checks**,
so a red `cargo test` / `clippy` / `--locked` / musl result does **not** currently
block a merge — the required-check analog of the same defect class ("the job ran
and turned red but nobody was required to look").

Wiring these gates (and `gate-manifest` itself) as required status checks needs
repository **admin** (the agent App token has none by design) and is a durable
merge-gating convention change — an **owner action**, tracked in **tsp-i9xs**
(sibling class of tsp-9ow6). This directory documents the requirement; tsp-i9xs
gives it teeth.

## Modelling choices (so a future editor understands, not reverts)

- **An `if:` on a required job/step is treated as skippable** unless the manifest
  whitelists it with `allow_if = true`. Evaluating GitHub expression truthiness for
  arbitrary events is out of scope, so the conservative reading is "a conditional
  required gate might skip". The one whitelisted case is `pr-review.yml`'s caller
  job, whose `if:` only skips *title-only* edits and never a code PR (and which is
  already the enforced required check).
- **`no-unlocked-cargo` is a universal invariant**, not a single-step presence
  check: *every* lock-consuming `cargo` invocation across *all* workflows must
  carry `--locked` (version/help forms exempt). That is the true shape of the
  tsp-ovee fix — a later step rewriting the lock would launder a drift a single
  step-presence check had "passed".

## Adding or changing a gate

Add the `[[gate]]` (or `[[universal]]`) block to `../quality-gates.toml` **and**
the invocation to a workflow. `check_gates.py` stays red until both exist — that
is the point. If you intentionally *remove* a gate, remove its manifest block in
the same PR and say why; the reviewer sees the manifest change and the tsp-i9xs /
precedent context, so a removal is a conscious, reviewable act rather than a silent
absence.
