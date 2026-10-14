//! Fail-closed authorization for creating a host-kernel input device.

use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;

pub const REFUSAL_REASON: &str = "unsafe_uinput_domain";
pub const ALLOW_REASON: &str = "safe_uinput_domain";
pub const BUILDER_MARKER: &str = "/etc/pocketforge/ephemeral-runner.conf";
pub const CURRENT_COMPATIBLES: &[&str] = &[
    "pocketforge,tsp",
    "allwinner,sun50i-a133",
    "allwinner,sun50i-a100",
];
pub const LEGACY_COMPATIBLES: &[&str] = &["allwinner,a133", "arm,sun50iw10p1"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Check {
    No,
    Yes,
    Error,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DtState {
    Missing,
    Ok,
    Error,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MarkerState {
    Missing,
    Valid,
    Invalid,
    Error,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogindState {
    NotProbed,
    NoSeat,
    Seat,
    Unreadable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Probe {
    pub container: Check,
    pub vm: Check,
    pub dt: DtState,
    pub compatibles: Vec<String>,
    pub marker: MarkerState,
    pub logind: LogindState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Domain {
    PfvdGuest,
    Dut,
    PfBuilderVm,
    Container,
    VmWithSeat,
    UntrustedVm,
    DutMismatch,
    BareMetal,
    ProbeError,
}

impl Domain {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PfvdGuest => "pfvd-guest",
            Self::Dut => "dut",
            Self::PfBuilderVm => "pf-builder-vm",
            Self::Container => "container",
            Self::VmWithSeat => "vm-with-seat",
            Self::UntrustedVm => "untrusted-vm",
            Self::DutMismatch => "dut-mismatch",
            Self::BareMetal => "bare-metal",
            Self::ProbeError => "probe-error",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Verdict {
    pub allowed: bool,
    pub reason: &'static str,
    pub domain: Domain,
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "reason={} domain={}", self.reason, self.domain.as_str())
    }
}

fn verdict(allowed: bool, domain: Domain) -> Verdict {
    Verdict {
        allowed,
        reason: if allowed {
            ALLOW_REASON
        } else {
            REFUSAL_REASON
        },
        domain,
    }
}

fn complete_set(values: &[String], required: &[&str]) -> bool {
    required
        .iter()
        .all(|wanted| values.iter().any(|got| got == wanted))
}

pub fn evaluate(probe: &Probe) -> Verdict {
    if probe.container == Check::Error {
        return verdict(false, Domain::ProbeError);
    }
    if probe.container == Check::Yes {
        return verdict(false, Domain::Container);
    }
    if probe.vm == Check::Error || probe.dt == DtState::Error {
        return verdict(false, Domain::ProbeError);
    }
    let current = probe.dt == DtState::Ok && complete_set(&probe.compatibles, CURRENT_COMPATIBLES);
    let legacy = probe.dt == DtState::Ok && complete_set(&probe.compatibles, LEGACY_COMPATIBLES);
    match probe.vm {
        Check::Yes if current => return verdict(true, Domain::PfvdGuest),
        Check::No if current || legacy => return verdict(true, Domain::Dut),
        _ => {}
    }
    if probe.vm == Check::Yes {
        if probe.marker == MarkerState::Error {
            return verdict(false, Domain::ProbeError);
        }
        if probe.marker != MarkerState::Valid {
            return verdict(false, Domain::UntrustedVm);
        }
        return match probe.logind {
            LogindState::NoSeat => verdict(true, Domain::PfBuilderVm),
            LogindState::Seat => verdict(false, Domain::VmWithSeat),
            LogindState::Unreadable | LogindState::NotProbed => verdict(false, Domain::ProbeError),
        };
    }
    if probe.dt == DtState::Ok && !probe.compatibles.is_empty() {
        verdict(false, Domain::DutMismatch)
    } else {
        verdict(false, Domain::BareMetal)
    }
}

fn command_check(args: &[&str]) -> Check {
    match Command::new("systemd-detect-virt").args(args).status() {
        Ok(status) if status.success() => Check::Yes,
        Ok(status) if status.code() == Some(1) => Check::No,
        _ => Check::Error,
    }
}

fn probe_dt() -> (DtState, Vec<String>) {
    match fs::read("/proc/device-tree/compatible") {
        Ok(bytes) => {
            let mut values = Vec::new();
            for value in bytes.split(|b| *b == 0).filter(|v| !v.is_empty()) {
                match std::str::from_utf8(value) {
                    Ok(value) => values.push(value.to_owned()),
                    Err(_) => return (DtState::Error, Vec::new()),
                }
            }
            (DtState::Ok, values)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => (DtState::Missing, Vec::new()),
        Err(_) => (DtState::Error, Vec::new()),
    }
}

fn probe_marker(path: &Path) -> MarkerState {
    match fs::metadata(path) {
        Ok(meta) if meta.is_file() && meta.uid() == 0 && meta.mode() & 0o022 == 0 => {
            match fs::File::open(path) {
                Ok(_) => MarkerState::Valid,
                Err(_) => MarkerState::Error,
            }
        }
        Ok(_) => MarkerState::Invalid,
        Err(error) if error.kind() == io::ErrorKind::NotFound => MarkerState::Missing,
        Err(_) => MarkerState::Error,
    }
}

fn probe_logind() -> LogindState {
    let list = match Command::new("loginctl")
        .args(["list-sessions", "--no-legend", "--no-pager"])
        .output()
    {
        Ok(output) if output.status.success() => output.stdout,
        _ => return LogindState::Unreadable,
    };
    let text = match std::str::from_utf8(&list) {
        Ok(text) => text,
        Err(_) => return LogindState::Unreadable,
    };
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let Some(id) = line.split_whitespace().next() else {
            return LogindState::Unreadable;
        };
        if !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b))
        {
            return LogindState::Unreadable;
        }
        let show = match Command::new("loginctl")
            .args(["show-session", id, "--no-pager", "-p", "Type", "-p", "Seat"])
            .output()
        {
            Ok(output) if output.status.success() => output.stdout,
            _ => return LogindState::Unreadable,
        };
        let show = match std::str::from_utf8(&show) {
            Ok(text) => text,
            Err(_) => return LogindState::Unreadable,
        };
        let mut ty = None;
        let mut seat = None;
        for property in show.lines() {
            if let Some(value) = property.strip_prefix("Type=") {
                if ty.replace(value).is_some() {
                    return LogindState::Unreadable;
                }
            } else if let Some(value) = property.strip_prefix("Seat=") {
                if seat.replace(value).is_some() {
                    return LogindState::Unreadable;
                }
            }
        }
        let (Some(ty), Some(seat)) = (ty, seat) else {
            return LogindState::Unreadable;
        };
        if !seat.is_empty() && matches!(ty, "x11" | "wayland" | "tty") {
            return LogindState::Seat;
        }
    }
    LogindState::NoSeat
}

pub fn probe_host() -> Probe {
    let container = command_check(&["--container", "--quiet"]);
    let vm = if container == Check::No {
        command_check(&["--vm", "--quiet"])
    } else {
        Check::No
    };
    let (dt, compatibles) = probe_dt();
    let marker = if vm == Check::Yes {
        probe_marker(Path::new(BUILDER_MARKER))
    } else {
        MarkerState::Missing
    };
    let logind = if vm == Check::Yes && marker == MarkerState::Valid {
        probe_logind()
    } else {
        LogindState::NotProbed
    };
    Probe {
        container,
        vm,
        dt,
        compatibles,
        marker,
        logind,
    }
}

pub fn require_safe(probe: &Probe) -> io::Result<Domain> {
    let result = evaluate(probe);
    if result.allowed {
        Ok(result.domain)
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            result.to_string(),
        ))
    }
}

pub fn require_safe_host() -> io::Result<Domain> {
    require_safe(&probe_host())
}

pub fn guarded_open<T, F>(probe: &Probe, opener: F) -> io::Result<T>
where
    F: FnOnce() -> io::Result<T>,
{
    require_safe(probe)?;
    opener()
}

pub fn guarded_host_open<T, F>(opener: F) -> io::Result<T>
where
    F: FnOnce() -> io::Result<T>,
{
    require_safe_host()?;
    opener()
}
