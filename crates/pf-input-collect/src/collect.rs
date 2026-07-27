//! The **collection engine** — the prompt state machine that walks a [`plan`], records the raw
//! evdev events each prompt produces, classifies binary-vs-analog triggers from observed
//! behaviour, and emits a candidate [`emit::Capabilities`].
//!
//! Two ways to drive it:
//!  - **Programmatic** (what the on-panel UI `tsp-bwrg.3` uses): the UI owns its own event pump
//!    (it is rendering the skin and reading the node anyway); it calls [`Collector::prompt`] to
//!    show the ask, feeds events with [`Collector::record`], then [`Collector::commit_current`]
//!    + [`Collector::advance`] (or [`Collector::back`]), and finally [`Collector::emit`].
//!  - **Headless CLI** ([`run`]): owns the pump itself against any [`EventSource`], for the
//!    `pf-input-collect` binary AND for the synthetic-stream unit tests.

use std::collections::HashMap;
use std::io::{self, Write};
use std::time::{Duration, Instant};

use crate::codes::{self, EV_ABS, EV_KEY};
use crate::emit::{self, Axis, Capabilities};
use crate::plan::{ControlSpec, Kind};
use crate::source::{AbsInfo, EventSource, Identity, RawEvent};

/// Binary vs analog, classified from OBSERVED behaviour (not the declared range).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Semantics {
    /// Only endpoint values ever observed — a switch on an analog wire (the a133 L2/R2 quirk).
    Binary,
    /// Intermediate travel observed — a genuine proportional axis.
    Analog,
}

impl Semantics {
    pub fn as_str(self) -> &'static str {
        match self {
            Semantics::Binary => "binary",
            Semantics::Analog => "analog",
        }
    }
}

/// What was recorded for one control.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Recorded {
    Button { code: u16 },
    Hat { x_code: u16, y_code: u16 },
    /// `x`/`y` carry the driver-declared `AbsInfo` (for fuzz/flat/resolution); `x_cal`/`y_cal`
    /// carry the OBSERVED `(min, max, centre)` measured from the live sweep — the real per-axis
    /// calibration envelope emitted into the axis row (`min`/`max` = observed travel, `value` = rest).
    Stick { x_code: u16, x: AbsInfo, y_code: u16, y: AbsInfo, x_cal: (i32, i32, i32), y_cal: (i32, i32, i32) },
    Trigger { code: u16, abs: AbsInfo, semantics: Semantics },
    /// A trigger that manifests as a single EV_KEY button — a binary switch on the wire, never an
    /// analog axis (the a133 L2/R2: the MCU reports them as a bit in the button bitmask, and the
    /// decoder emits `BTN_TL2`/`BTN_TR2`, per `tsp-ozbp.2` + the owner-verified decoder output).
    /// It is a `trigger` by intent but a button on the wire, so it emits an `EV_KEY` row carrying
    /// `semantics="binary"` (the exact `kind=trigger` + button-code shape caps.py already maps to
    /// SDL `lefttrigger`/`righttrigger`).
    TriggerButton { code: u16 },
    /// One D-PAD direction's captured hat axis (`HAT0X`/`HAT0Y`). INTERNAL — the four direction
    /// captures are MERGED at emit into the single `hat` row (`ABS_HAT0X,ABS_HAT0Y`); a `HatAxis`
    /// is never emitted directly, so the collected map is unchanged by the four-step prompt UX.
    HatAxis { code: u16 },
}

/// The result of committing the current control.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommitOutcome {
    Captured(Recorded),
    /// The control is `optional` and produced no activity — its row is omitted (missing hardware
    /// = row omission, never a fabricated row).
    Skipped,
}

/// An error from the engine.
#[derive(Debug)]
pub enum CollectError {
    /// A REQUIRED control produced no usable activity.
    NoActivity { id: String },
    /// Activity was seen but was incomplete/ambiguous for the control's kind.
    Incomplete { id: String, reason: String },
    /// Reading `EVIOCGABS` for an axis failed.
    AbsInfo { code: u16, source: io::Error },
    /// An observed code has no schema-valid name, so it cannot be emitted.
    UnknownCode { ev_type: u16, code: u16 },
    /// No control has been captured yet, so there is nothing to emit.
    Empty,
}

impl std::fmt::Display for CollectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CollectError::NoActivity { id } => write!(f, "no activity recorded for required control '{id}'"),
            CollectError::Incomplete { id, reason } => write!(f, "incomplete capture for '{id}': {reason}"),
            CollectError::AbsInfo { code, source } => write!(f, "EVIOCGABS(0x{code:x}) failed: {source}"),
            CollectError::UnknownCode { ev_type, code } => {
                write!(f, "observed code (ev_type=0x{ev_type:x}, code=0x{code:x}) has no schema name")
            }
            CollectError::Empty => write!(f, "nothing captured yet"),
        }
    }
}

impl std::error::Error for CollectError {}

/// The operator-supplied identity a guided run stamps onto the candidate (the wizard asks these
/// up front — a new device's manufacturer/model/id are human knowledge, not observable from a pad).
#[derive(Clone, Debug)]
pub struct DeviceMeta {
    /// `identity.id` — schema pattern `^[a-z0-9]+$`.
    pub id: String,
    pub manufacturer: String,
    pub model: String,
}

#[derive(Clone, Debug)]
struct Slot {
    spec: ControlSpec,
    recorded: Option<Recorded>,
    skipped: bool,
}

/// The guided-collection state machine.
pub struct Collector {
    slots: Vec<Slot>,
    idx: usize,
    working: Vec<RawEvent>,
}

impl Collector {
    /// Build a collector over an ordered prompt plan.
    pub fn new(plan: Vec<ControlSpec>) -> Collector {
        let slots = plan
            .into_iter()
            .map(|spec| Slot { spec, recorded: None, skipped: false })
            .collect();
        Collector { slots, idx: 0, working: Vec::new() }
    }

    /// The control the engine is currently prompting for (`None` once past the end).
    pub fn current(&self) -> Option<&ControlSpec> {
        self.slots.get(self.idx).map(|s| &s.spec)
    }

    /// The human prompt for the current control.
    pub fn prompt(&self) -> Option<&str> {
        self.current().map(|c| c.prompt.as_str())
    }

    /// `(current_index, total_controls)` — for a progress readout.
    pub fn position(&self) -> (usize, usize) {
        (self.idx, self.slots.len())
    }

    /// True once every control has been visited.
    pub fn is_done(&self) -> bool {
        self.idx >= self.slots.len()
    }

    /// Feed events observed for the CURRENT control (append to the working buffer). The UI calls
    /// this as events arrive; the CLI [`run`] loop fills it via the pump.
    pub fn record(&mut self, events: &[RawEvent]) {
        self.working.extend_from_slice(events);
    }

    /// Discard the working buffer for the current control (e.g. the user wants to retry a press).
    pub fn clear_working(&mut self) {
        self.working.clear();
    }

    /// Finalize the current control from its working buffer. Does NOT advance — call
    /// [`Collector::advance`] next. Needs the source for `EVIOCGABS` on axis controls.
    pub fn commit_current(
        &mut self,
        src: &mut dyn EventSource,
    ) -> Result<CommitOutcome, CollectError> {
        let spec = match self.slots.get(self.idx) {
            Some(s) => s.spec.clone(),
            None => return Err(CollectError::Empty),
        };
        let events = std::mem::take(&mut self.working);
        let outcome = finalize(&spec, &events, src);
        match outcome {
            Ok(rec) => {
                self.slots[self.idx].recorded = Some(rec.clone());
                self.slots[self.idx].skipped = false;
                Ok(CommitOutcome::Captured(rec))
            }
            Err(CollectError::NoActivity { .. }) if spec.optional => {
                self.slots[self.idx].recorded = None;
                self.slots[self.idx].skipped = true;
                Ok(CommitOutcome::Skipped)
            }
            Err(e) => Err(e),
        }
    }

    /// Advance to the next control (clears the working buffer).
    pub fn advance(&mut self) {
        if self.idx < self.slots.len() {
            self.idx += 1;
        }
        self.working.clear();
    }

    /// Step back to the previous control, dropping its recorded capture so it can be redone.
    pub fn back(&mut self) {
        if self.idx > 0 {
            self.idx -= 1;
        }
        self.slots[self.idx].recorded = None;
        self.slots[self.idx].skipped = false;
        self.working.clear();
    }

    /// The recorded capture for a control id, if any (for a UI to render progress).
    pub fn recorded(&self, id: &str) -> Option<&Recorded> {
        self.slots.iter().find(|s| s.spec.id == id).and_then(|s| s.recorded.as_ref())
    }

    /// Build the candidate descriptor from everything captured so far.
    pub fn emit(&self, src: &mut dyn EventSource, meta: &DeviceMeta) -> Result<Capabilities, CollectError> {
        let ident: Identity = src.identity().map_err(|e| CollectError::AbsInfo { code: 0, source: e })?;
        let mut inputs = Vec::new();
        let mut has_south = false;

        // The four dpad-direction (`HatDir`) captures MERGE into ONE `hat` row (`ABS_HAT0X,ABS_HAT0Y`),
        // emitted at the position of the first direction — so the four-step prompt UX leaves the
        // collected map identical to a single hat control (tsp-bwrg.6).
        let dpad_axes: std::collections::BTreeSet<u16> = self
            .slots
            .iter()
            .filter_map(|s| match &s.recorded {
                Some(Recorded::HatAxis { code }) => Some(*code),
                _ => None,
            })
            .collect();
        let mut dpad_emitted = false;
        for slot in &self.slots {
            let rec = match &slot.recorded {
                Some(r) => r,
                None => continue, // pending or skipped → row omitted
            };
            if slot.spec.id == "south" {
                has_south = true;
            }
            if let Recorded::HatAxis { .. } = rec {
                if !dpad_emitted {
                    inputs.push(merged_dpad_row(&dpad_axes)?);
                    dpad_emitted = true;
                }
                continue; // subsequent dpad directions fold into the one row already emitted
            }
            inputs.push(input_row(&slot.spec, rec)?);
        }

        if inputs.is_empty() {
            return Err(CollectError::Empty);
        }

        let identity = emit::Identity {
            id: meta.id.clone(),
            manufacturer: meta.manufacturer.clone(),
            model: meta.model.clone(),
            sdl_guid: emit::sdl_guid(ident.bus, ident.vid, ident.pid, ident.version),
            evdev_name: ident.name.clone(),
            vid: Some(format!("{:04x}", ident.vid)),
            pid: Some(format!("{:04x}", ident.pid)),
        };

        Ok(Capabilities {
            accept_default: has_south.then(|| "south".to_string()),
            identity,
            screens: vec![emit::Screen::minimal_primary()],
            inputs,
        })
    }
}

/// Convert a source `AbsInfo` into an emit `Axis`.
fn to_axis(a: &AbsInfo) -> Axis {
    Axis { min: a.min, max: a.max, fuzz: a.fuzz, flat: a.flat, resolution: a.resolution, value: None }
}

/// Build an axis row from the driver-declared `AbsInfo` (for fuzz/flat/resolution) overlaid with an
/// observed `(min, max, centre)` calibration envelope: min/max become the measured travel and
/// `value` records the measured rest/centre.
fn measured_axis(a: &AbsInfo, cal: (i32, i32, i32)) -> Axis {
    let (min, max, centre) = cal;
    Axis { min, max, fuzz: a.fuzz, flat: a.flat, resolution: a.resolution, value: Some(centre) }
}

/// The `ABS_HAT*` axis codes (`ABS_HAT0X`=0x10 … `ABS_HAT3Y`=0x17) — the kernel's D-PAD convention.
///
/// Distinguishing hat axes from proportional ones is what keeps a control of one class from being
/// completed by another class's actuation: a thumbstick brushed on the way to the D-PAD must not
/// satisfy a dpad prompt, and a set of dpad presses must not satisfy a stick prompt (tsp-bwrg.12).
pub fn is_hat_axis(code: u16) -> bool {
    (0x10..=0x17).contains(&code)
}

/// Whether a control has been actuated ENOUGH to complete. A 1-axis control needs only activity
/// (the caller already gates on `saw_activity`). A 2-axis control (a stick) needs BOTH axes swept
/// near their declared extremes in BOTH directions — a real full circular sweep, not a quarter-
/// roll: each seen axis must have reached within 30% of its declared min AND its declared max.
/// This both captures the full min/max calibration envelope AND stops the completion window closing
/// on a brief mid-roll centre-transit — the tsp-bwrg.6 defect where a stick completed after ~0.25s
/// and the remainder of the roll cascaded into (and falsely satisfied) later controls.
pub fn axes_fully_swept(
    seen: &std::collections::HashSet<u16>,
    span: &HashMap<u16, (i32, i32)>,
    declared: &HashMap<u16, AbsInfo>,
    need_axes: usize,
) -> bool {
    if need_axes < 2 {
        return true;
    }
    let full = seen
        .iter()
        .filter(|&&c| match (span.get(&c), declared.get(&c)) {
            (Some(&(lo, hi)), Some(ai)) => {
                let s = (ai.max - ai.min).max(1);
                let tol = s * 3 / 10;
                lo <= ai.min + tol && hi >= ai.max - tol
            }
            _ => false,
        })
        .count();
    full >= need_axes
}

/// Observed per-axis calibration envelope from a control's captured events: the real travel
/// extremes the user reached (min/max) and the rest/centre position (median — the continuous
/// a133 stream sits at rest for most frames, punctuated by brief excursions, so the median of an
/// axis's samples is its resting value). Falls back to the declared range if the axis never
/// appeared (should not happen for an axis finalize already ruled active).
fn axis_calibration(events: &[RawEvent], code: u16, declared: &AbsInfo) -> (i32, i32, i32) {
    let mut vals: Vec<i32> = events
        .iter()
        .filter(|e| e.ev_type == EV_ABS && e.code == code)
        .map(|e| e.value)
        .collect();
    if vals.is_empty() {
        return (declared.min, declared.max, (declared.min + declared.max) / 2);
    }
    let min = *vals.iter().min().unwrap();
    let max = *vals.iter().max().unwrap();
    vals.sort_unstable();
    let centre = vals[vals.len() / 2];
    (min, max, centre)
}

/// Build one `[[inputs]]` row from a recorded capture.
fn input_row(spec: &ControlSpec, rec: &Recorded) -> Result<emit::Input, CollectError> {
    let unknown_key = |c: u16| CollectError::UnknownCode { ev_type: EV_KEY, code: c };
    let unknown_abs = |c: u16| CollectError::UnknownCode { ev_type: EV_ABS, code: c };
    Ok(match rec {
        Recorded::Button { code } => emit::Input {
            id: spec.id.clone(),
            kind: spec.kind.as_str().to_string(),
            ev_type: "EV_KEY".to_string(),
            code: codes::key_name(*code).ok_or_else(|| unknown_key(*code))?.to_string(),
            semantics: None,
            range: None,
            x: None,
            y: None,
        },
        Recorded::Hat { x_code, y_code } => emit::Input {
            id: spec.id.clone(),
            kind: spec.kind.as_str().to_string(),
            ev_type: "EV_ABS".to_string(),
            code: format!(
                "{},{}",
                codes::abs_name(*x_code).ok_or_else(|| unknown_abs(*x_code))?,
                codes::abs_name(*y_code).ok_or_else(|| unknown_abs(*y_code))?
            ),
            semantics: None,
            range: None,
            x: None,
            y: None,
        },
        Recorded::Stick { x_code, x, y_code, y, x_cal, y_cal } => emit::Input {
            id: spec.id.clone(),
            kind: spec.kind.as_str().to_string(),
            ev_type: "EV_ABS".to_string(),
            code: format!(
                "{},{}",
                codes::abs_name(*x_code).ok_or_else(|| unknown_abs(*x_code))?,
                codes::abs_name(*y_code).ok_or_else(|| unknown_abs(*y_code))?
            ),
            semantics: None,
            range: None,
            // Observed calibration envelope: min/max = measured travel, value = measured rest/centre.
            x: Some(measured_axis(x, *x_cal)),
            y: Some(measured_axis(y, *y_cal)),
        },
        Recorded::Trigger { code, abs, semantics } => emit::Input {
            id: spec.id.clone(),
            kind: spec.kind.as_str().to_string(),
            ev_type: "EV_ABS".to_string(),
            code: codes::abs_name(*code).ok_or_else(|| unknown_abs(*code))?.to_string(),
            semantics: Some(semantics.as_str().to_string()),
            range: Some(to_axis(abs)),
            x: None,
            y: None,
        },
        Recorded::TriggerButton { code } => emit::Input {
            id: spec.id.clone(),
            // `kind` stays the control's INTENT (`trigger`); the EV_KEY code + `semantics=binary`
            // record that this trigger is realized as a button on the wire. caps.py accepts a
            // `kind=trigger` EV_KEY BTN_* row (SDL `lefttrigger:bN`) and keeps `semantics` valid
            // (semantics is only meaningful on kind=trigger) — no analog `range` to emit.
            kind: spec.kind.as_str().to_string(),
            ev_type: "EV_KEY".to_string(),
            code: codes::key_name(*code).ok_or_else(|| unknown_key(*code))?.to_string(),
            semantics: Some(Semantics::Binary.as_str().to_string()),
            range: None,
            x: None,
            y: None,
        },
        Recorded::HatAxis { .. } => unreachable!(
            "HatDir captures are merged into the hat row in Collector::emit, never emitted via input_row"
        ),
    })
}

/// Merge the four D-PAD direction (`HatDir`) captures into the single `hat` input row. The four
/// directions collectively actuate both hat axes (HAT0X from left/right, HAT0Y from up/down); this
/// emits ONE `EV_ABS` row `ABS_HAT0X,ABS_HAT0Y` — exactly the shape a single hat control produced,
/// so the collected map + gatediff vs the ground truth are unchanged by the four-step prompt UX.
fn merged_dpad_row(axes: &std::collections::BTreeSet<u16>) -> Result<emit::Input, CollectError> {
    let codes: Vec<u16> = axes.iter().copied().collect();
    if codes.len() < 2 {
        return Err(CollectError::Incomplete {
            id: "dpad".to_string(),
            reason: "the four D-PAD directions did not actuate both hat axes".to_string(),
        });
    }
    // BTreeSet iterates ascending: HAT0X (0x10) < HAT0Y (0x11).
    let (x_code, y_code) = (codes[0], codes[1]);
    Ok(emit::Input {
        id: "dpad".to_string(),
        kind: "hat".to_string(),
        ev_type: "EV_ABS".to_string(),
        code: format!(
            "{},{}",
            codes::abs_name(x_code).ok_or(CollectError::UnknownCode { ev_type: EV_ABS, code: x_code })?,
            codes::abs_name(y_code).ok_or(CollectError::UnknownCode { ev_type: EV_ABS, code: y_code })?
        ),
        semantics: None,
        range: None,
        x: None,
        y: None,
    })
}

/// The per-axis observed extent + whether intermediate travel was visited (jitter-deduped).
struct AxisObs {
    max: i32,
    visited_intermediate: bool,
}

/// Accumulate one axis's observed values with the same jitter dedupe evdev-probe.py uses
/// (delta = max(1, (declared_max - declared_min) / 64)), then note whether any deduped value
/// landed away from BOTH endpoints (declared range's endpoints, with an endpoint tolerance).
fn observe_axis(values: &[i32], declared: &AbsInfo) -> AxisObs {
    let span = (declared.max - declared.min).max(1);
    let jitter = (span / 64).max(1);
    let endpoint_tol = (span / 16).max(1);
    let mut last: Option<i32> = None;
    let mut hi = i32::MIN;
    let mut visited_intermediate = false;
    for &v in values {
        // jitter dedupe: skip values within `jitter` of the last COUNTED value (unless a new peak).
        if let Some(l) = last {
            if (v - l).abs() < jitter && v <= hi {
                continue;
            }
        }
        last = Some(v);
        hi = hi.max(v);
        let near_min = (v - declared.min).abs() <= endpoint_tol;
        let near_max = (v - declared.max).abs() <= endpoint_tol;
        if !near_min && !near_max {
            visited_intermediate = true;
        }
    }
    AxisObs { max: hi, visited_intermediate }
}

/// Turn a control's working events into a `Recorded` (or a `NoActivity`/`Incomplete` error).
fn finalize(
    spec: &ControlSpec,
    events: &[RawEvent],
    src: &mut dyn EventSource,
) -> Result<Recorded, CollectError> {
    match spec.kind {
        Kind::Button | Kind::StickClick => {
            // First key-down.
            let code = events
                .iter()
                .find(|e| e.ev_type == EV_KEY && e.value == 1)
                .map(|e| e.code)
                .ok_or_else(|| CollectError::NoActivity { id: spec.id.clone() })?;
            Ok(Recorded::Button { code })
        }
        Kind::Hat => {
            // A hat records HAT axes only — a swept thumbstick is not a D-PAD (tsp-bwrg.12).
            let axes: Vec<u16> =
                active_abs_codes(events, src).into_iter().filter(|&c| is_hat_axis(c)).collect();
            if axes.is_empty() {
                return Err(CollectError::NoActivity { id: spec.id.clone() });
            }
            if axes.len() < 2 {
                return Err(CollectError::Incomplete {
                    id: spec.id.clone(),
                    reason: format!(
                        "a hat needs two axes; saw only {}",
                        codes::abs_name(axes[0]).unwrap_or("?")
                    ),
                });
            }
            // Lowest code is the X axis (HAT0X=0x10 < HAT0Y=0x11), highest is Y.
            Ok(Recorded::Hat { x_code: axes[0], y_code: axes[1] })
        }
        Kind::HatDir => {
            // ONE dpad direction: a single HAT axis actuated. Record WHICH hat axis (merged into the
            // single hat row at emit).
            //
            // Only a real `ABS_HAT*` axis counts. The old `.or_else(|| axes.first())` fallback
            // recorded whatever else happened to be moving — a thumbstick brushed on the way across
            // to the D-PAD — as the direction, which is how a STICK axis ended up inside the emitted
            // D-PAD row (tsp-bwrg.12). When no hat axis actuated there is nothing honest to record:
            // report `NoActivity` so the wizard re-prompts, rather than fabricate a row. Same
            // never-fabricate bar as the ambient-rest-stream case (tsp-bwrg.6).
            let code = active_abs_codes(events, src).into_iter().find(|&c| is_hat_axis(c));
            match code {
                Some(code) => Ok(Recorded::HatAxis { code }),
                None => Err(CollectError::NoActivity { id: spec.id.clone() }),
            }
        }
        Kind::Stick => {
            // A stick records PROPORTIONAL axes only — a set of D-PAD presses drives both hat axes
            // to both extremes and would otherwise rank as a stick (tsp-bwrg.12).
            let axes: Vec<u16> =
                active_abs_codes(events, src).into_iter().filter(|&c| !is_hat_axis(c)).collect();
            if axes.is_empty() {
                return Err(CollectError::NoActivity { id: spec.id.clone() });
            }
            if axes.len() < 2 {
                return Err(CollectError::Incomplete {
                    id: spec.id.clone(),
                    reason: "a stick needs two axes; sweep it fully in a circle".to_string(),
                });
            }
            // Two most-active axes (largest observed span), then ordered x=lower code, y=higher.
            let mut ranked = rank_axes_by_span(events, &axes);
            ranked.truncate(2);
            ranked.sort_unstable();
            let (x_code, y_code) = (ranked[0], ranked[1]);
            let x = src.absinfo(x_code).map_err(|e| CollectError::AbsInfo { code: x_code, source: e })?;
            let y = src.absinfo(y_code).map_err(|e| CollectError::AbsInfo { code: y_code, source: e })?;
            let x_cal = axis_calibration(events, x_code, &x);
            let y_cal = axis_calibration(events, y_code, &y);
            Ok(Recorded::Stick { x_code, x, y_code, y, x_cal, y_cal })
        }
        Kind::Trigger => {
            // A trigger manifests one of two ways, and the a133 is the second:
            //  (1) an ANALOG axis (`ABS_Z`/`ABS_RZ`) that travels to a full press — a genuine
            //      proportional trigger (or an analog-wire switch that only hits its endpoints); or
            //  (2) a single EV_KEY BUTTON (`BTN_TL2`/`BTN_TR2`) — the a133 L2/R2, which the MCU
            //      reports as a binary bit and the decoder emits as a button (tsp-ozbp.2 + the
            //      owner-verified decoder output). There is NO analog value on the wire for it.
            // Prefer an axis ONLY if one actually reached a full press; otherwise a resting
            // neighbour axis (e.g. the same-side stick still streaming its centre value while the
            // owner squeezes the trigger) must NOT be mistaken for the trigger — fall back to the
            // key. This ordering is what makes case (2) robust against live stick crosstalk.
            let key_down = events
                .iter()
                .find(|e| e.ev_type == EV_KEY && e.value == 1)
                .map(|e| e.code);
            // Proportional axes only: a D-PAD press caught in the window is not a trigger axis.
            let axes: Vec<u16> =
                active_abs_codes(events, src).into_iter().filter(|&c| !is_hat_axis(c)).collect();
            if !axes.is_empty() {
                let code = rank_axes_by_span(events, &axes)[0];
                let abs = src.absinfo(code).map_err(|e| CollectError::AbsInfo { code, source: e })?;
                let vals: Vec<i32> =
                    events.iter().filter(|e| e.ev_type == EV_ABS && e.code == code).map(|e| e.value).collect();
                let obs = observe_axis(&vals, &abs);
                let span = (abs.max - abs.min).max(1);
                let reached_press = obs.max >= abs.max - (span / 16).max(1);
                if reached_press {
                    let semantics =
                        if obs.visited_intermediate { Semantics::Analog } else { Semantics::Binary };
                    return Ok(Recorded::Trigger { code, abs, semantics });
                }
                // Axis activity but no real press → not the trigger axis (crosstalk). Fall through.
            }
            // Case (2): a binary trigger realized as a button.
            if let Some(code) = key_down {
                return Ok(Recorded::TriggerButton { code });
            }
            if axes.is_empty() {
                return Err(CollectError::NoActivity { id: spec.id.clone() });
            }
            Err(CollectError::Incomplete {
                id: spec.id.clone(),
                reason: "trigger never reached a full press — squeeze it fully".to_string(),
            })
        }
    }
}

/// Fraction (of an axis's declared range) that a value must be off the axis MIDPOINT by to count
/// as a real actuation rather than rest jitter. The a133 decoder streams the sticks CONTINUOUSLY
/// at their (non-zero, near-midpoint) rest, so a plain `value != 0` filter treats a resting stick
/// as active — which mis-attributes the dpad hat to the stick axes (tsp-bwrg.6). Judging activity
/// by midpoint-relative deviation instead separates a resting stick (~1-2% off midpoint) from a
/// pressed hat (±full = 100% of its range) or a swept stick (~50-100%), with NO need for a captured
/// baseline. 15% is comfortably above stick rest jitter and well below any real actuation.
const ACTIVE_DEV_NUM: i64 = 15;
const ACTIVE_DEV_DEN: i64 = 100;

/// True if a single ABS value sits more than `ACTIVE_DEV` of the axis range off its midpoint —
/// i.e. a real actuation, not rest jitter. Public so the on-panel wizard's pump
/// (`pf-collect-ui`) applies the identical continuous-stream-safe activity test as this engine.
pub fn abs_value_is_active(value: i32, ai: &AbsInfo) -> bool {
    let mid = (ai.min as i64 + ai.max as i64) / 2;
    let range = ((ai.max as i64) - (ai.min as i64)).max(1);
    ((value as i64) - mid).abs() * ACTIVE_DEV_DEN >= range * ACTIVE_DEV_NUM
}

/// True if any EV_ABS event for `code` was really actuated (not just streaming at rest).
fn abs_is_active(code: u16, events: &[RawEvent], ai: &AbsInfo) -> bool {
    events
        .iter()
        .filter(|e| e.ev_type == EV_ABS && e.code == code)
        .any(|e| abs_value_is_active(e.value, ai))
}

/// The EV_ABS codes that were really ACTUATED (midpoint-deviation past the activity threshold),
/// sorted ascending. Reads each candidate axis's `EVIOCGABS` range via `src`; a resting
/// continuously-streaming stick is correctly excluded. Replaces the old `value != 0` filter.
fn active_abs_codes(events: &[RawEvent], src: &mut dyn EventSource) -> Vec<u16> {
    let mut codes: Vec<u16> = events.iter().filter(|e| e.ev_type == EV_ABS).map(|e| e.code).collect();
    codes.sort_unstable();
    codes.dedup();
    codes
        .into_iter()
        .filter(|&c| matches!(src.absinfo(c), Ok(ai) if abs_is_active(c, events, &ai)))
        .collect()
}

/// The candidate abs codes ordered by observed value span (largest first) — used to pick the
/// real stick/trigger axes and ignore crosstalk from an idle neighbour.
fn rank_axes_by_span(events: &[RawEvent], candidates: &[u16]) -> Vec<u16> {
    let mut spans: Vec<(u16, i32)> = candidates
        .iter()
        .map(|&c| {
            let mut lo = i32::MAX;
            let mut hi = i32::MIN;
            for e in events.iter().filter(|e| e.ev_type == EV_ABS && e.code == c) {
                lo = lo.min(e.value);
                hi = hi.max(e.value);
            }
            (c, (hi - lo).max(0))
        })
        .collect();
    spans.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    spans.into_iter().map(|(c, _)| c).collect()
}

// -------------------------------------------------------------------------------------------
// Headless CLI run loop (also the synthetic-stream test driver).
// -------------------------------------------------------------------------------------------

/// Pump/timing knobs (expressed in POLL UNITS so behaviour is deterministic in tests and
/// bounded in wall-clock via `poll_step`).
#[derive(Clone, Copy, Debug)]
pub struct RunConfig {
    /// How long one `poll()` blocks (real device); ignored by the scripted source.
    pub poll_step: Duration,
    /// Consecutive polls with NO active event AFTER activity that mean "the sweep/press settled".
    pub quiet_polls: usize,
    /// Consecutive inactive polls from the start before an OPTIONAL control is auto-skipped.
    pub idle_skip_polls: usize,
    /// Wall-clock cap per control — the real bound. Iteration counts are unreliable on a device
    /// that STREAMS continuously (the a133 pad ~48fps), where `poll()` never blocks and a fixed
    /// poll count burns through in a few seconds (tsp-bwrg.6). A generous wall-clock window gives a
    /// human time to react on every control.
    pub control_timeout: Duration,
    /// Polls discarded in the gap BETWEEN two controls — a FIXED dead-time window, not a
    /// drain-until-quiet (see [`drain_between_controls`] for why the distinction matters).
    pub drain_polls: usize,
    /// Infinite-loop guard (NOT the timing bound — that is `control_timeout`).
    pub max_polls: usize,
}

impl Default for RunConfig {
    fn default() -> RunConfig {
        RunConfig {
            poll_step: Duration::from_millis(50),
            quiet_polls: 3,
            idle_skip_polls: 40,
            control_timeout: Duration::from_secs(45), // generous per-control window (err long)
            // ~400ms of dead time between controls: long enough to cover the beat-then-re-press a
            // person produces when unsure a press registered, and the backlog the device buffers
            // while the next prompt renders; short enough that the next prompt still feels
            // immediate. Deliberately expressed in POLLS, not wall-clock: against a source whose
            // `poll()` returns instantly (the scripted test source) a wall-clock drain would spin
            // through the entire script.
            drain_polls: 8,
            max_polls: 100_000, // pure runaway guard
        }
    }
}

/// Read (and cache) an axis's driver-declared `EVIOCGABS` range. A code the source cannot describe
/// degrades to an all-zero range, which the significance test then reads as "never active" — the
/// safe direction (an undescribable axis cannot complete a control).
fn declared_abs(cache: &mut HashMap<u16, AbsInfo>, code: u16, src: &mut dyn EventSource) -> AbsInfo {
    if let Some(a) = cache.get(&code) {
        return *a;
    }
    let a = src.absinfo(code).unwrap_or_default();
    cache.insert(code, a);
    a
}

/// What a control's capture window should do next — the output of the one completion policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Not enough has been actuated yet. Keep the window open.
    Open,
    /// The control is structurally complete. Close the window and finalize it.
    Complete,
    /// An OPTIONAL control that was never actuated at all. Close the window; its row is omitted.
    Skip,
}

/// Whether the accumulated actuation SATISFIES the control's kind — the coverage half of the
/// policy, deliberately separate from the settle half.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Coverage {
    /// Nothing this kind of control could legitimately be has been actuated yet.
    None,
    /// Satisfied, but the input is a SWEEP that must be recorded to its end — hold the window until
    /// it settles, so the whole envelope lands in this control and its tail cannot bleed onward.
    Settling,
    /// Satisfied by a DISCRETE actuation. Nothing further can be learned by waiting.
    Complete,
}

/// One control's capture window: the events recorded for it, plus the structural evidence the
/// completion policy is judged on.
///
/// # This is the ONE completion policy — do not inline a second copy (tsp-bwrg.12)
///
/// Both pumps drive this type: the headless engine ([`run`]) and the on-panel wizard
/// (`pf_collect_ui::wizard::drive_live`, which owns its own render loop but must not own its own
/// completion rules). The two previously carried separate hand-kept-in-sync copies of the same
/// logic, which is this codebase's characteristic defect class — duplicated truth with no drift
/// check. If a caller ever genuinely needs different rules, add them HERE behind an explicit knob;
/// do not fork the loop.
///
/// # The invariant: quiet NEVER completes a control
///
/// Completion is judged from WHAT WAS ACTUATED, never from how long input has been quiet. Quiet is
/// only ever:
///   - a SETTLE condition applied AFTER [`Coverage::Settling`] is already reached (so a sweep is
///     recorded to its end), or
///   - the trigger for [`Verdict::Skip`] on an OPTIONAL control that was never actuated at all —
///     which omits the row, and is the one outcome quiet may cause. It never CAPTURES anything.
///
/// Quiet-driven completion was the shared root cause of every defect across five owner-attended
/// collection passes: a control "completed" the moment input went quiet, so a pause mid-actuation
/// mis-completed it and a fast overshoot bled into the next control. tsp-bwrg.6 fixed it for the
/// STICK class; this type generalizes the fix to every class.
pub struct Window {
    kind: Kind,
    optional: bool,
    events: Vec<RawEvent>,
    /// A key-DOWN was observed (any `EV_KEY` value 1).
    key_down: bool,
    /// `EV_ABS` codes observed at a SIGNIFICANT (midpoint-relative) deviation — i.e. really
    /// actuated, as opposed to a continuously-streaming axis sitting at rest.
    actuated: std::collections::HashSet<u16>,
    /// Observed `(min, max)` per `EV_ABS` code across the whole window.
    span: HashMap<u16, (i32, i32)>,
    /// `EVIOCGABS` cache.
    declared: HashMap<u16, AbsInfo>,
    saw_actuation: bool,
    quiet: usize,
}

impl Window {
    /// Open a capture window for one control.
    pub fn new(spec: &ControlSpec) -> Window {
        Window {
            kind: spec.kind,
            optional: spec.optional,
            events: Vec::new(),
            key_down: false,
            actuated: std::collections::HashSet::new(),
            span: HashMap::new(),
            declared: HashMap::new(),
            saw_actuation: false,
            quiet: 0,
        }
    }

    /// Feed one poll's worth of events. Returns whether THIS poll carried a real actuation of this
    /// control's kind — a caller can use it to show a "CAPTURING…" state — and maintains the quiet
    /// run the settle/skip conditions read.
    pub fn observe(&mut self, evs: &[RawEvent], src: &mut dyn EventSource) -> bool {
        let mut actuated_now = false;
        for e in evs {
            if e.ev_type == EV_KEY && e.value == 1 {
                self.key_down = true;
            }
            if e.ev_type == EV_ABS {
                let ai = declared_abs(&mut self.declared, e.code, src);
                let ent = self.span.entry(e.code).or_insert((e.value, e.value));
                if e.value < ent.0 {
                    ent.0 = e.value;
                }
                if e.value > ent.1 {
                    ent.1 = e.value;
                }
                if abs_value_is_active(e.value, &ai) {
                    self.actuated.insert(e.code);
                }
            }
            if poll_event_active(self.kind, e, src, &mut self.declared) {
                actuated_now = true;
            }
            self.events.push(*e);
        }
        if actuated_now {
            self.saw_actuation = true;
            self.quiet = 0;
        } else {
            self.quiet += 1;
        }
        actuated_now
    }

    /// The completion verdict for this window so far.
    pub fn verdict(&self, cfg: &RunConfig) -> Verdict {
        match self.coverage() {
            Coverage::Complete => Verdict::Complete,
            Coverage::Settling => {
                if self.quiet >= cfg.quiet_polls {
                    Verdict::Complete
                } else {
                    Verdict::Open
                }
            }
            // The ONLY thing quiet alone may decide, and it decides to record NOTHING.
            Coverage::None => {
                if self.optional && !self.saw_actuation && self.quiet >= cfg.idle_skip_polls {
                    Verdict::Skip
                } else {
                    Verdict::Open
                }
            }
        }
    }

    /// The events recorded for this control.
    pub fn events(&self) -> &[RawEvent] {
        &self.events
    }

    /// Consume the window, yielding its recorded events.
    pub fn into_events(self) -> Vec<RawEvent> {
        self.events
    }

    /// THE PER-CLASS COMPLETION PREDICATE. Every control class is gated on ACTUATION or COVERAGE;
    /// none is gated on quiet.
    fn coverage(&self) -> Coverage {
        match self.kind {
            // A press is discrete: the key-down IS the whole actuation.
            Kind::Button | Kind::StickClick => {
                if self.key_down { Coverage::Complete } else { Coverage::None }
            }
            // Two shapes on the wire. A binary trigger realized as a BUTTON (the a133 L2/R2, which
            // the MCU reports as a bit and the decoder emits as `BTN_TL2`/`BTN_TR2`) is discrete. A
            // genuinely ANALOG trigger must actually REACH a full press — a partial squeeze followed
            // by a human pause used to close the window at partial travel, and `finalize` then
            // failed the whole run with "never reached a full press" (tsp-bwrg.12).
            Kind::Trigger => {
                if self.key_down {
                    Coverage::Complete
                } else if self.axis_reached_full_press() {
                    Coverage::Settling
                } else {
                    Coverage::None
                }
            }
            // ONE dpad direction: a HAT axis must have actuated. Not "something moved" — a brushed
            // thumbstick is not a D-PAD press (tsp-bwrg.12). Settling, not Complete, so the return
            // to centre is recorded inside this control rather than leaking into the next one.
            Kind::HatDir => {
                if self.actuated.iter().any(|&c| is_hat_axis(c)) {
                    Coverage::Settling
                } else {
                    Coverage::None
                }
            }
            // A lumped hat: BOTH hat axes swept to both extremes.
            Kind::Hat => {
                if self.class_axes_fully_swept(true, 2) { Coverage::Settling } else { Coverage::None }
            }
            // A stick: BOTH proportional axes swept to both extremes — a real full circle, not a
            // quarter-roll (tsp-bwrg.6). Hat axes are excluded so a set of D-PAD presses, which
            // drives both hat axes to both extremes, cannot satisfy a stick prompt.
            Kind::Stick => {
                if self.class_axes_fully_swept(false, 2) { Coverage::Settling } else { Coverage::None }
            }
        }
    }

    /// Whether `need` axes OF THIS CLASS (hat vs proportional) have each been swept to within 30%
    /// of both declared extremes. Shares [`axes_fully_swept`]'s sweep maths — one implementation.
    fn class_axes_fully_swept(&self, hat: bool, need: usize) -> bool {
        let axes: std::collections::HashSet<u16> =
            self.actuated.iter().copied().filter(|&c| is_hat_axis(c) == hat).collect();
        axes.len() >= need && axes_fully_swept(&axes, &self.span, &self.declared, need)
    }

    /// Whether some proportional axis actually reached a FULL press — the same endpoint test
    /// `finalize` applies to a trigger, so the window closes exactly when finalize would accept it.
    fn axis_reached_full_press(&self) -> bool {
        self.actuated.iter().filter(|&&c| !is_hat_axis(c)).any(|c| {
            match (self.span.get(c), self.declared.get(c)) {
                (Some(&(_, hi)), Some(ai)) => {
                    let span = (ai.max - ai.min).max(1);
                    hi >= ai.max - (span / 16).max(1)
                }
                _ => false,
            }
        })
    }
}

/// Discard input in the gap BETWEEN two controls, so a tail from control N cannot enter control
/// N+1's window (tsp-bwrg.12).
///
/// Without this the wizard commits control N and opens control N+1's window in the same breath, so
/// whatever the device emits in between — a "did that take?" re-press, an overshoot past the
/// prompted direction, the backlog buffered while the next prompt renders — is the first thing the
/// NEXT control sees, and can satisfy it outright. The next control then records the PREVIOUS
/// control's input and the wizard advances past a control the owner never actuated. It is silent:
/// the emitted map can still look plausible while every capture is shifted by one.
///
/// Discarding is safe BY CONSTRUCTION because of WHERE this runs: after a control commits and
/// BEFORE the next prompt is shown. The owner has not been asked for anything yet, so anything
/// arriving in this interval is by definition a tail of the control just finished, never an answer
/// to the question about to be asked.
///
/// It is a FIXED dead-time window (`cfg.drain_polls` polls, unconditionally), NOT a drain-until-
/// quiet. That distinction is the whole point and was got wrong first: the realistic tail is
/// QUIET-THEN-INPUT, not a contiguous burst. A person presses, WAITS to see whether the prompt
/// advanced, decides it did not, and presses again — so a drain that stops at the first couple of
/// quiet polls stops in the pause and hands the re-press straight to the next control, i.e. it
/// exits precisely before the thing it exists to absorb. A fixed window has no such hole, needs no
/// significance heuristic, and is trivially predictable.
///
/// Returns how many polls it consumed.
pub fn drain_between_controls<S: EventSource>(src: &mut S, cfg: &RunConfig) -> io::Result<usize> {
    for _ in 0..cfg.drain_polls {
        let _discarded = src.poll(cfg.poll_step)?;
    }
    Ok(cfg.drain_polls)
}

/// Whether one event counts as a real actuation of a control of `kind` — used by the pump to drive
/// activity/settle. A button/stick-click actuates on a key-DOWN. A hat/stick/trigger actuates on a
/// SIGNIFICANT abs deviation (midpoint-relative, so the a133's continuous at-rest stick stream does
/// NOT read as activity) OR, for a trigger, a key-down (the a133 L2/R2 button-triggers).
fn poll_event_active(
    kind: Kind,
    e: &RawEvent,
    src: &mut dyn EventSource,
    cache: &mut HashMap<u16, AbsInfo>,
) -> bool {
    match kind {
        Kind::Button | Kind::StickClick => e.ev_type == EV_KEY && e.value == 1,
        Kind::Hat | Kind::Stick | Kind::Trigger | Kind::HatDir => {
            if e.ev_type == EV_KEY {
                return e.value == 1;
            }
            if e.ev_type != EV_ABS {
                return false;
            }
            let ai = match cache.get(&e.code) {
                Some(a) => a.clone(),
                None => {
                    let a = src.absinfo(e.code).unwrap_or(AbsInfo {
                        min: 0, max: 0, fuzz: 0, flat: 0, resolution: 0,
                    });
                    cache.insert(e.code, a.clone());
                    a
                }
            };
            abs_value_is_active(e.value, &ai)
        }
    }
}

/// Pump one control's events off the source until its [`Window`] reaches a terminal [`Verdict`].
/// Bounded by a WALL-CLOCK window (`control_timeout`) — robust to a continuously-streaming device —
/// with `max_polls` as a pure runaway guard.
///
/// The completion RULES live entirely in [`Window`] (the one policy both pumps share); this loop
/// only supplies polls and honours the verdict.
fn pump<S: EventSource>(src: &mut S, spec: &ControlSpec, cfg: &RunConfig) -> io::Result<Vec<RawEvent>> {
    let mut window = Window::new(spec);
    let deadline = Instant::now() + cfg.control_timeout;
    let mut iters = 0usize;

    while Instant::now() < deadline && iters < cfg.max_polls {
        iters += 1;
        let evs = src.poll(cfg.poll_step)?;
        window.observe(&evs, src);
        match window.verdict(cfg) {
            Verdict::Open => {}
            Verdict::Complete | Verdict::Skip => break,
        }
    }
    Ok(window.into_events())
}

/// Run the full guided sequence headlessly against `src`, writing prompts/results to `out`, and
/// return the emitted candidate. This is the CLI's engine AND the synthetic-stream test driver.
pub fn run<S: EventSource, W: Write>(
    collector: &mut Collector,
    src: &mut S,
    meta: &DeviceMeta,
    cfg: &RunConfig,
    out: &mut W,
) -> Result<Capabilities, CollectError> {
    while let Some(spec) = collector.current().cloned() {
        let (i, n) = collector.position();
        writeln!(out, "[{}/{}] {} — {}", i + 1, n, spec.id, spec.prompt).ok();
        let evs = pump(src, &spec, cfg).map_err(|e| CollectError::AbsInfo { code: 0, source: e })?;
        collector.record(&evs);
        match collector.commit_current(src)? {
            CommitOutcome::Captured(rec) => writeln!(out, "    recorded: {}", describe(&rec)).ok(),
            CommitOutcome::Skipped => writeln!(out, "    skipped (optional, no activity)").ok(),
        };
        collector.advance();
        // Discard this control's tail BEFORE the next prompt is written — see
        // `drain_between_controls` for why discarding here is safe by construction.
        if !collector.is_done() {
            drain_between_controls(src, cfg)
                .map_err(|e| CollectError::AbsInfo { code: 0, source: e })?;
        }
    }
    collector.emit(src, meta)
}

/// One-line human summary of a recorded capture.
fn describe(rec: &Recorded) -> String {
    match rec {
        Recorded::Button { code } => format!("button {}", codes::key_name(*code).unwrap_or("?")),
        Recorded::Hat { x_code, y_code } => format!(
            "hat {},{}",
            codes::abs_name(*x_code).unwrap_or("?"),
            codes::abs_name(*y_code).unwrap_or("?")
        ),
        Recorded::Stick { x_code, y_code, x_cal, y_cal, .. } => format!(
            "stick {}[{}..{} @{}],{}[{}..{} @{}]",
            codes::abs_name(*x_code).unwrap_or("?"), x_cal.0, x_cal.1, x_cal.2,
            codes::abs_name(*y_code).unwrap_or("?"), y_cal.0, y_cal.1, y_cal.2
        ),
        Recorded::Trigger { code, abs, semantics } => format!(
            "trigger {} [{}..{}] semantics={}",
            codes::abs_name(*code).unwrap_or("?"), abs.min, abs.max, semantics.as_str()
        ),
        Recorded::TriggerButton { code } => format!(
            "trigger {} (button, semantics=binary)",
            codes::key_name(*code).unwrap_or("?")
        ),
        Recorded::HatAxis { code } => format!("dpad direction {} (merges into the hat row)", codes::abs_name(*code).unwrap_or("?")),
    }
}
