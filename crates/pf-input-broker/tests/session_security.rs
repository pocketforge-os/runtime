use std::time::Duration;

use pf_input_broker::router::{
    AppProbeEffect, AppSectionProbe, EpochEffect, EpochFdGate, PeerIdentity, SdkCall, SdkCallError,
    SdkCallGate, SessionSignals, UserPressGate,
};
use pf_input_ipc::compositor::{PhysicalInputClass, RouterToCompositor};
use pf_input_ipc::events::RouterEvent;
use pf_input_ipc::OverlayRole;

fn peer(uid: u32, unit: &str) -> PeerIdentity {
    PeerIdentity::new(uid, unit)
}

#[test]
fn sdk_calls_require_the_session_uid_matching_unit_and_front_app() {
    let mut gate = SdkCallGate::new(1000);
    gate.set_front("org.example.front", "pf-app@front.service");

    assert_eq!(
        gate.authorize(
            "org.example.front",
            &peer(1000, "pf-app@front.service"),
            SdkCall::ShowKeyboard,
            32,
            Duration::ZERO,
        ),
        Ok(())
    );
    assert_eq!(
        gate.authorize(
            "org.example.background",
            &peer(1000, "pf-app@background.service"),
            SdkCall::ClaimPointer,
            32,
            Duration::ZERO,
        ),
        Err(SdkCallError::NotFront)
    );
    assert_eq!(
        gate.authorize(
            "org.example.front",
            &peer(1000, "pf-app@impostor.service"),
            SdkCall::UpdateMenu,
            32,
            Duration::ZERO,
        ),
        Err(SdkCallError::UnitMismatch)
    );
    assert_eq!(
        gate.authorize(
            "org.example.front",
            &peer(1001, "pf-app@front.service"),
            SdkCall::ShowKeyboard,
            32,
            Duration::ZERO,
        ),
        Err(SdkCallError::WrongUid)
    );
}

#[test]
fn every_sdk_call_is_size_and_rate_bounded_without_poisoning_valid_calls() {
    let mut gate = SdkCallGate::new(1000);
    gate.set_front("org.example.game", "pf-app@game.service");
    let identity = peer(1000, "pf-app@game.service");

    assert_eq!(
        gate.authorize(
            "org.example.game",
            &identity,
            SdkCall::ShowKeyboard,
            SdkCallGate::MAX_BODY_BYTES + 1,
            Duration::ZERO,
        ),
        Err(SdkCallError::TooLarge)
    );
    for index in 0..SdkCallGate::CALLS_PER_SECOND {
        assert_eq!(
            gate.authorize(
                "org.example.game",
                &identity,
                SdkCall::ShowKeyboard,
                64,
                Duration::from_millis(index as u64),
            ),
            Ok(())
        );
    }
    assert_eq!(
        gate.authorize(
            "org.example.game",
            &identity,
            SdkCall::ShowKeyboard,
            64,
            Duration::from_millis(99),
        ),
        Err(SdkCallError::RateLimited)
    );
    assert_eq!(
        gate.authorize(
            "org.example.game",
            &identity,
            SdkCall::ShowKeyboard,
            64,
            Duration::from_secs(1),
        ),
        Ok(())
    );
}

#[test]
fn one_routed_user_press_admits_one_front_app_open_for_one_second() {
    let mut gate = UserPressGate::default();
    gate.record("org.example.game", Duration::from_millis(10));
    assert!(!gate.consume("org.example.other", Duration::from_millis(20)));
    assert!(gate.consume("org.example.game", Duration::from_millis(1_010)));
    assert!(!gate.consume("org.example.game", Duration::from_millis(1_010)));

    gate.record("org.example.game", Duration::from_secs(2));
    assert!(!gate.consume("org.example.game", Duration::from_millis(3_001)));
}

#[test]
fn reregister_before_snapshot_never_delivers_an_old_or_early_fd() {
    let mut gate = EpochFdGate::default();
    assert_eq!(
        gate.register(41),
        vec![EpochEffect::Registered { epoch: 41 }]
    );
    assert_eq!(
        gate.register(42),
        vec![EpochEffect::Registered { epoch: 42 }]
    );
    assert!(
        gate.snapshot_applied(41).is_empty(),
        "stale epoch is ignored"
    );
    assert_eq!(
        gate.snapshot_applied(42),
        vec![
            EpochEffect::SnapshotApplied { epoch: 42 },
            EpochEffect::FdDelivered { epoch: 42 },
        ]
    );
}

#[test]
fn app_probe_dims_only_its_section_and_late_pong_recovers_without_focus_change() {
    let mut probe = AppSectionProbe::default();
    assert_eq!(
        probe.menu_opened("org.example.game", Duration::ZERO),
        vec![AppProbeEffect::Ping {
            app_id: "org.example.game".into(),
            sequence: 1,
        }]
    );
    assert!(probe.tick(Duration::from_millis(1_999)).is_empty());
    assert_eq!(
        probe.tick(Duration::from_secs(2)),
        vec![AppProbeEffect::DimSection {
            app_id: "org.example.game".into(),
            dimmed: true,
        }]
    );
    assert_eq!(
        probe.pong("org.example.game", 1),
        vec![AppProbeEffect::DimSection {
            app_id: "org.example.game".into(),
            dimmed: false,
        }]
    );
    assert!(probe.pong("org.example.other", 1).is_empty());
}

#[test]
fn modality_and_secure_entry_are_exact_compositor_relays() {
    let mut signals = SessionSignals::default();
    signals.register_compositor(7);
    assert!(signals.physical_input(7, PhysicalInputClass::Mouse).is_empty());
    assert_eq!(signals.snapshot(7, false), vec![RouterEvent::SecureEntry { active: false }]);
    assert_eq!(signals.controller_input(), pf_input_broker::router::InputModality::Controller);
    assert_eq!(
        signals.physical_input(7, PhysicalInputClass::Touch),
        vec![pf_input_broker::router::InputModality::Touch]
    );
    assert_eq!(
        signals.focus(7, true),
        vec![RouterEvent::SecureEntry { active: true }]
    );
    assert_eq!(
        signals.menu_secure_entry(false),
        Some(RouterToCompositor::SurfaceSecureEntry {
            epoch: 7,
            role: OverlayRole::Menu,
            active: false,
        })
    );
    assert!(signals.focus(6, false).is_empty(), "stale epochs cannot clear secrecy");
    assert_eq!(signals.disconnect(), vec![RouterEvent::SecureEntry { active: true }]);
    assert_eq!(signals.menu_secure_entry(false), None);
}
