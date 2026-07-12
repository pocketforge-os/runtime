# tsp-xubv.3 settings-UI — screen-reviewer baseline RESULTS

Screen-reviewer verdicts (opencode `screen-reviewer` agent, gemma-4-31B vision on
modelmaker, via `review-screen.sh` — image pixels routed through `opencode --file`,
never `Read` into any context) for the 16-scenario render matrix produced by
`driver.py` (DESIGN.md §1 navigation grammar). Each PNG is committed alongside this
file; the render transcript is `transcript.json`, the authority-write intent
transcript is `writes.json`.

**Reviewer prompt:** transcribe the title, each row label + its value widget
(`[ON]`/`[OFF]`/brightness bar+number/`unavailable`), the highlighted (focused) row,
any greyed row, and the bottom device line — report only what is unambiguously
visible, invent nothing.

**Result:** every frame reads exactly as the state machine intended — default focus
on the top row; focus moves + non-wrap guards; `[ON]`/`[OFF]` toggles reflecting the
authority writes; the brightness bar+number; and — the presence half of the E4
unification — the **a133 Haptics row rendered `unavailable`/greyed and skipped by
focus**, with the device line reading `a133 (no rumble motor)`.

---

## s01-a523-initial

*Default focus = top row (Reduce motion); schema defaults.*  

`profile=a523  focus=reduceMotion  values={"reduceMotion": false, "hapticsEnabled": true, "monoAudio": false, "brightness": 100}`

```
Report:
- Title: `Settings`
- Settings rows:
    - `Reduce motion`: `[ OFF ]`
    - `Haptics`: `[ ON ]`
    - `Mono audio`: `[ OFF ]`
    - `Brightness`: `[ ########## ] 100`
- Selected row: `Reduce motion` (bright outline box)
- Greyed-out rows: None
- Bottom line: `Up/Down: move    A: toggle    L/R: adjust    B: back` / `device: a523`
- Status: Running app (Settings menu)
```

## s02-a523-focus-haptics

*One dpad-down: focus Haptics (row index 1).*  

`profile=a523  focus=hapticsEnabled  values={"reduceMotion": false, "hapticsEnabled": true, "monoAudio": false, "brightness": 100}`

```
- Title: `Settings`
- Settings:
    - `Reduce motion`: `[ OFF ]`
    - `Haptics`: `[ ON ]`
    - `Mono audio`: `[ OFF ]`
    - `Brightness`: `[ ########## ] 100`
- Selected Row: `Haptics`
- Greyed-out Rows: None
- Bottom Device Line: `device: a523`
- Image Path: `/tmp/claude-1000/-home-matt-mission-control/c3e4860d-4025-4a82-8712-86d3761b0787/scratchpad/frames/s02-a523-focus-haptics.png`
```

## s03-a523-toggle-haptics-off

*Focus Haptics, A -> haptics OFF (authority write).*  

`profile=a523  focus=hapticsEnabled  values={"reduceMotion": false, "hapticsEnabled": false, "monoAudio": false, "brightness": 100}`

```
Report:
- Title: Settings
- Rows:
    - Reduce motion: [ OFF ]
    - Haptics: [ OFF ]
    - Mono audio: [ OFF ]
    - Brightness: [ ########## ] 100
- Highlighted Row: Haptics
- Greyed-out Rows: None
- Bottom Line: device: a523
```

## s04-a523-toggle-reduce-motion-on

*A on top row -> reduceMotion ON.*  

`profile=a523  focus=reduceMotion  values={"reduceMotion": true, "hapticsEnabled": true, "monoAudio": false, "brightness": 100}`

```
Report:
- Title: Settings
- Rows:
    - Reduce motion: [ ON ]
    - Haptics: [ ON ]
    - Mono audio: [ OFF ]
    - Brightness: [##########] 100
- Highlighted row: Reduce motion
- Greyed-out rows: None
- Bottom text: Up/Down: move    A: toggle    L/R: adjust    B: back
  device: a523
- State: Running app (Settings menu)
```

## s05-a523-toggle-mono-on

*Two dpad-down: focus Mono audio (index 2), A -> monoAudio ON.*  

`profile=a523  focus=monoAudio  values={"reduceMotion": false, "hapticsEnabled": true, "monoAudio": true, "brightness": 100}`

```
Report:
- Title: `Settings`
- Settings rows (top-to-bottom):
    - `Reduce motion`: `[ OFF ]`
    - `Haptics`: `[ ON ]`
    - `Mono audio`: `[ ON ]`
    - `Brightness`: `[ ########### ] 100`
- Selected row: `Mono audio` (has a bright outline box)
- Greyed-out rows: None
- Bottom device line: `device: a523`
- Screen State: Running app (Settings menu)
- Thumbstick LEDs: Not visible
```

## s06-a523-focus-brightness

*Focus the brightness scalar row.*  

`profile=a523  focus=brightness  values={"reduceMotion": false, "hapticsEnabled": true, "monoAudio": false, "brightness": 100}`

```
- **Screen Name:** Settings
- **State:** Running app (Settings menu)
- **Content:**
    - Title: `Settings`
    - Row 1: `Reduce motion` [ OFF ]
    - Row 2: `Haptics` [ ON ]
    - Row 3: `Mono audio` [ OFF ]
    - Row 4: `Brightness` [ ########## ] 100
- **Highlighted Row:** `Brightness` (enclosed in a bright outline box)
- **Greyed-out/Dimmed Rows:** None
- **Bottom Text:** `Up/Down: move    A: toggle    L/R: adjust    B: back` / `device: a523`
- **LED Rings:** Not visible in frame.
```

## s07-a523-brightness-down

*Focus brightness, six L presses -> 40.*  

`profile=a523  focus=brightness  values={"reduceMotion": false, "hapticsEnabled": true, "monoAudio": false, "brightness": 40}`

```
- **Screen Name**: Settings
- **Content**:
    - Title: `Settings`
    - Row 1: `Reduce motion` : `[ OFF ]`
    - Row 2: `Haptics` : `[ ON ]`
    - Row 3: `Mono audio` : `[ OFF ]`
    - Row 4: `Brightness` : `[ ####- - - - - ] 40`
- **Highlighted Row**: `Brightness` (surrounded by a bright outline box)
- **Greyed-out Rows**: None visible.
- **Bottom Text**: `Up/Down: move   A: toggle   L/R: adjust   B: back`
- **Device Line**: `device: a523`
- **Status**: Running app (Settings menu)
- **LED Rings**: Not visible in frame.
```

## s08-a523-brightness-up-clamp

*Focus brightness, R at max -> clamped at 100 (non-wrap).*  

`profile=a523  focus=brightness  values={"reduceMotion": false, "hapticsEnabled": true, "monoAudio": false, "brightness": 100}`

```
- **Screen Name**: Settings
- **State**: Running app (Settings menu)
- **Content**:
    - Title: `Settings`
    - Row 1: `Reduce motion` [ OFF ]
    - Row 2: `Haptics` [ ON ]
    - Row 3: `Mono audio` [ OFF ]
    - Row 4: `Brightness` [ ########## ] 100
- **Selected Row**: `Brightness` (highlighted with a bright outline box)
- **Greyed-out Rows**: None
- **Bottom Text**: `Up/Down: move    A: toggle    L/R: adjust    B: back`
- **Device Line**: `device: a523`
- **LED Rings**: Not visible in frame.
```

## s09-a523-focus-up-wrap-guard

*From top row, dpad-up -> stays on Reduce motion (no wrap).*  

`profile=a523  focus=reduceMotion  values={"reduceMotion": false, "hapticsEnabled": true, "monoAudio": false, "brightness": 100}`

```
Report:
- Title: `Settings`
- Settings Rows:
    - `Reduce motion` : `[ OFF ]`
    - `Haptics` : `[ ON ]`
    - `Mono audio` : `[ OFF ]`
    - `Brightness` : `[ ########## ] 100`
- Highlighted Row: `Reduce motion`
- Greyed-out/Dimmed Rows: None
- Bottom Device Line: `device: a523`
- Status: Running app (Settings menu)
- Image Path: `/tmp/claude-1000/-home-matt-mission-control/c3e4860d-4025-4a82-8712-86d3761b0787/scratchpad/frames/s09-a523-focus-up-wrap-guard.png`
```

## s10-a523-focus-down-wrap-guard

*Five dpad-down -> focus clamps at Brightness (no wrap).*  

`profile=a523  focus=brightness  values={"reduceMotion": false, "hapticsEnabled": true, "monoAudio": false, "brightness": 100}`

```
Report:
- Title: Settings
- Settings rows:
  - Reduce motion: [ OFF ]
  - Haptics: [ ON ]
  - Mono audio: [ OFF ]
  - Brightness: [ ########## ] 100
- Highlighted row: Brightness
- Greyed-out rows: None
- Bottom text: device: a523
- State: Running app (Settings menu)
```

## s11-a523-ignored-inputs

*X, Y, L1, home all no-op; focus unmoved.*  

`profile=a523  focus=reduceMotion  values={"reduceMotion": false, "hapticsEnabled": true, "monoAudio": false, "brightness": 100}`

```
- **Screen Name:** Settings
- **Status:** Running app (Settings menu)
- **Title:** Settings
- **Settings Rows:**
    - Reduce motion: [ OFF ] (Highlighted/Selected)
    - Haptics: [ ON ]
    - Mono audio: [ OFF ]
    - Brightness: [ ########## ] 100
- **Highlighted Row:** Reduce motion
- **Greyed-out Rows:** None
- **Bottom Text:** Up/Down: move A: toggle L/R: adjust B: back
- **Bottom Device Line:** device: a523
- **LED Rings:** Not visible/off-screen.
```

## s12-a523-back-then-noop

*Focus Mono (2 down), A -> mono ON, B -> back; later dpad no-ops (dismissed).*  

`profile=a523  focus=monoAudio  values={"reduceMotion": false, "hapticsEnabled": true, "monoAudio": true, "brightness": 100}`

```
Report:
- Title: Settings
- Settings Rows:
    - Reduce motion: [ OFF ]
    - Haptics: [ ON ]
    - Mono audio: [ ON ]
    - Brightness: [ ########## ] 100
- Highlighted/Selected: Mono audio
- Greyed-out/Dimmed: None
- Bottom device line: device: a523
- Status: Running app (Settings menu)
- Image reviewed: /tmp/claude-1000/-home-matt-mission-control/c3e4860d-4025-4a82-8712-86d3761b0787/scratchpad/frames/s12-a523-back-then-noop.png
```

## s13-a133-initial-haptics-absent

*a133: Haptics row greyed 'unavailable'; focus = Reduce motion.*  

`profile=a133  focus=reduceMotion  values={"reduceMotion": false, "hapticsEnabled": true, "monoAudio": false, "brightness": 100}`

```
- **Title**: Settings
- **Settings Rows**:
    - Reduce motion: [ OFF ]
    - Haptics: unavailable
    - Mono audio: [ OFF ]
    - Brightness: [ ########## ] 100
- **Highlighted/Selected Row**: Reduce motion (enclosed in a bright outline box)
- **Greyed-out/Dimmed Row**: Haptics
- **Bottom Device Line**: device: a133 (no rumble motor)
- **Status**: Running app (Settings menu)
```

## s14-a133-focus-skips-haptics

*One dpad-down -> focus jumps PAST absent Haptics to Mono audio.*  

`profile=a133  focus=monoAudio  values={"reduceMotion": false, "hapticsEnabled": true, "monoAudio": false, "brightness": 100}`

```
- **Screen Name:** Settings
- **Status:** Running app (settings menu)
- **Text Transcription:**
    - Title: `Settings`
    - Row 1: `Reduce motion` -> `[ OFF ]`
    - Row 2: `Haptics` -> `unavailable`
    - Row 3: `Mono audio` -> `[ OFF ]`
    - Row 4: `Brightness` -> `[ ########## ] 100`
    - Bottom line: `Up/Down: move   A: toggle   L/R: adjust   B: back`
    - Device line: `device: a133 (no rumble motor)`
- **Highlighted Row:** `Mono audio` (surrounded by a bright outline box)
- **Greyed-out Rows:** `Haptics`
- **LED Rings:** Not visible in frame.
```

## s15-a133-toggle-mono-on

*a133: focus Mono audio, A -> monoAudio ON (Haptics still absent).*  

`profile=a133  focus=monoAudio  values={"reduceMotion": false, "hapticsEnabled": true, "monoAudio": true, "brightness": 100}`

```
Report:
- Title: `Settings`
- Settings rows:
    - `Reduce motion`: `[ OFF ]`
    - `Haptics`: `unavailable`
    - `Mono audio`: `[ ON ]`
    - `Brightness`: `[ ########## ] 100`
- Highlighted row: `Mono audio`
- Greyed-out/dimmed rows: `Haptics`
- Bottom device line: `device: a133 (no rumble motor)`
- State: Running app (Settings menu)
```

## s16-a133-brightness-down

*a133: focus Brightness (2 downs past absent Haptics), three L -> 70.*  

`profile=a133  focus=brightness  values={"reduceMotion": false, "hapticsEnabled": true, "monoAudio": false, "brightness": 70}`

```
Report:
- Title: `Settings`
- Rows:
    - `Reduce motion`: `[ OFF ]`
    - `Haptics`: `unavailable`
    - `Mono audio`: `[ OFF ]`
    - `Brightness`: `[ #######- - - ] 70`
- Selected row: `Brightness` (outlined in bright box)
- Greyed-out row: `Haptics`
- Bottom device line: `device: a133 (no rumble motor)`
- State: Running app (Settings menu)
- Thumbstick LEDs: Not visible
```
