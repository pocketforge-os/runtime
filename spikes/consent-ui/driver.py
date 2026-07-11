#!/usr/bin/env python3
"""driver.py — tsp-ht0p.1 SPIKE-2 consent-UI state-machine driver.

Encodes the DESIGN.md §2 interaction model as a small pure-Python state machine, drives it
through the scripted event sequences below, and invokes ./build/consent-render at each state
to dump a PPM. Then converts each PPM to PNG via ppm2png.py so the screen-reviewer verifier
(review-screen.sh) can read it.

Verification of the PNGs is EXPLICITLY not done here: driver.py runs on modelmaker (where the
prototype builds and executes) and the screen-reviewer runs on modelmaker too (its vLLM host),
but the *only* sanctioned invocation path routes image pixels through opencode's `--file`
attach (or the `screen-reviewer` opencode agent). The driver just produces the PNGs; the
run-consent-ui.sh runner then invokes review-screen.sh for each so pixels never enter this
process's or any bd/pf-comms transcript.
"""
from __future__ import annotations

import argparse
import dataclasses
import json
import os
import subprocess
import sys
import time
from typing import Iterable

HERE = os.path.dirname(os.path.abspath(__file__))
BIN = os.path.join(HERE, "build", "consent-render")
PPM2PNG = os.path.join(HERE, "ppm2png.py")

FOCUSES = ("deny", "allow-once", "allow-always")


# ---------- state machine (mirrors DESIGN.md §2 verbatim) ----------
@dataclasses.dataclass
class DialogState:
    focus: str = "deny"                # DESIGN.md §2 "Default focus = Deny"
    state: str = "initial"             # initial | selected | cancelled
    events: list[str] = dataclasses.field(default_factory=list)

    def apply(self, event: str) -> "DialogState":
        # commit / cancel are terminal; further events no-op (matches supervisor behavior:
        # once the response is on the seam wire the dialog is dismissed)
        if self.state in ("selected", "cancelled"):
            return dataclasses.replace(self, events=self.events + [event + " (no-op)"])

        if event == "dpad-right":
            i = FOCUSES.index(self.focus)
            # DESIGN.md §2 "Focus wrap: dpad-right from [Allow always] does NOT wrap"
            j = min(i + 1, len(FOCUSES) - 1)
            return dataclasses.replace(self, focus=FOCUSES[j], events=self.events + [event])
        if event == "dpad-left":
            i = FOCUSES.index(self.focus)
            j = max(i - 1, 0)
            return dataclasses.replace(self, focus=FOCUSES[j], events=self.events + [event])
        if event in ("dpad-up", "dpad-down", "X", "Y", "L1", "L2", "R1", "R2",
                     "lstick", "rstick", "home", "menu", "select", "start"):
            # DESIGN.md §2 "Inputs deliberately NOT used" — no-op, but recorded for audit
            return dataclasses.replace(self, events=self.events + [event + " (no-op)"])
        if event == "A":
            # commit current focus
            return dataclasses.replace(self, state="selected",
                                       events=self.events + [event])
        if event == "B":
            # cancel — collapses to Deny outcome
            return dataclasses.replace(self, focus="deny", state="cancelled",
                                       events=self.events + [event])
        raise ValueError(f"unknown event {event!r}")

    def decision(self) -> str | None:
        if self.state == "selected":
            return self.focus                    # deny | allow-once | allow-always
        if self.state == "cancelled":
            return "deny"                        # DESIGN.md §2 "B on any focused state = deny"
        return None


# ---------- scripted scenarios ----------
# Each scenario is (id, description, [events]). We snapshot AFTER every event AND at
# the initial (no-event) state, so the resulting PNGs cover the full navigation transcript.
SCENARIOS: list[tuple[str, str, list[str]]] = [
    ("s01-initial",
     "Default focus: Deny (least-privilege); no input yet.",
     []),
    ("s02-focus-once",
     "One dpad-right: focus moves to Allow once.",
     ["dpad-right"]),
    ("s03-focus-always",
     "Two dpad-right: focus moves to Allow always.",
     ["dpad-right", "dpad-right"]),
    ("s04-focus-right-wrap-guard",
     "Three dpad-right: focus stays at Allow always (no wrap).",
     ["dpad-right", "dpad-right", "dpad-right"]),
    ("s05-focus-left-wrap-guard",
     "From initial, one dpad-left: focus stays at Deny (no wrap).",
     ["dpad-left"]),
    ("s06-commit-deny",
     "A pressed on default focus (Deny) — commit deny.",
     ["A"]),
    ("s07-commit-allow-once",
     "dpad-right x1, A — commit allow-once.",
     ["dpad-right", "A"]),
    ("s08-commit-allow-always",
     "dpad-right x2, A — commit allow-always.",
     ["dpad-right", "dpad-right", "A"]),
    ("s09-cancel-from-initial",
     "B from initial focus — cancelled, collapses to Deny.",
     ["B"]),
    ("s10-cancel-from-allow-always",
     "dpad-right x2 (focus Allow always), B — cancelled, collapses to Deny.",
     ["dpad-right", "dpad-right", "B"]),
    ("s11-ignored-inputs",
     "dpad-up, X, Y, L1, L2 all no-op; then A — commit Deny (focus never moved).",
     ["dpad-up", "X", "Y", "L1", "L2", "A"]),
    ("s12-post-commit-events-no-op",
     "Commit Deny then dpad-right — later dpad no-ops (dialog dismissed).",
     ["A", "dpad-right", "dpad-right"]),
]

APP_NAME = "Weather"
RESOURCE = "LOCATION"
PURPOSE = "show nearby weather"


def run_render(state: DialogState, out_ppm: str) -> None:
    if not os.path.exists(BIN):
        sys.exit(f"driver: prototype not built: {BIN} (run: make)")
    cmd = [BIN,
           "--app-name", APP_NAME,
           "--resource", RESOURCE,
           "--purpose", PURPOSE,
           "--focus", state.focus,
           "--state", state.state,
           "--out", out_ppm]
    subprocess.run(cmd, check=True)


def ppm_to_png(ppm: str, png: str) -> None:
    subprocess.run([sys.executable, PPM2PNG, ppm, png], check=True)


def main() -> int:
    ap = argparse.ArgumentParser(description="tsp-ht0p.1 consent-ui state-machine driver")
    ap.add_argument("--outdir", default=os.path.join(HERE, "build", "frames"),
                    help="where per-state PPMs+PNGs land")
    ap.add_argument("--json", default=None,
                    help="write a machine-readable transcript here")
    args = ap.parse_args()

    os.makedirs(args.outdir, exist_ok=True)
    transcript = {"generated_at": int(time.time()), "scenarios": []}

    for sid, sdesc, events in SCENARIOS:
        s = DialogState()
        for ev in events:
            s = s.apply(ev)
        ppm = os.path.join(args.outdir, f"{sid}.ppm")
        png = os.path.join(args.outdir, f"{sid}.png")
        run_render(s, ppm)
        ppm_to_png(ppm, png)

        row = {
            "id": sid,
            "description": sdesc,
            "events": events,
            "final_focus": s.focus,
            "final_state": s.state,
            "decision": s.decision(),
            "event_log": s.events,
            "ppm": ppm,
            "png": png,
        }
        transcript["scenarios"].append(row)
        print(f"[{sid}] events={events!r} focus={s.focus} state={s.state} "
              f"decision={s.decision()!r} -> {os.path.basename(png)}", flush=True)

    if args.json:
        with open(args.json, "w") as f:
            json.dump(transcript, f, indent=2)
        print(f"driver: wrote transcript -> {args.json}", flush=True)

    return 0


if __name__ == "__main__":
    sys.exit(main())
