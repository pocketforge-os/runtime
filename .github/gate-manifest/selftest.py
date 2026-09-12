#!/usr/bin/env python3
# =============================================================================
# selftest.py — proves check_gates.py CAN go red, and does so for the RIGHT
# gate. The guard-of-the-guard. (tsp-1x04)
# =============================================================================
#
# A meta-gate that cannot itself fail is instance #4 of the very defect this bead
# is about. So this selftest constructs the broken states and watches the checker
# go RED — the move from the `guards-must-be-shown-to-fail` memory: "ask what the
# thing would do IF WHAT IT GUARDS WERE BROKEN … construct the broken state and
# watch it go red, then check it goes red for the RIGHT reason and stays green on
# the rows you must not weaken."
#
# It mutates the REAL workflows (copied to a tempdir), one evasion at a time, and
# asserts (a) the unmutated tree PASSES, and (b) each mutation makes the checker
# FAIL with a reason naming the expected gate. Deriving the broken cases from the
# live tree — rather than committing hand-written fixture workflows — is a
# deliberate choice: a committed fixture copy is exactly the hand-copied artifact
# that silently drifts from what it mirrors (tsp-ozbp.16). This selftest can never
# go stale against the real workflows because it starts from them.
#
# Run in CI on every PR (see ../workflows/gate-manifest.yml), so the checker is
# proven able to fail on every run — not once in a pasted transcript.

from __future__ import annotations

import shutil
import os
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import check_gates  # noqa: E402

import yaml  # noqa: E402

RUNTIME_TESTS = ".github/workflows/runtime-tests.yml"
PREFS_E2E = ".github/workflows/prefs-e2e.yml"
PAYLOAD = "scripts/ci-container.sh"


# --------------------------------------------------------------------------- #
# helpers to mutate a copied tree                                             #
# --------------------------------------------------------------------------- #
def _load(root: Path, rel: str):
    return yaml.safe_load((root / rel).read_text())


def _dump(root: Path, rel: str, data) -> None:
    (root / rel).write_text(yaml.safe_dump(data, sort_keys=False))


def _test_job(wf):
    return wf["jobs"]["rust"]


def _payload_replace(root, old, new):
    path = root / PAYLOAD
    text = path.read_text()
    assert old in text, old
    path.write_text(text.replace(old, new))


def _find_step(job, needle):
    for i, step in enumerate(job.get("steps", [])):
        run = step.get("run")
        if isinstance(run, str) and needle in run:
            return i, step
    raise AssertionError(f"selftest bug: no step containing {needle!r}")


def _on(wf):
    # `on:` may be parsed as the boolean True key (YAML 1.1).
    return wf.get("on") if "on" in wf else wf.get(True)


# --------------------------------------------------------------------------- #
# mutations: each returns None and edits the tree in place                    #
# --------------------------------------------------------------------------- #
def m_delete_clippy_step(root):
    path = root / PAYLOAD
    path.write_text("\n".join(l for l in path.read_text().splitlines() if not l.startswith("cargo clippy")))


def m_delete_workspace_test(root):
    path = root / PAYLOAD
    path.write_text("\n".join(l for l in path.read_text().splitlines()
                              if not l.startswith("cargo test --locked --workspace")))


def m_narrow_workspace_test(root):
    _payload_replace(root, "cargo test --locked --workspace", "cargo test --locked -p pf-wire")


def m_delete_workspace_clippy(root):
    path = root / PAYLOAD
    path.write_text("\n".join(l for l in path.read_text().splitlines()
                              if not l.startswith("cargo clippy --locked --workspace")))


def m_narrow_workspace_clippy(root):
    _payload_replace(root, "cargo clippy --locked --workspace", "cargo clippy --locked -p pf-wire")


def m_weaken_only_workspace_clippy(root):
    _payload_replace(root, "cargo clippy --locked --workspace --offline --all-targets -- -D warnings",
                     "cargo clippy --locked --workspace --offline --all-targets")


def m_strip_locked_from_metadata(root):
    _payload_replace(root, "cargo metadata --offline --locked", "cargo metadata --offline")


def m_continue_on_error_clippy(root):
    wf = _load(root, RUNTIME_TESTS)
    job = _test_job(wf)
    job["continue-on-error"] = True
    _dump(root, RUNTIME_TESTS, wf)


def m_or_true_clippy(root):
    _payload_replace(root, "-- -D warnings", "-- -D warnings || true")


def m_path_exclude_crates(root):
    wf = _load(root, RUNTIME_TESTS)
    on = _on(wf)
    on["pull_request"]["paths"] = ["docs/**"]  # no longer covers crates/**
    _dump(root, RUNTIME_TESTS, wf)


def m_if_skip_clippy(root):
    _payload_replace(root, "cargo clippy --locked", "if false; then cargo clippy --locked")


def m_change_approved_job_if(root):
    wf = _load(root, RUNTIME_TESTS)
    job = _test_job(wf)
    job["if"] = "false && " + job["if"]
    _dump(root, RUNTIME_TESTS, wf)


def m_remove_approved_job_if(root):
    wf = _load(root, RUNTIME_TESTS)
    _test_job(wf).pop("if")
    _dump(root, RUNTIME_TESTS, wf)


def m_reformat_approved_job_if(root):
    wf = _load(root, RUNTIME_TESTS)
    job = _test_job(wf)
    job["if"] = job["if"].replace(" || ", "\n    ||\n")
    _dump(root, RUNTIME_TESTS, wf)


def m_weaken_clippy_flags(root):
    _payload_replace(root, " -- -D warnings", "")


def m_drop_matrix_row(root):
    wf = _load(root, PREFS_E2E)
    wf["jobs"]["matrix-row"]["strategy"]["matrix"]["descriptor"] = ["a523"]
    _dump(root, PREFS_E2E, wf)


def m_strip_platform_env(root):
    _payload_replace(root, '${PF_PLATFORM_DIR:?', '${UNSET_PLATFORM_DIR:?')


def m_remove_platform_checkout(root):
    wf = _load(root, RUNTIME_TESTS)
    # Checkout is owned by the immutable callee: changing it requires changing
    # its pin, which must invalidate the approved fixture/environment contract.
    _test_job(wf)["uses"] = _test_job(wf)["uses"].split("@")[0] + "@" + "0" * 40
    _dump(root, RUNTIME_TESTS, wf)


def m_mismatch_helper(root):
    wf = _load(root, RUNTIME_TESTS)
    _test_job(wf)["with"]["workflow-sha"] = "0" * 40
    _dump(root, RUNTIME_TESTS, wf)


def m_unpinned_image(root):
    wf = _load(root, RUNTIME_TESTS)
    _test_job(wf)["with"]["image"] = "ci-rust:latest"
    _dump(root, RUNTIME_TESTS, wf)


def m_bypass_result(root):
    wf = _load(root, RUNTIME_TESTS)
    wf["jobs"]["test"]["steps"][0]["run"] = "true"
    _dump(root, RUNTIME_TESTS, wf)


def m_early_success(root):
    _payload_replace(root, "set -euo pipefail", "set -euo pipefail\nexit 0")


def m_compound_success(root):
    group = "echo '::group::Lock and offline vendor gates (including negative control)'"
    _payload_replace(root, group, group + "; exit 0")
    # Execute the reviewer's evasion, not just a textual mutation. It exits zero
    # after the banner, without reaching any actual Cargo/vendor workload.
    platform = root / "platform"
    for rel in ("core/caps.py", "devices/a133/capabilities.toml", "devices/a523/capabilities.toml"):
        path = platform / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.touch()
    (platform / "skins/a133").mkdir(parents=True)
    result = subprocess.run(["bash", str(root / PAYLOAD)], cwd=root, capture_output=True,
                            text=True, timeout=5, env={**os.environ,
                                "PF_PLATFORM_DIR": str(platform), "PF_SOURCE_SHA": "a" * 40,
                                "SOURCE_DATE_EPOCH": "1", "RUSTFLAGS": "",
                                "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER": ""})
    assert result.returncode == 0 and result.stdout.strip() == group[6:-1], result


def m_compound_and_success(root):
    _payload_replace(root, "echo '::endgroup::'", "echo '::endgroup::' && exit 0")


def m_quoted_punctuation(root):
    _payload_replace(root, "echo '::endgroup::'", "echo '::endgroup:: literal ; and && punctuation'")


def m_script_redirect(root):
    wf = _load(root, RUNTIME_TESTS)
    _test_job(wf)["with"]["script"] = "scripts/empty.sh"
    _dump(root, RUNTIME_TESTS, wf)


def m_remove_format(root):
    _payload_replace(root, "cargo fmt --all -- --check", "echo formatter removed")


def m_delete_whole_workflow(root):
    (root / RUNTIME_TESTS).unlink()


# (mutation, expected-substring-in-some-failure, human label)
CASES = [
    (m_delete_clippy_step,       "[clippy-workspace]",            "clippy step deleted entirely (ABSENT)"),
    (m_strip_locked_from_metadata, "no-unlocked-cargo",           "--locked stripped (ABSENT flag, tsp-ovee)"),
    (m_continue_on_error_clippy, "continue-on-error",             "clippy marked continue-on-error (NEUTERED)"),
    (m_or_true_clippy,           "|| true",                       "clippy tail `|| true` (NEUTERED)"),
    (m_path_exclude_crates,      "guarded path 'crates/**'",      "paths filter excludes crates/** (FILTERED)"),
    (m_if_skip_clippy,           "conditionally skippable",       "clippy `if:` skip (SKIPPED STEP)"),
    (m_change_approved_job_if,   "does not match",                "approved job `if:` changed (WHITELIST MISMATCH)"),
    (m_remove_approved_job_if,   "does not match",                "approved job `if:` removed (WHITELIST MISMATCH)"),
    (m_weaken_clippy_flags,      "[clippy-workspace]",            "clippy `-D warnings` removed (WEAKENED/vacuous)"),
    (m_drop_matrix_row,          "[prefs-e2e-unification]",       "a133 matrix row dropped (silent single-row)"),
    (m_strip_platform_env,       "[workspace-tests]",             "PF_PLATFORM_DIR unset (VACUOUS skip)"),
    (m_remove_platform_checkout, "[workspace-tests]",             "platform checkout removed (VACUOUS skip)"),
    (m_delete_whole_workflow,    "does not exist",                "whole runtime-tests.yml deleted (EXCLUDED JOB)"),
    (m_mismatch_helper,          "inputs differ",                  "helper pin differs from approved callee"),
    (m_unpinned_image,           "inputs differ",                  "mutable image tag substituted"),
    (m_bypass_result,            "result gate",                    "required-name result gate masks failure"),
    (m_early_success,            "conditionally skippable",        "payload exits successfully before any tests"),
    (m_script_redirect,          "inputs differ",                  "caller redirects execution to empty script"),
    (m_remove_format,            "[format-workspace]",             "whole-workspace format check removed"),
    (m_delete_workspace_test,    "[workspace-tests]",              "workspace test deleted, keyboard test remains"),
    (m_narrow_workspace_test,    "[workspace-tests]",              "workspace test narrowed to pf-wire"),
    (m_delete_workspace_clippy,  "[clippy-workspace]",             "workspace clippy deleted, keyboard clippy remains"),
    (m_narrow_workspace_clippy,  "[clippy-workspace]",             "workspace clippy narrowed to pf-wire"),
    (m_weaken_only_workspace_clippy, "[clippy-workspace]",         "workspace warnings flag cannot come from keyboard"),
    (m_compound_success,         "compound shell",                 "executed echo; exit 0 bypass is rejected"),
    (m_compound_and_success,     "compound shell",                 "echo && exit 0 bypass is rejected"),
]

PASS_CASES = [
    (m_reformat_approved_job_if, "approved job `if:` whitespace/line-folding changed"),
    (m_quoted_punctuation, "quoted banner punctuation stays literal"),
]


def _copy_tree(src_root: Path, dst_root: Path):
    shutil.copytree(src_root / ".github", dst_root / ".github")
    (dst_root / "scripts").mkdir()
    shutil.copyfile(src_root / PAYLOAD, dst_root / PAYLOAD)


def main(argv=None):
    import argparse
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", default=".", help="repo root with the REAL .github/")
    args = ap.parse_args(argv)
    src = Path(args.root).resolve()

    results = []
    ok = True

    # --- POSITIVE control: unmutated tree must PASS -------------------------- #
    with tempfile.TemporaryDirectory() as td:
        dst = Path(td)
        _copy_tree(src, dst)
        failures = check_gates.evaluate(str(dst / ".github" / "quality-gates.toml"), str(dst))
        passed = len(failures) == 0
        ok = ok and passed
        results.append(("POSITIVE (unmutated tree)", "expect PASS", passed,
                        "PASS" if passed else f"UNEXPECTED FAIL: {failures}"))

    # Formatting-only differences must compare equal after YAML folding/whitespace
    # normalization; otherwise equivalent folded and single-line forms would drift.
    for mutate, label in PASS_CASES:
        with tempfile.TemporaryDirectory() as td:
            dst = Path(td)
            _copy_tree(src, dst)
            mutate(dst)
            failures = check_gates.evaluate(str(dst / ".github" / "quality-gates.toml"), str(dst))
            passed = len(failures) == 0
            ok = ok and passed
            results.append((label, "expect PASS", passed,
                            "PASS" if passed else f"UNEXPECTED FAIL: {failures}"))

    # --- NEGATIVE controls: each mutation must make it RED for the right gate  #
    for mutate, expect_sub, label in CASES:
        with tempfile.TemporaryDirectory() as td:
            dst = Path(td)
            _copy_tree(src, dst)
            mutate(dst)
            failures = check_gates.evaluate(str(dst / ".github" / "quality-gates.toml"), str(dst))
            went_red = len(failures) > 0
            right_reason = any(expect_sub in f for f in failures)
            case_ok = went_red and right_reason
            ok = ok and case_ok
            if case_ok:
                detail = "RED for the right reason"
            elif not went_red:
                detail = "DID NOT GO RED (meta-gate is blind to this evasion!)"
            else:
                detail = f"red but WRONG reason (want {expect_sub!r}): {failures}"
            results.append((label, f"expect RED ~ {expect_sub!r}", case_ok, detail))

    # --- report ------------------------------------------------------------- #
    print("=" * 78)
    print("gate-manifest selftest — proving the meta-gate can fail (and passes clean)")
    print("=" * 78)
    for label, expect, good, detail in results:
        mark = "✓" if good else "✗"
        print(f" {mark}  {label:<48} {detail}")
    print("-" * 78)
    if ok:
        print(f"SELFTEST PASS — {len(results)} controls, positive green + all negatives red for the right reason.")
        return 0
    print("SELFTEST FAIL — the meta-gate did not behave as required above.")
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
