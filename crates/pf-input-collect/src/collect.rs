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

use std::io::{self, Write};
use std::time::Duration;

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
    Stick { x_code: u16, x: AbsInfo, y_code: u16, y: AbsInfo },
    Trigger { code: u16, abs: AbsInfo, semantics: Semantics },
    /// A trigger that manifests as a single EV_KEY button — a binary switch on the wire, never an
    /// analog axis (the a133 L2/R2: the MCU reports them as a bit in the button bitmask, and the
    /// decoder emits `BTN_TL2`/`BTN_TR2`, per `tsp-ozbp.2` + the owner-verified decoder output).
    /// It is a `trigger` by intent but a button on the wire, so it emits an `EV_KEY` row carrying
    /// `semantics="binary"` (the exact `kind=trigger` + button-code shape caps.py already maps to
    /// SDL `lefttrigger`/`righttrigger`).
    TriggerButton { code: u16 },
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

        for slot in &self.slots {
            let rec = match &slot.recorded {
                Some(r) => r,
                None => continue, // pending or skipped → row omitted
            };
            if slot.spec.id == "south" {
                has_south = true;
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
    Axis { min: a.min, max: a.max, fuzz: a.fuzz, flat: a.flat, resolution: a.resolution }
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
        Recorded::Stick { x_code, x, y_code, y } => emit::Input {
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
            x: Some(to_axis(x)),
            y: Some(to_axis(y)),
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
            let axes = distinct_abs_codes(events);
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
        Kind::Stick => {
            let axes = distinct_abs_codes(events);
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
            Ok(Recorded::Stick { x_code, x, y_code, y })
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
            let axes = distinct_abs_codes(events);
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

/// The distinct EV_ABS codes that showed a non-zero value, sorted ascending.
fn distinct_abs_codes(events: &[RawEvent]) -> Vec<u16> {
    let mut codes: Vec<u16> = events
        .iter()
        .filter(|e| e.ev_type == EV_ABS && e.value != 0)
        .map(|e| e.code)
        .collect();
    codes.sort_unstable();
    codes.dedup();
    codes
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
    /// Consecutive empty polls AFTER activity that mean "the sweep/press has settled".
    pub quiet_polls: usize,
    /// Consecutive empty polls from the start with NO activity before an OPTIONAL control is
    /// auto-skipped.
    pub idle_skip_polls: usize,
    /// Hard cap on polls per control.
    pub max_polls: usize,
}

impl Default for RunConfig {
    fn default() -> RunConfig {
        RunConfig {
            poll_step: Duration::from_millis(50),
            quiet_polls: 3,
            idle_skip_polls: 40, // ~2s of quiet on a real device
            max_polls: 600,      // ~30s hard cap per control
        }
    }
}

fn is_relevant(kind: Kind, e: &RawEvent) -> bool {
    match kind {
        Kind::Button | Kind::StickClick => e.ev_type == EV_KEY,
        Kind::Hat | Kind::Stick => e.ev_type == EV_ABS,
        // A trigger may be an analog axis OR a binary button (a133 L2/R2 → BTN_TL2/BTN_TR2), so
        // EITHER an axis OR a key press counts as activity for it.
        Kind::Trigger => e.ev_type == EV_ABS || e.ev_type == EV_KEY,
    }
}

/// Pump one control's events off the source per the completion heuristic for its kind.
fn pump<S: EventSource>(src: &mut S, spec: &ControlSpec, cfg: &RunConfig) -> io::Result<Vec<RawEvent>> {
    let mut buf: Vec<RawEvent> = Vec::new();
    let mut saw_activity = false;
    let mut empties = 0usize;

    for _ in 0..cfg.max_polls {
        let evs = src.poll(cfg.poll_step)?;
        if evs.is_empty() {
            empties += 1;
            if !saw_activity {
                if spec.optional && empties >= cfg.idle_skip_polls {
                    break; // optional & idle → skip
                }
                // required: keep waiting (bounded by max_polls)
            } else if empties >= cfg.quiet_polls {
                break; // activity then quiet → settled
            }
            continue;
        }
        empties = 0;
        for e in evs {
            if is_relevant(spec.kind, &e) {
                saw_activity = true;
            }
            buf.push(e);
        }
        // A button/click completes the instant a key-down is seen — and so does a trigger that
        // turns out to be a binary button (the a133 L2/R2 → BTN_TL2/BTN_TR2). A genuinely analog
        // trigger emits no key, so this never short-circuits its sweep.
        if matches!(spec.kind, Kind::Button | Kind::StickClick | Kind::Trigger)
            && buf.iter().any(|e| e.ev_type == EV_KEY && e.value == 1)
        {
            break;
        }
    }
    Ok(buf)
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
        Recorded::Stick { x_code, x, y_code, y } => format!(
            "stick {}[{}..{}],{}[{}..{}]",
            codes::abs_name(*x_code).unwrap_or("?"), x.min, x.max,
            codes::abs_name(*y_code).unwrap_or("?"), y.min, y.max
        ),
        Recorded::Trigger { code, abs, semantics } => format!(
            "trigger {} [{}..{}] semantics={}",
            codes::abs_name(*code).unwrap_or("?"), abs.min, abs.max, semantics.as_str()
        ),
        Recorded::TriggerButton { code } => format!(
            "trigger {} (button, semantics=binary)",
            codes::key_name(*code).unwrap_or("?")
        ),
    }
}
