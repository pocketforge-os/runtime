pub mod key;

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
    MenuPoll,
    MenuAck { request_id: String },
    Request { action: PowerAction },
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    Menu {
        request_id: Option<String>,
    },
    MenuAck {
        acknowledged: bool,
    },
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

fn authorize(identity: io::Result<PeerServiceIdentity>, allowed_uid: u32) -> io::Result<()> {
    match identity {
        Ok(identity) if identity.cred.uid == allowed_uid && identity.unit == SHELL_UNIT => Ok(()),
        Ok(identity) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "unauthorized peer uid={} unit={}",
                identity.cred.uid, identity.unit
            ),
        )),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "peer identity unavailable",
        )),
    }
}

pub struct AuthorizedConnection(UnixStream);

pub fn authorize_connection(
    stream: UnixStream,
    allowed_uid: u32,
) -> io::Result<AuthorizedConnection> {
    let identity = pf_peer_identity::peer_service_identity(&stream);
    authorize_identified(stream, identity, allowed_uid)
}

fn authorize_identified(
    stream: UnixStream,
    identity: io::Result<PeerServiceIdentity>,
    allowed_uid: u32,
) -> io::Result<AuthorizedConnection> {
    authorize(identity, allowed_uid)?;
    Ok(AuthorizedConnection(stream))
}

pub fn handle(body: &[u8], executor: &dyn CommandExecutor) -> Response {
    handle_with_menu(
        body,
        executor,
        &std::sync::Mutex::new(key::Menu::new(String::new())),
    )
}

pub fn handle_with_menu(
    body: &[u8],
    executor: &dyn CommandExecutor,
    menu: &std::sync::Mutex<key::Menu>,
) -> Response {
    let request: Request = match serde_json::from_slice(body) {
        Ok(request) => request,
        Err(_) => {
            return Response::Refused {
                reason: "malformed request".into(),
            }
        }
    };
    match request {
        Request::MenuPoll => Response::Menu {
            request_id: menu.lock().expect("power menu mutex poisoned").pending(),
        },
        Request::MenuAck { request_id } => Response::MenuAck {
            acknowledged: menu
                .lock()
                .expect("power menu mutex poisoned")
                .acknowledge(&request_id),
        },
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
    stream: UnixStream,
    allowed_uid: u32,
    executor: &dyn CommandExecutor,
) -> io::Result<()> {
    authorize_connection(stream, allowed_uid)?.serve(executor)
}

impl AuthorizedConnection {
    pub fn serve(self, executor: &dyn CommandExecutor) -> io::Result<()> {
        self.serve_with_menu(
            executor,
            &std::sync::Mutex::new(key::Menu::new(String::new())),
        )
    }

    pub fn serve_with_menu(
        mut self,
        executor: &dyn CommandExecutor,
        menu: &std::sync::Mutex<key::Menu>,
    ) -> io::Result<()> {
        self.0.set_read_timeout(Some(CONNECTION_TIMEOUT))?;
        self.0.set_write_timeout(Some(CONNECTION_TIMEOUT))?;
        let body = pf_wire::read_frame(&mut self.0).map_err(map_wire)?;
        let response = handle_with_menu(&body, executor, menu);
        let body = serde_json::to_vec(&response).map_err(io::Error::other)?;
        pf_wire::write_frame(&mut self.0, &body).map_err(map_wire)
    }
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
            let response = handle(body.as_bytes(), &executor);
            assert!(
                matches!(response, Response::Accepted { action: got, status: 0, .. } if got == action)
            );
        }
        assert_eq!(executor.0.lock().unwrap().len(), 3);
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
        assert!(matches!(
            handle(br#"{"method":"request","action":"power_off"}"#, &ExitExecutor(124)),
            Response::Refused { reason } if reason == "systemctl exited 124"
        ));
    }

    #[test]
    fn malformed_request_never_invokes_power() {
        let executor = FakeExecutor::default();
        assert!(matches!(
            handle(b"not json", &executor),
            Response::Refused { .. }
        ));
        assert!(executor.0.lock().unwrap().is_empty());
    }

    #[test]
    fn physical_key_poll_is_supported_without_a_power_command() {
        let executor = FakeExecutor::default();
        let response =
            serde_json::to_value(handle(br#"{"method":"menu_poll"}"#, &executor)).unwrap();
        assert_eq!(
            response,
            serde_json::json!({"result":"menu", "request_id":null})
        );
        assert!(executor.0.lock().unwrap().is_empty());
    }

    #[test]
    fn wrong_peer_is_refused_before_any_frame_is_read() {
        let (server, _client) = UnixStream::pair().unwrap();
        let error = authorize_identified(server, identity(1000, "pf-app@evil.service"), 1000)
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        for peer in [
            identity(0, SHELL_UNIT),
            identity(1001, SHELL_UNIT),
            Err(io::Error::other("no identity")),
        ] {
            let (server, _client) = UnixStream::pair().unwrap();
            assert!(authorize_identified(server, peer, 1000).is_err());
        }
    }

    #[test]
    fn menu_socket_poll_crash_retry_and_ack_never_execute_power() {
        let menu = Mutex::new(key::Menu::new("daemon-1".into()));
        menu.lock().unwrap().request();
        let executor = FakeExecutor::default();
        let exchange = |body: &[u8]| {
            let (server, mut client) = UnixStream::pair().unwrap();
            let authorized =
                authorize_identified(server, identity(1000, SHELL_UNIT), 1000).unwrap();
            std::thread::scope(|scope| {
                let worker = scope.spawn(|| authorized.serve_with_menu(&executor, &menu).unwrap());
                pf_wire::write_frame(&mut client, body).unwrap();
                let reply = pf_wire::read_frame(&mut client).unwrap();
                worker.join().unwrap();
                serde_json::from_slice::<serde_json::Value>(&reply).unwrap()
            })
        };
        let first = exchange(br#"{"method":"menu_poll"}"#);
        assert_eq!(
            first,
            serde_json::json!({"result":"menu","request_id":"daemon-1:1"})
        );
        assert_eq!(exchange(br#"{"method":"menu_poll"}"#), first);
        assert_eq!(
            exchange(br#"{"method":"menu_ack","request_id":"stale:1"}"#)["acknowledged"],
            false
        );
        assert_eq!(
            exchange(br#"{"method":"menu_ack","request_id":"daemon-1:1"}"#)["acknowledged"],
            true
        );
        assert!(exchange(br#"{"method":"menu_poll"}"#)["request_id"].is_null());
        assert!(executor.0.lock().unwrap().is_empty());
    }

    #[test]
    fn service_uses_a_dedicated_root_owned_runtime_directory() {
        let unit = include_str!("../../../systemd/pf-powerd.service");
        assert!(unit.contains("RuntimeDirectory=pocketforge-power\nRuntimeDirectoryMode=0750"));
        assert!(unit.contains("/run/pocketforge-power/power.sock"));
        assert!(!unit.contains("/run/pocketforge/power.sock"));
    }
}
