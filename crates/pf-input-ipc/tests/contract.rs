//! Contract tests for pf-input-ipc: every fixture round-trips, every message type has a
//! fixture, unknown majors are refused before the shape is looked at, and the pure state
//! machines honour the timeout constants defined once in the crate.

use pf_input_ipc::compositor::{CompositorToRouter, OverlayState, RouterToCompositor};
use pf_input_ipc::events::RouterEvent;
use pf_input_ipc::keyboard::{KeyboardToRouter, RouterToKeyboard};
use pf_input_ipc::menu::{DismissReason, MenuAction, MenuToRouter, RouterToMenu};
use pf_input_ipc::{
    decode, encode, sockets, timeouts, IpcError, MenuPhase, MenuSession, MenuStep, PadDestination,
    PadGate, Secret, SCHEMA_MAJOR,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeSet;
use std::fmt::Debug;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn fixture_dir(channel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(channel)
}

fn fixtures(channel: &str) -> Vec<(String, Vec<u8>)> {
    let dir = fixture_dir(channel);
    let mut out: Vec<(String, Vec<u8>)> = fs::read_dir(&dir)
        .unwrap_or_else(|error| panic!("fixture dir {}: {error}", dir.display()))
        .map(|entry| entry.expect("fixture entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .map(|path| {
            let name = path.file_stem().unwrap().to_string_lossy().into_owned();
            (name, fs::read(&path).expect("fixture bytes"))
        })
        .collect();
    out.sort();
    assert!(!out.is_empty(), "no fixtures under {}", dir.display());
    out
}

/// Decode → encode → decode every fixture of a channel; the encoded form must be semantically
/// identical to the fixture (same keys, same values, version present) and the set of `type`
/// names must equal the channel's published list.
fn round_trip_channel<M>(channel: &str, published: &[&str])
where
    M: Serialize + DeserializeOwned + PartialEq + Debug,
{
    let mut seen = BTreeSet::new();
    for (name, body) in fixtures(channel) {
        let fixture: Value = serde_json::from_slice(&body).expect("fixture is JSON");
        assert_eq!(
            fixture.get("version").and_then(Value::as_u64),
            Some(u64::from(SCHEMA_MAJOR)),
            "{channel}/{name}: fixture must carry the current major"
        );
        let message: M = decode(&body)
            .unwrap_or_else(|error| panic!("{channel}/{name}: decode failed: {error}"));
        let encoded = encode(&message);
        let encoded_value: Value = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(
            encoded_value, fixture,
            "{channel}/{name}: encode drifted from fixture"
        );
        let again: M = decode(&encoded).unwrap();
        assert_eq!(again, message, "{channel}/{name}: second decode differs");
        seen.insert(fixture["type"].as_str().unwrap().to_owned());
    }
    let published: BTreeSet<String> = published.iter().map(|s| (*s).to_owned()).collect();
    assert_eq!(
        seen, published,
        "{channel}: fixtures must cover exactly the published types"
    );
}

#[test]
fn menu_to_router_fixtures_round_trip_and_cover_every_type() {
    round_trip_channel::<MenuToRouter>("menu-to-router", pf_input_ipc::menu::TO_ROUTER_TYPES);
}

#[test]
fn router_to_menu_fixtures_round_trip_and_cover_every_type() {
    round_trip_channel::<RouterToMenu>("router-to-menu", pf_input_ipc::menu::FROM_ROUTER_TYPES);
}

#[test]
fn keyboard_to_router_fixtures_round_trip_and_cover_every_type() {
    round_trip_channel::<KeyboardToRouter>(
        "keyboard-to-router",
        pf_input_ipc::keyboard::TO_ROUTER_TYPES,
    );
}

#[test]
fn router_to_keyboard_fixtures_round_trip_and_cover_every_type() {
    round_trip_channel::<RouterToKeyboard>(
        "router-to-keyboard",
        pf_input_ipc::keyboard::FROM_ROUTER_TYPES,
    );
}

#[test]
fn compositor_to_router_fixtures_round_trip_and_cover_every_type() {
    round_trip_channel::<CompositorToRouter>(
        "compositor-to-router",
        pf_input_ipc::compositor::TO_ROUTER_TYPES,
    );
}

#[test]
fn router_to_compositor_fixtures_round_trip_and_cover_every_type() {
    round_trip_channel::<RouterToCompositor>(
        "router-to-compositor",
        pf_input_ipc::compositor::FROM_ROUTER_TYPES,
    );
}

#[test]
fn router_event_fixtures_round_trip_and_cover_every_type() {
    round_trip_channel::<RouterEvent>("events", pf_input_ipc::events::TYPES);
}

/// Positive control (major 1 accepted) and every refusal in one invocation. A frame with an
/// unsupported major is refused as `UnsupportedVersion` even when its `type` is unknown,
/// proving the version is checked before the shape.
#[test]
fn unknown_major_version_is_refused_before_the_message_shape() {
    let read = |name: &str| fs::read(fixture_dir("refused").join(format!("{name}.json"))).unwrap();

    let accepted: MenuToRouter = decode(&encode(&MenuToRouter::Register)).unwrap();
    assert_eq!(
        accepted,
        MenuToRouter::Register,
        "positive control: major 1 decodes"
    );

    assert_eq!(
        decode::<MenuToRouter>(&read("major-2")),
        Err(IpcError::UnsupportedVersion {
            got: 2,
            supported: SCHEMA_MAJOR
        })
    );
    assert_eq!(
        decode::<MenuToRouter>(&read("major-0")),
        Err(IpcError::UnsupportedVersion {
            got: 0,
            supported: SCHEMA_MAJOR
        })
    );
    let future_unknown_type = br#"{"version":2,"type":"teleport"}"#;
    assert_eq!(
        decode::<MenuToRouter>(future_unknown_type),
        Err(IpcError::UnsupportedVersion {
            got: 2,
            supported: SCHEMA_MAJOR
        }),
        "the major is refused before the unknown type is seen"
    );
    for name in [
        "missing-version",
        "unknown-type",
        "wrong-shape",
        "not-an-object",
    ] {
        match decode::<MenuToRouter>(&read(name)) {
            Err(IpcError::Malformed(_)) => {}
            other => panic!("refused/{name}: expected Malformed, got {other:?}"),
        }
    }
}

/// Minor growth within a major: unknown fields are ignored, so an older peer keeps working
/// when a newer one adds an optional field.
#[test]
fn unknown_fields_are_ignored_within_the_major() {
    let grown = br#"{"version":1,"type":"ping","seq":4,"later_optional":"x"}"#;
    assert_eq!(
        decode::<RouterToMenu>(grown).unwrap(),
        RouterToMenu::Ping { seq: 4 }
    );
    let still_refused = br#"{"version":1,"type":"ping","later_optional":"x"}"#;
    assert!(
        matches!(
            decode::<RouterToMenu>(still_refused),
            Err(IpcError::Malformed(_))
        ),
        "a missing mandatory field is still refused"
    );
}

/// The menu channel is the provider protocol `pf-input-broker` already speaks (runtime#108).
/// Each constant it compares byte-for-byte must decode here and re-encode to the same JSON.
#[test]
fn menu_frames_match_the_merged_broker_provider_protocol() {
    use pf_input_broker::safe_return as broker;
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

#[test]
fn secrets_never_render_in_debug_or_display() {
    let secret = Secret("hunter2".into());
    assert!(!format!("{secret:?}").contains("hunter2"));
    assert!(!format!("{secret}").contains("hunter2"));
    let result = KeyboardToRouter::ModalResult {
        request_id: 1,
        outcome: pf_input_ipc::keyboard::ModalOutcome::Submitted,
        text: Some(secret),
    };
    assert!(!format!("{result:?}").contains("hunter2"));
    // The wire still carries it: the app needs the text.
    assert!(String::from_utf8(encode(&result))
        .unwrap()
        .contains("hunter2"));
}

#[test]
fn socket_constants_live_under_one_group_gated_directory() {
    for socket in [
        sockets::COMPOSITOR_SOCKET,
        sockets::KEYBOARD_SOCKET,
        sockets::MENU_SOCKET,
    ] {
        assert!(socket.starts_with(&format!("{}/", sockets::INPUT_RUN_DIR)));
        assert!(socket.ends_with(".sock"));
    }
    assert_eq!(sockets::INPUT_RUN_DIR_MODE, 0o750);
    assert_eq!(sockets::SOCKET_MODE, 0o660);
    let groups = BTreeSet::from([
        sockets::INPUT_GROUP,
        sockets::COMPOSITOR_GROUP,
        sockets::KEYBOARD_GROUP,
        sockets::MENU_GROUP,
    ]);
    assert_eq!(
        groups.len(),
        4,
        "one distinct group per endpoint plus the directory"
    );
    assert!(sockets::RECONNECT_INITIAL < sockets::RECONNECT_MAX);
}

/// Ordering rule (c): the keyboard gets the pad only with BOTH halves; failure is toward the
/// app; an open menu wins. Positive and negative controls in one invocation.
#[test]
fn pad_gate_needs_both_halves_and_fails_toward_the_app() {
    let mut gate = PadGate::new();
    assert_eq!(gate.destination(), PadDestination::App);

    gate.keyboard_visibility(true, true);
    assert_eq!(
        gate.destination(),
        PadDestination::App,
        "pf-osk alone is not enough"
    );
    gate.keyboard_overlay(OverlayState::Mapped);
    assert_eq!(
        gate.destination(),
        PadDestination::Keyboard,
        "both halves: keyboard"
    );

    gate.menu_open(true);
    assert_eq!(
        gate.destination(),
        PadDestination::Menu,
        "an open menu wins"
    );
    gate.menu_open(false);
    assert_eq!(gate.destination(), PadDestination::Keyboard);

    gate.keyboard_visibility(true, false);
    assert_eq!(
        gate.destination(),
        PadDestination::App,
        "strip visible, no pad wanted"
    );
    gate.keyboard_visibility(true, true);
    gate.keyboard_overlay(OverlayState::Unmapped);
    assert_eq!(
        gate.destination(),
        PadDestination::App,
        "unmapped overlay releases the pad"
    );

    gate.keyboard_overlay(OverlayState::Mapped);
    assert_eq!(gate.destination(), PadDestination::Keyboard);
    gate.keyboard_disconnected();
    assert_eq!(
        gate.destination(),
        PadDestination::App,
        "EOF releases the pad"
    );
    gate.keyboard_overlay(OverlayState::Mapped);
    assert_eq!(
        gate.destination(),
        PadDestination::App,
        "a stale map after EOF cannot route without a fresh visibility claim"
    );
}

/// Ordering rules (d) and (e) against the constants: ack inside 250 ms and shown inside 500 ms
/// open the menu; one millisecond past either deadline falls back; a provider that goes quiet
/// for a second is dismissed; the Return item and Resume take the pad back without fallback.
#[test]
fn menu_session_honours_the_timeout_constants() {
    let ms = Duration::from_millis;
    let ack = timeouts::MENU_ACK_TIMEOUT;
    let shown = timeouts::MENU_SHOWN_TIMEOUT;
    let health = timeouts::MENU_HEALTH_TIMEOUT;

    // Positive control: everything on time.
    let mut good = MenuSession::new();
    assert_eq!(
        good.tick(ack),
        MenuStep::None,
        "exactly at the ack deadline is still fine"
    );
    assert_eq!(good.ack(ack), MenuStep::None);
    assert_eq!(good.phase(), MenuPhase::AwaitingShown);
    assert_eq!(good.tick(ack + shown), MenuStep::None);
    assert_eq!(good.shown(ack + shown), MenuStep::RoutePadToMenu);
    assert_eq!(good.phase(), MenuPhase::Open);
    let t_open = ack + shown;
    assert_eq!(
        good.tick(t_open + health),
        MenuStep::None,
        "no pong yet, inside the window"
    );
    assert_eq!(good.pong(t_open + ms(400)), MenuStep::None);
    assert_eq!(
        good.tick(t_open + ms(400) + health),
        MenuStep::None,
        "pong moved the window"
    );
    assert_eq!(good.provider_closed(), MenuStep::RoutePadToApp);
    assert_eq!(good.phase(), MenuPhase::Closed { fallback: false });
    assert_eq!(good.tick(ms(60_000)), MenuStep::None, "closed is terminal");

    // Negative: late ack → the router runs the return path itself.
    let mut late_ack = MenuSession::new();
    assert_eq!(late_ack.tick(ack + ms(1)), MenuStep::FallbackReturn);
    assert_eq!(late_ack.phase(), MenuPhase::Closed { fallback: true });
    assert_eq!(
        late_ack.ack(ack + ms(2)),
        MenuStep::None,
        "an ack after fallback is ignored"
    );

    // Negative: ack arrives late (clock skew on the worker) → same fallback.
    let mut late_ack_direct = MenuSession::new();
    assert_eq!(late_ack_direct.ack(ack + ms(1)), MenuStep::FallbackReturn);

    // Negative: shown too late after the ack.
    let mut late_shown = MenuSession::new();
    assert_eq!(late_shown.ack(ms(100)), MenuStep::None);
    assert_eq!(late_shown.tick(ms(100) + shown), MenuStep::None);
    assert_eq!(
        late_shown.tick(ms(100) + shown + ms(1)),
        MenuStep::Dismiss(DismissReason::ShownTimeout)
    );
    assert_eq!(late_shown.shown(ms(100) + shown + ms(2)), MenuStep::None);
    assert_eq!(late_shown.phase(), MenuPhase::Closed { fallback: true });

    // Negative: open, then silent for more than a second.
    let mut quiet = MenuSession::new();
    quiet.ack(ms(50));
    assert_eq!(quiet.shown(ms(100)), MenuStep::RoutePadToMenu);
    assert_eq!(quiet.tick(ms(100) + health), MenuStep::None);
    assert_eq!(
        quiet.tick(ms(100) + health + ms(1)),
        MenuStep::Dismiss(DismissReason::Unhealthy)
    );
    assert_eq!(
        quiet.pong(ms(2_000)),
        MenuStep::None,
        "a late pong cannot revive it"
    );
    assert_eq!(quiet.phase(), MenuPhase::Closed { fallback: false });

    // Hold Menu: below the threshold nothing; at it, dismiss + return, regardless of phase.
    let mut held = MenuSession::new();
    held.ack(ms(10));
    assert_eq!(
        held.held(timeouts::MENU_HOLD_RETURN - ms(1)),
        MenuStep::None,
        "1999 ms is a press, not a hold"
    );
    assert_eq!(
        held.held(timeouts::MENU_HOLD_RETURN),
        MenuStep::Dismiss(DismissReason::HoldReturn)
    );
    assert_eq!(held.phase(), MenuPhase::Closed { fallback: true });
    assert_eq!(held.held(ms(5_000)), MenuStep::None, "already closed");

    // The constants themselves, as the design states them.
    assert_eq!(ack, ms(250));
    assert_eq!(shown, ms(500));
    assert_eq!(health, ms(1_000));
    assert_eq!(timeouts::MENU_HEARTBEAT_INTERVAL * 2, health);
    assert_eq!(timeouts::MENU_HOLD_RETURN, ms(2_000));
    assert_eq!(timeouts::USER_PRESS_WINDOW, ms(1_000));
}
