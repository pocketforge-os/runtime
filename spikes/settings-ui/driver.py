#!/usr/bin/env python3
"""driver.py — tsp-xubv.3 gamepad settings-UI state-machine driver.

Encodes the settings-UI navigation grammar (DESIGN.md §1 — the consent-ui
DESIGN.md §2 idiom ROTATED onto the vertical row axis) as a small pure-Python
state machine, drives it through the scripted scenarios below for BOTH device
profiles, and invokes ./build/settings-render at each state to dump a PPM. Then
converts each PPM to PNG via ppm2png.py so the screen-reviewer verifier
(review-screen.sh) can read it.

It ALSO emits an *authority-write intent* transcript: the ordered list of
(key, value) writes the navigation produced (every A-toggle and L/R-adjust).
liveness/ (the Rust harness) replays those same intents against the REAL merged
pf-prefs store + pocketforge observer to prove the writes ride the authority
path and flip live behavior — so the nav grammar (here) and the store→observer
liveness (Rust) are joined by one shared transcript, not two separate stories.

Verification of the PNGs is EXPLICITLY not done here (same contract as
consent-ui/driver.py): this driver only produces PNGs + the transcript;
run-settings-ui.sh routes each PNG through review-screen.sh so image pixels
never enter this process's or any bd/pf-comms transcript.
"""
from __future__ import annotations

import argparse
import dataclasses
import json
import os
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
BIN = os.path.join(HERE, "build", "settings-render")
PPM2PNG = os.path.join(HERE, "ppm2png.py")

# Schema order (matches pf_prefs::SCHEMA). `absent_on` marks presence read from the
# E1 descriptor — a133 has NO rumble actuator, so hapticsEnabled is honest-absent there.
BOOL_KEYS = ("reduceMotion", "hapticsEnabled", "monoAudio")
ALL_ROWS = ("reduceMotion", "hapticsEnabled", "monoAudio", "brightness")
ABSENT_ON = {"a133": {"hapticsEnabled"}, "a523": set()}
BRIGHTNESS_STEP = 10
SCHEMA_DEFAULTS = {"reduceMotion": False, "hapticsEnabled": True, "monoAudio": False,
                   "brightness": 100}

IGNORED = ("X", "Y", "L1", "L2", "R1", "R2", "lstick", "rstick",
           "home", "menu", "select", "start")


# ---------- navigation state machine (DESIGN.md §1 — rotated consent §2 idiom) ----------
@dataclasses.dataclass
class SettingsState:
    profile: str = "a523"
    reduceMotion: bool = SCHEMA_DEFAULTS["reduceMotion"]
    hapticsEnabled: bool = SCHEMA_DEFAULTS["hapticsEnabled"]
    monoAudio: bool = SCHEMA_DEFAULTS["monoAudio"]
    brightness: int = SCHEMA_DEFAULTS["brightness"]
    focus_i: int = 0                 # index into present_rows()
    exited: bool = False             # B pressed -> screen dismissed (terminal)
    events: list[str] = dataclasses.field(default_factory=list)
    # authority-write intents produced by this run, in order: {"key","value","via"}
    writes: list[dict] = dataclasses.field(default_factory=list)

    def present_rows(self) -> tuple[str, ...]:
        """Focus stops = schema rows MINUS honest-absent ones (not focus stops)."""
        absent = ABSENT_ON.get(self.profile, set())
        return tuple(k for k in ALL_ROWS if k not in absent)

    def focus_key(self) -> str:
        return self.present_rows()[self.focus_i]

    def _record_write(self, key: str, value, via: str) -> None:
        self.writes.append({"key": key, "value": value, "via": via})

    def apply(self, event: str) -> "SettingsState":
        s = dataclasses.replace(self, events=list(self.events),
                                writes=[dict(w) for w in self.writes])
        # terminal after B: the screen is dismissed, later inputs no-op (consent §2
        # "terminal-after-commit", rotated: settings' terminal input is B/back).
        if s.exited:
            s.events.append(event + " (no-op: screen dismissed)")
            return s

        rows = s.present_rows()
        if event == "dpad-down":
            # DESIGN.md §1: non-wrap guard at the bottom row (rotated consent wrap guard)
            s.focus_i = min(s.focus_i + 1, len(rows) - 1)
            s.events.append(event)
        elif event == "dpad-up":
            s.focus_i = max(s.focus_i - 1, 0)   # non-wrap guard at the top row
            s.events.append(event)
        elif event == "A":
            key = s.focus_key()
            if key in BOOL_KEYS:
                newv = not getattr(s, key)
                setattr(s, key, newv)
                s._record_write(key, newv, "A-toggle")
                s.events.append(f"A -> toggle {key}={newv}")
            else:  # brightness row: A is a no-op (adjust with L/R), stays consistent
                s.events.append("A (no-op on scalar row; use L/R)")
        elif event in ("dpad-left", "dpad-right"):
            key = s.focus_key()
            if key == "brightness":
                delta = BRIGHTNESS_STEP if event == "dpad-right" else -BRIGHTNESS_STEP
                # clamp to [0,100] — same non-wrap clamp philosophy as focus guards
                newv = max(0, min(100, s.brightness + delta))
                if newv != s.brightness:
                    s.brightness = newv
                    s._record_write("brightness", newv, "LR-adjust")
                    s.events.append(f"{event} -> brightness={newv}")
                else:
                    s.events.append(f"{event} (brightness clamped at {s.brightness})")
            else:  # L/R is a no-op on boolean rows
                s.events.append(event + " (no-op on bool row)")
        elif event == "B":
            s.exited = True
            s.events.append("B -> back (screen dismissed)")
        elif event in IGNORED:
            s.events.append(event + " (no-op)")   # DESIGN.md §1 "Inputs NOT used"
        else:
            raise ValueError(f"unknown event {event!r}")
        return s


# ---------- scripted scenarios ----------
# (id, profile, description, [events]) — snapshot is taken at the FINAL state of each.
SCENARIOS: list[tuple[str, str, str, list[str]]] = [
    # --- a523: full list present ---
    ("s01-a523-initial", "a523",
     "Default focus = top row (Reduce motion); schema defaults.", []),
    ("s02-a523-focus-haptics", "a523",
     "One dpad-down: focus Haptics (row index 1).", ["dpad-down"]),
    ("s03-a523-toggle-haptics-off", "a523",
     "Focus Haptics, A -> haptics OFF (authority write).",
     ["dpad-down", "A"]),
    ("s04-a523-toggle-reduce-motion-on", "a523",
     "A on top row -> reduceMotion ON.", ["A"]),
    ("s05-a523-toggle-mono-on", "a523",
     "Two dpad-down: focus Mono audio (index 2), A -> monoAudio ON.",
     ["dpad-down", "dpad-down", "A"]),
    ("s06-a523-focus-brightness", "a523",
     "Focus the brightness scalar row.",
     ["dpad-down", "dpad-down", "dpad-down"]),
    ("s07-a523-brightness-down", "a523",
     "Focus brightness, six L presses -> 40.",
     ["dpad-down", "dpad-down", "dpad-down",
      "dpad-left", "dpad-left", "dpad-left", "dpad-left", "dpad-left", "dpad-left"]),
    ("s08-a523-brightness-up-clamp", "a523",
     "Focus brightness, R at max -> clamped at 100 (non-wrap).",
     ["dpad-down", "dpad-down", "dpad-down", "dpad-right"]),
    ("s09-a523-focus-up-wrap-guard", "a523",
     "From top row, dpad-up -> stays on Reduce motion (no wrap).", ["dpad-up"]),
    ("s10-a523-focus-down-wrap-guard", "a523",
     "Five dpad-down -> focus clamps at Brightness (no wrap).",
     ["dpad-down", "dpad-down", "dpad-down", "dpad-down", "dpad-down"]),
    ("s11-a523-ignored-inputs", "a523",
     "X, Y, L1, home all no-op; focus unmoved.",
     ["X", "Y", "L1", "home"]),
    ("s12-a523-back-then-noop", "a523",
     "Focus Mono (2 down), A -> mono ON, B -> back; later dpad no-ops (dismissed).",
     ["dpad-down", "dpad-down", "A", "B", "dpad-down", "dpad-down"]),
    # --- a133: Haptics honest-absent (not a focus stop) ---
    ("s13-a133-initial-haptics-absent", "a133",
     "a133: Haptics row greyed 'unavailable'; focus = Reduce motion.", []),
    ("s14-a133-focus-skips-haptics", "a133",
     "One dpad-down -> focus jumps PAST absent Haptics to Mono audio.",
     ["dpad-down"]),
    ("s15-a133-toggle-mono-on", "a133",
     "a133: focus Mono audio, A -> monoAudio ON (Haptics still absent).",
     ["dpad-down", "A"]),
    ("s16-a133-brightness-down", "a133",
     "a133: focus Brightness (2 downs past absent Haptics), three L -> 70.",
     ["dpad-down", "dpad-down", "dpad-left", "dpad-left", "dpad-left"]),
]


def render(state: SettingsState, out_ppm: str) -> None:
    if not os.path.exists(BIN):
        sys.exit(f"driver: prototype not built: {BIN} (run: make)")
    cmd = [BIN,
           "--profile", state.profile,
           "--reduce-motion", "on" if state.reduceMotion else "off",
           "--haptics", "on" if state.hapticsEnabled else "off",
           "--mono", "on" if state.monoAudio else "off",
           "--brightness", str(state.brightness),
           "--focus", state.focus_key(),
           "--out", out_ppm]
    subprocess.run(cmd, check=True)


def ppm_to_png(ppm: str, png: str) -> None:
    subprocess.run([sys.executable, PPM2PNG, ppm, png], check=True)


def main() -> int:
    ap = argparse.ArgumentParser(description="tsp-xubv.3 settings-ui state-machine driver")
    ap.add_argument("--outdir", default=os.path.join(HERE, "build", "frames"))
    ap.add_argument("--json", default=None, help="machine-readable render transcript")
    ap.add_argument("--writes-json", default=None,
                    help="authority-write intent transcript for the Rust liveness harness")
    args = ap.parse_args()

    os.makedirs(args.outdir, exist_ok=True)
    transcript = {"generated_at": int(time.time()), "scenarios": []}

    for sid, profile, sdesc, events in SCENARIOS:
        s = SettingsState(profile=profile)
        for ev in events:
            s = s.apply(ev)
        ppm = os.path.join(args.outdir, f"{sid}.ppm")
        png = os.path.join(args.outdir, f"{sid}.png")
        render(s, ppm)
        ppm_to_png(ppm, png)

        row = {
            "id": sid, "profile": profile, "description": sdesc, "events": events,
            "focus": s.focus_key(), "exited": s.exited,
            "values": {"reduceMotion": s.reduceMotion, "hapticsEnabled": s.hapticsEnabled,
                       "monoAudio": s.monoAudio, "brightness": s.brightness},
            "writes": s.writes, "event_log": s.events, "ppm": ppm, "png": png,
        }
        transcript["scenarios"].append(row)
        print(f"[{sid}] profile={profile} focus={s.focus_key()} "
              f"writes={len(s.writes)} -> {os.path.basename(png)}", flush=True)

    if args.json:
        with open(args.json, "w") as f:
            json.dump(transcript, f, indent=2)
        print(f"driver: wrote render transcript -> {args.json}", flush=True)

    if args.writes_json:
        # The liveness harness consumes the a523 haptics + brightness write sequence
        # (the only rows whose live effect is asserted); it is the union of all
        # scenario writes, deduped-in-order per (scenario) is unnecessary — the harness
        # drives its OWN canonical liveness sequence and uses this file only to assert
        # the nav grammar actually PRODUCES authority writes with the expected shape.
        flat = []
        for sc in transcript["scenarios"]:
            for w in sc["writes"]:
                flat.append({"scenario": sc["id"], "profile": sc["profile"], **w})
        allwrites = {"generated_at": int(time.time()), "writes": flat}
        with open(args.writes_json, "w") as f:
            json.dump(allwrites, f, indent=2)
        print(f"driver: wrote {len(flat)} authority-write intents -> {args.writes_json}",
              flush=True)

    return 0


if __name__ == "__main__":
    sys.exit(main())
