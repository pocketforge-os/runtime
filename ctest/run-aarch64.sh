#!/usr/bin/env bash
# Cross-link (but do not execute) a GNU AArch64 C consumer against the staged SDK.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/.." && pwd)"
build="$here/build-aarch64"
sdk="$build/sdk"
epoch="${SOURCE_DATE_EPOCH:-$(git -C "$root" show -s --format=%ct HEAD)}"
cross_cc="${PF_AARCH64_GNU_CC:-aarch64-none-linux-gnu-gcc}"
cross_readelf="${PF_AARCH64_GNU_READELF:-aarch64-none-linux-gnu-readelf}"

command -v "$cross_cc" >/dev/null || {
  echo "ctest: GNU AArch64 compiler not found: $cross_cc" >&2
  exit 2
}
command -v "$cross_readelf" >/dev/null || {
  echo "ctest: GNU AArch64 readelf not found: $cross_readelf" >&2
  exit 2
}

rm -rf "$build"
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER="$cross_cc" \
  SOURCE_DATE_EPOCH="$epoch" "$root/scripts/build-sdk.sh" "$sdk"
"$cross_cc" -Wall -Wextra -O2 \
  -ffile-prefix-map="$root"=. -fdebug-prefix-map="$root"=. \
  -I"$sdk/include" "$here/smoke.c" "$sdk/lib/libpocketforge.a" \
  -lpthread -ldl -lm -o "$build/smoke"

test "$(od -An -tx1 -j18 -N2 "$build/smoke" | tr -d ' ')" = b700
"$cross_readelf" -h "$build/smoke" | grep -Fq 'AArch64'
echo "ctest: AArch64 GNU static-link smoke passed"
