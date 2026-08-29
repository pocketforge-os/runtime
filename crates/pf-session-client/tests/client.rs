use pf_ports::*;
use pf_session_authority::{serve_connection, Authority, FileStore, Observation, SessionSystem};
use pf_session_authority::{AuthorityApi, AuthorityError};
use pf_session_client::SessionClient;
use pf_session_client::SocketTransport;
use std::os::unix::net::UnixListener;
use std::time::Duration;

struct Transport {
    events: Vec<(u64, SessionEvent)>,
    cursors: std::collections::HashMap<String, u64>,
}
impl AuthorityApi for Transport {
    fn launch(&mut self, _: LaunchRequest) -> Result<LaunchResult, AuthorityError> {
        Ok(LaunchResult::Accepted {
            session_id: "s".into(),
        })
    }
    fn events_for(&self, client_id: &str) -> Vec<(u64, SessionEvent)> {
        let sequence = self.cursors.get(client_id).copied().unwrap_or(0);
        self.events
            .iter()
            .filter(|(s, _)| *s > sequence)
            .cloned()
            .collect()
    }
    fn acknowledge(&mut self, client_id: &str, sequence: u64) -> Result<(), AuthorityError> {
        self.cursors.insert(client_id.to_owned(), sequence);
        Ok(())
    }
    fn history(&self) -> Vec<SessionEvent> {
        vec![]
    }
}
#[test]
fn implements_session_port_and_tracks_transport_sequence() {
    let transport = Transport {
        events: vec![
            (
                1,
                SessionEvent::Observed(ObservedSessionState::ObservationComplete),
            ),
            (
                2,
                SessionEvent::Terminal(TerminalReceipt::Returned {
                    session_id: "s".into(),
                }),
            ),
        ],
        cursors: Default::default(),
    };
    let mut client = SessionClient::new("launcher", transport);
    assert!(matches!(
        client.launch(LaunchRequest {
            item_id: "x".into()
        }),
        Ok(LaunchResult::Accepted { .. })
    ));
    client.acknowledge_last().unwrap();
    assert!(matches!(
        client.next_event(Deadline(MonotonicTime::ZERO)),
        Ok(SessionPoll::Event(SessionEvent::Observed(
            ObservedSessionState::ObservationComplete
        )))
    ));
    client.acknowledge_last().unwrap();
    assert!(matches!(
        client.next_event(Deadline(MonotonicTime::ZERO)),
        Ok(SessionPoll::Event(SessionEvent::Terminal(_)))
    ));
    client.acknowledge_last().unwrap();
    assert_eq!(
        client.next_event(Deadline(MonotonicTime::ZERO)),
        Ok(SessionPoll::Idle)
    );
    assert_eq!(client.history().len(), 2);
}

#[test]
fn acknowledged_cursor_survives_client_restart_and_unconsumed_event_remains() {
    let transport = Transport {
        events: vec![
            (
                1,
                SessionEvent::Observed(ObservedSessionState::ObservationComplete),
            ),
            (
                2,
                SessionEvent::Terminal(TerminalReceipt::Returned {
                    session_id: "s".into(),
                }),
            ),
        ],
        cursors: Default::default(),
    };
    let mut client = SessionClient::new("launcher", transport);
    assert!(matches!(
        client.next_event(Deadline(MonotonicTime::ZERO)),
        Ok(SessionPoll::Event(_))
    ));
    client.acknowledge_last().unwrap();
    let transport = client.into_inner();

    let mut restarted = SessionClient::new("launcher", transport);
    assert!(matches!(
        restarted.next_event(Deadline(MonotonicTime::ZERO)),
        Ok(SessionPoll::Event(SessionEvent::Terminal(_)))
    ));
    restarted.acknowledge_last().unwrap();
    let transport = restarted.into_inner();

    let mut restarted = SessionClient::new("launcher", transport);
    assert_eq!(
        restarted.next_event(Deadline(MonotonicTime::ZERO)),
        Ok(SessionPoll::Idle)
    );
}

#[derive(Default)]
struct SocketSystem;
impl SessionSystem for SocketSystem {
    fn start_foreground(&mut self, _: &LaunchRequest, _: &str) -> Result<bool, String> {
        Ok(true)
    }
    fn request_graceful_stop(&mut self, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn enforce_termination(&mut self, _: &str) -> Result<(), String> {
        Ok(())
    }
    fn activate_selected_owner(&mut self) -> Result<(), String> {
        Ok(())
    }
}

#[test]
fn socket_transport_orders_receipt_and_resumes_durable_cursor() {
    let dir = std::env::temp_dir().join(format!("pf-session-socket-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("authority.sock");
    let state = dir.join("authority.json");
    let listener = UnixListener::bind(&socket).unwrap();
    let thread = std::thread::spawn(move || {
        let mut authority = Authority::open(
            FileStore::new(&state),
            SocketSystem,
            TestClock::new(),
            4,
            Duration::from_secs(1),
        )
        .unwrap();
        for step in 0..10 {
            if step == 2 {
                authority.observe(Observation::SessionRunning).unwrap();
            }
            if step == 5 {
                authority = Authority::open(
                    FileStore::new(&state),
                    SocketSystem,
                    TestClock::new(),
                    4,
                    Duration::from_secs(1),
                )
                .unwrap();
                authority
                    .observe(Observation::SessionExitedCleanly)
                    .unwrap();
                authority.observe(Observation::UnitInactive).unwrap();
                authority.observe(Observation::TargetReleased).unwrap();
                authority.observe(Observation::SelectedOwnerActive).unwrap();
                authority
                    .observe(Observation::PresentationAcknowledged)
                    .unwrap();
            }
            let (mut stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            serve_connection(&mut authority, &mut stream, &mut writer).unwrap();
        }
    });
    let mut client = SessionClient::new("launcher", SocketTransport::connect(&socket));
    assert!(matches!(
        client
            .launch(LaunchRequest {
                item_id: "game".into()
            })
            .unwrap(),
        LaunchResult::Accepted { .. }
    ));
    assert!(matches!(
        client.next_event(Deadline(MonotonicTime::ZERO)).unwrap(),
        SessionPoll::Event(SessionEvent::Observed(ObservedSessionState::Starting))
    ));
    client.acknowledge_last().unwrap();
    assert!(matches!(
        client.next_event(Deadline(MonotonicTime::ZERO)).unwrap(),
        SessionPoll::Event(SessionEvent::Observed(ObservedSessionState::Running))
    ));
    client.acknowledge_last().unwrap();
    let transport = client.into_inner();
    let mut restarted = SessionClient::new("launcher", transport);
    assert!(matches!(
        restarted.next_event(Deadline(MonotonicTime::ZERO)).unwrap(),
        SessionPoll::Event(SessionEvent::Observed(
            ObservedSessionState::ObservationComplete
        ))
    ));
    restarted.acknowledge_last().unwrap();
    assert!(matches!(
        restarted.next_event(Deadline(MonotonicTime::ZERO)).unwrap(),
        SessionPoll::Event(SessionEvent::Terminal(_))
    ));
    restarted.acknowledge_last().unwrap();
    let entries = restarted.transport_mut().history_entries().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].item_id, "game");
    assert!(entries[0].started_at.is_some());
    assert!(entries[0].ended_at.is_some());
    thread.join().unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn socket_transport_reports_daemon_down_instead_of_idle() {
    let dir = std::env::temp_dir().join(format!("pf-session-socket-down-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("authority.sock");
    let mut client = SessionClient::new("launcher", SocketTransport::connect(&socket));

    assert_eq!(
        client.next_event(Deadline(MonotonicTime::ZERO)),
        Err(SessionError::BackendUnavailable)
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn socket_transport_reports_mid_stream_drop_on_next_call() {
    let dir = std::env::temp_dir().join(format!("pf-session-socket-drop-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("authority.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let thread = std::thread::spawn(move || {
        let (mut first, _) = listener.accept().unwrap();
        let request = pf_wire::read_frame(&mut first).unwrap();
        let _: pf_session_authority::RpcRequest = serde_json::from_slice(&request).unwrap();
        let response =
            serde_json::to_vec(&pf_session_authority::RpcResponse::Events { events: vec![] })
                .unwrap();
        pf_wire::write_frame(&mut first, &response).unwrap();

        let (_dropped, _) = listener.accept().unwrap();
    });
    let mut client = SessionClient::new("launcher", SocketTransport::connect(&socket));
    assert_eq!(
        client.next_event(Deadline(MonotonicTime::ZERO)),
        Ok(SessionPoll::Idle)
    );
    assert_eq!(
        client.next_event(Deadline(MonotonicTime::ZERO)),
        Err(SessionError::BackendUnavailable)
    );

    thread.join().unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}
