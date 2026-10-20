use pf_volumed::jack::{
    advertises_headphone_switch, discover, route_from_switch_bits, EvdevJack, JACK_DEVICE_NAME,
};
use pf_volumed::Output;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("pf-volumed-jack-{}-{nonce}", std::process::id()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn device(&self, event: &str, name: &str) {
        let dir = self.0.join(event).join("device");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("name"), format!("{name}\n")).unwrap();
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        fn clear(path: &Path) {
            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    let child = entry.path();
                    if child.is_dir() {
                        clear(&child);
                        let _ = fs::remove_dir(&child);
                    } else {
                        let _ = fs::remove_file(&child);
                    }
                }
            }
        }
        clear(&self.0);
        let _ = fs::remove_dir(&self.0);
    }
}

#[test]
fn switch_bitmap_maps_only_the_headphone_insert_bit() {
    let mut bits = [0_u8; 8];
    assert_eq!(route_from_switch_bits(&bits).unwrap(), Output::Speaker);
    bits[0] = 0b0000_0100;
    assert_eq!(route_from_switch_bits(&bits).unwrap(), Output::Headphones);
    // Negative controls: neighbouring switches (lineout/microphone insert) are not the headphone route.
    bits[0] = 0b0000_1011;
    assert_eq!(route_from_switch_bits(&bits).unwrap(), Output::Speaker);
    assert!(route_from_switch_bits(&[]).is_err());
}

#[test]
fn only_a_device_advertising_sw_headphone_insert_is_a_route_source() {
    assert!(advertises_headphone_switch(&[0b0000_0100]));
    assert!(advertises_headphone_switch(&[0b0000_1111, 0xff]));
    // Negative controls: other switches only, nothing, and a truncated capability bitmap.
    assert!(!advertises_headphone_switch(&[0b0000_1011]));
    assert!(!advertises_headphone_switch(&[0]));
    assert!(!advertises_headphone_switch(&[]));
}

#[test]
fn discovery_matches_the_exact_device_name() {
    let sysfs = Scratch::new();
    sysfs.device("event0", "adc-keys");
    sysfs.device("event3", JACK_DEVICE_NAME);
    sysfs.device("event5", "H616 Audio Codec Headphone Jack extra");
    let found = discover(&sysfs.0, Path::new("/dev/input")).unwrap();
    assert_eq!(found, Some(PathBuf::from("/dev/input/event3")));
}

#[test]
fn discovery_distinguishes_absent_ambiguous_and_unreadable() {
    let sysfs = Scratch::new();
    sysfs.device("event0", "adc-keys");
    assert_eq!(discover(&sysfs.0, Path::new("/dev/input")).unwrap(), None);
    sysfs.device("event1", JACK_DEVICE_NAME);
    sysfs.device("event2", JACK_DEVICE_NAME);
    assert!(discover(&sysfs.0, Path::new("/dev/input")).is_err());
    assert!(discover(&sysfs.0.join("missing"), Path::new("/dev/input")).is_err());
}

#[test]
fn a_node_that_is_not_an_evdev_switch_is_an_unreadable_route() {
    let scratch = Scratch::new();
    let node = scratch.0.join("event0");
    fs::write(&node, b"").unwrap();
    // An all-zero EVIOCGSW would read as "speaker"; a node that cannot prove the capability is not a source.
    assert!(
        EvdevJack::open(&node).is_err(),
        "a non-evdev file must not be accepted as the route source"
    );
    assert!(EvdevJack::open(&scratch.0.join("absent")).is_err());
}
