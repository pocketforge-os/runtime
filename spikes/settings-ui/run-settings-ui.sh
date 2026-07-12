#!/usr/bin/env bash
# run-settings-ui.sh — tsp-xubv.3 settings-UI end-to-end runner (on modelmaker).
#
# Three coupled halves, one command:
#   1. RENDER + NAV   — build the C prototype against the sim's static SDL3-render,
#      drive it through the DESIGN.md §1 navigation matrix via driver.py (both device
#      profiles), dump per-state PNGs + a render transcript + an authority-write
#      intent transcript.
#   2. NAV-GRAMMAR ASSERT — a lightweight check that the navigation actually PRODUCED
#      authority writes of the expected shape (A-toggle on hapticsEnabled; L/R-adjust
#      on brightness) — the seam between the nav half and the liveness half.
#   3. LIVENESS       — build + run the Rust harness that drives the REAL merged
#      pf-prefs store + pocketforge observer, proving a toggle flips a running app
#      with no restart (both write paths), store round-trip, brightness scalar, and
#      the a133 honest-absent leg.
#   4. REVIEW         — route each PNG through review-screen.sh (screen-reviewer VLM
#      on modelmaker); image pixels never enter this transcript.
#
# Usage:
#   ./run-settings-ui.sh                 # build + render + assert + liveness + review
#   ./run-settings-ui.sh --skip-review   # everything except the VLM review pass
#   ./run-settings-ui.sh --skip-build    # reuse existing build/ (C) but still liveness+review
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
cd "$HERE"

: "${SDLR:=/home/mm/sim-build/sdl3-render}"
: "${OUTDIR:=$HERE/build/frames}"
: "${TRANSCRIPT_JSON:=$HERE/build/transcript.json}"
: "${WRITES_JSON:=$HERE/build/writes.json}"
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

echo "=== settings-ui build (SDLR=$SDLR) ==="
if [ "$SKIP_BUILD" = 0 ]; then
  SDLR="$SDLR" make build/settings-render
else
  echo "(skipped)"
fi

echo "=== settings-ui render + nav matrix ==="
python3 driver.py --outdir "$OUTDIR" --json "$TRANSCRIPT_JSON" --writes-json "$WRITES_JSON"

echo "=== nav-grammar assert (writes of expected shape produced) ==="
python3 - "$WRITES_JSON" <<'PY'
import json, sys
w = json.load(open(sys.argv[1]))["writes"]
def has(key, via, profile=None):
    return any(x["key"] == key and x["via"] == via and (profile is None or x["profile"] == profile) for x in w)
fails = 0
def want(cond, msg):
    global fails
    print(("ok   " if cond else "FAIL ") + ": " + msg)
    if not cond: fails += 1
want(has("hapticsEnabled", "A-toggle", "a523"), "a523 A-toggle produced a hapticsEnabled authority write")
want(has("brightness", "LR-adjust", "a523"), "a523 L/R-adjust produced a brightness authority write")
want(has("reduceMotion", "A-toggle", "a523"), "a523 A-toggle produced a reduceMotion authority write")
want(has("monoAudio", "A-toggle"), "A-toggle produced a monoAudio authority write")
want(not has("hapticsEnabled", "A-toggle", "a133"), "a133 never produced a hapticsEnabled write (row absent, not a focus stop)")
sys.exit(1 if fails else 0)
PY

echo "=== settings-ui LIVENESS (real pf-prefs store + observer) ==="
( cd liveness && cargo run --quiet )

if [ "$SKIP_REVIEW" = 1 ]; then
  echo "=== review skipped (--skip-review) ==="
  echo "PNGs: $OUTDIR/*.png"
  exit 0
fi

echo "=== settings-ui screen-review pass (routes pixels via opencode --file) ==="
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
echo "  render transcript: $TRANSCRIPT_JSON"
echo "  write intents:     $WRITES_JSON"
echo "  verdicts:          $VERDICTS"
echo "  review fails:      $FAILS"
exit $FAILS
