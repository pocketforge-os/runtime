//! Fail-closed authorization for creating a host-kernel input device.

use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub const REFUSAL_REASON: &str = "unsafe_uinput_domain";
pub const ALLOW_REASON: &str = "safe_uinput_domain";
pub const BUILDER_MARKER: &str = "/etc/pocketforge/ephemeral-runner.conf";
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
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
    pub probe: Option<&'static str>,
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "reason={} domain={}", self.reason, self.domain.as_str())?;
        if let Some(probe) = self.probe {
            write!(f, " probe={probe}")?;
        }
        Ok(())
    }
}

fn verdict(allowed: bool, domain: Domain, probe: Option<&'static str>) -> Verdict {
    Verdict {
        allowed,
        reason: if allowed {
            ALLOW_REASON
        } else {
            REFUSAL_REASON
        },
        domain,
        probe,
    }
}

fn complete_set(values: &[String], required: &[&str]) -> bool {
    required
        .iter()
        .all(|wanted| values.iter().any(|got| got == wanted))
}

pub fn evaluate(probe: &Probe) -> Verdict {
    if probe.container == Check::Error {
        return verdict(false, Domain::ProbeError, Some("systemd-detect-virt"));
    }
    if probe.container == Check::Yes {
        return verdict(false, Domain::Container, None);
    }
    if probe.vm == Check::Error {
        return verdict(false, Domain::ProbeError, Some("systemd-detect-virt"));
    }
    if probe.dt == DtState::Error {
        return verdict(false, Domain::ProbeError, Some("device-tree-compatible"));
    }
    let current = probe.dt == DtState::Ok && complete_set(&probe.compatibles, CURRENT_COMPATIBLES);
    let legacy = probe.dt == DtState::Ok && complete_set(&probe.compatibles, LEGACY_COMPATIBLES);
    match probe.vm {
        Check::Yes if current => return verdict(true, Domain::PfvdGuest, None),
        Check::No if current || legacy => return verdict(true, Domain::Dut, None),
        _ => {}
    }
    if probe.vm == Check::Yes {
        if probe.marker == MarkerState::Error {
            return verdict(false, Domain::ProbeError, Some("builder-marker"));
        }
        if probe.marker != MarkerState::Valid {
            return verdict(false, Domain::UntrustedVm, None);
        }
        return match probe.logind {
            LogindState::NoSeat => verdict(true, Domain::PfBuilderVm, None),
            LogindState::Seat => verdict(false, Domain::VmWithSeat, None),
            LogindState::Unreadable | LogindState::NotProbed => {
                verdict(false, Domain::ProbeError, Some("loginctl"))
            }
        };
    }
    if probe.dt == DtState::Ok && !probe.compatibles.is_empty() {
        verdict(false, Domain::DutMismatch, None)
    } else {
        verdict(false, Domain::BareMetal, None)
    }
}

fn run_command(program: &Path, args: &[&str], timeout: Duration) -> io::Result<Output> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            let stdout = reader
                .join()
                .map_err(|_| io::Error::other("probe output reader panicked"))??;
            return Ok(Output {
                status,
                stdout,
                stderr: Vec::new(),
            });
        }
        if started.elapsed() >= timeout {
            child.kill()?;
            child.wait()?;
            let _ = reader.join();
            return Err(io::Error::new(io::ErrorKind::TimedOut, "probe timed out"));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn command_check_program(program: &Path, args: &[&str], timeout: Duration) -> Check {
    match run_command(program, args, timeout) {
        Ok(output) if output.status.success() => Check::Yes,
        Ok(output) if output.status.code() == Some(1) => Check::No,
        _ => Check::Error,
    }
}

fn command_check(args: &[&str]) -> Check {
    command_check_program(Path::new("systemd-detect-virt"), args, PROBE_TIMEOUT)
}

fn parse_dt_bytes(bytes: &[u8]) -> (DtState, Vec<String>) {
    if !bytes.is_empty() && !bytes.ends_with(&[0]) {
        return (DtState::Error, Vec::new());
    }
    let mut values = Vec::new();
    for value in bytes.split(|b| *b == 0).filter(|v| !v.is_empty()) {
        match std::str::from_utf8(value) {
            Ok(value) => values.push(value.to_owned()),
            Err(_) => return (DtState::Error, Vec::new()),
        }
    }
    (DtState::Ok, values)
}

fn probe_dt() -> (DtState, Vec<String>) {
    match fs::read("/proc/device-tree/compatible") {
        Ok(bytes) => parse_dt_bytes(&bytes),
        Err(error) if error.kind() == io::ErrorKind::NotFound => (DtState::Missing, Vec::new()),
        Err(_) => (DtState::Error, Vec::new()),
    }
}

fn probe_marker(path: &Path) -> MarkerState {
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return MarkerState::Missing,
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => return MarkerState::Invalid,
        Err(_) => return MarkerState::Error,
    };
    match file.metadata() {
        Ok(meta) if meta.is_file() && meta.uid() == 0 && meta.mode() & 0o022 == 0 => {
            MarkerState::Valid
        }
        Ok(_) => MarkerState::Invalid,
        Err(_) => MarkerState::Error,
    }
}

fn parse_session_seat(show: &[u8]) -> Option<bool> {
    let text = std::str::from_utf8(show).ok()?;
    let mut ty = None;
    let mut seat = None;
    for property in text.lines() {
        if let Some(value) = property.strip_prefix("Type=") {
            if ty.replace(value).is_some() {
                return None;
            }
        } else {
            let value = property.strip_prefix("Seat=")?;
            if seat.replace(value).is_some() {
                return None;
            }
        }
    }
    let (Some(_), Some(seat)) = (ty, seat) else {
        return None;
    };
    Some(!seat.is_empty())
}

fn probe_logind() -> LogindState {
    let list = match run_command(
        Path::new("loginctl"),
        &["list-sessions", "--no-legend", "--no-pager"],
        PROBE_TIMEOUT,
    ) {
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
        let show = match run_command(
            Path::new("loginctl"),
            &["show-session", id, "--no-pager", "-p", "Type", "-p", "Seat"],
            PROBE_TIMEOUT,
        ) {
            Ok(output) if output.status.success() => output.stdout,
            _ => return LogindState::Unreadable,
        };
        match parse_session_seat(&show) {
            Some(true) => return LogindState::Seat,
            Some(false) => {}
            None => return LogindState::Unreadable,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    fn scratch() -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("pf-uinput-domain-{}-{nonce}", std::process::id()));
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn sleeping_and_missing_process_probes_fail_closed() {
        let root = scratch();
        let sleeper = root.join("sleeping-probe");
        fs::write(&sleeper, "#!/bin/sh\nexec /bin/sleep 5\n").unwrap();
        fs::set_permissions(&sleeper, fs::Permissions::from_mode(0o755)).unwrap();
        let missing = root.join("missing-probe");
        let timeout = Duration::from_millis(50);

        let started = Instant::now();
        assert_eq!(
            command_check_program(&sleeper, &["--quiet"], timeout),
            Check::Error
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(
            command_check_program(&missing, &["--quiet"], timeout),
            Check::Error
        );
        assert!(run_command(&sleeper, &["list-sessions"], timeout).is_err());
        assert!(run_command(&missing, &["list-sessions"], timeout).is_err());

        fs::remove_file(sleeper).unwrap();
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn parser_and_marker_malformed_inputs_refuse() {
        assert_eq!(
            parse_dt_bytes(b"pocketforge,tsp\0allwinner,sun50i-a133\0allwinner,sun50i-a100"),
            (DtState::Error, Vec::new())
        );
        for ty in [
            "x11",
            "wayland",
            "tty",
            "mir",
            "web",
            "unspecified",
            "",
            "future",
        ] {
            let properties = format!("Type={ty}\nSeat=seat0\n");
            assert_eq!(
                parse_session_seat(properties.as_bytes()),
                Some(true),
                "{ty}"
            );
        }
        assert_eq!(parse_session_seat(b"Type=mir\nSeat=\n"), Some(false));

        let root = scratch();
        let link = root.join("marker");
        symlink(root.join("missing-target"), &link).unwrap();
        assert_eq!(probe_marker(&link), MarkerState::Invalid);
        fs::remove_file(link).unwrap();
        fs::remove_dir(root).unwrap();
    }
}
