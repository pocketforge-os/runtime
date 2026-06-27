#!/usr/bin/env bash
# itest/run.sh — the v0 INPUT broker enforcement proof (tsp-e1b.6), device-free but root-required.
#
# Proves, against the E5 sim's descriptor-synthesized source, on BOTH x86 (native) and arm64
# (under qemu-tsp):
#   1. the broker EVIOCGRABs the real source + re-emits a uinput device the app reads;
#   2. the descriptor action-map NORMALIZES the X360 driver quirk — physical WEST emits BTN_X
#      (0x133/307) on the source, but the app reads canonical BTN_WEST (0x134/308);
#   3. ENFORCEMENT IS REAL — a reader of the grabbed source sees ZERO events (cannot bypass);
#   4. the native leg acquires the read fd via Acquire("input") + SCM_RIGHTS (the wire §4.1 path).
#
# Run as root (uinput + the created event nodes are root-only):
#   sudo QEMU_TSP=~/qemu-tsp/build/qemu-tsp/qemu-aarch64 PF_SIM=~/sim bash itest/run.sh
set -euo pipefail

QEMU_TSP="${QEMU_TSP:-$HOME/qemu-tsp/build/qemu-tsp/qemu-aarch64}"
PF_SIM="${PF_SIM:-$HOME/sim}"
CRATE="$(cd "$(dirname "$0")/.." && pwd)"                 # crates/pf-input-broker
ROOT="$(cd "$CRATE/../.." && pwd)"                        # workspace root
DEVICE="${DEVICE:-a133}"
FIXTURE="$ROOT/crates/pocketforge/tests/fixtures/${DEVICE}-capabilities.toml"

[ -x "$QEMU_TSP" ] || { echo "FAIL: QEMU_TSP not executable at $QEMU_TSP"; exit 1; }
[ -f "$PF_SIM/synth/uinput_synth.py" ] || { echo "FAIL: sim synth not at $PF_SIM/synth/uinput_synth.py"; exit 1; }
[ -f "$FIXTURE" ] || { echo "FAIL: descriptor $FIXTURE missing"; exit 1; }

WORK="$(mktemp -d)"
PLATFORM="$WORK/platform/devices/$DEVICE"
mkdir -p "$PLATFORM"
cp "$FIXTURE" "$PLATFORM/capabilities.toml"

DRIVE_PID=""; BROKER_PID=""
cleanup() {
  touch "$WORK/stop" 2>/dev/null || true
  [ -n "$BROKER_PID" ] && kill "$BROKER_PID" 2>/dev/null || true
  [ -n "$DRIVE_PID" ] && kill "$DRIVE_PID" 2>/dev/null || true
  sleep 0.2
  rm -rf "$WORK"
}
trap cleanup EXIT

# Binaries are pre-built by the (non-root) caller — `cargo build --release -p pf-input-broker` —
# because cargo/rustup live in the invoking user's env, not root's. The harness only does the
# root-required steps (uinput, grab, reading root-owned nodes).
BROKER="$ROOT/target/release/pf-input-broker"
READ_RS="$ROOT/target/release/pf-input-read"
[ -x "$BROKER" ] && [ -x "$READ_RS" ] || {
  echo "FAIL: build first (as the cargo user): cargo build --release -p pf-input-broker"; exit 1; }

echo "== compile C readers (x86 + aarch64-static) =="
gcc -O0 -o "$WORK/reader.x86" "$CRATE/itest/reader.c"
aarch64-linux-gnu-gcc -O0 -static -o "$WORK/reader.arm64" "$CRATE/itest/reader.c"

echo "== modprobe uinput =="
modprobe uinput

echo "== synthesize + drive the descriptor source ($DEVICE), pressing south,west =="
python3 "$CRATE/itest/source_drive.py" --sim "$PF_SIM" --platform "$WORK/platform" --device "$DEVICE" \
  --press south,west --ready-file "$WORK/src.node" --inject-file "$WORK/inject" --stop-file "$WORK/stop" \
  >/dev/null 2>"$WORK/drive.err" &
DRIVE_PID=$!
for _ in $(seq 1 100); do [ -s "$WORK/src.node" ] && break; sleep 0.05; done
SRC="$(cat "$WORK/src.node" 2>/dev/null || true)"
[ -n "$SRC" ] || { echo "FAIL: source node not created"; cat "$WORK/drive.err"; exit 1; }
echo "   source = $SRC"

echo "== start the broker (grab $SRC, re-emit, serve Acquire(input)) =="
"$BROKER" --source "$SRC" --descriptor "$FIXTURE" --acquire-sock "$WORK/in.sock" \
  >"$WORK/broker.out" 2>"$WORK/broker.err" &
BROKER_PID=$!
for _ in $(seq 1 100); do grep -q '^ready' "$WORK/broker.out" 2>/dev/null && break; sleep 0.05; done
grep -q '^ready' "$WORK/broker.out" || { echo "FAIL: broker not ready"; cat "$WORK/broker.err"; exit 1; }
REEMIT="$(sed -n 's/^node=//p' "$WORK/broker.out" | head -1)"
echo "   re-emit = $REEMIT"

fail=0
assert_grep() { # <file> <pattern> <msg>
  if grep -q -- "$2" "$1"; then echo "   ok   - $3"; else echo "   FAIL - $3"; echo "     (missing /$2/ in:)"; sed 's/^/       /' "$1"; fail=1; fi
}

echo "== LEG 1: native consumer via Acquire(\"input\") + SCM_RIGHTS =="
"$READ_RS" --from-broker "$WORK/in.sock" --also-check-source "$SRC" --ms 2500 \
  >"$WORK/out.native" 2>"$WORK/err.native" &
RPID=$!
sleep 0.6; touch "$WORK/inject"; sleep 0.8; touch "$WORK/inject"; sleep 0.4
wait "$RPID" || true
assert_grep "$WORK/out.native" "EV 1 304 1" "south press → canonical BTN_SOUTH (0x130/304)"
assert_grep "$WORK/out.native" "EV 1 308 1" "WEST press → canonical BTN_WEST (0x134/308) [driver emitted BTN_X 307]"
assert_grep "$WORK/out.native" "^SOURCE_EVENTS 0$" "grabbed source is SILENT to the app (enforcement)"

echo "== LEG 2: arm64 consumer UNDER qemu-tsp (direct node read) =="
"$QEMU_TSP" "$WORK/reader.arm64" "$REEMIT" "$SRC" 2200 >"$WORK/out.qemu" 2>"$WORK/err.qemu" &
QPID=$!
sleep 0.6; touch "$WORK/inject"; sleep 0.8; touch "$WORK/inject"; sleep 0.4
wait "$QPID" || true
assert_grep "$WORK/out.qemu" "EV 1 308 1" "WEST→BTN_WEST normalization visible under qemu-tsp"
assert_grep "$WORK/out.qemu" "^SOURCE_EVENTS 0$" "enforcement holds in the arm64 target (qemu-tsp)"

echo
if [ "$fail" -eq 0 ]; then
  echo "PASS: v0 INPUT broker — grab + descriptor-remap + SCM_RIGHTS handoff + enforcement, native AND qemu-tsp"
else
  echo "FAIL: see assertions above"; exit 1
fi
