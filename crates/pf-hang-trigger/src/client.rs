use crate::transport;
use crate::{proc_start_time, ForceStopReason, TargetIdentity, TriggerRequest, MAX_PREPARED_GRACE};
use std::io;
use std::os::fd::{AsFd, BorrowedFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

fn valid_app_id(id: &str) -> bool {
    fn valid_label(label: &str) -> bool {
        let bytes = label.as_bytes();
        let edge = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
        !bytes.is_empty()
            && edge(bytes[0])
            && edge(bytes[bytes.len() - 1])
            && bytes
                .iter()
                .all(|byte| edge(*byte) || matches!(*byte, b'_' | b'-'))
    }

    if !(3..=200).contains(&id.len()) || !id.is_ascii() {
        return false;
    }
    let mut labels = id.split('.');
    matches!((labels.next(), labels.next()), (Some(first), Some(second))
        if valid_label(first) && valid_label(second) && labels.all(valid_label))
}

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

fn validate_item_id(item_id: &str) -> io::Result<()> {
    if valid_app_id(item_id) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid application id",
        ))
    }
}

pub fn send_suspect_no_reply(
    socket: &Path,
    target: TargetIdentity,
    hang_nonce: u64,
    process: BorrowedFd<'_>,
) -> io::Result<()> {
    transport::send_request(
        socket,
        &TriggerRequest::SuspectNoReply { target, hang_nonce },
        Some(process),
    )
}

pub fn send_recovered(socket: &Path, target: TargetIdentity, hang_nonce: u64) -> io::Result<()> {
    transport::send_request(
        socket,
        &TriggerRequest::Recovered { target, hang_nonce },
        None,
    )
}

pub fn send_prepare(
    socket: &Path,
    item_id: &str,
    session_id: &str,
    grace: Duration,
) -> io::Result<()> {
    validate_item_id(item_id)?;
    if grace > MAX_PREPARED_GRACE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "grace period exceeds hang-sampler limit",
        ));
    }
    let grace_ms = grace
        .as_millis()
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "grace period is too large"))?;
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
        grace_ms,
    };
    transport::send_request(socket, &request, Some(handle.as_fd()))
}

pub fn send_force_stop(
    socket: &Path,
    item_id: &str,
    session_id: &str,
    reason: ForceStopReason,
) -> io::Result<()> {
    validate_item_id(item_id)?;
    transport::send_request(
        socket,
        &TriggerRequest::ForceStop {
            item_id: item_id.to_owned(),
            session_id: session_id.to_owned(),
            reason,
        },
        None,
    )
}

pub fn send_discard(socket: &Path, item_id: &str, session_id: &str) -> io::Result<()> {
    validate_item_id(item_id)?;
    transport::send_request(
        socket,
        &TriggerRequest::DiscardPrepared {
            item_id: item_id.to_owned(),
            session_id: session_id.to_owned(),
        },
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_id_validation_matches_the_trigger_contract() {
        for valid in ["org.example.game", "a.b", "org.example.game-2"] {
            assert!(validate_item_id(valid).is_ok(), "{valid}");
        }
        for invalid in ["org", "Org.example", "org..game", "org.example/"] {
            assert!(validate_item_id(invalid).is_err(), "{invalid}");
        }
    }
}
