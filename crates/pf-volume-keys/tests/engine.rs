use pf_volume_keys::{Key, Outcome, PrefStore, ToastModel, VolumeKeys, VolumeState};
use pf_volumed::jack::{RouteError, RouteSource};
use pf_volumed::Output;
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Default)]
struct FakeStore {
    state: Option<VolumeState>,
    writes: Vec<(String, Value)>,
    fail_writes: bool,
}

impl PrefStore for FakeStore {
    fn load(&mut self) -> Result<VolumeState, String> {
        self.state.ok_or_else(|| String::from("prefsd down"))
    }
    fn store(&mut self, key: &str, value: Value) -> Result<(), String> {
        if self.fail_writes {
            return Err(String::from("write refused"));
        }
        self.writes.push((key.to_owned(), value.clone()));
        let state = self.state.as_mut().unwrap();
        match (key, &value) {
            ("volumeSpeaker", Value::Number(n)) => state.speaker = n.as_u64().unwrap() as u8,
            ("volumeHeadphones", Value::Number(n)) => state.headphones = n.as_u64().unwrap() as u8,
            ("muted", Value::Bool(b)) => state.muted = *b,
            _ => panic!("unexpected key {key}"),
        }
        Ok(())
    }
}

struct FakeRoute(Result<Output, &'static str>);

impl RouteSource for FakeRoute {
    fn route(&mut self) -> Result<Output, RouteError> {
        self.0.map_err(RouteError::new)
    }
}

fn keys(
    speaker: u8,
    headphones: u8,
    muted: bool,
    route: Output,
) -> VolumeKeys<FakeStore, FakeRoute> {
    VolumeKeys::new(
        FakeStore {
            state: Some(VolumeState {
                speaker,
                headphones,
                muted,
            }),
            ..FakeStore::default()
        },
        FakeRoute(Ok(route)),
    )
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn level(outcome: &Outcome) -> (u8, Output, bool) {
    match outcome {
        Outcome::Shown(ToastModel::Level {
            percent,
            output,
            muted,
        }) => (*percent, *output, *muted),
        other => panic!("not a level toast: {other:?}"),
    }
}

#[test]
fn up_and_down_step_by_five_and_write_the_active_outputs_key() {
    let mut k = keys(60, 40, false, Output::Speaker);
    assert_eq!(
        level(&k.press(Key::Up, ms(0)).unwrap()),
        (65, Output::Speaker, false)
    );
    k.release(Key::Up);
    assert_eq!(
        level(&k.press(Key::Down, ms(50)).unwrap()),
        (60, Output::Speaker, false)
    );
    k.release(Key::Down);
    assert_eq!(
        k.store().writes,
        [
            ("volumeSpeaker".to_owned(), json!(65)),
            ("volumeSpeaker".to_owned(), json!(60))
        ]
    );
}

#[test]
fn headphones_route_writes_the_headphone_key_and_leaves_the_speaker_level_alone() {
    let mut k = keys(60, 40, false, Output::Headphones);
    assert_eq!(
        level(&k.press(Key::Up, ms(0)).unwrap()),
        (45, Output::Headphones, false)
    );
    assert_eq!(
        k.store().writes,
        [("volumeHeadphones".to_owned(), json!(45))]
    );
    assert_eq!(k.store().state.unwrap().speaker, 60);
}

#[test]
fn levels_clamp_at_the_ends_without_a_redundant_write() {
    let mut k = keys(98, 40, false, Output::Speaker);
    assert_eq!(level(&k.press(Key::Up, ms(0)).unwrap()).0, 100);
    k.release(Key::Up);
    assert_eq!(level(&k.press(Key::Up, ms(10)).unwrap()).0, 100);
    k.release(Key::Up);
    assert_eq!(
        k.store().writes.len(),
        1,
        "no write when already at the maximum"
    );
    let mut k = keys(3, 40, false, Output::Speaker);
    assert_eq!(level(&k.press(Key::Down, ms(0)).unwrap()).0, 0);
    k.release(Key::Down);
    assert_eq!(level(&k.press(Key::Down, ms(10)).unwrap()).0, 0);
    assert_eq!(
        k.store().writes.len(),
        1,
        "0 is just 0; no write below the minimum"
    );
    assert!(
        !k.store().state.unwrap().muted,
        "reaching 0 does not set muted"
    );
}

#[test]
fn holding_repeats_after_400_ms_every_100_ms_and_release_stops_it() {
    let mut k = keys(20, 40, false, Output::Speaker);
    k.press(Key::Up, ms(0)).unwrap();
    assert!(
        k.tick(ms(399)).is_empty(),
        "no repeat before the 400 ms delay"
    );
    assert_eq!(k.tick(ms(400)).len(), 1);
    assert_eq!(k.tick(ms(499)).len(), 0);
    assert_eq!(k.tick(ms(500)).len(), 1);
    // A late tick catches up on every missed 100 ms step instead of dropping them.
    assert_eq!(k.tick(ms(800)).len(), 3);
    assert_eq!(k.store().state.unwrap().speaker, 20 + 5 * 6);
    k.release(Key::Up);
    assert!(k.tick(ms(2000)).is_empty());
}

#[test]
fn a_press_while_muted_unmutes_after_the_level_is_written() {
    let mut k = keys(60, 40, true, Output::Speaker);
    let outcome = k.press(Key::Up, ms(0)).unwrap();
    assert_eq!(level(&outcome), (65, Output::Speaker, false));
    assert_eq!(
        k.store().writes,
        [
            ("volumeSpeaker".to_owned(), json!(65)),
            ("muted".to_owned(), json!(false))
        ],
        "level first, unmute last, so the new level is never exposed before it is stored"
    );
}

#[test]
fn prefsd_write_failure_does_not_crash_and_shows_an_error_toast() {
    let mut k = keys(60, 40, false, Output::Speaker);
    k.store_mut().fail_writes = true;
    match k.press(Key::Up, ms(0)).unwrap() {
        Outcome::Shown(ToastModel::Error { message }) => {
            assert!(message.contains("write refused"), "{message}")
        }
        other => panic!("{other:?}"),
    }
    k.release(Key::Up);
    k.store_mut().fail_writes = false;
    assert_eq!(
        level(&k.press(Key::Up, ms(1000)).unwrap()).0,
        65,
        "recovers on the next press"
    );
}

#[test]
fn unreadable_prefs_or_unknown_route_fail_safe_with_an_error_toast_and_no_write() {
    let mut k = keys(60, 40, false, Output::Speaker);
    k.store_mut().state = None;
    assert!(matches!(
        k.press(Key::Up, ms(0)).unwrap(),
        Outcome::Shown(ToastModel::Error { .. })
    ));
    let mut k = VolumeKeys::new(
        FakeStore {
            state: Some(VolumeState {
                speaker: 60,
                headphones: 40,
                muted: false,
            }),
            ..FakeStore::default()
        },
        FakeRoute(Err("EVIOCGSW: gone")),
    );
    assert!(matches!(
        k.press(Key::Up, ms(0)).unwrap(),
        Outcome::Shown(ToastModel::Error { .. })
    ));
    assert!(k.store().writes.is_empty());
}

#[test]
fn a_repeated_press_event_for_a_held_key_does_not_restart_the_repeat_schedule() {
    let mut k = keys(20, 40, false, Output::Speaker);
    k.press(Key::Up, ms(0)).unwrap();
    assert!(
        k.press(Key::Up, ms(100)).is_none(),
        "kernel autorepeat (value 2) is ignored; this crate repeats itself"
    );
    assert_eq!(k.tick(ms(400)).len(), 1);
}
