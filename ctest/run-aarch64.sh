#!/usr/bin/env bash
# Cross-link (but do not execute) a GNU AArch64 C consumer against the staged SDK.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/.." && pwd)"
build="$here/build-aarch64"
sdk="$build/sdk"
epoch="${SOURCE_DATE_EPOCH:-$(git -C "$root" show -s --format=%ct HEAD)}"

rm -rf "$build"
SOURCE_DATE_EPOCH="$epoch" "$root/scripts/build-sdk.sh" "$sdk"
aarch64-none-linux-gnu-gcc -Wall -Wextra -O2 \
  -ffile-prefix-map="$root"=. -fdebug-prefix-map="$root"=. \
  -I"$sdk/include" "$here/smoke.c" "$sdk/lib/libpocketforge.a" \
  -lpthread -ldl -lm -o "$build/smoke"

test "$(od -An -tx1 -j18 -N2 "$build/smoke" | tr -d ' ')" = b700
aarch64-none-linux-gnu-readelf -h "$build/smoke" | grep -Fq 'AArch64'
echo "ctest: AArch64 GNU static-link smoke passed"
