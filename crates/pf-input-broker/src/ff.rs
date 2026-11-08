#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    const EINVAL: i32 = -libc::EINVAL;

    #[derive(Clone, Default)]
    struct Log(Rc<RefCell<Vec<String>>>);
    struct Fake(Log, i16);
    impl FfSink for Fake {
        fn upload(&mut self, vib: i16, e: &FfEffect) -> std::io::Result<i16> {
            let id = if vib < 0 {
                self.1 += 1;
                self.1
            } else {
                vib
            };
            (self.0).0.borrow_mut().push(format!(
                "up {id} {:#x} {:#x} {}",
                e.strong, e.weak, e.length_ms
            ));
            Ok(id)
        }
        fn play(&mut self, vib: i16, value: i32) -> std::io::Result<()> {
            (self.0).0.borrow_mut().push(format!("play {vib} {value}"));
            Ok(())
        }
        fn erase(&mut self, vib: i16) -> std::io::Result<()> {
            (self.0).0.borrow_mut().push(format!("erase {vib}"));
            Ok(())
        }
    }

    fn rumble(len: u16) -> FfEffect {
        FfEffect { kind: FF_RUMBLE, strong: 0x8000, weak: 0x2000, length_ms: len }
    }

    fn fwd() -> (Forwarder<Fake>, Log) {
        let log = Log::default();
        let mut f = Forwarder::new(Fake(log.clone(), 40));
        f.focus_gained();
        (f, log)
    }

    #[test]
    fn forwards_upload_play_erase_to_vibrator() {
        let (mut f, log) = fwd();
        assert_eq!(f.upload(3, &rumble(200)), 0);
        f.play(3, 1).unwrap();
        assert_eq!(f.erase(3), 0);
        assert_eq!(*log.0.borrow(), ["up 41 0x8000 0x2000 200", "play 41 1", "erase 41"]);

        // Non-RUMBLE is refused without touching the vibrator; long effects are clamped.
        log.0.borrow_mut().clear();
        let periodic = FfEffect { kind: 0x51, ..rumble(10) };
        assert_eq!(f.upload(4, &periodic), EINVAL);
        assert!(log.0.borrow().is_empty());
        assert_eq!(f.upload(5, &rumble(9000)), 0);
        assert_eq!(*log.0.borrow(), ["up 42 0x8000 0x2000 5000"]);
    }

    #[test]
    fn stops_on_focus_loss_close_and_safe_return() {
        for ev in ["focus", "close", "safe", "shutdown"] {
            let (mut f, log) = fwd();
            assert_eq!(f.upload(1, &rumble(100)), 0);
            f.play(1, 1).unwrap();
            log.0.borrow_mut().clear();
            match ev {
                "focus" => f.focus_lost(),
                "close" => f.reader_closed(),
                "safe" => f.safe_return(),
                _ => f.shutdown(),
            }
            .unwrap();
            assert_eq!(*log.0.borrow(), ["play 41 0", "erase 41"], "{ev}");
            // Nothing plays without focus.
            log.0.borrow_mut().clear();
            f.play(1, 1).unwrap();
            assert!(log.0.borrow().is_empty(), "{ev}");
        }
    }

    #[test]
    fn focus_regained_replays_from_the_retained_effect() {
        let (mut f, log) = fwd();
        f.upload(1, &rumble(100));
        f.focus_lost().unwrap();
        f.focus_gained();
        log.0.borrow_mut().clear();
        f.play(1, 1).unwrap();
        assert_eq!(*log.0.borrow(), ["up 42 0x8000 0x2000 100", "play 42 1"]);
    }

    #[test]
    fn vibrator_selected_by_identity() {
        let c = |p: &str, n: &str, ff: bool| VibratorCandidate {
            path: p.into(),
            name: n.into(),
            has_ff_rumble: ff,
        };
        let a = [
            c("/dev/input/event0", "gpio-vibrator", false), // lookalike without FF
            c("/dev/input/event1", "other", true),
            c("/dev/input/event9", "gpio-vibrator", true),
        ];
        assert_eq!(select_vibrator(&a, "gpio-vibrator").unwrap().path, "/dev/input/event9");
        let b = [a[2].clone(), a[0].clone()];
        assert_eq!(select_vibrator(&b, "gpio-vibrator").unwrap().path, "/dev/input/event9");
        assert!(select_vibrator(&a[..2], "gpio-vibrator").is_none());
    }

    struct FakeUinput {
        events: VecDeque<FfEvent>,
        uploads: Vec<(u32, (i16, FfEffect))>,
        erases: Vec<(u32, i16)>,
        ended: Vec<String>,
    }
    impl FfUinput for FakeUinput {
        fn next_event(&mut self, _timeout_ms: i32) -> std::io::Result<Option<FfEvent>> {
            Ok(self.events.pop_front())
        }
        fn begin_upload(&mut self, rid: u32) -> std::io::Result<(i16, FfEffect)> {
            Ok(self.uploads.iter().find(|u| u.0 == rid).unwrap().1)
        }
        fn end_upload(&mut self, rid: u32, retval: i32) -> std::io::Result<()> {
            self.ended.push(format!("up {rid} {retval}"));
            Ok(())
        }
        fn begin_erase(&mut self, rid: u32) -> std::io::Result<i16> {
            Ok(self.erases.iter().find(|u| u.0 == rid).unwrap().1)
        }
        fn end_erase(&mut self, rid: u32, retval: i32) -> std::io::Result<()> {
            self.ended.push(format!("er {rid} {retval}"));
            Ok(())
        }
    }

    #[test]
    fn pump_answers_upload_erase_requests_and_drives_play() {
        let (mut f, log) = fwd();
        let mut u = FakeUinput {
            events: VecDeque::from([
                FfEvent::Upload(7),
                FfEvent::Play { id: 2, value: 1 },
                FfEvent::Upload(8),
                FfEvent::Erase(9),
            ]),
            uploads: vec![(7, (2, rumble(50))), (8, (3, FfEffect { kind: 0x52, ..rumble(5) }))],
            erases: vec![(9, 2)],
            ended: vec![],
        };
        while pump_once(&mut u, &mut f, 0).unwrap() {}
        assert_eq!(u.ended, ["up 7 0", format!("up 8 {EINVAL}").as_str(), "er 9 0"]);
        assert_eq!(
            *log.0.borrow(),
            ["up 41 0x8000 0x2000 50", "play 41 1", "erase 41"]
        );
    }

    #[test]
    fn rumble_actuator_is_read_from_the_descriptor() {
        let d = |extra: &str| {
            pocketforge::descriptor::Descriptor::from_toml(&format!(
                "[identity]\nid=\"x\"\nmanufacturer=\"x\"\nmodel=\"y\"\nsdl_guid=\"00000000000000000000000000000000\"\n\
                 [[inputs]]\nid=\"south\"\nkind=\"button\"\nev_type=\"EV_KEY\"\ncode=\"BTN_A\"\n{extra}"
            ))
            .unwrap()
        };
        let with = d("[[actuators]]\nid=\"rumble\"\nkind=\"rumble\"\nev_type=\"EV_FF\"\ncode=\"FF_RUMBLE\"\nsource=\"gpio-vibrator\"\n");
        let led = d("[[actuators]]\nid=\"leds\"\nkind=\"led_array\"\ncount=3\n");
        assert_eq!(vibrator_source(&with).as_deref(), Some("gpio-vibrator"));
        assert_eq!(vibrator_source(&led), None);
        assert_eq!(vibrator_source(&d("")), None);
    }
}
