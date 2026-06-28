#!/usr/bin/env bash
# check-abi.sh — the libpocketforge ABI-diff guard (tsp-e1b.5).
#
# Asserts the built libpocketforge.so still exports EVERY symbol in the frozen golden file
# (abi/libpocketforge.v1.abi). A removed/renamed frozen symbol is a BREAKING change and FAILS
# here (never silent). NEW pf_* exports are reported as additive (minor) — not a failure, but a
# reminder to append them to the golden file in the same change. Pair with the wire frozen-contract
# test: `cargo test -p pf-wire --test frozen_contract`.
#
# Usage: bash abi/check-abi.sh   (run from the workspace root; builds the cdylib if needed)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
GOLDEN="$ROOT/abi/libpocketforge.v1.abi"
SO="$ROOT/target/release/libpocketforge.so"

echo "== build libpocketforge (release cdylib) =="
( cd "$ROOT" && cargo build --release -p libpocketforge >/dev/null )
[ -f "$SO" ] || { echo "FAIL: $SO not built"; exit 1; }

# Exported, defined, external pf_* symbols actually present in the binary.
present="$(nm -D --defined-only --extern-only "$SO" | awk '{print $3}' | grep '^pf_' | sort -u)"
# The frozen contract (comments + blanks stripped).
frozen="$(grep -vE '^\s*#|^\s*$' "$GOLDEN" | sort -u)"

missing="$(comm -23 <(printf '%s\n' "$frozen") <(printf '%s\n' "$present"))"
added="$(comm -13 <(printf '%s\n' "$frozen") <(printf '%s\n' "$present"))"

fail=0
if [ -n "$missing" ]; then
  echo "FAIL: frozen ABI symbol(s) MISSING from libpocketforge.so (BREAKING — bump major + golden):"
  printf '  - %s\n' $missing
  fail=1
else
  echo "ok   - all $(printf '%s\n' "$frozen" | wc -l | tr -d ' ') frozen symbols are present"
fi

if [ -n "$added" ]; then
  echo "note - new pf_* export(s) not yet in the golden file (additive/minor — append them):"
  printf '  + %s\n' $added
fi

[ "$fail" -eq 0 ] || exit 1
echo "ABI OK (libpocketforge v1 contract intact)"
