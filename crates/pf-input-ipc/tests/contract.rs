//! Contract tests for pf-input-ipc: every fixture round-trips, every message type has a
//! fixture, unknown majors are refused before the shape is looked at, and the pure state
//! machines honour the timeout constants defined once in the crate.

use pf_input_ipc::compositor::{
    CompositorSession, CompositorSnapshot, CompositorStateError, CompositorToRouter, OverlayState,
    PowerState, PowerStep, RouterToCompositor,
};
use pf_input_ipc::events::RouterEvent;
use pf_input_ipc::keyboard::{KeyboardToRouter, RouterToKeyboard};
use pf_input_ipc::menu::{DismissReason, MenuToRouter, RouterToMenu};
use pf_input_ipc::{
    decode, encode, route_pad_input, sockets, timeouts, validate_protected_power_order,
    GamescopeInputStep, IpcError, MenuPhase, MenuSession, MenuStep, PadControl, PadDestination,
    PadGate, PadInputDisposition, Secret, SCHEMA_MAJOR,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
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

#[test]
fn trusted_endpoint_identities_are_role_specific_and_pinned() {
    assert_eq!(
        (
            sockets::COMPOSITOR_USER,
            sockets::COMPOSITOR_GROUP,
            sockets::COMPOSITOR_UNIT,
        ),
        (
            "pf-gamescope",
            "pf-input-compositor",
            "pocketforge-gamescope.service",
        )
    );
    assert_eq!(
        (
            sockets::KEYBOARD_USER,
            sockets::KEYBOARD_GROUP,
            sockets::KEYBOARD_UNIT,
        ),
        ("pf-ime", "pf-input-keyboard", "pf-osk.service")
    );
    assert_eq!(
        (sockets::MENU_USER, sockets::MENU_GROUP, sockets::MENU_UNIT),
        ("pf-menu", "pf-input-menu", "pf-system-menu.service")
    );
    assert_eq!(
        BTreeSet::from([
            sockets::COMPOSITOR_USER,
            sockets::KEYBOARD_USER,
            sockets::MENU_USER,
        ])
        .len(),
        3,
        "a service uid may not authorize two trusted roles"
    );
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
    assert_eq!(good.ping(7, t_open + ms(100)), MenuStep::None);
    assert_eq!(good.pong(7, t_open + ms(400)), MenuStep::None);
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
        MenuStep::DismissAndReturn(DismissReason::ShownTimeout)
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
        quiet.pong(99, ms(2_000)),
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
        MenuStep::DismissAndReturn(DismissReason::HoldReturn)
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

/// Heartbeats refresh health only for one fresh reply to an outstanding ping. Unsolicited,
/// wrong-sequence, duplicate and stale replies are negative controls in the same invocation.
#[test]
fn menu_session_rejects_invalid_pongs() {
    let ms = Duration::from_millis;

    let opened = || {
        let mut session = MenuSession::new();
        assert_eq!(session.ack(ms(10)), MenuStep::None);
        assert_eq!(session.shown(ms(20)), MenuStep::RoutePadToMenu);
        session
    };

    // Positive control: a fresh reply with the matching sequence refreshes health.
    let mut fresh = opened();
    assert_eq!(fresh.ping(10, ms(100)), MenuStep::None);
    assert_eq!(fresh.pong(10, ms(150)), MenuStep::None);
    assert_eq!(fresh.tick(ms(1_150)), MenuStep::None);

    // An unsolicited reply cannot move the health window.
    let mut unsolicited = opened();
    assert_eq!(unsolicited.pong(10, ms(900)), MenuStep::None);
    assert_eq!(
        unsolicited.tick(ms(1_021)),
        MenuStep::Dismiss(DismissReason::Unhealthy)
    );

    // A wrong sequence cannot consume the outstanding ping or refresh health.
    let mut wrong = opened();
    assert_eq!(wrong.ping(10, ms(100)), MenuStep::None);
    assert_eq!(wrong.pong(11, ms(900)), MenuStep::None);
    assert_eq!(
        wrong.tick(ms(1_021)),
        MenuStep::Dismiss(DismissReason::Unhealthy)
    );

    // A matching reply is one-shot; replaying it cannot keep the provider healthy.
    let mut duplicate = opened();
    assert_eq!(duplicate.ping(10, ms(100)), MenuStep::None);
    assert_eq!(duplicate.pong(10, ms(150)), MenuStep::None);
    assert_eq!(duplicate.pong(10, ms(1_100)), MenuStep::None);
    assert_eq!(
        duplicate.tick(ms(1_151)),
        MenuStep::Dismiss(DismissReason::Unhealthy)
    );

    // A matching echo older than PONG_MAX_AGE is ignored.
    let mut stale = opened();
    assert_eq!(stale.ping(10, ms(100)), MenuStep::None);
    assert_eq!(stale.pong(10, ms(1_101)), MenuStep::None);
    assert_eq!(
        stale.tick(ms(1_101)),
        MenuStep::Dismiss(DismissReason::Unhealthy)
    );
}

/// Every fallback branch tells the caller to run safe_return, including a directly delivered
/// late `shown` and holds in all three live phases.
#[test]
fn menu_session_fallback_steps_always_request_safe_return() {
    let ms = Duration::from_millis;

    let mut late_shown = MenuSession::new();
    assert_eq!(late_shown.ack(ms(10)), MenuStep::None);
    assert_eq!(
        late_shown.shown(ms(10) + timeouts::MENU_SHOWN_TIMEOUT + ms(1)),
        MenuStep::DismissAndReturn(DismissReason::ShownTimeout)
    );

    let mut awaiting_ack = MenuSession::new();
    assert_eq!(
        awaiting_ack.held(timeouts::MENU_HOLD_RETURN),
        MenuStep::DismissAndReturn(DismissReason::HoldReturn)
    );

    let mut awaiting_shown = MenuSession::new();
    assert_eq!(awaiting_shown.ack(ms(10)), MenuStep::None);
    assert_eq!(
        awaiting_shown.held(timeouts::MENU_HOLD_RETURN),
        MenuStep::DismissAndReturn(DismissReason::HoldReturn)
    );

    let mut open = MenuSession::new();
    assert_eq!(open.ack(ms(10)), MenuStep::None);
    assert_eq!(open.shown(ms(20)), MenuStep::RoutePadToMenu);
    assert_eq!(
        open.held(timeouts::MENU_HOLD_RETURN),
        MenuStep::DismissAndReturn(DismissReason::HoldReturn)
    );
}

#[derive(Debug, Deserialize)]
struct RecoveryTranscript {
    old_epoch: u64,
    new_epoch: u64,
    request_id: u64,
    next_power_sequence: u64,
    snapshot: CompositorSnapshot,
    sequence: Vec<String>,
}

fn recovery_transcript(name: &str) -> RecoveryTranscript {
    let path = fixture_dir("transcripts").join(format!("{name}.json"));
    serde_json::from_slice(&fs::read(&path).expect("recovery transcript bytes"))
        .expect("recovery transcript shape")
}

#[derive(Debug, Deserialize)]
struct ProtectedPowerOrderFixture {
    accepted: Vec<GamescopeInputStep>,
}

#[derive(Debug, Deserialize)]
struct RefusedPowerOrderFixture {
    refused: Vec<Vec<GamescopeInputStep>>,
}

#[test]
fn protected_power_is_consumed_before_seat_delivery() {
    let path = fixture_dir("transcripts").join("protected-power-before-seat.json");
    let fixture: ProtectedPowerOrderFixture =
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(validate_protected_power_order(&fixture.accepted), Ok(()));
    let refused_path = fixture_dir("transcripts").join("refused-power-seat-first.json");
    let refused_fixture: RefusedPowerOrderFixture =
        serde_json::from_slice(&fs::read(refused_path).unwrap()).unwrap();
    for refused in refused_fixture.refused {
        assert!(
            validate_protected_power_order(&refused).is_err(),
            "negative fixture unexpectedly accepted: {refused:?}"
        );
    }
}

#[derive(Debug, Deserialize)]
struct MenuFirstFixture {
    destinations: Vec<PadDestination>,
    protected_control: PadControl,
    ordinary_control: PadControl,
}

#[test]
fn menu_is_consumed_before_every_destination() {
    let path = fixture_dir("transcripts").join("menu-before-every-destination.json");
    let fixture: MenuFirstFixture = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    for destination in fixture.destinations {
        assert_eq!(
            route_pad_input(destination, fixture.protected_control),
            PadInputDisposition::ConsumedByRouter,
            "Menu leaked to {destination:?}"
        );
        assert_eq!(
            route_pad_input(destination, fixture.ordinary_control),
            PadInputDisposition::RoutedTo(destination),
            "negative control did not route to {destination:?}"
        );
    }
}

#[derive(Debug, Deserialize)]
struct RefusalFixture {
    scenario: String,
    current_epoch: u64,
    message_epoch: u64,
    #[serde(default)]
    request_id: u64,
    #[serde(default)]
    fd_count: usize,
    #[serde(default)]
    sequence: u64,
    #[serde(default)]
    state: Option<PowerState>,
    #[serde(default)]
    snapshot_pressed: Option<bool>,
    #[serde(default)]
    snapshot_last_sequence: Option<u64>,
    #[serde(default)]
    expected_last_sequence: Option<u64>,
    #[serde(default)]
    role: Option<pf_input_ipc::OverlayRole>,
}

fn snapshot_for_epoch(epoch: u64) -> CompositorSnapshot {
    let bytes = fs::read(fixture_dir("compositor-to-router").join("state-snapshot.json")).unwrap();
    let message: CompositorToRouter = decode(&bytes).unwrap();
    let CompositorToRouter::StateSnapshot { mut snapshot } = message else {
        panic!("state-snapshot fixture decoded as the wrong message")
    };
    snapshot.epoch = epoch;
    snapshot
}

#[test]
fn every_stateful_refusal_has_a_named_fixture_and_positive_control() {
    let expected: BTreeSet<String> = [
        "refused-delta-before-snapshot",
        "refused-duplicate-snapshot",
        "refused-fd-on-refusal",
        "refused-incomplete-snapshot",
        "refused-missing-fd",
        "refused-multiple-fds",
        "refused-old-epoch",
        "refused-power-before-snapshot",
        "refused-power-duplicate-pressed-after-delta",
        "refused-power-duplicate-pressed-from-snapshot",
        "refused-power-duplicate-released-after-delta",
        "refused-power-duplicate-released-from-snapshot",
        "refused-power-old-epoch",
        "refused-power-replayed-sequence",
        "refused-role-already-bound",
        "refused-role-not-bound",
        "refused-stale-request-id",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    let mut seen = BTreeSet::new();

    for entry in fs::read_dir(fixture_dir("transcripts")).unwrap() {
        let path = entry.unwrap().path();
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if !stem.starts_with("refused-") {
            continue;
        }
        // Ordering refusals use a different fixture shape and are driven by their dedicated
        // positive/negative contract test above.
        if stem == "refused-power-seat-first" {
            continue;
        }
        seen.insert(stem.to_owned());
        let fixture: RefusalFixture = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let role = fixture.role.unwrap_or(pf_input_ipc::OverlayRole::Keyboard);
        let mut session = CompositorSession::new(fixture.current_epoch);
        let error = match fixture.scenario.as_str() {
            "delta_before_snapshot" => session.accept_delta(fixture.message_epoch).unwrap_err(),
            "old_epoch" => session.accept_delta(fixture.message_epoch).unwrap_err(),
            "duplicate_snapshot" => {
                let snapshot = snapshot_for_epoch(fixture.current_epoch);
                session.apply_snapshot(snapshot.clone()).unwrap();
                session.apply_snapshot(snapshot).unwrap_err()
            }
            "incomplete_snapshot" => {
                let mut snapshot = snapshot_for_epoch(fixture.current_epoch);
                snapshot.overlays.pop();
                session.apply_snapshot(snapshot).unwrap_err()
            }
            "power_before_snapshot" => session
                .protected_power(fixture.message_epoch, fixture.sequence, PowerState::Pressed)
                .unwrap_err(),
            "power_old_epoch" => {
                session
                    .apply_snapshot(snapshot_for_epoch(fixture.current_epoch))
                    .unwrap();
                session
                    .protected_power(fixture.message_epoch, fixture.sequence, PowerState::Pressed)
                    .unwrap_err()
            }
            "power_duplicate_from_snapshot" => {
                let state = fixture.state.expect("duplicate state");
                let mut snapshot = snapshot_for_epoch(fixture.current_epoch);
                snapshot.power.pressed = fixture.snapshot_pressed.expect("snapshot level");
                snapshot.power.last_sequence = fixture
                    .snapshot_last_sequence
                    .expect("snapshot sequence watermark");
                session.apply_snapshot(snapshot).unwrap();
                session
                    .protected_power(fixture.message_epoch, fixture.sequence, state)
                    .unwrap_err()
            }
            "power_duplicate_after_delta" => {
                let state = fixture.state.expect("duplicate state");
                let mut snapshot = snapshot_for_epoch(fixture.current_epoch);
                snapshot.power.pressed = fixture.snapshot_pressed.expect("snapshot level");
                snapshot.power.last_sequence = fixture
                    .snapshot_last_sequence
                    .expect("snapshot sequence watermark");
                session.apply_snapshot(snapshot).unwrap();
                session
                    .protected_power(fixture.message_epoch, fixture.sequence, state)
                    .unwrap();
                session
                    .protected_power(fixture.message_epoch, fixture.sequence + 1, state)
                    .unwrap_err()
            }
            "power_replayed_sequence" | "replayed_power_sequence" => {
                session
                    .apply_snapshot(snapshot_for_epoch(fixture.current_epoch))
                    .unwrap();
                session
                    .protected_power(fixture.message_epoch, fixture.sequence, PowerState::Pressed)
                    .unwrap_err()
            }
            "stale_request_id" => {
                session
                    .apply_snapshot(snapshot_for_epoch(fixture.current_epoch))
                    .unwrap();
                session
                    .finish_trusted_connection(
                        fixture.message_epoch,
                        fixture.request_id,
                        fixture.fd_count,
                    )
                    .unwrap_err()
            }
            "missing_fd" | "multiple_fds" | "fd_on_refusal" => {
                session
                    .apply_snapshot(snapshot_for_epoch(fixture.current_epoch))
                    .unwrap();
                session
                    .begin_trusted_connection(fixture.message_epoch, fixture.request_id, role)
                    .unwrap();
                if fixture.scenario == "fd_on_refusal" {
                    session
                        .refuse_trusted_connection(
                            fixture.message_epoch,
                            fixture.request_id,
                            fixture.fd_count,
                        )
                        .unwrap_err()
                } else {
                    session
                        .finish_trusted_connection(
                            fixture.message_epoch,
                            fixture.request_id,
                            fixture.fd_count,
                        )
                        .unwrap_err()
                }
            }
            "role_already_bound" => {
                session
                    .apply_snapshot(snapshot_for_epoch(fixture.current_epoch))
                    .unwrap();
                session
                    .begin_trusted_connection(fixture.message_epoch, fixture.request_id, role)
                    .unwrap();
                session
                    .finish_trusted_connection(
                        fixture.message_epoch,
                        fixture.request_id,
                        fixture.fd_count,
                    )
                    .unwrap();
                session
                    .begin_trusted_connection(fixture.message_epoch, fixture.request_id + 1, role)
                    .unwrap_err()
            }
            "role_not_bound" => {
                session
                    .apply_snapshot(snapshot_for_epoch(fixture.current_epoch))
                    .unwrap();
                session
                    .release_trusted_role(fixture.message_epoch, role)
                    .unwrap_err()
            }
            other => panic!("unknown refusal scenario {other}"),
        };
        let expected_error = match fixture.scenario.as_str() {
            "delta_before_snapshot" => CompositorStateError::SnapshotRequired,
            "old_epoch" => CompositorStateError::OldEpoch {
                got: fixture.message_epoch,
                current: fixture.current_epoch,
            },
            "duplicate_snapshot" => CompositorStateError::SnapshotAlreadyApplied,
            "incomplete_snapshot" => CompositorStateError::IncompleteSnapshot,
            "power_before_snapshot" => CompositorStateError::SnapshotRequired,
            "power_old_epoch" => CompositorStateError::OldEpoch {
                got: fixture.message_epoch,
                current: fixture.current_epoch,
            },
            "power_duplicate_from_snapshot" | "power_duplicate_after_delta" => {
                CompositorStateError::DuplicatePowerState {
                    state: fixture.state.expect("duplicate state"),
                }
            }
            "power_replayed_sequence" | "replayed_power_sequence" => {
                CompositorStateError::NonMonotonicPowerSequence {
                    got: fixture.sequence,
                    last: fixture.sequence,
                }
            }
            "stale_request_id" => CompositorStateError::UnexpectedRequestId(fixture.request_id),
            "missing_fd" => CompositorStateError::MissingFileDescriptor(fixture.request_id),
            "multiple_fds" | "fd_on_refusal" => {
                CompositorStateError::UnexpectedFileDescriptorCount {
                    request_id: fixture.request_id,
                    got: fixture.fd_count,
                }
            }
            "role_already_bound" => CompositorStateError::RoleAlreadyBound(role),
            "role_not_bound" => CompositorStateError::RoleNotBound(role),
            other => panic!("unknown refusal scenario {other}"),
        };
        assert_eq!(error, expected_error, "{stem} exercised the wrong refusal");
        if fixture.scenario.starts_with("power_duplicate_") {
            assert_eq!(
                session.snapshot().unwrap().power.last_sequence,
                fixture.expected_last_sequence.expect("retained watermark"),
                "{stem} advanced the watermark for a non-transition"
            );
        }
    }
    assert_eq!(seen, expected);

    // Positive controls run in the same invocation: current epoch, complete snapshot, fresh
    // Power sequence, one pending request, exactly one fd, destroy-time release, and re-mint.
    let mut good = CompositorSession::new(42);
    good.apply_snapshot(snapshot_for_epoch(42)).unwrap();
    assert_eq!(
        good.protected_power(42, 93, PowerState::Pressed),
        Ok(PowerStep::Accepted)
    );
    assert_eq!(
        good.protected_power(42, 94, PowerState::Released),
        Ok(PowerStep::Accepted)
    );
    assert_eq!(
        good.protected_power(42, 95, PowerState::Pressed),
        Ok(PowerStep::Accepted)
    );
    good.begin_trusted_connection(42, 8001, pf_input_ipc::OverlayRole::Menu)
        .unwrap();
    good.finish_trusted_connection(42, 8001, 1).unwrap();
    good.release_trusted_role(42, pf_input_ipc::OverlayRole::Menu)
        .unwrap();
    good.begin_trusted_connection(42, 8002, pf_input_ipc::OverlayRole::Menu)
        .unwrap();
    good.finish_trusted_connection(42, 8002, 1).unwrap();
}

#[derive(Debug, Deserialize)]
struct RoleReleaseRemintFixture {
    epoch: u64,
    first_request_id: u64,
    second_request_id: u64,
    role: pf_input_ipc::OverlayRole,
    sequence: Vec<String>,
}

#[test]
fn destroyed_role_is_released_before_a_fresh_fd_is_reminted() {
    let path = fixture_dir("transcripts").join("role-release-remint.json");
    let fixture: RoleReleaseRemintFixture =
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(
        fixture.sequence,
        [
            "mint_fresh_fd",
            "destroy_client",
            "release_role",
            "remint_fresh_fd"
        ]
    );
    let mut session = CompositorSession::new(fixture.epoch);
    session
        .apply_snapshot(snapshot_for_epoch(fixture.epoch))
        .unwrap();
    session
        .begin_trusted_connection(fixture.epoch, fixture.first_request_id, fixture.role)
        .unwrap();
    session
        .finish_trusted_connection(fixture.epoch, fixture.first_request_id, 1)
        .unwrap();
    session
        .release_trusted_role(fixture.epoch, fixture.role)
        .unwrap();
    session
        .begin_trusted_connection(fixture.epoch, fixture.second_request_id, fixture.role)
        .unwrap();
    session
        .finish_trusted_connection(fixture.epoch, fixture.second_request_id, 1)
        .unwrap();
}

/// Router death: the new connection refuses deltas and the old epoch until a complete replay,
/// then accepts only fresh Power sequencing and fresh fd-bearing trust results.
#[test]
fn router_restart_requires_snapshot_and_refuses_old_epoch_state() {
    let tx = recovery_transcript("router-restart");
    assert_eq!(
        tx.sequence,
        [
            "delta_before_snapshot",
            "snapshot",
            "pending_trust",
            "eof",
            "reconnect",
            "old_epoch_result_with_fd",
            "replay",
            "remint_roles_with_fresh_fds",
            "power",
            "replayed_power_sequence",
        ]
    );
    let mut old = CompositorSession::new(tx.old_epoch);

    assert_eq!(
        old.accept_delta(tx.old_epoch),
        Err(CompositorStateError::SnapshotRequired),
        "negative control: no deltas before the authoritative snapshot"
    );
    old.apply_snapshot(tx.snapshot.clone()).unwrap();
    assert_eq!(old.accept_delta(tx.old_epoch), Ok(()));
    old.begin_trusted_connection(
        tx.old_epoch,
        tx.request_id,
        pf_input_ipc::OverlayRole::Keyboard,
    )
    .unwrap();

    let effects = old.disconnect(true);
    assert!(effects.secure_entry);
    assert!(effects.quarantine_pad);
    assert!(effects.close_role_peers);
    assert!(effects.safe_return);
    assert_eq!(effects.invalidated_request_ids, vec![tx.request_id]);

    let mut fresh = CompositorSession::new(tx.new_epoch);
    assert_eq!(
        fresh.apply_snapshot(tx.snapshot.clone()),
        Err(CompositorStateError::OldEpoch {
            got: tx.old_epoch,
            current: tx.new_epoch,
        })
    );
    assert_eq!(
        fresh.finish_trusted_connection(tx.old_epoch, tx.request_id, 1),
        Err(CompositorStateError::OldEpoch {
            got: tx.old_epoch,
            current: tx.new_epoch,
        }),
        "an old result and attached fd cannot revive trust"
    );

    let mut replay = tx.snapshot.clone();
    replay.epoch = tx.new_epoch;
    fresh.apply_snapshot(replay).unwrap();
    assert_eq!(fresh.snapshot().unwrap().epoch, tx.new_epoch);
    assert!(fresh.snapshot().unwrap().focus.secure_entry);
    assert_eq!(
        fresh
            .snapshot()
            .unwrap()
            .overlays
            .iter()
            .filter(|overlay| overlay.state == OverlayState::Mapped)
            .count(),
        2,
        "unchanged keyboard and menu overlay state is restored by replay"
    );
    assert_eq!(fresh.accept_delta(tx.new_epoch), Ok(()));
    assert_eq!(
        fresh.apply_snapshot({
            let mut duplicate = tx.snapshot.clone();
            duplicate.epoch = tx.new_epoch;
            duplicate
        }),
        Err(CompositorStateError::SnapshotAlreadyApplied)
    );

    assert_eq!(
        fresh.protected_power(tx.new_epoch, tx.next_power_sequence, PowerState::Pressed),
        Ok(PowerStep::Accepted)
    );
    let fresh_request = tx.request_id + 1;
    fresh
        .begin_trusted_connection(
            tx.new_epoch,
            fresh_request,
            pf_input_ipc::OverlayRole::Keyboard,
        )
        .unwrap();
    fresh
        .finish_trusted_connection(tx.new_epoch, fresh_request, 1)
        .unwrap();
    fresh
        .begin_trusted_connection(
            tx.new_epoch,
            fresh_request + 1,
            pf_input_ipc::OverlayRole::Menu,
        )
        .unwrap();
    fresh
        .finish_trusted_connection(tx.new_epoch, fresh_request + 1, 1)
        .unwrap();
    assert_eq!(
        fresh.protected_power(tx.new_epoch, tx.next_power_sequence, PowerState::Pressed),
        Err(CompositorStateError::NonMonotonicPowerSequence {
            got: tx.next_power_sequence,
            last: tx.next_power_sequence,
        })
    );
}

/// Gamescope death: pending trust is invalidated fail-closed; recovery needs a complete snapshot,
/// and a replacement Wayland fd is accepted only for a pending request in the new epoch.
#[test]
fn gamescope_restart_remints_roles_and_rejects_missing_or_unexpected_fds() {
    let tx = recovery_transcript("gamescope-restart");
    assert_eq!(
        tx.sequence,
        [
            "snapshot",
            "live_role_and_pending_trust",
            "eof",
            "reconnect",
            "incomplete_replay",
            "duplicate_role_replay",
            "replay",
            "old_epoch_result_with_fd",
            "unexpected_result_with_fd",
            "pending_result_without_fd",
            "pending_result_with_two_fds",
            "remint_roles_with_fresh_fds",
            "refused_result_with_fd",
            "refused_result_without_fd",
        ]
    );
    let mut session = CompositorSession::new(tx.old_epoch);
    session.apply_snapshot(tx.snapshot.clone()).unwrap();
    session
        .begin_trusted_connection(tx.old_epoch, tx.request_id, pf_input_ipc::OverlayRole::Menu)
        .unwrap();
    session
        .finish_trusted_connection(tx.old_epoch, tx.request_id, 1)
        .unwrap();
    assert_eq!(
        session.begin_trusted_connection(
            tx.old_epoch,
            tx.request_id + 99,
            pf_input_ipc::OverlayRole::Menu,
        ),
        Err(CompositorStateError::RoleAlreadyBound(
            pf_input_ipc::OverlayRole::Menu,
        ))
    );
    let pending_id = tx.request_id + 1;
    session
        .begin_trusted_connection(
            tx.old_epoch,
            pending_id,
            pf_input_ipc::OverlayRole::Keyboard,
        )
        .unwrap();
    let effects = session.disconnect(false);
    assert!(effects.secure_entry && effects.quarantine_pad && effects.close_role_peers);
    assert!(!effects.safe_return);
    assert_eq!(effects.invalidated_request_ids, vec![pending_id]);

    let mut fresh = CompositorSession::new(tx.new_epoch);
    let mut incomplete = tx.snapshot.clone();
    incomplete.epoch = tx.new_epoch;
    incomplete.overlays.pop();
    assert_eq!(
        fresh.apply_snapshot(incomplete),
        Err(CompositorStateError::IncompleteSnapshot)
    );
    let mut duplicate_role = tx.snapshot.clone();
    duplicate_role.epoch = tx.new_epoch;
    duplicate_role.overlays[2].role = pf_input_ipc::OverlayRole::Keyboard;
    assert_eq!(
        fresh.apply_snapshot(duplicate_role),
        Err(CompositorStateError::IncompleteSnapshot)
    );

    let mut replay = tx.snapshot.clone();
    replay.epoch = tx.new_epoch;
    fresh.apply_snapshot(replay).unwrap();
    assert_eq!(fresh.snapshot().unwrap().focus, tx.snapshot.focus);
    assert_eq!(
        fresh.finish_trusted_connection(tx.old_epoch, pending_id, 1),
        Err(CompositorStateError::OldEpoch {
            got: tx.old_epoch,
            current: tx.new_epoch,
        }),
        "the fd from the dead compositor cannot revive trust"
    );
    assert_eq!(
        fresh.finish_trusted_connection(tx.new_epoch, pending_id, 1),
        Err(CompositorStateError::UnexpectedRequestId(pending_id)),
        "an fd is accepted only for a request issued in this epoch"
    );
    fresh
        .begin_trusted_connection(tx.new_epoch, tx.request_id, pf_input_ipc::OverlayRole::Menu)
        .unwrap();
    assert_eq!(
        fresh.finish_trusted_connection(tx.new_epoch, tx.request_id, 0),
        Err(CompositorStateError::MissingFileDescriptor(tx.request_id))
    );
    assert_eq!(
        fresh.finish_trusted_connection(tx.new_epoch, tx.request_id, 2),
        Err(CompositorStateError::UnexpectedFileDescriptorCount {
            request_id: tx.request_id,
            got: 2,
        })
    );
    assert_eq!(
        fresh.finish_trusted_connection(tx.new_epoch, tx.request_id, 1),
        Ok(())
    );
    fresh
        .begin_trusted_connection(
            tx.new_epoch,
            pending_id,
            pf_input_ipc::OverlayRole::Keyboard,
        )
        .unwrap();
    fresh
        .finish_trusted_connection(tx.new_epoch, pending_id, 1)
        .unwrap();
    let refused_id = pending_id + 1;
    fresh
        .begin_trusted_connection(tx.new_epoch, refused_id, pf_input_ipc::OverlayRole::Toast)
        .unwrap();
    assert_eq!(
        fresh.refuse_trusted_connection(tx.new_epoch, refused_id, 1),
        Err(CompositorStateError::UnexpectedFileDescriptorCount {
            request_id: refused_id,
            got: 1,
        })
    );
    fresh
        .refuse_trusted_connection(tx.new_epoch, refused_id, 0)
        .unwrap();
}
