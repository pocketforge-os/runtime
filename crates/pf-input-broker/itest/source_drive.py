#!/usr/bin/env python3
"""source_drive.py — synthesize + drive the REAL evdev SOURCE for the v0 INPUT broker proof.

REUSES the E5 sim's descriptor-driven uinput synth (`sim/synth/uinput_synth.py`) — zero per-device
code — to create the "TRIMUI Player1" source node the broker grabs. Then, on demand, it injects
specific physical buttons (e.g. WEST, whose X360-driver code is BTN_X/0x133) so the harness can
assert the broker re-emits the CANONICAL code (BTN_WEST/0x134) — the driver-quirk normalization.

Run as root (uinput). Protocol (file-based, robust across processes):
  * writes the pad source node path to <ready-file> once created, and prints it to stdout;
  * each time <inject-file> appears, presses+releases the configured ids, then deletes the file;
  * exits when <stop-file> appears.

Usage:
  source_drive.py --sim <sim-dir> --platform <dir> --device <id> \
      --press south,west --ready-file R --inject-file I --stop-file S
"""
import argparse
import os
import sys
import time


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--sim", required=True, help="the E5 sim checkout (holds synth/uinput_synth.py)")
    ap.add_argument("--platform", required=True)
    ap.add_argument("--device", required=True)
    ap.add_argument("--press", required=True, help="comma-separated input ids to inject per trigger")
    ap.add_argument("--ready-file", required=True)
    ap.add_argument("--inject-file", required=True)
    ap.add_argument("--stop-file", required=True)
    a = ap.parse_args()

    sys.path.insert(0, os.path.join(a.sim, "synth"))
    from uinput_synth import Synth, load_descriptor  # noqa: E402  (reuse the sim's synth)

    desc = load_descriptor(a.platform, a.device)
    synth = Synth(desc).create()
    pad = next((n["node"] for n in synth.nodes() if n["role"] == "pad" and n["node"]), None)
    if not pad:
        print("NO_PAD_NODE", file=sys.stderr)
        return 2
    with open(a.ready_file, "w") as f:
        f.write(pad)
    print(pad, flush=True)

    ids = [s for s in a.press.split(",") if s]
    try:
        while not os.path.exists(a.stop_file):
            if os.path.exists(a.inject_file):
                for iid in ids:
                    synth.press(iid)
                    time.sleep(0.02)
                    synth.release(iid)
                    time.sleep(0.02)
                os.unlink(a.inject_file)
            time.sleep(0.03)
    finally:
        synth.destroy()
    return 0


if __name__ == "__main__":
    sys.exit(main())
