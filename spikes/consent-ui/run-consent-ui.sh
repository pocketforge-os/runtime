#!/usr/bin/env bash
# run-consent-ui.sh — tsp-ht0p.1 SPIKE-2 end-to-end runner (on modelmaker).
#
# Build the C prototype against the sim's static SDL3-render, drive it through
# the DESIGN.md §2 state matrix via driver.py, dump per-state PNGs, and (unless
# --skip-review) route each PNG through review-screen.sh for VLM verification
# on modelmaker (image pixels never enter this transcript).
#
# Usage:
#   ./run-consent-ui.sh                   # build + render all + review all
#   ./run-consent-ui.sh --skip-review     # build + render only (no VLM step)
#   ./run-consent-ui.sh --skip-build      # skip make (use existing build/)
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
cd "$HERE"

: "${SDLR:=/home/mm/sim-build/sdl3-render}"
: "${OUTDIR:=$HERE/build/frames}"
: "${TRANSCRIPT_JSON:=$HERE/build/transcript.json}"
: "${REVIEW_SH:=/home/matt/pocketforge-automation/scripts/review-screen.sh}"

SKIP_REVIEW=0
SKIP_BUILD=0
for arg in "$@"; do
  case "$arg" in
    --skip-review) SKIP_REVIEW=1 ;;
    --skip-build)  SKIP_BUILD=1  ;;
    *) echo "unknown arg $arg"; exit 2 ;;
  esac
done

echo "=== consent-ui build (SDLR=$SDLR) ==="
if [ "$SKIP_BUILD" = 0 ]; then
  SDLR="$SDLR" make build/consent-render
else
  echo "(skipped)"
fi

echo "=== consent-ui render matrix (12 scenarios) ==="
python3 driver.py --outdir "$OUTDIR" --json "$TRANSCRIPT_JSON"

if [ "$SKIP_REVIEW" = 1 ]; then
  echo "=== review skipped (--skip-review) ==="
  echo "PNGs: $OUTDIR/*.png"
  exit 0
fi

echo "=== consent-ui screen-review pass (routes pixels via opencode --file) ==="
if [ ! -x "$REVIEW_SH" ]; then
  echo "WARN: review-screen.sh not found at $REVIEW_SH — install pocketforge-automation to route PNGs to the reviewer" >&2
  echo "PNGs: $OUTDIR/*.png"
  exit 0
fi

VERDICTS="$HERE/build/verdicts.jsonl"
: > "$VERDICTS"
FAILS=0
for png in "$OUTDIR"/*.png; do
  sid=$(basename "$png" .png)
  echo "--- reviewing $sid ---"
  # review-screen.sh emits text; we capture verdict + confidence per bead §"Verify" contract.
  if verdict=$("$REVIEW_SH" "$png" 2>&1); then
    printf '{"id":"%s","png":"%s","status":"ok","verdict":%s}\n' \
      "$sid" "$png" "$(printf '%s' "$verdict" | python3 -c 'import json,sys;print(json.dumps(sys.stdin.read()))')" \
      >> "$VERDICTS"
    printf '%s\n' "$verdict" | head -8
  else
    printf '{"id":"%s","png":"%s","status":"error","verdict":%s}\n' \
      "$sid" "$png" "$(printf '%s' "$verdict" | python3 -c 'import json,sys;print(json.dumps(sys.stdin.read()))')" \
      >> "$VERDICTS"
    FAILS=$((FAILS + 1))
    echo "REVIEW ERROR for $sid" >&2
  fi
done

echo
echo "=== summary ==="
echo "  transcript: $TRANSCRIPT_JSON"
echo "  verdicts:   $VERDICTS"
echo "  fails:      $FAILS"
exit $FAILS
