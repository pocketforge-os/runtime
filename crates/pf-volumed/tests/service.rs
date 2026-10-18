use pf_volumed::{MixerBackend, Output, VolumePreferences, VolumeService};

#[derive(Debug, Default)]
struct FakeMixer {
    applied: Vec<(Output, u8, bool)>,
    fail_next: bool,
}

impl MixerBackend for FakeMixer {
    type Error = &'static str;

    fn apply(&mut self, output: Output, percent: u8, muted: bool) -> Result<(), Self::Error> {
        if std::mem::take(&mut self.fail_next) {
            return Err("injected mixer failure");
        }
        self.applied.push((output, percent, muted));
        Ok(())
    }
}

fn prefs(speaker: u8, headphones: u8, muted: bool) -> VolumePreferences {
    VolumePreferences::new(speaker, headphones, muted).unwrap()
}

#[test]
fn preference_change_applies_only_the_active_output() {
    let mut service = VolumeService::new(FakeMixer::default(), false, prefs(60, 40, false));

    service.apply_current().unwrap();
    service.preferences_changed(prefs(25, 75, false)).unwrap();

    assert_eq!(
        service.backend().applied,
        vec![(Output::Speaker, 60, false), (Output::Speaker, 25, false)]
    );
    assert!(service
        .backend()
        .applied
        .iter()
        .all(|(output, _, _)| *output != Output::Headphones));
}

#[test]
fn plug_transitions_switch_endpoint_and_reapply_its_own_preference() {
    let mut service = VolumeService::new(FakeMixer::default(), false, prefs(61, 39, false));

    service.apply_current().unwrap();
    service.plug_changed(true).unwrap();
    service.plug_changed(false).unwrap();

    assert_eq!(
        service.backend().applied,
        vec![
            (Output::Speaker, 61, false),
            (Output::Headphones, 39, false),
            (Output::Speaker, 61, false),
        ]
    );
    assert_eq!(service.active_output(), Output::Speaker);
}

#[test]
fn mute_round_trip_preserves_the_active_outputs_level() {
    let mut service = VolumeService::new(FakeMixer::default(), true, prefs(60, 42, false));

    service.apply_current().unwrap();
    service.preferences_changed(prefs(60, 42, true)).unwrap();
    service.preferences_changed(prefs(60, 42, false)).unwrap();

    assert_eq!(
        service.backend().applied,
        vec![
            (Output::Headphones, 42, false),
            (Output::Headphones, 42, true),
            (Output::Headphones, 42, false),
        ]
    );
}

#[test]
fn failed_plug_apply_keeps_the_old_endpoint_and_a_retry_can_succeed() {
    let mut service = VolumeService::new(FakeMixer::default(), false, prefs(55, 35, false));
    service.apply_current().unwrap();
    service.backend_mut().fail_next = true;

    assert_eq!(service.plug_changed(true), Err("injected mixer failure"));
    assert_eq!(service.active_output(), Output::Speaker);
    assert_eq!(
        service.backend().applied,
        vec![(Output::Speaker, 55, false)]
    );

    service.plug_changed(true).unwrap();
    assert_eq!(service.active_output(), Output::Headphones);
    assert_eq!(
        service.backend().applied,
        vec![
            (Output::Speaker, 55, false),
            (Output::Headphones, 35, false),
        ]
    );
}

#[test]
fn volume_preferences_reject_out_of_range_values() {
    assert!(VolumePreferences::new(100, 0, false).is_ok());
    assert!(VolumePreferences::new(101, 40, false).is_err());
    assert!(VolumePreferences::new(60, 101, false).is_err());
}
