use pf_volumed::jack::{RouteError, RouteSource};
use pf_volumed::{MixerBackend, Output, SyncError, VolumePreferences, VolumeService};

#[derive(Debug, Eq, PartialEq)]
enum Operation {
    Apply(Output, u8, bool),
    Quiesce,
    Select(Output),
    TransitionApply(Output, u8, bool),
}

#[derive(Debug, Default)]
struct FakeMixer {
    operations: Vec<Operation>,
    fail_apply: bool,
    fail_transition_after_select: bool,
    quiesced: bool,
}

impl MixerBackend for FakeMixer {
    type Error = &'static str;

    fn quiesce(&mut self) -> Result<(), Self::Error> {
        self.operations.push(Operation::Quiesce);
        self.quiesced = true;
        Ok(())
    }

    fn apply(&mut self, output: Output, percent: u8, muted: bool) -> Result<(), Self::Error> {
        if std::mem::take(&mut self.fail_apply) {
            return Err("injected mixer failure");
        }
        self.operations
            .push(Operation::Apply(output, percent, muted));
        Ok(())
    }

    fn transition(&mut self, to: Output, percent: u8, muted: bool) -> Result<(), Self::Error> {
        self.quiesce()?;
        self.operations.push(Operation::Select(to));
        if std::mem::take(&mut self.fail_transition_after_select) {
            return Err("injected route failure");
        }
        self.operations
            .push(Operation::TransitionApply(to, percent, muted));
        self.quiesced = false;
        Ok(())
    }
}

fn prefs(speaker: u8, headphones: u8, muted: bool) -> VolumePreferences {
    VolumePreferences::new(speaker, headphones, muted).unwrap()
}

#[test]
fn preference_change_applies_only_the_active_output() {
    let mut service =
        VolumeService::new(FakeMixer::default(), Output::Speaker, prefs(60, 40, false));

    service.apply_current().unwrap();
    service
        .reconcile(Output::Speaker, prefs(25, 75, false))
        .unwrap();
    service
        .reconcile(Output::Speaker, prefs(25, 75, false))
        .unwrap();

    assert_eq!(
        service.backend().operations,
        vec![
            Operation::Apply(Output::Speaker, 60, false),
            Operation::Apply(Output::Speaker, 25, false),
        ]
    );
}

#[test]
fn route_transitions_quiesce_before_selecting_and_apply_each_destination_level() {
    let mut service =
        VolumeService::new(FakeMixer::default(), Output::Speaker, prefs(61, 39, false));

    service.apply_current().unwrap();
    service
        .reconcile(Output::Headphones, prefs(61, 39, false))
        .unwrap();
    service
        .reconcile(Output::Speaker, prefs(61, 39, false))
        .unwrap();

    assert_eq!(
        service.backend().operations,
        vec![
            Operation::Apply(Output::Speaker, 61, false),
            Operation::Quiesce,
            Operation::Select(Output::Headphones),
            Operation::TransitionApply(Output::Headphones, 39, false),
            Operation::Quiesce,
            Operation::Select(Output::Speaker),
            Operation::TransitionApply(Output::Speaker, 61, false),
        ]
    );
    assert_eq!(service.active_output(), Output::Speaker);
}

#[test]
fn route_and_preference_change_use_the_new_destination_value_in_one_transaction() {
    let mut service =
        VolumeService::new(FakeMixer::default(), Output::Speaker, prefs(60, 40, false));

    service
        .reconcile(Output::Headphones, prefs(25, 73, true))
        .unwrap();

    assert_eq!(
        service.backend().operations,
        vec![
            Operation::Quiesce,
            Operation::Select(Output::Headphones),
            Operation::TransitionApply(Output::Headphones, 73, true),
        ]
    );
    assert_eq!(service.active_output(), Output::Headphones);
}

#[test]
fn failed_transition_stays_quiesced_and_retry_uses_the_latest_preference() {
    let mut service =
        VolumeService::new(FakeMixer::default(), Output::Speaker, prefs(55, 35, false));
    service.backend_mut().fail_transition_after_select = true;

    assert_eq!(
        service.reconcile(Output::Headphones, prefs(55, 70, false)),
        Err("injected route failure")
    );
    assert_eq!(service.active_output(), Output::Speaker);
    assert!(service.backend().quiesced);

    service
        .reconcile(Output::Headphones, prefs(55, 82, true))
        .unwrap();
    assert_eq!(service.active_output(), Output::Headphones);
    assert_eq!(
        service.backend().operations,
        vec![
            Operation::Quiesce,
            Operation::Select(Output::Headphones),
            Operation::Quiesce,
            Operation::Select(Output::Headphones),
            Operation::TransitionApply(Output::Headphones, 82, true),
        ]
    );
}

#[test]
fn failed_transition_requires_a_safe_transition_even_to_the_old_output() {
    let mut service =
        VolumeService::new(FakeMixer::default(), Output::Speaker, prefs(55, 35, false));
    service.backend_mut().fail_transition_after_select = true;

    assert!(service
        .reconcile(Output::Headphones, prefs(55, 35, false))
        .is_err());
    service
        .reconcile(Output::Speaker, prefs(72, 35, false))
        .unwrap();

    assert_eq!(
        service.backend().operations.last(),
        Some(&Operation::TransitionApply(Output::Speaker, 72, false))
    );
    assert_eq!(service.active_output(), Output::Speaker);
}

#[test]
fn mute_round_trip_preserves_the_active_outputs_level() {
    let mut service = VolumeService::new(
        FakeMixer::default(),
        Output::Headphones,
        prefs(60, 42, false),
    );

    service.apply_current().unwrap();
    service
        .reconcile(Output::Headphones, prefs(60, 42, true))
        .unwrap();
    service
        .reconcile(Output::Headphones, prefs(60, 42, false))
        .unwrap();

    assert_eq!(
        service.backend().operations,
        vec![
            Operation::Apply(Output::Headphones, 42, false),
            Operation::Apply(Output::Headphones, 42, true),
            Operation::Apply(Output::Headphones, 42, false),
        ]
    );
}

#[test]
fn failed_same_route_apply_keeps_the_old_preferences() {
    let mut service =
        VolumeService::new(FakeMixer::default(), Output::Speaker, prefs(55, 35, false));
    service.backend_mut().fail_apply = true;

    assert_eq!(
        service.reconcile(Output::Speaker, prefs(70, 35, false)),
        Err("injected mixer failure")
    );
    service
        .reconcile(Output::Speaker, prefs(55, 35, false))
        .unwrap();
    assert!(service.backend().operations.is_empty());
}

#[test]
fn volume_preferences_reject_out_of_range_values() {
    assert!(VolumePreferences::new(100, 0, false).is_ok());
    assert!(VolumePreferences::new(101, 40, false).is_err());
    assert!(VolumePreferences::new(60, 101, false).is_err());
}

struct FakeSwitch(std::collections::VecDeque<Result<Output, &'static str>>);

impl RouteSource for FakeSwitch {
    fn route(&mut self) -> Result<Output, RouteError> {
        self.0
            .pop_front()
            .expect("script exhausted")
            .map_err(RouteError::new)
    }
}

fn switch(script: &[Result<Output, &'static str>]) -> FakeSwitch {
    FakeSwitch(script.iter().cloned().collect())
}

#[test]
fn startup_with_headphones_out_applies_the_speaker_level_through_a_transition() {
    let mut service = VolumeService::unrouted(FakeMixer::default(), prefs(60, 40, false));
    service
        .sync_route(&mut switch(&[Ok(Output::Speaker)]), prefs(60, 40, false))
        .unwrap();
    assert_eq!(
        service.backend().operations,
        [
            Operation::Quiesce,
            Operation::Select(Output::Speaker),
            Operation::TransitionApply(Output::Speaker, 60, false)
        ]
    );
}

#[test]
fn startup_with_headphones_in_applies_the_headphone_level_never_the_speaker_level() {
    let mut service = VolumeService::unrouted(FakeMixer::default(), prefs(60, 40, false));
    service
        .sync_route(&mut switch(&[Ok(Output::Headphones)]), prefs(60, 40, false))
        .unwrap();
    assert_eq!(
        service.backend().operations,
        [
            Operation::Quiesce,
            Operation::Select(Output::Headphones),
            Operation::TransitionApply(Output::Headphones, 40, false)
        ]
    );
}

#[test]
fn switch_events_in_both_directions_transition_and_an_unchanged_route_is_a_no_op() {
    let mut service = VolumeService::unrouted(FakeMixer::default(), prefs(60, 40, false));
    let mut jack = switch(&[
        Ok(Output::Speaker),
        Ok(Output::Speaker),
        Ok(Output::Headphones),
        Ok(Output::Speaker),
    ]);
    for _ in 0..4 {
        service.sync_route(&mut jack, prefs(60, 40, false)).unwrap();
    }
    assert_eq!(
        service.backend().operations,
        [
            Operation::Quiesce,
            Operation::Select(Output::Speaker),
            Operation::TransitionApply(Output::Speaker, 60, false),
            // second read: same route, same prefs -> nothing
            Operation::Quiesce,
            Operation::Select(Output::Headphones),
            Operation::TransitionApply(Output::Headphones, 40, false),
            Operation::Quiesce,
            Operation::Select(Output::Speaker),
            Operation::TransitionApply(Output::Speaker, 60, false),
        ]
    );
}

#[test]
fn an_unreadable_switch_quiesces_and_the_next_good_read_recovers_with_the_latest_prefs() {
    let mut service = VolumeService::unrouted(FakeMixer::default(), prefs(60, 40, false));
    let mut jack = switch(&[
        Ok(Output::Speaker),
        Err("EVIOCGSW: gone"),
        Ok(Output::Speaker),
    ]);
    service.sync_route(&mut jack, prefs(60, 40, false)).unwrap();
    service.backend_mut().operations.clear();
    let error = service
        .sync_route(&mut jack, prefs(60, 40, false))
        .unwrap_err();
    assert!(matches!(error, SyncError::Route(_)));
    assert_eq!(service.backend().operations, [Operation::Quiesce]);
    service.backend_mut().operations.clear();
    // Even though the route is unchanged, recovery must re-run a full transition at the new pref.
    service.sync_route(&mut jack, prefs(80, 40, false)).unwrap();
    assert_eq!(
        service.backend().operations,
        [
            Operation::Quiesce,
            Operation::Select(Output::Speaker),
            Operation::TransitionApply(Output::Speaker, 80, false)
        ]
    );
}

#[test]
fn no_switch_means_no_level_is_ever_applied() {
    let service = VolumeService::unrouted(FakeMixer::default(), prefs(60, 40, false));
    assert!(service.backend().operations.is_empty());
}

#[test]
fn route_is_known_only_after_a_successful_transition() {
    let mut service = VolumeService::unrouted(FakeMixer::default(), prefs(60, 40, false));
    assert!(!service.route_known());
    service.backend_mut().fail_transition_after_select = true;
    assert!(service
        .reconcile(Output::Headphones, prefs(60, 40, false))
        .is_err());
    assert!(!service.route_known());
    service
        .reconcile(Output::Headphones, prefs(60, 40, false))
        .unwrap();
    assert!(service.route_known());
}
