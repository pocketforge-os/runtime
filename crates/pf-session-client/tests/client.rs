use pf_ports::*;
use pf_session_authority::{AuthorityApi, AuthorityError};
use pf_session_client::SessionClient;

struct Transport {
    events: Vec<(u64, SessionEvent)>,
}
impl AuthorityApi for Transport {
    fn launch(&mut self, _: LaunchRequest) -> Result<LaunchResult, AuthorityError> {
        Ok(LaunchResult::Accepted {
            session_id: "s".into(),
        })
    }
    fn events_after(&self, sequence: u64) -> Vec<(u64, SessionEvent)> {
        self.events
            .iter()
            .filter(|(s, _)| *s > sequence)
            .cloned()
            .collect()
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
    };
    let mut client = SessionClient::new(transport);
    assert!(matches!(
        client.launch(LaunchRequest {
            item_id: "x".into()
        }),
        Ok(LaunchResult::Accepted { .. })
    ));
    assert!(matches!(
        client.next_event(Deadline(MonotonicTime::ZERO)),
        Ok(SessionPoll::Event(SessionEvent::Observed(
            ObservedSessionState::ObservationComplete
        )))
    ));
    assert!(matches!(
        client.next_event(Deadline(MonotonicTime::ZERO)),
        Ok(SessionPoll::Event(SessionEvent::Terminal(_)))
    ));
    assert_eq!(
        client.next_event(Deadline(MonotonicTime::ZERO)),
        Ok(SessionPoll::Idle)
    );
    assert_eq!(client.history().len(), 2);
}
