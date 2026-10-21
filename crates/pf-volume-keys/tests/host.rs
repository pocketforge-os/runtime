use pf_volume_keys::host::{
    advertises_volume_keys, decode_feed, encode_feed, run, KeyEvent, KeySource, ToastSink,
};
use pf_volume_keys::{Key, PrefStore, ToastModel, VolumeKeys, VolumeState};
use pf_volumed::jack::{RouteError, RouteSource};
use pf_volumed::Output;
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

struct Store(VolumeState, Vec<(String, Value)>, bool);
impl PrefStore for Store {
    fn load(&mut self) -> Result<VolumeState, String> {
        Ok(self.0)
    }
    fn store(&mut self, key: &str, value: Value) -> Result<(), String> {
        if self.2 {
            return Err("write refused".into());
        }
        self.1.push((key.into(), value.clone()));
        if key == "volumeSpeaker" {
            self.0.speaker = value.as_u64().unwrap() as u8;
        }
        Ok(())
    }
}
struct Route;
impl RouteSource for Route {
    fn route(&mut self) -> Result<Output, RouteError> {
        Ok(Output::Speaker)
    }
}
/// Scripted events at fake times; the source advances a shared fake clock.
struct Script(VecDeque<(u64, KeyEvent)>, std::rc::Rc<std::cell::Cell<u64>>);
impl KeySource for Script {
    fn next_event(&mut self, timeout: Duration) -> Result<Option<KeyEvent>, String> {
        let now = self.1.get();
        match self.0.front() {
            Some((at, _)) if *at <= now + timeout.as_millis() as u64 => {
                let (at, event) = self.0.pop_front().unwrap();
                self.1.set(at.max(now));
                Ok(Some(event))
            }
            Some(_) => {
                self.1.set(now + timeout.as_millis() as u64);
                Ok(None)
            }
            None => Err("script exhausted".into()),
        }
    }
}
#[derive(Default)]
struct Sink(Vec<ToastModel>);
impl ToastSink for Sink {
    fn show(&mut self, model: &ToastModel) {
        self.0.push(model.clone());
    }
}

fn drive(events: &[(u64, Key, u8)], fail: bool) -> (Sink, Store) {
    let clock = std::rc::Rc::new(std::cell::Cell::new(0));
    let mut source = Script(
        events
            .iter()
            .map(|(at, key, value)| {
                (
                    *at,
                    KeyEvent {
                        key: *key,
                        value: *value,
                    },
                )
            })
            .collect(),
        clock.clone(),
    );
    let mut volume = VolumeKeys::new(
        Store(
            VolumeState {
                speaker: 20,
                headphones: 40,
                muted: false,
            },
            vec![],
            fail,
        ),
        Route,
    );
    let mut sink = Sink::default();
    let stop = AtomicBool::new(false);
    let c = clock.clone();
    let _ = run(
        &mut volume,
        &mut source,
        &mut sink,
        move || Duration::from_millis(c.get()),
        &stop,
    );
    let store = volume.store();
    let store = Store(store.0, store.1.clone(), false);
    (sink, store)
}

#[test]
fn a_held_key_produces_a_toast_per_step_through_the_run_loop_and_stops_on_release() {
    // down at 0, held 1000 ms (value 2 kernel repeats ignored), up at 1000, then the script ends.
    let (sink, store) = drive(
        &[(0, Key::Up, 1), (200, Key::Up, 2), (1000, Key::Up, 0)],
        false,
    );
    // press step + repeats at 400..=900 (6); the release at 1000 lands before that tick, so no more.
    assert_eq!(sink.0.len(), 7, "{:?}", sink.0);
    assert_eq!(store.0.speaker, 55);
}

#[test]
fn write_failure_reaches_the_sink_as_an_error_toast_and_the_loop_survives() {
    let (sink, _) = drive(
        &[
            (0, Key::Down, 1),
            (50, Key::Down, 0),
            (900, Key::Up, 1),
            (950, Key::Up, 0),
        ],
        true,
    );
    assert_eq!(sink.0.len(), 2);
    assert!(sink.0.iter().all(|m| matches!(m, ToastModel::Error { .. })));
}

#[test]
fn feed_lines_round_trip_and_reject_garbage() {
    for model in [
        ToastModel::Level {
            percent: 65,
            output: Output::Speaker,
            muted: false,
        },
        ToastModel::Level {
            percent: 40,
            output: Output::Headphones,
            muted: true,
        },
        ToastModel::Error {
            message: "write refused".into(),
        },
    ] {
        assert_eq!(decode_feed(&encode_feed(&model)).unwrap(), model);
    }
    for bad in [
        "",
        "level 101 speaker 0",
        "level 5 radio 0",
        "level x speaker 0",
        "wat",
        "level 5 speaker 2",
    ] {
        assert!(decode_feed(bad).is_err(), "{bad:?}");
    }
    assert!(!encode_feed(&ToastModel::Error {
        message: "a\nb".into()
    })
    .trim_end()
    .contains('\n'));
}

#[test]
fn only_a_node_advertising_both_volume_keys_is_a_key_source() {
    // KEY_VOLUMEDOWN=114, KEY_VOLUMEUP=115 live in word 1 (bits 50 and 51) of 64-bit words.
    let both = format!("{:x} 0", (1_u64 << 50) | (1_u64 << 51));
    assert!(advertises_volume_keys(&both, 64));
    let only_up = format!("{:x} 0", 1_u64 << 51);
    assert!(!advertises_volume_keys(&only_up, 64));
    assert!(!advertises_volume_keys("0", 64));
    assert!(!advertises_volume_keys("zz 0", 64));
}

#[test]
fn an_absent_jack_is_a_route_error_not_a_guess() {
    let mut lazy = pf_volume_keys::host::LazyJack::new("/nonexistent-sysfs", "/dev/input");
    assert!(pf_volumed::jack::RouteSource::route(&mut lazy).is_err());
}

#[test]
fn prefs_endpoint_precedence_is_flag_then_env_then_default() {
    use pf_volume_keys::host::{resolve_prefs_sock, DEFAULT_PREFS_SOCK};
    use std::path::PathBuf;
    let env = |v: &str| Some(std::ffi::OsString::from(v));
    assert_eq!(
        resolve_prefs_sock(Some("/flag.sock".into()), env("/env.sock")),
        PathBuf::from("/flag.sock")
    );
    assert_eq!(
        resolve_prefs_sock(None, env("/env.sock")),
        PathBuf::from("/env.sock")
    );
    assert_eq!(
        resolve_prefs_sock(None, None),
        PathBuf::from(DEFAULT_PREFS_SOCK)
    );
    // An empty override is not an endpoint.
    assert_eq!(
        resolve_prefs_sock(None, env("")),
        PathBuf::from(DEFAULT_PREFS_SOCK)
    );
}
