#!/usr/bin/env python3
# =============================================================================
# check_gates.py — the runtime workspace's quality-gate META-GATE  (tsp-1x04)
# =============================================================================
#
# Reads .github/quality-gates.toml (the committed statement of required gates)
# and FAILS (exit 1) when a declared gate is ABSENT from, or NEUTERED in, the
# GitHub Actions workflows. A missing gate becomes a RED here instead of a
# silence on the PR — the whole point of tsp-1x04. See ../quality-gates.toml
# for WHY this exists (tsp-ozbp.16 / tsp-7rts / tsp-ovee) and README.md for the
# design + the HONEST BOUND on what this can and cannot catch.
#
# NOT A NAIVE GREP. The obvious way to build this — grep the YAML for a flag
# string — is instance #4 of the very joke this bead is about: it passes on a
# flag in a step that never runs, a job excluded by a paths filter, or a step
# marked continue-on-error. So instead of "does the text appear somewhere?" this
# asks "is the gate in a step that WOULD RUN on a normal PR and COULD fail?" by
# modelling reachability: trigger, branch, paths filter vs the paths the gate
# guards, job/step `if:`, `continue-on-error`, and `|| true`-style neutering.
# What it models — and the residual gap it CANNOT close — is spelled out in
# README.md; read it before trusting a green from this script.
#
# FAIL-CLOSED. Every failure to establish a fact (a workflow that will not
# parse, a missing PyYAML, an unreadable manifest) is a FAILURE, never a skip.
# "In CI, absent-input must FAIL, never skip" — a fail-open skip is how the
# gates this bead is about slipped through.
#
# Usable two ways:
#   * CLI:   python3 check_gates.py --root <repo-root> [--manifest <path>]
#   * import: evaluate(manifest_path, repo_root) -> list[str] of failure reasons
#            (the selftest drives this to prove the checker can go red).
# =============================================================================

from __future__ import annotations

import argparse
import fnmatch
import sys
import re
from pathlib import Path

# Fail-closed on a missing dep: a meta-gate that cannot run is not a pass.
try:
    import yaml  # PyYAML — preinstalled on GitHub-hosted ubuntu runners.
except Exception as exc:  # pragma: no cover - environment failure
    sys.stderr.write(
        f"FATAL: PyYAML is required and did not import ({exc}). "
        "This is a hard failure, not a skip.\n"
    )
    sys.exit(2)

try:
    import tomllib  # stdlib >= 3.11
except Exception as exc:  # pragma: no cover
    sys.stderr.write(f"FATAL: tomllib (stdlib >=3.11) required: {exc}\n")
    sys.exit(2)


# --- neutering patterns: a run step that ends its command with any of these ---
# "runs but cannot fail the job". Matched per logical (continuation-joined) line.
_NEUTER_TAIL = re.compile(r"(\|\|\s*(true|:|exit\s+0)\b|;\s*true\s*$)")


def _norm_on(wf: dict):
    """Return the `on:` spec, working around PyYAML parsing bare `on:` as the
    boolean key True (YAML 1.1). A silent miss here would fail-CLOSED (red
    everything) rather than fail-open, but handle it explicitly so the reason
    is 'no pull_request trigger' and never a spurious parse artifact."""
    if "on" in wf:
        return wf["on"]
    if True in wf:  # `on:` swallowed into the boolean-true key
        return wf[True]
    return None


def _pull_request_spec(on_spec):
    """Extract the pull_request trigger config. Returns (present: bool, cfg:dict|None).
    cfg is {} when pull_request is listed with no sub-keys (all branches/paths)."""
    if on_spec is None:
        return False, None
    if isinstance(on_spec, str):
        return (on_spec == "pull_request"), {}
    if isinstance(on_spec, list):
        return ("pull_request" in on_spec), {}
    if isinstance(on_spec, dict):
        if "pull_request" not in on_spec:
            return False, None
        cfg = on_spec["pull_request"]
        return True, (cfg if isinstance(cfg, dict) else {})
    return False, None


def _branch_ok(pr_cfg: dict, branch: str) -> bool:
    branches = pr_cfg.get("branches")
    if not branches:
        return True  # no branch constraint = all branches
    return any(fnmatch.fnmatch(branch, b) for b in branches)


def _concrete_repr(glob_pat: str) -> str:
    """A representative concrete path for a guarded glob, so we can test whether
    a workflow paths entry would match a change under it."""
    rep = glob_pat.replace("**", "seg/seg").replace("*", "seg")
    return rep


def _paths_cover(guards_paths, pr_cfg: dict):
    """Does the job's pull_request `paths` filter actually TRIGGER on the paths
    this gate guards? Returns (ok: bool, reason: str).

    guards_paths == ['*'] means 'this gate must run on ALL PRs' → the workflow
    must have NO paths filter. Anything narrower is the excluded-path evasion."""
    wf_paths = pr_cfg.get("paths")
    if guards_paths == ["*"]:
        if wf_paths:
            return False, (
                f"gate must run on all PRs but the workflow restricts to paths={wf_paths} "
                "(a paths filter can exclude the change that removes the gate)"
            )
        return True, ""
    if not wf_paths:
        return True, ""  # no filter = every path triggers = all guarded paths covered
    for g in guards_paths:
        rep = _concrete_repr(g)
        # fnmatch's * spans '/', so a GH glob like 'crates/**' matches the repr.
        if not any(fnmatch.fnmatch(rep, wp) or fnmatch.fnmatch(g, wp) for wp in wf_paths):
            return False, (
                f"guarded path {g!r} is NOT covered by the workflow paths filter {wf_paths} "
                "— a change there would not trigger the job, so the gate would not run"
            )
    return True, ""


def _iter_lock_lines(run_text: str):
    """Yield logical (continuation-joined) command lines from a run block,
    with shell-comment-only lines dropped."""
    logical = []
    buf = ""
    for raw in run_text.splitlines():
        line = raw.rstrip()
        stripped = line.strip()
        if stripped.startswith("#"):
            continue
        if line.endswith("\\"):
            buf += line[:-1] + " "
            continue
        buf += line
        if buf.strip():
            logical.append(buf.strip())
        buf = ""
    if buf.strip():
        logical.append(buf.strip())
    return logical


def _run_of(step: dict) -> str:
    r = step.get("run")
    return r if isinstance(r, str) else ""


def _step_neutered(job: dict, step: dict):
    """A step is neutered (runs but cannot fail the job) if it or its job is
    continue-on-error, if it carries any `if:` (we cannot evaluate GH expression
    truthiness for arbitrary events, so a conditional required step is treated
    as skippable — see README), or if its command tail is `|| true`-style.
    Returns (neutered: bool, reason: str)."""
    if job.get("continue-on-error") in (True, "true"):
        return True, "job is continue-on-error"
    if step.get("continue-on-error") in (True, "true"):
        return True, "step is continue-on-error"
    if "if" in step:
        return True, f"step is conditionally skippable (if: {step.get('if')!r})"
    for line in _iter_lock_lines(_run_of(step)):
        if _NEUTER_TAIL.search(line):
            return True, f"step command is neutered by a `|| true`-style tail: {line!r}"
    return False, ""


def _job_reachable(wf: dict, wf_path: str, job: dict, gate: dict, defaults: dict, allow_if: bool):
    """Is the job that must contain this gate actually reachable on a normal PR?"""
    reasons = []
    on_spec = _norm_on(wf)
    present, pr_cfg = _pull_request_spec(on_spec)
    if not present:
        reasons.append(f"{wf_path}: no `pull_request` trigger (gate can never run on a PR)")
        return False, reasons
    branch = gate.get("trigger_branch", defaults.get("trigger_branch", "main"))
    if not _branch_ok(pr_cfg, branch):
        reasons.append(f"{wf_path}: pull_request does not target branch {branch!r}")
    ok, why = _paths_cover(gate.get("guards_paths", ["*"]), pr_cfg)
    if not ok:
        reasons.append(f"{wf_path}: {why}")
    if job.get("continue-on-error") in (True, "true"):
        reasons.append(f"{wf_path}: job {gate['job']!r} is continue-on-error (its red cannot fail the run)")
    if "if" in job and not allow_if:
        reasons.append(
            f"{wf_path}: job {gate['job']!r} is conditionally skippable (if: {job.get('if')!r}); "
            "a required gate must not be — whitelist with allow_if only if it never skips a code PR"
        )
    return (len(reasons) == 0), reasons


def _load_workflow(repo_root: Path, rel: str):
    p = repo_root / rel
    if not p.exists():
        return None, f"{rel}: workflow file does not exist"
    try:
        return yaml.safe_load(p.read_text()), None
    except Exception as exc:
        return None, f"{rel}: YAML did not parse ({exc})"


def _find_job(wf: dict, job_id: str):
    jobs = wf.get("jobs") if isinstance(wf, dict) else None
    if not isinstance(jobs, dict):
        return None
    return jobs.get(job_id)


def _matrix_values(job: dict, key: str):
    strat = job.get("strategy") or {}
    matrix = strat.get("matrix") or {}
    vals = matrix.get(key)
    return vals if isinstance(vals, list) else None


def _step_env_has(job: dict, wf: dict, step: dict, env_key: str) -> bool:
    for scope in (step.get("env"), job.get("env"), wf.get("env")):
        if isinstance(scope, dict) and env_key in scope:
            return True
    return False


def _job_checks_out(job: dict, repo: str) -> bool:
    for step in job.get("steps", []) or []:
        uses = step.get("uses", "")
        if isinstance(uses, str) and uses.startswith("actions/checkout"):
            with_ = step.get("with") or {}
            if with_.get("repository") == repo:
                return True
    return False


def evaluate(manifest_path: str, repo_root: str):
    """Return a list of human-readable failure reasons. Empty list = all gates
    present and reachable. Never raises for a gate failure — raises only if the
    manifest itself is unreadable (which is itself fail-closed at the CLI)."""
    repo_root = Path(repo_root)
    failures: list[str] = []

    manifest = tomllib.loads(Path(manifest_path).read_text())
    defaults = manifest.get("defaults", {})

    # Cache parsed workflows referenced anywhere, plus ALL workflow files for the
    # universal cargo scan.
    wf_cache: dict[str, dict] = {}

    def get_wf(rel: str):
        if rel not in wf_cache:
            wf, err = _load_workflow(repo_root, rel)
            wf_cache[rel] = wf if err is None else None
            if err:
                failures.append(f"[load] {err}")
        return wf_cache[rel]

    # --- UNIVERSAL invariants (property over ALL workflow files) --------------
    for uni in manifest.get("universal", []):
        if uni.get("id") == "no-unlocked-cargo" or uni.get("lock_consuming_subcommands"):
            subs = set(uni["lock_consuming_subcommands"])
            flag = uni.get("required_flag", "--locked")
            wf_dir = repo_root / ".github" / "workflows"
            if not wf_dir.is_dir():
                failures.append("[universal:no-unlocked-cargo] .github/workflows/ missing")
                continue
            for wf_file in sorted(wf_dir.glob("*.yml")) + sorted(wf_dir.glob("*.yaml")):
                try:
                    wf = yaml.safe_load(wf_file.read_text())
                except Exception as exc:
                    failures.append(f"[universal:no-unlocked-cargo] {wf_file.name}: parse error {exc}")
                    continue
                for job_id, job in (wf.get("jobs") or {}).items():
                    for step in (job.get("steps") or []):
                        for line in _iter_lock_lines(_run_of(step)):
                            m = re.search(r"\bcargo\s+(\w[\w-]*)", line)
                            if not m:
                                continue
                            sub = m.group(1)
                            if sub not in subs:
                                continue
                            if "--version" in line or "--help" in line:
                                continue
                            if flag not in line:
                                failures.append(
                                    f"[universal:no-unlocked-cargo] {wf_file.name} job {job_id!r}: "
                                    f"lock-consuming `cargo {sub}` without {flag}: {line!r}"
                                )

    # --- per-gate step-invocation / uses / matrix checks ----------------------
    for gate in manifest.get("gate", []):
        gid = gate.get("id", "<unnamed>")
        rel = gate["workflow"]
        wf = get_wf(rel)
        if wf is None:
            failures.append(f"[{gid}] workflow {rel} unavailable")
            continue
        job = _find_job(wf, gate["job"])
        if job is None:
            failures.append(f"[{gid}] job {gate['job']!r} not found in {rel}")
            continue

        allow_if = bool(gate.get("allow_if", False))
        reachable, why = _job_reachable(wf, rel, job, gate, defaults, allow_if)
        if not reachable:
            for r in why:
                failures.append(f"[{gid}] {r}")
            # keep checking the step too — more reasons is better than fewer.

        # required matrix values
        req_matrix = gate.get("require_matrix")
        if isinstance(req_matrix, dict):
            key = req_matrix["key"]
            want = set(req_matrix["values"])
            have = _matrix_values(job, key)
            if have is None:
                failures.append(f"[{gid}] job {gate['job']!r} has no matrix.{key} (need {sorted(want)})")
            elif not want.issubset(set(have)):
                failures.append(
                    f"[{gid}] matrix.{key}={have} is missing required values {sorted(want - set(have))}"
                )

        # `uses:` caller gate
        if "required_uses" in gate:
            want_uses = gate["required_uses"]
            uses = job.get("uses", "")
            if not (isinstance(uses, str) and uses.startswith(want_uses)):
                failures.append(
                    f"[{gid}] job {gate['job']!r} does not call {want_uses!r} (uses={uses!r})"
                )
            continue  # a uses: job has no run steps to inspect

        # step-invocation gate.
        tokens = gate.get("required_tokens")
        if tokens and gate.get("tokens_in_separate_steps"):
            # Each token must be run by SOME non-neutered step (the tokens are
            # separate commands, not one invocation). Used for the meta-gate's own
            # entry, where check_gates.py and selftest.py are two distinct steps.
            for tok in tokens:
                ok_tok = False
                neuter_reasons = []
                for step in (job.get("steps") or []):
                    body = "\n".join(_iter_lock_lines(_run_of(step)))
                    if tok in body:
                        neutered, nreason = _step_neutered(job, step)
                        if neutered:
                            neuter_reasons.append(nreason)
                            continue
                        ok_tok = True
                        break
                if not ok_tok:
                    if neuter_reasons:
                        failures.append(
                            f"[{gid}] the step invoking {tok!r} is neutered: " + "; ".join(neuter_reasons)
                        )
                    else:
                        failures.append(
                            f"[{gid}] no reachable step in job {gate['job']!r} runs {tok!r}"
                        )
            continue

        if tokens:
            candidate = None
            neuter_reasons = []
            for step in (job.get("steps") or []):
                run_text = _run_of(step)
                if not run_text:
                    continue
                # strip shell-comment lines before matching (decorative-match guard)
                body = "\n".join(_iter_lock_lines(run_text))
                if all(t in body for t in tokens):
                    neutered, nreason = _step_neutered(job, step)
                    if neutered:
                        neuter_reasons.append(nreason)
                        continue
                    candidate = step
                    break
            if candidate is None:
                if neuter_reasons:
                    failures.append(
                        f"[{gid}] a step invoking {tokens} EXISTS but is neutered: "
                        + "; ".join(neuter_reasons)
                    )
                else:
                    failures.append(
                        f"[{gid}] no reachable step in job {gate['job']!r} runs all of {tokens}"
                    )
            else:
                # anti-vacuity structural requirements
                for env_key in gate.get("requires_env", []):
                    if not _step_env_has(job, wf, candidate, env_key):
                        failures.append(
                            f"[{gid}] gate step does not set required env {env_key!r} "
                            "(without it the platform-backed assertions silently skip)"
                        )
                for repo in gate.get("requires_checkout", []):
                    if not _job_checks_out(job, repo):
                        failures.append(
                            f"[{gid}] job does not check out {repo!r} "
                            "(the platform-backed assertions cannot run without it)"
                        )

    return failures


def main(argv=None):
    ap = argparse.ArgumentParser(description="runtime quality-gate meta-gate")
    ap.add_argument("--root", default=".", help="repo root containing .github/")
    ap.add_argument("--manifest", default=None, help="path to quality-gates.toml")
    args = ap.parse_args(argv)

    root = Path(args.root)
    manifest = args.manifest or str(root / ".github" / "quality-gates.toml")
    if not Path(manifest).exists():
        sys.stderr.write(f"FATAL: manifest not found: {manifest}\n")
        return 2

    failures = evaluate(manifest, str(root))
    if failures:
        print("QUALITY-GATE MANIFEST CHECK: FAIL")
        for f in failures:
            print("  ✗ " + f)
        print(f"\n{len(failures)} gate problem(s). A required quality gate is absent or neutered.")
        print("Fix the workflow (or the manifest, if a gate was intentionally removed) — do not delete this check.")
        return 1
    print("QUALITY-GATE MANIFEST CHECK: PASS — every required gate is present and reachable.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
