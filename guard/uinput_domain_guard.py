"""Fail-closed execution-domain guard for Linux kernel input creators."""

from dataclasses import dataclass
from enum import Enum
import errno
import os
from pathlib import Path
import stat
import subprocess

REFUSAL_REASON = "unsafe_uinput_domain"
ALLOW_REASON = "safe_uinput_domain"
BUILDER_MARKER = Path("/etc/pocketforge/ephemeral-runner.conf")
CURRENT_COMPATIBLES = frozenset(("pocketforge,tsp", "allwinner,sun50i-a133", "allwinner,sun50i-a100"))
LEGACY_COMPATIBLES = frozenset(("allwinner,a133", "arm,sun50iw10p1"))
PROBE_TIMEOUT_SECONDS = 2.0


class Check(Enum):
    NO = "no"
    YES = "yes"
    ERROR = "error"


class Domain(Enum):
    PFVD_GUEST = "pfvd-guest"
    DUT = "dut"
    PF_BUILDER_VM = "pf-builder-vm"
    CONTAINER = "container"
    VM_WITH_SEAT = "vm-with-seat"
    UNTRUSTED_VM = "untrusted-vm"
    DUT_MISMATCH = "dut-mismatch"
    BARE_METAL = "bare-metal"
    PROBE_ERROR = "probe-error"


@dataclass(frozen=True)
class Probe:
    container: Check
    vm: Check
    dt: str
    compatibles: frozenset[str]
    marker: str
    logind: str


@dataclass(frozen=True)
class Verdict:
    allowed: bool
    reason: str
    domain: Domain
    probe: str | None = None

    def token(self):
        token = f"reason={self.reason} domain={self.domain.value}"
        return f"{token} probe={self.probe}" if self.probe else token


class UnsafeUinputDomain(PermissionError):
    pass


def _verdict(allowed, domain, probe=None):
    return Verdict(allowed, ALLOW_REASON if allowed else REFUSAL_REASON, domain, probe)


def evaluate(probe):
    if probe.container is Check.ERROR:
        return _verdict(False, Domain.PROBE_ERROR, "systemd-detect-virt")
    if probe.container is Check.YES:
        return _verdict(False, Domain.CONTAINER)
    if probe.vm is Check.ERROR:
        return _verdict(False, Domain.PROBE_ERROR, "systemd-detect-virt")
    if probe.dt == "error":
        return _verdict(False, Domain.PROBE_ERROR, "device-tree-compatible")
    current = probe.dt == "ok" and CURRENT_COMPATIBLES <= probe.compatibles
    legacy = probe.dt == "ok" and LEGACY_COMPATIBLES <= probe.compatibles
    if probe.vm is Check.YES and current:
        return _verdict(True, Domain.PFVD_GUEST)
    if probe.vm is Check.NO and (current or legacy):
        return _verdict(True, Domain.DUT)
    if probe.vm is Check.YES:
        if probe.marker == "error":
            return _verdict(False, Domain.PROBE_ERROR, "builder-marker")
        if probe.marker != "valid":
            return _verdict(False, Domain.UNTRUSTED_VM)
        if probe.logind in ("no-seat", "mir-seatless"):
            return _verdict(True, Domain.PF_BUILDER_VM)
        if probe.logind == "seat" or probe.logind.endswith("-seat"):
            return _verdict(False, Domain.VM_WITH_SEAT)
        return _verdict(False, Domain.PROBE_ERROR, "loginctl")
    if probe.dt == "ok" and probe.compatibles:
        return _verdict(False, Domain.DUT_MISMATCH)
    return _verdict(False, Domain.BARE_METAL)


def _command_check(*args, program="systemd-detect-virt", timeout=PROBE_TIMEOUT_SECONDS):
    try:
        result = subprocess.run((program, *args), stdin=subprocess.DEVNULL,
                                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False,
                                timeout=timeout)
    except (OSError, subprocess.TimeoutExpired):
        return Check.ERROR
    return Check.YES if result.returncode == 0 else Check.NO if result.returncode == 1 else Check.ERROR


def _probe_dt(path=Path("/proc/device-tree/compatible")):
    try:
        raw = path.read_bytes()
    except FileNotFoundError:
        return "missing", frozenset()
    except OSError:
        return "error", frozenset()
    if raw and not raw.endswith(b"\0"):
        return "error", frozenset()
    try:
        values = frozenset(part.decode("utf-8") for part in raw.split(b"\0") if part)
    except UnicodeDecodeError:
        return "error", frozenset()
    return "ok", values


def _probe_marker(path=BUILDER_MARKER):
    flags = os.O_RDONLY | getattr(os, "O_CLOEXEC", 0) | getattr(os, "O_NOFOLLOW", 0)
    try:
        fd = os.open(path, flags)
    except FileNotFoundError:
        return "missing"
    except OSError as error:
        return "invalid" if error.errno == errno.ELOOP else "error"
    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != 0 or info.st_mode & 0o022:
            return "invalid"
        return "valid"
    except OSError:
        return "error"
    finally:
        os.close(fd)


def _run_loginctl(*args, program="loginctl", timeout=PROBE_TIMEOUT_SECONDS):
    try:
        result = subprocess.run((program, *args), stdin=subprocess.DEVNULL,
                                stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                text=True, encoding="utf-8", check=False, timeout=timeout)
    except (OSError, UnicodeError, subprocess.TimeoutExpired):
        return None
    return result.stdout if result.returncode == 0 else None


def _probe_logind():
    listing = _run_loginctl("list-sessions", "--no-legend", "--no-pager")
    if listing is None:
        return "unreadable"
    for line in (line for line in listing.splitlines() if line.strip()):
        fields = line.split()
        if not fields:
            return "unreadable"
        session = fields[0]
        if not all(c.isalnum() or c in "_-." for c in session):
            return "unreadable"
        shown = _run_loginctl("show-session", session, "--no-pager", "-p", "Type", "-p", "Seat")
        if shown is None:
            return "unreadable"
        props = {}
        for line in shown.splitlines():
            if "=" not in line:
                return "unreadable"
            key, value = line.split("=", 1)
            if key in props:
                return "unreadable"
            props[key] = value
        if set(props) != {"Type", "Seat"}:
            return "unreadable"
        if props["Seat"]:
            return "seat"
    return "no-seat"


def probe_host():
    container = _command_check("--container", "--quiet")
    vm = _command_check("--vm", "--quiet") if container is Check.NO else Check.NO
    dt, compatibles = _probe_dt()
    marker = _probe_marker() if vm is Check.YES else "missing"
    logind = _probe_logind() if vm is Check.YES and marker == "valid" else "not-probed"
    return Probe(container, vm, dt, compatibles, marker, logind)


def require_safe(probe):
    result = evaluate(probe)
    if not result.allowed:
        raise UnsafeUinputDomain(result.token())
    return result.domain


def require_safe_host():
    return require_safe(probe_host())


def guarded_open(path, flags, mode=0o666, *, probe=None, opener=os.open):
    require_safe(probe if probe is not None else probe_host())
    return opener(path, flags, mode)
