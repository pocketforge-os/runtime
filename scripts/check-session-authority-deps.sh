#!/usr/bin/env bash
# Keep the reusable authority library free of the privileged sampler/unwinder stack.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

tree="$(cargo tree --offline --locked -p pf-session-authority -e normal --prefix none)"
for forbidden in pf-hang-sampler framehop gimli object; do
    if grep -Eq "^${forbidden}( |$)" <<<"${tree}"; then
        echo "session-authority dependency boundary: forbidden normal dependency ${forbidden}" >&2
        exit 1
    fi
done

echo "session-authority dependency boundary: pass"
