use pf_volumed::{MixerBackend, Output, VolumePreferences, VolumeService};

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
