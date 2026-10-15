use crate::protocol::{proc_start_time, ForceStopReason, TargetIdentity, TriggerRequest};
use crate::transport;
use std::io;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::process::Command;

fn process_handle(pid: i32) -> io::Result<OwnedFd> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32 };
    if fd >= 0 {
        return Ok(unsafe { OwnedFd::from_raw_fd(fd) });
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() != Some(libc::ENOSYS) {
        return Err(error);
    }
    std::fs::File::open(format!("/proc/{pid}")).map(Into::into)
}

fn main_pid(unit: &str) -> io::Result<i32> {
    let output = Command::new("systemctl")
        .args(["show", "--property=MainPID", "--value", unit])
        .output()?;
    if !output.status.success() || output.stdout.len() > 32 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "application has no main process",
        ));
    }
    let text = std::str::from_utf8(&output.stdout)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid MainPID"))?;
    let pid = text
        .trim()
        .parse::<i32>()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid MainPID"))?;
    if pid <= 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "application has no main process",
        ));
    }
    Ok(pid)
}

pub fn send_prepare(socket: &Path, item_id: &str, session_id: &str) -> io::Result<()> {
    pf_app_manifest::validate_app_id(item_id)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid application id"))?;
    let unit = format!("pf-app@{item_id}.service");
    let pid = main_pid(&unit)?;
    let handle = process_handle(pid)?;
    let request = TriggerRequest::Prepare {
        target: TargetIdentity {
            unit,
            pid,
            start_time: proc_start_time(pid)?,
        },
        session_id: session_id.to_owned(),
    };
    transport::send_request(socket, &request, Some(handle.as_fd()))
}

pub fn send_force_stop(
    socket: &Path,
    item_id: &str,
    session_id: &str,
    reason: ForceStopReason,
) -> io::Result<()> {
    pf_app_manifest::validate_app_id(item_id)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid application id"))?;
    let request = TriggerRequest::ForceStop {
        item_id: item_id.to_owned(),
        session_id: session_id.to_owned(),
        reason,
    };
    transport::send_request(socket, &request, None)
}

pub fn send_discard(socket: &Path, item_id: &str, session_id: &str) -> io::Result<()> {
    pf_app_manifest::validate_app_id(item_id)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid application id"))?;
    transport::send_request(
        socket,
        &TriggerRequest::DiscardPrepared {
            item_id: item_id.to_owned(),
            session_id: session_id.to_owned(),
        },
        None,
    )
}
