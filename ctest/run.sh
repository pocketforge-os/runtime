#!/usr/bin/env bash
# ctest/run.sh — build libpocketforge as a staticlib, compile smoke.c against it with plain
# gcc, and run it. Proves the C ABI links + behaves (the "any-language OCI app links
# libpocketforge" claim, off-hardware). Run from the repo root on the build host (modelmaker).
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/.." && pwd)"
build="$here/build"
mkdir -p "$build"

# Descriptor: arg 1, or the committed a133 test fixture.
desc="${1:-$root/crates/pocketforge/tests/fixtures/a133-capabilities.toml}"
[ -f "$desc" ] || { echo "ctest: no descriptor at $desc" >&2; exit 2; }

echo "ctest: building libpocketforge staticlib (release)..."
cargo build --release -p libpocketforge >/dev/null

lib="$root/target/release/libpocketforge.a"
[ -f "$lib" ] || { echo "ctest: missing $lib" >&2; exit 1; }

echo "ctest: compiling smoke.c against the staticlib with gcc..."
# Rust staticlib needs the std's native deps linked explicitly.
gcc -Wall -Wextra -O2 -I"$root/include" "$here/smoke.c" "$lib" \
    -lpthread -ldl -lm -o "$build/smoke"

echo "ctest: running smoke..."
"$build/smoke" "$desc"
