#!/usr/bin/env bash
set -euo pipefail

tmp="$(mktemp -d "${RUNNER_TEMP:-/tmp}/pf-clean-ci-cross-artifacts.XXXXXX")"
cleanup() {
  find "$tmp" -mindepth 1 -delete
  rmdir "$tmp"
}
trap cleanup EXIT

mkdir -p \
  "$tmp/target/aarch64-unknown-linux-musl/release" \
  "$tmp/target/aarch64-unknown-linux-gnu/debug" \
  "$tmp/ctest/build-aarch64/nested" \
  "$tmp/target/debug"
touch \
  "$tmp/.pf-ci-artifact-root" \
  "$tmp/target/aarch64-unknown-linux-musl/release/app" \
  "$tmp/target/aarch64-unknown-linux-gnu/debug/lib.a" \
  "$tmp/ctest/build-aarch64/nested/test" \
  "$tmp/target/debug/keep-me" \
  "$tmp/keep-me"

PF_CI_ARTIFACT_ROOT="$tmp" bash scripts/clean-ci-cross-artifacts.sh

test ! -e "$tmp/target/aarch64-unknown-linux-musl"
test ! -e "$tmp/target/aarch64-unknown-linux-gnu"
test ! -e "$tmp/ctest/build-aarch64"
test -f "$tmp/target/debug/keep-me"
test -f "$tmp/keep-me"

mkdir "$tmp/unmarked"
touch "$tmp/unmarked/keep-me"
if PF_CI_ARTIFACT_ROOT="$tmp/unmarked" bash scripts/clean-ci-cross-artifacts.sh; then
  echo "clean-ci-cross-artifacts: unmarked-root negative control unexpectedly passed" >&2
  exit 1
fi
test -f "$tmp/unmarked/keep-me"

echo "clean-ci-cross-artifacts: positive and sibling negative controls passed"
