//! The **input broker** — the load-bearing v0 enforcement: open the real evdev source,
//! `EVIOCGRAB` it (exclusive), and pump its events through the descriptor remap + the rate-limit
//! policy into a uinput re-emit device the app reads. The app gets the re-emit read fd via
//! `Acquire("input")` over a Unix socket (`SCM_RIGHTS`); it can no longer reach the real node.

use std::collections::{HashMap, HashSet};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use pf_input_decode::builtin::{Trust, Verdict};
use pf_wire::{recv_request, send_response, Op, Request, Response, Status};
use pocketforge::descriptor::Descriptor;
use pocketforge::server::handle_request;
use pocketforge::Backend;

use crate::evdev::Evdev;
use crate::io::{EventSink, EventSource, SourceState};
use crate::ioc;
use crate::policy::TokenBucket;
use crate::remap::{AbsAction, Posture, Remap};
use crate::safe_return::{SystemMenuGate, SystemMenuRouter, GUIDE_CODE};
use crate::scm;
use crate::uinput::Uinput;

const MAX_REPORT_EVENTS: usize = 256;
const ACQUIRE_CLIENT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

/// How the broker authenticates its source device.
///
/// Production callers use [`SourceIdentity::system`], which requires the decoder's virtual path,
/// `phys` value and trusted runtime record. Simulation and test callers must opt out by spelling
/// [`SourceIdentity::allow_unidentified`]; there is no implicit `None` policy.
#[derive(Debug, Clone)]
pub struct SourceIdentity {
    trust: Option<Trust>,
}

impl SourceIdentity {
    /// Require the decoder identity record in the production `/sys` and `/run` trees.
    pub fn system() -> SourceIdentity {
        SourceIdentity::require_builtin(Trust::system())
    }

    /// Require built-in identity according to an explicit trust source (for hermetic tests).
    pub fn require_builtin(trust: Trust) -> SourceIdentity {
        SourceIdentity { trust: Some(trust) }
    }

    /// Explicitly accept a descriptor-matching source without decoder identity.
    ///
    /// This is only for desktop simulation and tests. The daemon logs a warning when its CLI flag
    /// selects this policy.
    pub fn allow_unidentified() -> SourceIdentity {
        SourceIdentity { trust: None }
    }

    /// Whether decoder identity is enforced.
    pub fn is_enforcing(&self) -> bool {
        self.trust.is_some()
    }

    fn verify_fd(&self, fd: BorrowedFd<'_>) -> io::Result<()> {
        match self.trust.as_ref() {
            Some(trust) => require_builtin(trust.identify_fd(fd)),
            None => Ok(()),
        }
    }

    #[cfg(test)]
    fn verify_verdict(&self, verdict: io::Result<Verdict>) -> io::Result<()> {
        if self.is_enforcing() {
            require_builtin(verdict)
        } else {
            Ok(())
        }
    }
}

/// A source that can answer the `EVIOCGBIT` question the broker's capability check asks: does it
/// advertise every one of `codes` for `event_type`? [`Evdev`] answers from the opened fd; a
/// hermetic test answers from a device's uinput setup (`tsp-f3fm.217`), so the check the daemon
/// runs and the check the contract test runs are one function, [`missing_source_capabilities`].
pub trait SourceCapabilities {
    /// Whether every code in `codes` is advertised for `event_type` (`EV_KEY` / `EV_ABS`).
    fn supports(&self, event_type: u16, codes: &[u16]) -> io::Result<bool>;
}

impl SourceCapabilities for Evdev {
    fn supports(&self, event_type: u16, codes: &[u16]) -> io::Result<bool> {
        Evdev::supports(self, event_type, codes)
    }
}

/// The descriptor-required source codes a source does NOT advertise. Empty means the source
/// satisfies the descriptor.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MissingCapabilities {
    /// Required `EV_KEY` codes the source lacks.
    pub keys: Vec<u16>,
    /// Required `EV_ABS` codes the source lacks.
    pub abs: Vec<u16>,
}

impl MissingCapabilities {
    /// True when nothing required is missing.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.abs.is_empty()
    }
}

impl std::fmt::Display for MissingCapabilities {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let hex = |v: &[u16]| {
            v.iter()
                .map(|c| format!("{c:#x}"))
                .collect::<Vec<_>>()
                .join(",")
        };
        write!(
            f,
            "EV_KEY [{}] EV_ABS [{}]",
            hex(&self.keys),
            hex(&self.abs)
        )
    }
}

/// The broker's source capability check: every `EV_KEY`/`EV_ABS` code the descriptor's
/// non-system rows require ([`Remap::required_source_keys`] / [`Remap::required_source_abs`]),
/// minus what `source` advertises. `class = "system"` rows (VOL±, read from their own `source`
/// node) never enter the required set, so they can never make a pad source fail.
pub fn missing_source_capabilities(
    source: &impl SourceCapabilities,
    remap: &Remap,
) -> io::Result<MissingCapabilities> {
    let mut missing = MissingCapabilities::default();
    for &code in remap.required_source_keys() {
        if !source.supports(ioc::EV_KEY, &[code])? {
            missing.keys.push(code);
        }
    }
    for &code in remap.required_source_abs() {
        if !source.supports(ioc::EV_ABS, &[code])? {
            missing.abs.push(code);
        }
    }
    Ok(missing)
}

fn wire_err(e: pf_wire::WireError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

/// Read events from a raw evdev fd (non-blocking; 0 on `EAGAIN`). Used to read the handed/shared
/// app fd and to prove the grabbed source is silent.
pub fn read_events_raw(fd: RawFd, out: &mut [libc::input_event]) -> io::Result<usize> {
    let cap = std::mem::size_of_val(out);
    // SAFETY: out is a valid buffer of `cap` bytes.
    let n = unsafe { libc::read(fd, out.as_mut_ptr() as *mut libc::c_void, cap) };
    if n < 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EAGAIN) {
            return Ok(0);
        }
        return Err(e);
    }
    Ok(n as usize / std::mem::size_of::<libc::input_event>())
}

/// The grabbed source + re-emit sink + remap/policy. Owns the live devices; the grab is released
/// on drop.
pub struct InputBroker {
    source: Evdev,
    sink: Uinput,
    start: std::time::Instant,
    pump: ReportPump,
}

/// Configuration for the shared production/private event loop.
pub struct RunConfig<'a> {
    descriptor: Option<&'a Descriptor>,
    pump: Option<&'a mut ReportPump>,
    token_bucket: Option<TokenBucket>,
    start: std::time::Instant,
    stop: &'a AtomicBool,
}

impl<'a> RunConfig<'a> {
    /// Build a fresh descriptor-derived remap and policy pump for a private event stream.
    pub fn new(descriptor: &'a Descriptor, stop: &'a AtomicBool) -> Self {
        Self {
            descriptor: Some(descriptor),
            pump: None,
            token_bucket: None,
            start: std::time::Instant::now(),
            stop,
        }
    }

    /// Override the default report-rate policy, primarily for deterministic policy tests.
    pub fn with_token_bucket(mut self, token_bucket: TokenBucket) -> Self {
        self.token_bucket = Some(token_bucket);
        self
    }

    fn existing(pump: &'a mut ReportPump, start: std::time::Instant, stop: &'a AtomicBool) -> Self {
        Self {
            descriptor: None,
            pump: Some(pump),
            token_bucket: None,
            start,
            stop,
        }
    }
}

/// The device-free report pipeline: descriptor remap, rate-limit policy, report framing,
/// `SYN_DROPPED` resync and (with `--safe-return-sock`) the protected System Menu gate.
pub(crate) struct ReportPump {
    remap: Remap,
    /// The posture [`Remap::remap_axis_event`] re-expresses spatial axis events for.
    /// Production wiring is [`Posture::Normal`]: TODO(tsp-mc9m.41.1026.6) — the
    /// session orientation policy designs where the runtime posture comes from;
    /// do not invent a source here.
    posture: Posture,
    bucket: TokenBucket,
    pending_report: Vec<(u16, u16, i32)>,
    pending_report_oversized: bool,
    resynchronizing: bool,
    pressed: HashSet<u16>,
    abs_state: HashMap<u16, i32>,
    /// `None` (no `--safe-return-sock`) leaves the stream exactly as before: guide passes through.
    system_menu: Option<SystemMenuGate>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedIdentity {
    pub name: String,
    pub bus: u16,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
}

impl ExpectedIdentity {
    pub fn from_descriptor(descriptor: &Descriptor) -> io::Result<Self> {
        let m = descriptor.identity.r#match.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "descriptor identity.match is required",
            )
        })?;
        let hex = |s: &str| {
            u16::from_str_radix(s.trim_start_matches("0x"), 16).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid identity hex {s:?}"),
                )
            })
        };
        let word = |offset: usize| -> io::Result<u16> {
            let guid = descriptor.identity.sdl_guid.as_bytes();
            if guid.len() != 32 || !guid.is_ascii() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid SDL GUID",
                ));
            }
            let byte = |at: usize| {
                std::str::from_utf8(&guid[at..at + 2])
                    .ok()
                    .and_then(|s| u8::from_str_radix(s, 16).ok())
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid SDL GUID"))
            };
            let lo = byte(offset)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid SDL GUID"))?;
            let hi = byte(offset + 2)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid SDL GUID"))?;
            Ok(lo as u16 | (hi as u16) << 8)
        };
        Ok(Self {
            name: m.evdev_name.clone(),
            bus: word(0)?,
            vendor: hex(&m.vid)?,
            product: hex(&m.pid)?,
            version: word(24)?,
        })
    }

    pub fn matches(&self, name: &str, id: (u16, u16, u16, u16)) -> bool {
        name == self.name
            && id.0 == self.bus
            && id.1 == self.vendor
            && id.2 == self.product
            && id.3 == self.version
            && !name.starts_with("PocketForge Input (")
    }
}

impl InputBroker {
    /// Open `source_path`, grab it (the enforcing default), and stand up the descriptor-derived
    /// re-emit device.
    pub fn start(
        source_path: impl AsRef<Path>,
        descriptor: &Descriptor,
    ) -> io::Result<InputBroker> {
        InputBroker::start_with(source_path, descriptor, true)
    }

    /// As [`start`](Self::start), but `grab=false` is the R-C **blessed-binary** path (Steam Link):
    /// re-emit + hand a fd WITHOUT the exclusive grab, so a consumer that is itself a `uinput`
    /// producer is not broken. The re-emit device still normalizes codes; it just is not exclusive.
    pub fn start_with(
        source_path: impl AsRef<Path>,
        descriptor: &Descriptor,
        grab: bool,
    ) -> io::Result<InputBroker> {
        InputBroker::start_checked(source_path, descriptor, grab, &SourceIdentity::system())
    }

    /// As [`start_with`](Self::start_with), with an explicit source-identity policy. The production
    /// default requires the built-in controller by [`pf_input_decode::builtin`]; simulation/tests
    /// can explicitly allow an unidentified source. The check runs on the opened fd, before the
    /// grab, so a lookalike is never grabbed.
    pub fn start_checked(
        source_path: impl AsRef<Path>,
        descriptor: &Descriptor,
        grab: bool,
        identity: &SourceIdentity,
    ) -> io::Result<InputBroker> {
        let remap = Remap::from_descriptor(descriptor)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        let (source, sink) = acquire_then_create(
            || {
                let mut source = Evdev::open(source_path)?;
                Self::validate_source(&source, descriptor, &remap)?;
                identity.verify_fd(source.as_fd())?;
                if grab {
                    source.grab()?;
                }
                Ok(source)
            },
            |_| Uinput::create(remap.spec()),
        )?;
        Ok(InputBroker {
            source,
            sink,
            start: std::time::Instant::now(),
            pump: ReportPump::new(remap),
        })
    }

    /// Make this broker the protected System Menu intake: guide/`BTN_MODE` transitions are
    /// withheld from apps, and each press routes through the provider/fallback policy.
    pub fn with_system_menu(mut self, router: SystemMenuRouter) -> InputBroker {
        self.pump.system_menu = Some(SystemMenuGate::new(router));
        self
    }

    fn validate_source(source: &Evdev, descriptor: &Descriptor, remap: &Remap) -> io::Result<()> {
        let expected = ExpectedIdentity::from_descriptor(descriptor)?;
        if !expected.matches(&source.name()?, source.id()?) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "opened source identity mismatch",
            ));
        }
        let missing = missing_source_capabilities(source, remap)?;
        if !missing.is_empty() {
            // The prefix is kept verbatim (journal greps key on it); the suffix names the codes,
            // which the tsp-f3fm.215 bench had to reconstruct from bitmaps.
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("opened source lacks descriptor-required capabilities: missing {missing}"),
            ));
        }
        Ok(())
    }

    /// Discover exactly one descriptor-matching event node. Every candidate is identified from
    /// its opened fd; `start` opens and validates the winner again before grabbing it.
    pub fn discover(descriptor: &Descriptor) -> io::Result<PathBuf> {
        InputBroker::discover_checked(descriptor, &SourceIdentity::system())
    }

    /// As [`discover`](Self::discover), with an explicit source-identity policy. Under the
    /// production default only the built-in controller is a candidate, so a lookalike cannot make
    /// discovery ambiguous or win it.
    pub fn discover_checked(
        descriptor: &Descriptor,
        identity: &SourceIdentity,
    ) -> io::Result<PathBuf> {
        let expected = ExpectedIdentity::from_descriptor(descriptor)?;
        let remap = Remap::from_descriptor(descriptor)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        let mut candidates = Vec::new();
        for entry in std::fs::read_dir("/dev/input")? {
            let path = entry?.path();
            if !path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("event"))
            {
                continue;
            }
            candidates.push(path);
        }
        let mut identity_rejection = None;
        let result = discover_candidates(candidates, |path| {
            let dev = Evdev::open(path)?;
            if !(expected.matches(&dev.name()?, dev.id()?)
                && missing_source_capabilities(&dev, &remap)?.is_empty())
            {
                return Ok(false);
            }
            match identity.trust.as_ref() {
                Some(trust) => match trust.identify_fd(dev.as_fd()) {
                    Ok(Verdict::BuiltIn { .. }) => Ok(true),
                    Ok(Verdict::NotBuiltIn(reason)) => {
                        identity_rejection
                            .get_or_insert_with(|| format!("{}: {reason:?}", path.display()));
                        Ok(false)
                    }
                    Err(error) => {
                        identity_rejection.get_or_insert_with(|| {
                            format!("{}: identification error: {error}", path.display())
                        });
                        Ok(false)
                    }
                },
                None => Ok(true),
            }
        });
        finish_discovery(result, identity_rejection)
    }

    fn resolve_matches(mut matches: Vec<PathBuf>) -> io::Result<PathBuf> {
        match matches.len() {
            1 => Ok(matches.remove(0)),
            0 => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no input device matches descriptor identity and capabilities",
            )),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("multiple input devices match descriptor: {matches:?}"),
            )),
        }
    }

    /// The re-emit `/dev/input/eventN` node path (what the app reads / the fd handed over points at).
    pub fn node_path(&self) -> Option<String> {
        self.sink.node().map(|s| s.to_string())
    }

    /// The grabbed source device's name (for logging / the blessed-binary check).
    pub fn source_name(&self) -> io::Result<String> {
        self.source.name()
    }

    /// Drain pending source events through remap + policy into the sink. Returns events emitted.
    pub fn pump_once(&mut self) -> io::Result<usize> {
        let mut buf: [libc::input_event; 64] = unsafe { std::mem::zeroed() };
        let n = self.source.read_events(&mut buf)?;
        let now = self.start.elapsed().as_secs_f64();
        let sink = &self.sink;
        self.pump
            .process(&buf[..n], now, &self.source, &mut |ty, code, value| {
                sink.emit(ty, code, value)
            })
    }

    /// Block up to `timeout_ms` for the source to become readable. `true` if events are pending.
    pub fn wait_readable(&self, timeout_ms: i32) -> io::Result<bool> {
        self.source.wait_readable(timeout_ms)
    }

    /// Run the pump until `stop` is set (poll-driven; no busy spin).
    pub fn run(&mut self, stop: &AtomicBool) -> io::Result<()> {
        let config = RunConfig::existing(&mut self.pump, self.start, stop);
        Self::run_with_io(&mut self.source, &mut self.sink, config)
    }

    /// Run the real descriptor remap and policy loop with explicit event adapters.
    ///
    /// Production [`run`](Self::run) calls this with [`Evdev`] and [`Uinput`]. Hermetic callers
    /// use an [`FdEventSource`](crate::io::FdEventSource) and a private or recording sink.
    pub fn run_with_io<S, K>(
        source: &mut S,
        sink: &mut K,
        mut config: RunConfig<'_>,
    ) -> io::Result<()>
    where
        S: EventSource + SourceState,
        K: EventSink,
    {
        if let Some(pump) = config.pump.take() {
            if let Some(token_bucket) = config.token_bucket.take() {
                pump.bucket = token_bucket;
            }
            return run_io_loop(source, sink, pump, config.start, config.stop);
        }
        let descriptor = config
            .descriptor
            .expect("a fresh run config always carries a descriptor");
        let remap = Remap::from_descriptor(descriptor)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        let mut pump = ReportPump::new(remap);
        if let Some(token_bucket) = config.token_bucket {
            pump.bucket = token_bucket;
        }
        run_io_loop(source, sink, &mut pump, config.start, config.stop)
    }

    /// Open a fresh read fd on the re-emit node — the fd handed to an app via `SCM_RIGHTS`.
    pub fn open_app_fd(&self) -> io::Result<OwnedFd> {
        let node = self
            .node_path()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "re-emit node not resolved"))?;
        open_read_fd(&node)
    }
}

fn run_io_loop<S, K>(
    source: &mut S,
    sink: &mut K,
    pump: &mut ReportPump,
    start: std::time::Instant,
    stop: &AtomicBool,
) -> io::Result<()>
where
    S: EventSource + SourceState,
    K: EventSink,
{
    let mut buffer: [libc::input_event; 64] = unsafe { std::mem::zeroed() };
    while !stop.load(Ordering::Acquire) {
        let count = source.read_events(&mut buffer)?;
        if count == 0 {
            continue;
        }
        let mut output = Vec::new();
        pump.process(
            &buffer[..count],
            start.elapsed().as_secs_f64(),
            source,
            &mut |event_type, code, value| {
                let mut event: libc::input_event = unsafe { std::mem::zeroed() };
                event.type_ = event_type;
                event.code = code;
                event.value = value;
                output.push(event);
                if event_type == ioc::EV_SYN && code == ioc::SYN_REPORT {
                    sink.write_events(&output)?;
                    output.clear();
                }
                Ok(())
            },
        )?;
        if !output.is_empty() {
            sink.write_events(&output)?;
        }
    }
    Ok(())
}

fn finish_discovery(
    result: io::Result<PathBuf>,
    identity_rejection: Option<String>,
) -> io::Result<PathBuf> {
    match (result, identity_rejection) {
        (Err(error), Some(reason)) if error.kind() == io::ErrorKind::NotFound => {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("descriptor-matching source failed built-in controller identity: {reason}"),
            ))
        }
        (result, _) => result,
    }
}

impl ReportPump {
    fn new(remap: Remap) -> ReportPump {
        // The legacy uinput setup initializes every advertised ABS value to zero.
        let abs_state = remap
            .spec()
            .abs
            .iter()
            .map(|(code, _)| (*code, 0))
            .collect();
        ReportPump {
            remap,
            // TODO(tsp-mc9m.41.1026.6): the runtime posture source is designed
            // by the session orientation policy; Normal is the only value wired
            // until that lands.
            posture: Posture::Normal,
            bucket: TokenBucket::default_broker(),
            pending_report: Vec::new(),
            pending_report_oversized: false,
            resynchronizing: false,
            pressed: HashSet::new(),
            abs_state,
            system_menu: None,
        }
    }

    /// `true` when the System Menu gate owns this canonical key transition.
    fn consumed_by_system_menu(&mut self, code: u16, value: i32) -> bool {
        self.system_menu
            .as_mut()
            .is_some_and(|gate| gate.consume_key(code, value))
    }

    /// Route one batch of source events through remap + policy + the guide gate into `emit`.
    /// Returns events emitted.
    pub(crate) fn process(
        &mut self,
        events: &[libc::input_event],
        now: f64,
        source: &impl SourceState,
        emit: &mut impl FnMut(u16, u16, i32) -> io::Result<()>,
    ) -> io::Result<usize> {
        let mut emitted = 0usize;
        for ev in events {
            let t = ev.type_;
            if t == ioc::EV_SYN && ev.code == ioc::SYN_DROPPED {
                self.pending_report.clear();
                self.pending_report_oversized = false;
                self.resynchronizing = true;
            } else if self.resynchronizing {
                // Events following SYN_DROPPED belong to the unreliable tail of the overrun.
                // Once its boundary arrives, query authoritative state before accepting reports.
                if t == ioc::EV_SYN && ev.code == ioc::SYN_REPORT {
                    let out = self.resynchronize_source_state(source)?;
                    for (ty, code, value) in out {
                        emit(ty, code, value)?;
                        emitted += 1;
                    }
                    self.resynchronizing = false;
                }
            } else if t == ioc::EV_SYN && ev.code == ioc::SYN_REPORT {
                let allowed = self.bucket.allow(now);
                let out = finish_report(
                    &mut self.pending_report,
                    &mut self.pending_report_oversized,
                    &mut self.pressed,
                    allowed,
                );
                for (ty, code, value) in out {
                    emit(ty, code, value)?;
                    if ty == ioc::EV_ABS {
                        self.abs_state.insert(code, value);
                    }
                    emitted += 1;
                }
            } else if t == ioc::EV_KEY {
                let code = self.remap.remap_key(ev.code);
                if !self.consumed_by_system_menu(code, ev.value) {
                    push_report_event(
                        &mut self.pending_report,
                        &mut self.pending_report_oversized,
                        (t, code, ev.value),
                    );
                }
            } else if t == ioc::EV_ABS {
                // Analog axes pass through; a physically-binary trigger (semantics="binary") is
                // reclassified to an EV_KEY press/release on its canonical button (descriptor-driven).
                match self.remap.classify_abs(ev.code, ev.value) {
                    AbsAction::Passthrough => {
                        // Spatial axes are re-expressed in the logical frame for the
                        // current posture (identity for Normal / a screen-only swivel).
                        let (code, value) =
                            self.remap.remap_axis_event(self.posture, ev.code, ev.value);
                        push_report_event(
                            &mut self.pending_report,
                            &mut self.pending_report_oversized,
                            (t, code, value),
                        );
                    }
                    AbsAction::Button { code, value } => {
                        if !self.consumed_by_system_menu(code, value) {
                            push_report_event(
                                &mut self.pending_report,
                                &mut self.pending_report_oversized,
                                (ioc::EV_KEY, code, value),
                            );
                        }
                    }
                    AbsAction::None => {} // inside the hysteresis band / no state change — drop
                }
            }
            // Other event types are outside the descriptor-controlled input surface.
        }
        Ok(emitted)
    }

    fn resynchronize_source_state(
        &mut self,
        source: &impl SourceState,
    ) -> io::Result<Vec<(u16, u16, i32)>> {
        let mut actual_pressed: HashSet<u16> = source
            .pressed_keys(self.remap.required_source_keys())?
            .into_iter()
            .map(|code| self.remap.remap_key(code))
            .collect();
        let abs_values = self
            .remap
            .required_source_abs()
            .iter()
            .copied()
            .map(|code| source.abs_value(code).map(|value| (code, value)))
            .collect::<io::Result<Vec<_>>>()?;
        let mut actual_abs = Vec::new();
        let mut actual_binary = Vec::new();
        for (code, value) in abs_values {
            match self.remap.resync_abs(code, value) {
                // The held state is diffed in the LOGICAL frame (abs_state is keyed by
                // emitted codes), so the authoritative body-frame value is re-expressed
                // for the current posture exactly like the live path.
                AbsAction::Passthrough => {
                    actual_abs.push(self.remap.remap_axis_event(self.posture, code, value))
                }
                AbsAction::Button { code, value } => actual_binary.push((code, value != 0)),
                AbsAction::None => unreachable!("resync_abs always yields authoritative state"),
            }
        }
        if let Some(gate) = self.system_menu.as_mut() {
            // The guide key never enters the visible state; its authoritative level feeds the
            // gate instead, so a press lost inside the overrun still opens Menu or falls back.
            let binary_guide = actual_binary
                .iter()
                .any(|&(code, down)| code == GUIDE_CODE && down);
            actual_binary.retain(|&(code, _)| code != GUIDE_CODE);
            let key_guide = actual_pressed.remove(&GUIDE_CODE);
            gate.set_down(key_guide || binary_guide);
        }
        Ok(diff_resynchronized_state(
            &mut self.pressed,
            &mut self.abs_state,
            actual_pressed,
            actual_abs,
            actual_binary,
        ))
    }
}

/// The broker's built-in gate: only [`Verdict::BuiltIn`] passes. A lookalike and an identification
/// failure both refuse the source (fail closed); the error says which.
fn require_builtin(verdict: io::Result<Verdict>) -> io::Result<()> {
    match verdict {
        Ok(Verdict::BuiltIn { .. }) => Ok(()),
        Ok(Verdict::NotBuiltIn(reason)) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("opened source is not the built-in controller: {reason:?}"),
        )),
        Err(e) => Err(io::Error::new(
            e.kind(),
            format!("cannot identify the opened source as the built-in controller: {e}"),
        )),
    }
}

fn discover_candidates(
    candidates: impl IntoIterator<Item = PathBuf>,
    mut probe: impl FnMut(&Path) -> io::Result<bool>,
) -> io::Result<PathBuf> {
    let matches = candidates
        .into_iter()
        // A disappearing or non-evdev candidate is not a discovery-wide failure. The next
        // candidate may still be the descriptor-selected controller.
        .filter(|path| matches!(probe(path), Ok(true)))
        .collect();
    InputBroker::resolve_matches(matches)
}

fn finish_report(
    pending: &mut Vec<(u16, u16, i32)>,
    oversized: &mut bool,
    pressed: &mut HashSet<u16>,
    allowed: bool,
) -> Vec<(u16, u16, i32)> {
    let mut out = Vec::new();
    if allowed && !*oversized {
        for &(ty, code, value) in pending.iter() {
            if ty == ioc::EV_KEY {
                if value == 0 {
                    pressed.remove(&code);
                } else {
                    pressed.insert(code);
                }
            }
            out.push((ty, code, value));
        }
    } else {
        // A suppressed report may contain the release for any currently-visible press. Release
        // every visible key: this fail-safe direction cannot strand a button down.
        let mut releases: Vec<_> = pressed.drain().collect();
        releases.sort_unstable();
        out.extend(releases.into_iter().map(|code| (ioc::EV_KEY, code, 0)));
    }
    pending.clear();
    *oversized = false;
    // A rejected report produces no sink write at all unless releases are needed. In that case
    // the boundary commits those releases atomically. An admitted report always retains its
    // boundary, including an otherwise-empty report.
    if allowed || !out.is_empty() {
        out.push((ioc::EV_SYN, ioc::SYN_REPORT, 0));
    }
    out
}

fn diff_resynchronized_state(
    pressed: &mut HashSet<u16>,
    abs_state: &mut HashMap<u16, i32>,
    mut actual_pressed: HashSet<u16>,
    actual_abs: Vec<(u16, i32)>,
    actual_binary: Vec<(u16, bool)>,
) -> Vec<(u16, u16, i32)> {
    for (code, down) in actual_binary {
        if down {
            actual_pressed.insert(code);
        } else {
            actual_pressed.remove(&code);
        }
    }
    let mut changed_keys: Vec<_> = pressed
        .symmetric_difference(&actual_pressed)
        .copied()
        .collect();
    changed_keys.sort_unstable();
    let mut out: Vec<_> = changed_keys
        .into_iter()
        .map(|code| (ioc::EV_KEY, code, i32::from(actual_pressed.contains(&code))))
        .collect();
    *pressed = actual_pressed;
    for (code, value) in actual_abs {
        if abs_state.get(&code) != Some(&value) {
            out.push((ioc::EV_ABS, code, value));
            abs_state.insert(code, value);
        }
    }
    if !out.is_empty() {
        out.push((ioc::EV_SYN, ioc::SYN_REPORT, 0));
    }
    out
}

fn push_report_event(
    pending: &mut Vec<(u16, u16, i32)>,
    oversized: &mut bool,
    event: (u16, u16, i32),
) {
    if *oversized {
        return;
    }
    if pending.len() == MAX_REPORT_EVENTS {
        pending.clear();
        *oversized = true;
        return;
    }
    pending.push(event);
}

fn acquire_then_create<A, B>(
    acquire: impl FnOnce() -> io::Result<A>,
    create: impl FnOnce(&A) -> io::Result<B>,
) -> io::Result<(A, B)> {
    let acquired = acquire()?;
    let created = create(&acquired)?;
    Ok((acquired, created))
}

/// Open a node read-only, non-blocking, close-on-exec (the consumer's read fd shape).
pub fn open_read_fd(path: impl AsRef<Path>) -> io::Result<OwnedFd> {
    let c = std::ffi::CString::new(path.as_ref().as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path has NUL"))?;
    // SAFETY: valid C string.
    let raw = unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fresh owned fd.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

// --- the Acquire("input") fd-handoff server (wire §4.1) -------------------------------------

/// Serve `Acquire("input")` on `listener`, handing the re-emit read fd over `SCM_RIGHTS`. Each
/// connection gets ONE acquisition then closes. Runs until `stop` is set.
pub fn serve_acquire(
    listener: &UnixListener,
    app_fd_path: &str,
    stop: &AtomicBool,
) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                let _ = handle_acquire(stream, app_fd_path);
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Handle one acquisition connection: reply to `Acquire("input")` with `Ok` + the fd; anything
/// else gets a typed error (this socket only vends input).
pub fn handle_acquire(mut stream: UnixStream, app_fd_path: &str) -> io::Result<()> {
    stream.set_read_timeout(Some(ACQUIRE_CLIENT_TIMEOUT))?;
    stream.set_write_timeout(Some(ACQUIRE_CLIENT_TIMEOUT))?;
    stream.set_nonblocking(false)?;
    let req = match recv_request(&mut stream) {
        Ok(r) => r,
        Err(_) => return Ok(()), // malformed / closed → drop
    };
    if req.op == Op::Acquire && req.name.eq_ignore_ascii_case("input") {
        let fd = open_read_fd(app_fd_path)?;
        let mut framed = Vec::new();
        send_response(&mut framed, &Response::ok()).map_err(wire_err)?; // framed PFW1 Response bytes
        scm::send_fd(stream.as_raw_fd(), &framed, fd.as_raw_fd())?;
    } else {
        // This socket vends only the input fd; everything else is unsupported here.
        let _ = send_response(&mut stream, &Response::err(Status::Unsupported));
    }
    Ok(())
}

/// Serve one app connection as a persistent PFW1 request/response loop (`tsp-f3fm.202.1`).
///
/// * `Acquire("input")` replies `Ok` plus a fresh re-emit read fd over `SCM_RIGHTS`, as many
///   times as the client asks, so a client that dropped its fd can re-acquire on the same session
///   connection.
/// * Preference reads (`GetAppearance`, `GetAppearanceSource`, and `GetPreference`) go through
///   [`pocketforge::server::handle_request`]: prefsd at `$PF_PREFSD_SOCK` when set, otherwise the
///   backend/default response.
/// * Every other op gets a typed `Unsupported`.
///
/// The client (the app's `BrokerClientBackend`) holds this connection for its whole session, so
/// the idle wait between requests is unbounded. Once a request's first byte arrives, the request
/// and its reply are each bounded by the acquisition I/O deadline, so a client that stalls
/// mid-frame cannot pin the thread. EOF, a hangup or a protocol error ends the loop.
pub fn serve_client(
    stream: UnixStream,
    app_fd_path: &str,
    backend: &dyn Backend,
) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(ACQUIRE_CLIENT_TIMEOUT))?;
    stream.set_write_timeout(Some(ACQUIRE_CLIENT_TIMEOUT))?;
    while wait_for_request(&stream)? {
        let req = match recv_request(&mut &stream) {
            Ok(r) => r,
            Err(_) => return Ok(()), // closed / malformed / stalled mid-frame → drop
        };
        respond(&stream, &req, app_fd_path, backend)?;
    }
    Ok(())
}

/// Block until the client sends something (`true`) or hangs up (`false`).
fn wait_for_request(stream: &UnixStream) -> io::Result<bool> {
    loop {
        let mut pfd = libc::pollfd {
            fd: stream.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd; -1 waits until readable or hung up.
        let rc = unsafe { libc::poll(&mut pfd, 1, -1) };
        if rc < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(e);
        }
        // Pending bytes win over a simultaneous hangup: a request written just before close
        // is still answered (or fails cleanly at EOF).
        return Ok(pfd.revents & libc::POLLIN != 0);
    }
}

fn respond(
    stream: &UnixStream,
    req: &Request,
    app_fd_path: &str,
    backend: &dyn Backend,
) -> io::Result<()> {
    let mut writer = stream;
    match req.op {
        Op::Acquire if req.name.eq_ignore_ascii_case("input") => {
            let fd = match open_read_fd(app_fd_path) {
                Ok(fd) => fd,
                Err(e) => {
                    // Keep the session connection: a typed refusal, never a fabricated fd.
                    eprintln!("pf-input-broker: re-emit node {app_fd_path} unavailable: {e}");
                    return send_response(&mut writer, &Response::err(Status::HardwareAbsent))
                        .map_err(wire_err);
                }
            };
            let mut framed = Vec::new();
            send_response(&mut framed, &Response::ok()).map_err(wire_err)?;
            scm::send_fd(stream.as_raw_fd(), &framed, fd.as_raw_fd())
        }
        Op::GetAppearance | Op::GetAppearanceSource | Op::GetPreference => {
            send_response(&mut writer, &handle_request(backend, req)).map_err(wire_err)
        }
        // This socket vends the input fd and read-only preferences; nothing mutating.
        _ => send_response(&mut writer, &Response::err(Status::Unsupported)).map_err(wire_err),
    }
}

/// Client side: `Acquire("input")` from the broker at `sock_path`, returning the PFW1 response +
/// the shared re-emit read fd. This is the `libpocketforge` input-acquisition path the `.2`
/// facade reserves — the fd, not RPC, is the hot path.
pub fn acquire_input_fd(sock_path: impl AsRef<Path>) -> io::Result<(Response, OwnedFd)> {
    use pf_wire::{recv_response, send_request};
    let mut stream = UnixStream::connect(sock_path)?;
    send_request(&mut stream, &Request::new(Op::Acquire, "input")).map_err(wire_err)?;

    let mut buf = [0u8; 256];
    let (n, fd) = scm::recv_fd(stream.as_raw_fd(), &mut buf)?;
    let mut cur = io::Cursor::new(&buf[..n]);
    let resp = recv_response(&mut cur).map_err(wire_err)?;
    match fd {
        Some(fd) if resp.status == Status::Ok => Ok((resp, fd)),
        Some(_) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "broker refused input",
        )),
        None => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "broker sent no fd",
        )),
    }
}

#[cfg(test)]
mod readiness_tests {
    use super::*;

    #[test]
    fn systemd_decoder_restart_is_recovered_by_broker_failure_restart() {
        let unit = include_str!("../systemd/pf-input-broker.service");
        assert!(unit.lines().any(|line| {
            line == "After=local-fs.target dev-uinput.device pf-input-decode.service"
        }));
        assert!(unit.lines().any(|line| line == "Restart=on-failure"));
        assert!(
            !unit.contains("--allow-unidentified-pad"),
            "the production service must keep built-in controller identity enforcement"
        );
        assert!(
            !unit.lines().any(|line| {
                line.starts_with("BindsTo=")
                    || (line.starts_with("Requires=")
                        && line.contains("pf-input-decode.service"))
            }),
            "decoder lifetime must not stop the broker cleanly: fd loss must fail the pump, restart the broker, rediscover the decoder, and take a fresh grab"
        );
    }

    #[test]
    fn identity_match_rejects_wrong_name_or_id() {
        let e = ExpectedIdentity {
            name: "TRIMUI Player1".into(),
            bus: 3,
            vendor: 0x045e,
            product: 0x028e,
            version: 0x0110,
        };
        assert!(e.matches("TRIMUI Player1", (3, 0x045e, 0x028e, 0x0110)));
        assert!(!e.matches("event-gamepad", (3, 0x045e, 0x028e, 0x0110)));
        assert!(!e.matches("TRIMUI Player1", (3, 0x1234, 0x028e, 0x0110)));
        assert!(!e.matches("TRIMUI Player1", (5, 0x045e, 0x028e, 0x0110)));
    }

    #[test]
    fn builtin_gate_passes_only_the_built_in_verdict() {
        use pf_input_decode::builtin::Reason;
        assert!(require_builtin(Ok(Verdict::BuiltIn {
            sysname: "input5".into()
        }))
        .is_ok());
        for reason in [
            Reason::NotInputCharDevice,
            Reason::NotVirtual {
                devpath: "devices/platform/usb1/1-1/input/input9/event4".into(),
            },
            Reason::PhysMismatch,
            Reason::RecordAbsent,
            Reason::RecordUntrusted("mode 666".into()),
            Reason::SysnameMismatch {
                device: "input9".into(),
                record: "input5".into(),
            },
        ] {
            let err = require_builtin(Ok(Verdict::NotBuiltIn(reason.clone()))).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{reason:?}");
            assert!(err.to_string().contains("not the built-in controller"));
        }
        let err =
            require_builtin(Err(io::Error::new(io::ErrorKind::NotFound, "gone"))).unwrap_err();
        assert_eq!(
            err.kind(),
            io::ErrorKind::NotFound,
            "an I/O error is never a pass"
        );
    }

    #[test]
    fn source_identity_policy_refuses_lookalikes_and_missing_records_by_default() {
        use pf_input_decode::builtin::Reason;

        let default = SourceIdentity::system();
        assert!(default
            .verify_verdict(Ok(Verdict::BuiltIn {
                sysname: "input5".into(),
            }))
            .is_ok());
        for reason in [
            Reason::SysnameMismatch {
                device: "input9".into(),
                record: "input5".into(),
            },
            Reason::RecordAbsent,
        ] {
            assert!(
                default
                    .verify_verdict(Ok(Verdict::NotBuiltIn(reason.clone())))
                    .is_err(),
                "default must fail closed for {reason:?}"
            );
        }

        let simulation = SourceIdentity::allow_unidentified();
        assert!(simulation
            .verify_verdict(Ok(Verdict::NotBuiltIn(Reason::SysnameMismatch {
                device: "input9".into(),
                record: "input5".into(),
            })))
            .is_ok());
        assert!(simulation
            .verify_verdict(Ok(Verdict::NotBuiltIn(Reason::RecordAbsent)))
            .is_ok());

        let missing_record = finish_discovery(
            Err(io::Error::new(io::ErrorKind::NotFound, "no match")),
            Some("/dev/input/event9: RecordAbsent".into()),
        )
        .unwrap_err();
        assert_eq!(missing_record.kind(), io::ErrorKind::PermissionDenied);
        assert!(missing_record.to_string().contains("RecordAbsent"));
        let decoder = PathBuf::from("/dev/input/event5");
        assert_eq!(
            finish_discovery(Ok(decoder.clone()), Some("ignored lookalike".into())).unwrap(),
            decoder,
            "a valid decoder device remains the positive control"
        );
    }

    #[test]
    fn discovery_skips_failing_candidate_before_valid_match() {
        let bad = PathBuf::from("/dev/input/event0");
        let good = PathBuf::from("/dev/input/event1");
        let found = discover_candidates(vec![bad.clone(), good.clone()], |path| {
            if path == bad {
                Err(io::Error::new(io::ErrorKind::NotFound, "unplugged"))
            } else {
                Ok(true)
            }
        })
        .unwrap();
        assert_eq!(found, good);
    }

    #[test]
    fn syn_dropped_resyncs_actual_keys_and_axes_without_stale_state() {
        let mut pressed = HashSet::from([0x130, 0x131]);
        let mut abs = HashMap::from([(0, 10), (1, 20)]);
        assert_eq!(
            diff_resynchronized_state(
                &mut pressed,
                &mut abs,
                HashSet::from([0x130, 0x132]),
                vec![(0, 10), (1, 99)],
                vec![],
            ),
            vec![
                (ioc::EV_KEY, 0x131, 0),
                (ioc::EV_KEY, 0x132, 1),
                (ioc::EV_ABS, 1, 99),
                (ioc::EV_SYN, ioc::SYN_REPORT, 0),
            ]
        );
        assert_eq!(pressed, HashSet::from([0x130, 0x132]));
        assert_eq!(abs, HashMap::from([(0, 10), (1, 99)]));
    }

    #[test]
    fn syn_dropped_keeps_key_pressed_across_overrun_and_releases_actual_up_key() {
        let mut pressed = HashSet::from([0x130, 0x131]);
        let mut abs = HashMap::new();
        assert_eq!(
            diff_resynchronized_state(
                &mut pressed,
                &mut abs,
                HashSet::from([0x130]),
                vec![],
                vec![],
            ),
            vec![(ioc::EV_KEY, 0x131, 0), (ioc::EV_SYN, ioc::SYN_REPORT, 0)]
        );
        assert!(
            pressed.contains(&0x130),
            "held-across-drop key stays pressed"
        );
        assert!(
            !pressed.contains(&0x131),
            "key released in dropped tail converges up"
        );
    }

    #[test]
    fn suppressed_report_synthesizes_release_for_visible_press() {
        let mut pressed = HashSet::from([0x130]);
        let mut report = vec![(ioc::EV_KEY, 0x130, 0)];
        assert_eq!(
            finish_report(&mut report, &mut false, &mut pressed, false),
            vec![(ioc::EV_KEY, 0x130, 0), (ioc::EV_SYN, ioc::SYN_REPORT, 0)]
        );
        assert!(pressed.is_empty());
    }

    #[test]
    fn allowed_report_keeps_all_payload_with_its_syn_report() {
        let mut pressed = HashSet::new();
        let mut report = vec![(ioc::EV_KEY, 0x130, 1), (ioc::EV_ABS, 0, 17)];
        assert_eq!(
            finish_report(&mut report, &mut false, &mut pressed, true),
            vec![
                (ioc::EV_KEY, 0x130, 1),
                (ioc::EV_ABS, 0, 17),
                (ioc::EV_SYN, ioc::SYN_REPORT, 0)
            ]
        );
        assert!(pressed.contains(&0x130));
    }

    #[test]
    fn rejected_reports_emit_nothing_after_required_release_commit() {
        let mut pressed = HashSet::from([0x130]);
        let mut oversized = false;
        let mut report = vec![(ioc::EV_KEY, 0x130, 1)];
        assert_eq!(
            finish_report(&mut report, &mut oversized, &mut pressed, false),
            vec![(ioc::EV_KEY, 0x130, 0), (ioc::EV_SYN, ioc::SYN_REPORT, 0)]
        );
        for _ in 0..1_000 {
            report.push((ioc::EV_ABS, 0, 1));
            assert!(finish_report(&mut report, &mut oversized, &mut pressed, false).is_empty());
        }
    }

    #[test]
    fn oversized_report_is_bounded_released_and_resynchronizes() {
        let mut pressed = HashSet::from([0x130]);
        let mut oversized = false;
        let mut report = Vec::new();
        for _ in 0..MAX_REPORT_EVENTS + 10_000 {
            push_report_event(&mut report, &mut oversized, (ioc::EV_ABS, 0, 1));
        }
        assert!(oversized);
        assert!(report.len() <= MAX_REPORT_EVENTS);
        assert_eq!(
            finish_report(&mut report, &mut oversized, &mut pressed, true),
            vec![(ioc::EV_KEY, 0x130, 0), (ioc::EV_SYN, ioc::SYN_REPORT, 0)]
        );
        push_report_event(&mut report, &mut oversized, (ioc::EV_KEY, 0x131, 1));
        assert_eq!(
            finish_report(&mut report, &mut oversized, &mut pressed, true),
            vec![(ioc::EV_KEY, 0x131, 1), (ioc::EV_SYN, ioc::SYN_REPORT, 0)]
        );
    }

    #[test]
    fn stalled_acquire_client_is_deadline_bounded() {
        // The ceiling is the SUBJECT of the test, not a synchronisation window: a silent
        // client must be cut off by the production ACQUIRE_CLIENT_TIMEOUT (250 ms), so a
        // healthy run finishes at the deadline; only a regression to unbounded blocking can
        // reach the 1 s assert bound.
        let (server, _silent_client) = UnixStream::pair().unwrap();
        let started = std::time::Instant::now();
        handle_acquire(server, "/unused").unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn grab_failure_never_creates_sink() {
        use std::cell::Cell;
        let created = Cell::new(false);
        let result: io::Result<((), ())> = acquire_then_create(
            || {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "grab failed",
                ))
            },
            |_| {
                created.set(true);
                Ok(())
            },
        );
        assert!(result.is_err());
        assert!(
            !created.get(),
            "sink factory must not run before successful acquisition"
        );
    }
}

/// Hermetic tests for the persistent PFW1 session loop, GetAppearance pass-through, and the
/// protected System Menu gate inside the report pump.
#[cfg(test)]
mod session_and_system_menu_tests {
    use super::*;
    use crate::safe_return::{
        SafeReturnIntake, SystemMenuRouter, CONNECT_TIMEOUT, IO_TIMEOUT, SAFE_RETURN_BODY,
    };
    use pocketforge::backends::{BrokerClientBackend, InProcessBackend};
    use std::io::Write;
    use std::sync::mpsc::{channel, Receiver};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    const SOUTH: u16 = 0x130;

    fn descriptor() -> Descriptor {
        Descriptor::from_toml(
            r#"
[identity]
id = "synthguide"
manufacturer = "PocketForge"
model = "Guide Rig (synthetic test descriptor)"
sdl_guid = "030000005e0400008e02000010010000"

[[inputs]]
id = "south"
kind = "button"
ev_type = "EV_KEY"
code = "BTN_A"

[[inputs]]
id = "guide"
kind = "button"
ev_type = "EV_KEY"
code = "BTN_MODE"

[[inputs]]
id = "ltrig"
kind = "trigger"
ev_type = "EV_ABS"
code = "ABS_Z"
range = { min = 0, max = 255, fuzz = 0, flat = 0 }
"#,
        )
        .unwrap()
    }

    fn pump(intake: Option<SafeReturnIntake>) -> ReportPump {
        let mut pump = ReportPump::new(Remap::from_descriptor(&descriptor()).unwrap());
        pump.system_menu =
            intake.map(|intake| SystemMenuGate::new(SystemMenuRouter::spawn(intake).unwrap()));
        pump
    }

    #[derive(Default)]
    struct FakeSource {
        pressed: Vec<u16>,
        abs: Vec<(u16, i32)>,
    }
    impl SourceState for FakeSource {
        fn pressed_keys(&self, codes: &[u16]) -> io::Result<Vec<u16>> {
            Ok(codes
                .iter()
                .copied()
                .filter(|c| self.pressed.contains(c))
                .collect())
        }
        fn abs_value(&self, code: u16) -> io::Result<i32> {
            Ok(self
                .abs
                .iter()
                .find(|(held, _)| *held == code)
                .map_or(0, |(_, value)| *value))
        }
    }

    fn ev(ty: u16, code: u16, value: i32) -> libc::input_event {
        // SAFETY: input_event is plain old data.
        let mut e: libc::input_event = unsafe { std::mem::zeroed() };
        e.type_ = ty;
        e.code = code;
        e.value = value;
        e
    }
    fn key(code: u16, value: i32) -> libc::input_event {
        ev(ioc::EV_KEY, code, value)
    }
    fn syn() -> libc::input_event {
        ev(ioc::EV_SYN, ioc::SYN_REPORT, 0)
    }

    fn run(pump: &mut ReportPump, events: &[libc::input_event]) -> Vec<(u16, u16, i32)> {
        run_with(pump, events, &FakeSource::default())
    }
    fn run_with(
        pump: &mut ReportPump,
        events: &[libc::input_event],
        source: &FakeSource,
    ) -> Vec<(u16, u16, i32)> {
        let mut out = Vec::new();
        pump.process(events, 0.0, source, &mut |t, c, v| {
            out.push((t, c, v));
            Ok(())
        })
        .unwrap();
        out
    }

    // --- posture wiring (tsp-mc9m.41.1026.1) --------------------------------------------------
    //
    // The report pump's posture is a PURE INPUT to the axis remap (`remap::Posture`); its
    // runtime source is designed by tsp-mc9m.41.1026.6 and until that lands production
    // constructs the pump with Posture::Normal. The rotation convention (body rotated N°
    // clockwise; logical = rotate-clockwise(body vector, N)) is named and tested in remap.rs;
    // what is proven HERE is the wiring: only a whole-body rotation rotates the emitted
    // stream, on both the live path and the SYN_DROPPED resync path.

    /// Synthetic descriptor with the spatial axes (d-pad hat + both stick axes, so every
    /// rotated code is advertised by the spec) plus one binary trigger.
    fn posture_descriptor() -> Descriptor {
        Descriptor::from_toml(
            r#"
[identity]
id = "synthguide"
manufacturer = "PocketForge"
model = "Guide Rig (synthetic test descriptor)"
sdl_guid = "030000005e0400008e02000010010000"

[[inputs]]
id = "dpad"
kind = "hat"
ev_type = "EV_ABS"
code = "ABS_HAT0X,ABS_HAT0Y"

[[inputs]]
id = "lstick"
kind = "stick"
ev_type = "EV_ABS"
code = "ABS_X,ABS_Y"
x = { min = 0, max = 4095, fuzz = 0, flat = 0 }
y = { min = 0, max = 4095, fuzz = 0, flat = 0 }

[[inputs]]
id = "ltrig"
kind = "trigger"
ev_type = "EV_ABS"
code = "ABS_Z"
semantics = "binary"
range = { min = 0, max = 255, fuzz = 0, flat = 0 }
"#,
        )
        .unwrap()
    }

    fn posture_pump() -> ReportPump {
        ReportPump::new(Remap::from_descriptor(&posture_descriptor()).unwrap())
    }

    #[test]
    fn pump_rotates_axes_only_for_whole_body_rotation() {
        use crate::remap::{Posture, Rotation};
        let hat_left = [ev(ioc::EV_ABS, 0x10 /* ABS_HAT0X */, -1), syn()];
        // Production wiring: Posture::Normal. Positive control — the d-pad passes
        // through untouched, exactly the stream from before postures existed.
        let mut pump = posture_pump();
        assert_eq!(pump.posture, Posture::Normal);
        let out = run(&mut pump, &hat_left);
        assert!(out.contains(&(ioc::EV_ABS, 0x10, -1)), "{out:?}");
        // Negative control for the tate remap: a screen-only swivel rotates the PICTURE
        // but not the control deck, so the gamepad stream is unchanged at every angle.
        for angle in [Rotation::Cw90, Rotation::Cw180, Rotation::Cw270] {
            let mut pump = posture_pump();
            pump.posture = Posture::ScreenOnlyRotated(angle);
            let out = run(&mut pump, &hat_left);
            assert!(out.contains(&(ioc::EV_ABS, 0x10, -1)), "{angle:?}: {out:?}");
        }
        // Whole-body rotation (tate): the body-left d-pad push is world-up after a 90° cw
        // body rotation, so it emits logical up (HAT0Y = -1) and the physical HAT0X code
        // never reaches the app.
        let mut pump = posture_pump();
        pump.posture = Posture::WholeBodyRotated(Rotation::Cw90);
        let out = run(&mut pump, &hat_left);
        assert!(
            out.contains(&(ioc::EV_ABS, 0x11 /* ABS_HAT0Y */, -1)),
            "{out:?}"
        );
        assert!(
            !out.iter()
                .any(|&(t, code, _)| t == ioc::EV_ABS && code == 0x10),
            "{out:?}"
        );
        // Stick axes (UNSIGNED 0..4095, the a133's real calibration shape) ride the same
        // rotation with values kept inside the advertised range: the copy branch keeps the
        // value, the negating branch reflects it about the range (4095−v), never about zero.
        let out = run(&mut pump, &[ev(ioc::EV_ABS, 0x00 /* ABS_X */, 4095), syn()]);
        assert!(
            out.contains(&(ioc::EV_ABS, 0x01 /* ABS_Y */, 4095)),
            "{out:?}"
        );
        assert!(
            !out.iter()
                .any(|&(t, code, _)| t == ioc::EV_ABS && code == 0x00),
            "{out:?}"
        );
        let out = run(&mut pump, &[ev(ioc::EV_ABS, 0x01 /* ABS_Y */, 0), syn()]);
        assert!(
            out.contains(&(ioc::EV_ABS, 0x00 /* ABS_X */, 4095)),
            "body-up stick must emit logical-right at full scale, not -0: {out:?}"
        );
        assert!(
            !out.iter()
                .any(|&(t, code, _)| t == ioc::EV_ABS && code == 0x01),
            "{out:?}"
        );
        // The binary trigger (DEPTH, not direction) emits its identical button under any
        // posture and never leaks onto the axis wire.
        let out = run(&mut pump, &[ev(ioc::EV_ABS, 0x02 /* ABS_Z */, 255), syn()]);
        assert!(
            out.contains(&(ioc::EV_KEY, 0x138 /* BTN_TL2 */, 1)),
            "{out:?}"
        );
        assert!(
            !out.iter()
                .any(|&(t, code, _)| t == ioc::EV_ABS && code == 0x02),
            "{out:?}"
        );
    }

    #[test]
    fn resync_under_tate_reexpresses_held_axes_in_the_logical_frame() {
        use crate::remap::{Posture, Rotation};
        let mut pump = posture_pump();
        pump.posture = Posture::WholeBodyRotated(Rotation::Cw90);
        // Live path: physical HAT0Y = 1 (body-down) emits logical HAT0X = -1, so the
        // pump's held state after the report is the LOGICAL frame.
        let out = run(&mut pump, &[ev(ioc::EV_ABS, 0x11, 1), syn()]);
        assert!(out.contains(&(ioc::EV_ABS, 0x10, -1)), "{out:?}");
        // The source now holds HAT0X = 1 physically, released HAT0Y, holds the left stick
        // full-right (ABS_X = 4095) with ABS_Y = 0. After SYN_DROPPED the resync reads the
        // body-frame truth and must re-express it in the logical frame: logical HAT0X back
        // to 0, logical HAT0Y to 1, logical ABS_Y to 4095 and logical ABS_X to 4095
        // (0 reflected about the unsigned range, NOT -0/about zero). A resync that wrote
        // the unrotated source values would instead emit the spurious (HAT0X, 1) and miss
        // the stick corrections entirely.
        let source = FakeSource {
            pressed: Vec::new(),
            abs: vec![(0x10, 1), (0x00, 4095)],
        };
        let out = run_with(
            &mut pump,
            &[ev(ioc::EV_SYN, ioc::SYN_DROPPED, 0), syn()],
            &source,
        );
        assert!(out.contains(&(ioc::EV_ABS, 0x10, 0)), "{out:?}");
        assert!(out.contains(&(ioc::EV_ABS, 0x11, 1)), "{out:?}");
        assert!(
            out.contains(&(ioc::EV_ABS, 0x01 /* ABS_Y */, 4095)),
            "{out:?}"
        );
        assert!(
            out.contains(&(ioc::EV_ABS, 0x00 /* ABS_X */, 4095)),
            "{out:?}"
        );
        assert!(
            !out.iter()
                .any(|&(t, c, v)| t == ioc::EV_ABS && c == 0x10 && v == 1),
            "{out:?}"
        );
    }

    fn scratch(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("pf-input-broker-{tag}-{}", std::process::id()))
    }

    fn intake(sock: &Path) -> (SafeReturnIntake, Receiver<String>) {
        let (tx, rx) = channel();
        let intake = SafeReturnIntake::spawn_with(
            sock,
            CONNECT_TIMEOUT,
            IO_TIMEOUT,
            Box::new(move |line| {
                let _ = tx.send(line.to_owned());
            }),
        )
        .unwrap();
        (intake, rx)
    }

    fn guide_press_release() -> Vec<libc::input_event> {
        vec![key(GUIDE_CODE, 1), syn(), key(GUIDE_CODE, 0), syn()]
    }

    #[test]
    fn without_flag_guide_passes_through_unchanged() {
        let mut p = pump(None);
        assert_eq!(
            run(&mut p, &guide_press_release()),
            vec![
                (ioc::EV_KEY, GUIDE_CODE, 1),
                (ioc::EV_SYN, ioc::SYN_REPORT, 0),
                (ioc::EV_KEY, GUIDE_CODE, 0),
                (ioc::EV_SYN, ioc::SYN_REPORT, 0),
            ]
        );
    }

    #[test]
    fn guide_is_suppressed_while_other_codes_pass_through() {
        let sock = scratch("suppress-absent.sock");
        let _ = std::fs::remove_file(&sock);
        let (intake, _log) = intake(&sock);
        let mut p = pump(Some(intake));
        let out = run(
            &mut p,
            &[
                key(SOUTH, 1),
                key(GUIDE_CODE, 1),
                syn(),
                key(GUIDE_CODE, 2),
                syn(),
                key(GUIDE_CODE, 0),
                key(SOUTH, 0),
                syn(),
            ],
        );
        assert_eq!(
            out,
            vec![
                (ioc::EV_KEY, SOUTH, 1),
                (ioc::EV_SYN, ioc::SYN_REPORT, 0),
                (ioc::EV_SYN, ioc::SYN_REPORT, 0),
                (ioc::EV_KEY, SOUTH, 0),
                (ioc::EV_SYN, ioc::SYN_REPORT, 0),
            ]
        );
        assert!(!out.iter().any(|&(_, code, _)| code == GUIDE_CODE));
        assert!(!p.pressed.contains(&GUIDE_CODE));
    }

    #[test]
    fn fake_authority_receives_exactly_one_frame_per_press() {
        let sock = scratch("one-frame.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        listener.set_nonblocking(true).unwrap();
        let (frames_tx, frames) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = stop.clone();
        let server = std::thread::spawn(move || {
            // Bounded polling wait, not an assertion window: the thread exits on the stop
            // flag, and a connection that never arrives fails the test through the bounded
            // channel receives in the test body.
            while !server_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        let body = pf_wire::read_frame(&mut stream).unwrap();
                        // Publish the frame BEFORE answering: the intake worker can observe
                        // "ok" only after this send, so once the test has seen the outcome
                        // line and both gates are idle, every completed round trip's frame
                        // is already in the channel — the final drain is exhaustive with a
                        // zero window.
                        frames_tx.send(body).unwrap();
                        pf_wire::write_frame(&mut stream, br#"{"result":"ok"}"#).unwrap();
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(e) => panic!("{e}"),
                }
            }
        });
        let (intake, log) = intake(&sock);
        let mut p = pump(Some(intake));
        for press in 1..=2 {
            // A press, autorepeat and release are one press edge.
            run(
                &mut p,
                &[
                    key(GUIDE_CODE, 1),
                    syn(),
                    key(GUIDE_CODE, 2),
                    syn(),
                    key(GUIDE_CODE, 0),
                    syn(),
                ],
            );
            // Event-driven receives with generous bounds: the "ok" line is emitted after the
            // intake worker re-armed its gate, and the router barrier drains its command
            // queue, so the flag asserts below are direct — no polling window.
            let line = log.recv_timeout(Duration::from_secs(10)).unwrap();
            assert!(line.ends_with(": ok"), "press {press}: {line}");
            assert_eq!(
                frames.recv_timeout(Duration::from_secs(10)).unwrap(),
                SAFE_RETURN_BODY
            );
            let router = p.system_menu.as_ref().unwrap().router();
            router.sync().unwrap();
            assert!(
                !router.in_flight() && !router.fallback_in_flight(),
                "press {press} left a request in flight"
            );
        }
        // Zero-window negative control: idle gates mean no dispatch can still be in flight,
        // and the fake authority publishes every frame before answering, so a duplicate frame
        // from a regressed coalescing path would already sit in the channel.
        assert!(frames.try_recv().is_err(), "no frame beyond one per press");
        stop.store(true, Ordering::Release);
        server.join().unwrap();
        let _ = std::fs::remove_file(sock);
    }

    #[test]
    fn stalled_authority_never_blocks_the_input_pump() {
        // Bound but never accepting or answering: a single-threaded authority stuck in
        // `systemctl stop`. The worker waits out its full production 5 s read timeout.
        let sock = scratch("stalled.sock");
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();
        let (intake, log) = intake(&sock);
        let mut p = pump(Some(intake));
        let mut events = Vec::new();
        for _ in 0..50 {
            events.extend(guide_press_release());
            events.extend([key(SOUTH, 1), syn(), key(SOUTH, 0), syn()]);
        }
        let started = Instant::now();
        let out = run(&mut p, &events);
        // Generous liveness bound, not a synchronisation window: the pump path performs no
        // I/O, so a healthy run finishes in milliseconds; only a regression that blocks the
        // pump on the authority (which would stall on the 5 s socket read) can reach 1 s.
        let elapsed = started.elapsed();
        assert!(elapsed < Duration::from_secs(1), "pump took {elapsed:?}");
        let south: Vec<_> = out.iter().filter(|e| e.1 == SOUTH).collect();
        assert_eq!(south.len(), 100, "every non-guide event still flows");
        assert!(!out.iter().any(|&(_, code, _)| code == GUIDE_CODE));

        let router = p.system_menu.as_ref().unwrap().router().clone();
        // FIFO barrier: once sync() returns, all 50 presses have been coalesced or processed.
        // Exactly one of them dispatched a SafeReturn request, which is stalled on the socket
        // read (the authority never answers; the production IO_TIMEOUT of 5 s has not
        // elapsed), so the fallback gate must still be armed — a direct assert, no polling
        // window.
        router.sync().unwrap();
        assert!(
            router.fallback_in_flight(),
            "the first press is still waiting on fallback return"
        );

        // The dispatched request surfaces as exactly one connection in the listener's
        // backlog. Accept it on a blocking helper thread bounded by a channel timeout
        // instead of racing a non-blocking accept against a sleep.
        let blocking = listener.try_clone().unwrap();
        let (conn_tx, conn_rx) = channel();
        std::thread::spawn(move || {
            let _ = conn_tx.send(blocking.accept());
        });
        let (first, _) = conn_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("no SafeReturn connection within 10 s")
            .expect("accept failed");
        drop(first);
        // Dropping the connection ends the worker's stalled round trip with an error. The
        // intake worker re-arms its gate BEFORE emitting the outcome line, so receiving that
        // line proves the request completed and nothing can still be pending: the router
        // barrier above drained its command queue, and its busy gate coalesced all 50
        // presses into the single dispatch.
        let line = log
            .recv_timeout(Duration::from_secs(10))
            .expect("no safe_return outcome line within 10 s");
        assert!(
            line.contains("safe_return") && line.contains("failed"),
            "{line}"
        );
        assert!(
            !router.in_flight() && !router.fallback_in_flight(),
            "the completed round trip must re-arm both gates"
        );
        // Zero-window negative control: with the command queue drained and both gates
        // re-armed, no further dispatch can exist, so a second connection could only come
        // from a regression that re-dispatched a coalesced press — which would have opened
        // it before the outcome line above was ever logged.
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock,
            "coalesced presses must not open more connections"
        );
        let _ = std::fs::remove_file(sock);
    }

    #[test]
    fn absent_authority_is_logged_and_the_pump_continues() {
        let sock = scratch("absent.sock");
        let _ = std::fs::remove_file(&sock);
        let (intake, log) = intake(&sock);
        let mut p = pump(Some(intake));
        let out = run(&mut p, &guide_press_release());
        // The absent socket fails the round trip immediately (NotFound), and the intake worker
        // re-arms its gate BEFORE emitting the outcome line, so receiving the line is the
        // completion event itself — a generous bound that only a regression can reach, not a
        // timing window.
        let line = log.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(
            line.contains("safe_return") && line.contains("failed") && line.contains("NotFound"),
            "{line}"
        );
        assert!(!out.iter().any(|&(_, code, _)| code == GUIDE_CODE));
        // The router barrier proves the Press command was fully processed (its busy gate
        // re-armed), so the flag asserts are direct and the next press cannot be silently
        // coalesced into a completed request.
        let router = p.system_menu.as_ref().unwrap().router().clone();
        router.sync().unwrap();
        assert!(
            !router.in_flight() && !router.fallback_in_flight(),
            "first failed return did not become idle"
        );
        let out = run(&mut p, &[key(SOUTH, 1), syn()]);
        assert_eq!(
            out,
            vec![(ioc::EV_KEY, SOUTH, 1), (ioc::EV_SYN, ioc::SYN_REPORT, 0)]
        );
        run(&mut p, &guide_press_release());
        let again = log.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(again.contains("failed"), "the next press retries: {again}");
    }

    #[test]
    fn syn_dropped_resync_withholds_guide_and_counts_its_press() {
        let sock = scratch("resync-absent.sock");
        let _ = std::fs::remove_file(&sock);
        let (intake, log) = intake(&sock);
        let mut p = pump(Some(intake));
        let source = FakeSource {
            pressed: vec![SOUTH, GUIDE_CODE],
            abs: Vec::new(),
        };
        let out = run_with(
            &mut p,
            &[
                ev(ioc::EV_SYN, ioc::SYN_DROPPED, 0),
                key(GUIDE_CODE, 1),
                syn(),
            ],
            &source,
        );
        assert_eq!(
            out,
            vec![(ioc::EV_KEY, SOUTH, 1), (ioc::EV_SYN, ioc::SYN_REPORT, 0)]
        );
        assert!(!p.pressed.contains(&GUIDE_CODE));
        // Event-driven completion: the press edge's fallback attempt fails immediately on
        // the absent socket and the outcome line arrives through the channel; the bound is
        // generous and only a regression can reach it.
        assert!(
            log.recv_timeout(Duration::from_secs(10)).is_ok(),
            "press edge requested"
        );
    }

    // --- the persistent PFW1 session loop -----------------------------------------------------

    fn node_file(tag: &str) -> PathBuf {
        let path = scratch(tag);
        std::fs::write(&path, b"re-emit-node").unwrap();
        path
    }

    fn backend() -> Arc<dyn Backend> {
        Arc::new(InProcessBackend::new(Arc::new(descriptor())))
    }

    fn serve_pair(node: &Path) -> (UnixStream, std::thread::JoinHandle<io::Result<()>>) {
        let (client, server) = UnixStream::pair().unwrap();
        let node = node.to_str().unwrap().to_owned();
        let backend = backend();
        let handle = std::thread::spawn(move || serve_client(server, &node, &*backend));
        (client, handle)
    }

    fn acquire_on(client: &UnixStream) -> (Response, Option<OwnedFd>) {
        pf_wire::send_request(&mut &*client, &Request::new(Op::Acquire, "input")).unwrap();
        let mut buf = [0u8; 256];
        let (n, fd) = scm::recv_fd(client.as_raw_fd(), &mut buf).unwrap();
        let resp = pf_wire::recv_response(&mut io::Cursor::new(&buf[..n])).unwrap();
        (resp, fd)
    }

    fn call(client: &UnixStream, op: Op, name: &str) -> Response {
        pf_wire::send_request(&mut &*client, &Request::new(op, name)).unwrap();
        pf_wire::recv_response(&mut &*client).unwrap()
    }

    #[test]
    fn two_acquires_on_one_connection_both_return_fds() {
        let node = node_file("two-acquires.node");
        let (client, server) = serve_pair(&node);
        let (first, fd1) = acquire_on(&client);
        let (second, fd2) = acquire_on(&client);
        assert_eq!(first.status, Status::Ok);
        assert_eq!(second.status, Status::Ok);
        let (fd1, fd2) = (fd1.expect("first fd"), fd2.expect("second fd"));
        assert_ne!(fd1.as_raw_fd(), fd2.as_raw_fd());
        for fd in [&fd1, &fd2] {
            let mut buf = [0u8; 12];
            let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            assert_eq!(&buf[..n as usize], b"re-emit-node");
        }
        drop(client);
        server.join().unwrap().unwrap();
        let _ = std::fs::remove_file(node);
    }

    #[test]
    fn preference_reads_are_served_and_mutating_ops_stay_unsupported() {
        let node = node_file("appearance.node");
        let (client, server) = serve_pair(&node);
        let appearance = call(&client, Op::GetAppearance, "");
        assert_eq!(appearance.status, Status::Ok);
        assert!(
            appearance.flag <= 2,
            "a valid Appearance: {}",
            appearance.flag
        );
        for (op, name) in [
            (Op::IsPresent, "input"),
            (Op::Acquire, "imu"),
            (Op::SetCapability, "settings"),
        ] {
            assert_eq!(
                call(&client, op, name).status,
                Status::Unsupported,
                "{op:?}"
            );
        }
        assert_eq!(
            call(&client, Op::GetAppearanceSource, "").status,
            Status::Ok
        );
        assert_eq!(call(&client, Op::GetPreference, "").status, Status::Ok);
        // The session survives all of that: input can still be acquired.
        assert!(acquire_on(&client).1.is_some());
        drop(client);
        server.join().unwrap().unwrap();
        let _ = std::fs::remove_file(node);
    }

    #[test]
    fn broker_client_backend_reacquires_after_dropping_its_fd() {
        let node = node_file("reacquire.node");
        let (client, server) = serve_pair(&node);
        let be = BrokerClientBackend::from_stream(client);
        let fd = be.acquire_input_fd().expect("first acquire");
        drop(fd);
        let _ = be.appearance(); // a facade poll between acquisitions keeps the session
        let fd = be.acquire_input_fd().expect("re-acquire after drop");
        let mut buf = [0u8; 12];
        let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        assert_eq!(&buf[..n as usize], b"re-emit-node");
        drop(be);
        server.join().unwrap().unwrap();
        let _ = std::fs::remove_file(node);
    }

    #[test]
    fn missing_node_is_a_typed_refusal_and_the_session_survives() {
        let node = scratch("missing.node");
        let _ = std::fs::remove_file(&node);
        let (client, server) = serve_pair(&node);
        pf_wire::send_request(&mut &client, &Request::new(Op::Acquire, "input")).unwrap();
        let resp = pf_wire::recv_response(&mut &client).unwrap();
        assert_eq!(resp.status, Status::HardwareAbsent);
        assert_eq!(call(&client, Op::GetAppearance, "").status, Status::Ok);
        drop(client);
        server.join().unwrap().unwrap();
    }

    #[test]
    fn client_stalled_mid_frame_is_deadline_bounded() {
        // The ceiling is the SUBJECT of the test, not a synchronisation window: serve_client
        // must cut a mid-frame stall at the acquisition I/O deadline (250 ms), so a healthy
        // run finishes at the deadline; only a regression to unbounded blocking can reach
        // the 1 s assert bound.
        let node = node_file("stalled-frame.node");
        let (mut client, server) = serve_pair(&node);
        client.write_all(&[0, 0]).unwrap(); // half a length prefix, then silence
        let started = Instant::now();
        server.join().unwrap().unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        let _ = std::fs::remove_file(node);
    }
}
