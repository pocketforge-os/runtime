#!/usr/bin/env bash
# Build and stage the GNU AArch64 C SDK from reproducible runtime inputs.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
out="${1:-$root/out/sdk}"
: "${SOURCE_DATE_EPOCH:?set SOURCE_DATE_EPOCH to the runtime commit timestamp}"

export CARGO_INCREMENTAL=0
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$root=. --remap-path-prefix=/work/runtime=. --remap-path-prefix=/opt/rust=/toolchain/rust --remap-path-prefix=/opt/arm-10.3-2021.07=/toolchain/gcc -C debuginfo=0"
export ZERO_AR_DATE=1

cd "$root"
scripts/check-vendor.sh
cargo build --offline --locked --release --target aarch64-unknown-linux-gnu -p libpocketforge
install -D -m0644 include/pocketforge.h "$out/include/pocketforge.h"
install -D -m0644 target/aarch64-unknown-linux-gnu/release/libpocketforge.a "$out/lib/libpocketforge.a"
echo "build-sdk: staged $out"
