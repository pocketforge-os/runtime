//! Narrow, authenticated power-control protocol used by the system menu.

use pf_peer_identity::PeerServiceIdentity;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io;
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::time::Duration;

pub const CONNECTION_TIMEOUT: Duration = Duration::from_secs(3);
pub const SHELL_UNIT: &str = "pf-shell-selected.service";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerAction {
    PowerOff,
    Restart,
    Suspend,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum Request {
    Capabilities,
    Request { action: PowerAction },
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    Capabilities {
        actions: [PowerAction; 3],
    },
    Accepted {
        action: PowerAction,
        status: i32,
        stdout_sha256: String,
        stderr_sha256: String,
    },
    Refused {
        reason: String,
    },
}

#[derive(Debug)]
pub struct CommandReceipt {
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub trait CommandExecutor {
    fn execute(&self, action: PowerAction) -> io::Result<CommandReceipt>;
}

pub struct SystemctlExecutor;

pub fn systemctl_argv(action: PowerAction) -> [&'static str; 6] {
    let verb = match action {
        PowerAction::PowerOff => "poweroff",
        PowerAction::Restart => "reboot",
        PowerAction::Suspend => "suspend",
    };
    [
        "/usr/bin/timeout",
        "--signal=KILL",
        "3",
        "/usr/bin/systemctl",
        "--no-block",
        verb,
    ]
}

impl CommandExecutor for SystemctlExecutor {
    fn execute(&self, action: PowerAction) -> io::Result<CommandReceipt> {
        let argv = systemctl_argv(action);
        let output = Command::new(argv[0]).args(&argv[1..]).output()?;
        Ok(CommandReceipt {
            status: output.status.code().unwrap_or(137),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn handle(
    body: &[u8],
    identity: io::Result<PeerServiceIdentity>,
    allowed_uid: u32,
    executor: &dyn CommandExecutor,
) -> Response {
    match identity {
        Ok(identity) if identity.cred.uid == allowed_uid && identity.unit == SHELL_UNIT => {}
        Ok(identity) => {
            return Response::Refused {
                reason: format!(
                    "unauthorized peer uid={} unit={}",
                    identity.cred.uid, identity.unit
                ),
            };
        }
        Err(_) => {
            return Response::Refused {
                reason: "peer identity unavailable".into(),
            };
        }
    }
    let request: Request = match serde_json::from_slice(body) {
        Ok(request) => request,
        Err(_) => {
            return Response::Refused {
                reason: "malformed request".into(),
            }
        }
    };
    match request {
        Request::Capabilities => Response::Capabilities {
            actions: [
                PowerAction::PowerOff,
                PowerAction::Restart,
                PowerAction::Suspend,
            ],
        },
        Request::Request { action } => match executor.execute(action) {
            Ok(receipt) if receipt.status == 0 => Response::Accepted {
                action,
                status: receipt.status,
                stdout_sha256: digest(&receipt.stdout),
                stderr_sha256: digest(&receipt.stderr),
            },
            Ok(receipt) => Response::Refused {
                reason: format!("systemctl exited {}", receipt.status),
            },
            Err(_) => Response::Refused {
                reason: "systemctl unavailable".into(),
            },
        },
    }
}

pub fn serve_connection(
    mut stream: UnixStream,
    allowed_uid: u32,
    executor: &dyn CommandExecutor,
) -> io::Result<()> {
    stream.set_read_timeout(Some(CONNECTION_TIMEOUT))?;
    stream.set_write_timeout(Some(CONNECTION_TIMEOUT))?;
    let identity = pf_peer_identity::peer_service_identity(&stream);
    let body = pf_wire::read_frame(&mut stream).map_err(map_wire)?;
    let response = handle(&body, identity, allowed_uid, executor);
    let body = serde_json::to_vec(&response).map_err(io::Error::other)?;
    pf_wire::write_frame(&mut stream, &body).map_err(map_wire)
}

fn map_wire(error: pf_wire::WireError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_peer_identity::{PeerCred, PeerServiceIdentity};
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeExecutor(Mutex<Vec<PowerAction>>);

    impl CommandExecutor for FakeExecutor {
        fn execute(&self, action: PowerAction) -> io::Result<CommandReceipt> {
            self.0.lock().unwrap().push(action);
            Ok(CommandReceipt {
                status: 0,
                stdout: b"queued\n".to_vec(),
                stderr: vec![],
            })
        }
    }

    struct ExitExecutor(i32);

    impl CommandExecutor for ExitExecutor {
        fn execute(&self, _action: PowerAction) -> io::Result<CommandReceipt> {
            Ok(CommandReceipt {
                status: self.0,
                stdout: vec![],
                stderr: vec![],
            })
        }
    }

    fn identity(uid: u32, unit: &str) -> io::Result<PeerServiceIdentity> {
        Ok(PeerServiceIdentity {
            cred: PeerCred {
                pid: 42,
                uid,
                gid: uid,
            },
            unit: unit.into(),
        })
    }

    #[test]
    fn action_matrix_is_closed_and_receipted() {
        let executor = FakeExecutor::default();
        for (wire, action) in [
            ("power_off", PowerAction::PowerOff),
            ("restart", PowerAction::Restart),
            ("suspend", PowerAction::Suspend),
        ] {
            let body = format!(r#"{{"method":"request","action":"{wire}"}}"#);
            let response = handle(body.as_bytes(), identity(1000, SHELL_UNIT), 1000, &executor);
            assert!(
                matches!(response, Response::Accepted { action: got, status: 0, .. } if got == action)
            );
        }
        assert_eq!(
            *executor.0.lock().unwrap(),
            vec![
                PowerAction::PowerOff,
                PowerAction::Restart,
                PowerAction::Suspend
            ]
        );
        assert_eq!(
            systemctl_argv(PowerAction::Restart),
            [
                "/usr/bin/timeout",
                "--signal=KILL",
                "3",
                "/usr/bin/systemctl",
                "--no-block",
                "reboot"
            ]
        );
    }

    #[test]
    fn malformed_or_wrong_peer_never_invokes_power() {
        let executor = FakeExecutor::default();
        for response in [
            handle(b"not json", identity(1000, SHELL_UNIT), 1000, &executor),
            handle(
                br#"{"method":"request","action":"restart"}"#,
                identity(1001, SHELL_UNIT),
                1000,
                &executor,
            ),
            handle(
                br#"{"method":"request","action":"restart"}"#,
                identity(1000, "pf-app@evil.service"),
                1000,
                &executor,
            ),
        ] {
            assert!(matches!(response, Response::Refused { .. }));
        }
        assert!(executor.0.lock().unwrap().is_empty());
    }

    #[test]
    fn timed_out_systemctl_is_a_refusal() {
        let response = handle(
            br#"{"method":"request","action":"power_off"}"#,
            identity(1000, SHELL_UNIT),
            1000,
            &ExitExecutor(124),
        );
        assert_eq!(
            response,
            Response::Refused {
                reason: "systemctl exited 124".into()
            }
        );
    }
}
