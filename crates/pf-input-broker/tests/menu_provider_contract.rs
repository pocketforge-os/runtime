use pf_input_broker::safe_return as broker;
use pf_input_ipc::menu::{MenuAction, MenuToRouter, RouterToMenu};
use pf_input_ipc::{encode, timeouts};
use serde::Serialize;
use serde_json::Value;

/// The menu channel is the provider protocol `pf-input-broker` already speaks (runtime#108).
/// Each constant it compares byte-for-byte must decode here and re-encode to the same JSON.
#[test]
fn menu_frames_match_the_merged_broker_provider_protocol() {
    let cases: [(&[u8], Value); 7] = [
        (
            broker::SYSTEM_MENU_REGISTER_BODY,
            encode_value(&MenuToRouter::Register),
        ),
        (
            broker::SYSTEM_MENU_REGISTERED_BODY,
            encode_value(&RouterToMenu::Registered),
        ),
        (
            broker::SYSTEM_MENU_ACTION_BODY,
            encode_value(&RouterToMenu::SystemMenu { front: None }),
        ),
        (
            broker::SYSTEM_MENU_ACK_BODY,
            encode_value(&MenuToRouter::Ack {
                action: MenuAction::SystemMenu,
            }),
        ),
        (
            broker::SYSTEM_MENU_SHOWN_BODY,
            encode_value(&MenuToRouter::Shown),
        ),
        (
            broker::SYSTEM_MENU_RETURN_BODY,
            encode_value(&MenuToRouter::Return),
        ),
        (
            broker::SYSTEM_MENU_RETURNED_BODY,
            encode_value(&RouterToMenu::Ack {
                action: MenuAction::Return,
            }),
        ),
    ];
    for (broker_body, ours) in cases {
        let broker_value: Value = serde_json::from_slice(broker_body).unwrap();
        assert_eq!(
            ours,
            broker_value,
            "{}",
            String::from_utf8_lossy(broker_body)
        );
    }
    assert_eq!(
        broker::SYSTEM_MENU_ACK_TIMEOUT,
        timeouts::MENU_ACK_TIMEOUT,
        "the broker's ack deadline is the contract's"
    );
}

fn encode_value<M: Serialize>(message: &M) -> Value {
    serde_json::from_slice(&encode(message)).unwrap()
}
