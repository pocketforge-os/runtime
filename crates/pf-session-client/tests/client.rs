use pf_ports::*;
use pf_session_authority::{AuthorityApi, AuthorityError};
use pf_session_client::SessionClient;

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
