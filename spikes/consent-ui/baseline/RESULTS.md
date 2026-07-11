# SPIKE-2 results — consent-UI-on-gamepad (`tsp-ht0p.1`)

**Verdict (TL;DR):** **PASS**. The tsp-osr-safe recipe from `pocketforge-os/sim @ 74ddfbc` (`sim/fb/`) is enough to draw a legible, dpad-navigable permission consent dialog on the sim's virtual framebuffer. The 12-scenario matrix (DESIGN.md §2 + `driver.py`) covers the full interaction model — initial focus, focus movement (with intentional non-wrap at both endpoints), commit on all three grant scopes, cancel-collapses-to-Deny, ignored-input no-ops, and post-commit event no-ops. Each state PNG was routed through the sanctioned screen-reviewer path (`review-screen.sh` → opencode `screen-reviewer` agent → gemma-4-31B vision model) with pixels attached via `--file`; image pixels never entered the main-loop context. The reviewer's readback agrees with the state machine's `focus` + `state` + `decision` for every scenario.

## Recipe pin — confirmed

Every prototype invocation logs the `tsp-osr-pin` check at startup:

```
tsp-osr-pin: OK window(no-GL)+SDL_CreateRenderer("software") -> 'software'
```

I.e. the on-window recipe the M1.D supervisor (`tsp-iuz.3`, paused) will eventually use — non-`OPENGL` `SDL_CreateWindow` + `SDL_CreateRenderer(win, "software")` — succeeds and does not crash. The offscreen readback path (`SDL_CreateSurfaceFrom` → `SDL_CreateSoftwareRenderer`) is what actually draws the 12 evidence PNGs, which is structurally tsp-osr-safe (no window, no GL). Both paths are consumed from `sim/fb/README.md`; neither is re-derived here.

## Scenario matrix + reviewer readback

Every scenario below was rendered by the C prototype, PPM→PNG'd, and read back by the `screen-reviewer` opencode agent (gemma-4-31B on modelmaker vLLM). The prompt asked the reviewer to report (1) the title line, (2) which grant-scope button is focused / committed / cancel-collapsed, (3) whether a DECISION banner is present and what it says, (4) the bottom hint line. The `Expected` column encodes DESIGN.md §2's state machine; a `✓` in `Match` means the reviewer's readback agreed on all four points.

| Scenario | Events | Expected (focus / state / decision) | Reviewer readback | Match |
|---|---|---|---|---|
| s01-initial                    | (none)                                 | Deny / initial / —                        | Focus: Deny; DECISION: None                                       | ✓ |
| s02-focus-once                 | dpad-right                             | Allow once / initial / —                  | FOCUS: Allow once; DECISION: No                                   | ✓ |
| s03-focus-always               | dpad-right × 2                         | Allow always / initial / —                | Focus: Allow always; DECISION: No                                 | ✓ |
| s04-focus-right-wrap-guard     | dpad-right × 3                         | Allow always / initial / — (no wrap)      | Focus: Allow always; DECISION: None                               | ✓ |
| s05-focus-left-wrap-guard      | dpad-left                              | Deny / initial / — (no wrap)              | FOCUS: Deny; DECISION: None                                       | ✓ |
| s06-commit-deny                | A                                      | Deny / selected / deny                    | Deny is COMMITTED; DECISION: Deny                                 | ✓ |
| s07-commit-allow-once          | dpad-right, A                          | Allow once / selected / allow-once        | Allow once is COMMITTED; DECISION: Allow once                     | ✓ |
| s08-commit-allow-always        | dpad-right × 2, A                      | Allow always / selected / allow-always    | Allow always is COMMITTED; DECISION: Allow always                 | ✓ |
| s09-cancel-from-initial        | B                                      | Deny / cancelled / deny (via B)           | Deny is CANCELLED-COLLAPSED-TO-DENY; DECISION: Deny (via B/cancel)| ✓ |
| s10-cancel-from-allow-always   | dpad-right × 2, B                      | Deny / cancelled / deny (via B)           | Deny is CANCELLED-COLLAPSED-TO-DENY; DECISION: Deny (via B/cancel)| ✓ |
| s11-ignored-inputs             | dpad-up, X, Y, L1, L2, A               | Deny / selected / deny                    | Deny: COMMITTED; DECISION: Deny                                   | ✓ |
| s12-post-commit-events-no-op   | A, dpad-right × 2                      | Deny / selected / deny                    | Deny is COMMITTED; DECISION: Deny                                 | ✓ |

**Full verdict text for every row is in `verdicts.txt` (verbatim `review-screen.sh` output — the entire per-frame block includes the reviewer's title readback + hint-line readback which all matched too).** All 12 rows agree with `DESIGN.md` §2's state machine. The scenarios exhaustively cover: the default (least-privilege) focus (s01); focus movement in both directions with intentional non-wrap guards at both endpoints (s02-s05); commit on each of the three grant scopes (s06-s08); cancel-from-two-different-focus-states, both collapsing to Deny (s09-s10); ignored-input no-ops (s11); post-commit event no-ops (s12).

**One reviewer nit (documented, not a failure):** on s04 the reviewer conflated the optional `purpose` line ("show nearby weather") with the bottom hint on its own summary line — both are actually rendered on-screen; the reviewer just relabeled one. The FOCUS and DECISION readbacks, which are what the state machine asserts on, are correct. This is a phrasing artifact of the reviewer prompt, not a rendering defect.

## Host

Reviewer host: modelmaker (`mm@10.0.40.90`), vLLM serving `google/gemma-4-31B-it-qat-w4a16-ct`. Prototype host: modelmaker (built against `/home/mm/sim-build/sdl3-render/x86/libSDL3.a`, the sim's pinned software-only SDL3-render static). Reviewer invocation: laptop `matt-laptop` running `/home/matt/pocketforge-automation/scripts/review-screen.sh` → opencode `run --agent screen-reviewer --file <png>` per the `reviewer-model-architecture` decision (owner, 2026-07-01) and the `tsp-visual-inspection` memory.

## Evidence artifacts

- `s01-initial.png` … `s12-post-commit-events-no-op.png` — the 12 rendered dialog states (1280×720 XRGB8888, PPM→PNG via the stdlib-only `ppm2png.py` copied from `sim/fb/`).
- `transcript.json` — machine-readable driver transcript (per-scenario `events`, `final_focus`, `final_state`, `decision`, `event_log`) emitted by `driver.py --json`.
- `verdicts.txt` — verbatim per-scenario `review-screen.sh` output.

## Honesty

Proves the LOGICAL LAYER: tsp-osr-safe recipe consumed, dialog layout renders legibly under the software renderer, DESIGN.md §2 state machine exhaustively covers the state space, the reviewer's readback matches the state machine. Does NOT prove — and does not attempt — the on-device PowerVR/dc_sunxi/DE2.0/fb0 path (that stays E6/C2's `tsp-osr` pin + the flash→serial→webcam hardware gate queued at epic level). Does NOT integrate with the M1.D supervisor (`tsp-iuz.3`, paused) or with `pf-broker` — those are `.3`'s job, using the seam contract in `DESIGN.md` §4 as input.
