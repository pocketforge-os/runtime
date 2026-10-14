#!/usr/bin/env bash
# Drop verified cross-target outputs before host-only lint reuses the CI budget.
set -euo pipefail

root="${PF_CI_ARTIFACT_ROOT:-$PWD}"
if [[ ! -f "$root/Cargo.toml" && ! -f "$root/.pf-ci-artifact-root" ]]; then
  echo "clean-ci-cross-artifacts: refusing unmarked root: $root" >&2
  exit 2
fi
for relative in \
  target/aarch64-unknown-linux-musl \
  target/aarch64-unknown-linux-gnu \
  ctest/build-aarch64
do
  build_dir="$root/$relative"
  if [[ -d "$build_dir" ]]; then
    find "$build_dir" -mindepth 1 -delete
    rmdir "$build_dir"
  fi
done
