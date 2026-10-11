//! SECURITY (`tsp-ihne1.8`): the built-in controller is recognised by its virtual-device path, the
//! decoder-set `phys` property and the decoder's root-owned record, never by name.
//!
//! Each case builds a fake sysfs tree laid out like the kernel's (`/sys/dev/char/13:<minor>`
//! symlinks into `/sys/devices/...`), so the verdicts are hermetic: CI has no `/dev/uinput`. The
//! real decoder device (the positive control) and every impostor run in the same invocation, and
//! the impostors carry byte-identical `name`, `id/*` and capability files, so the old name+id
//! match could not tell them apart (asserted below, the negative control for the old rule).

use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};

use pf_input_decode::builtin::{self, Reason, Trust, Verdict};

const INPUT_MAJOR: u32 = 13;
/// What the decoder writes, and what a perfect clone copies (`lib.rs::identity`).
const NAME: &str = "TRIMUI Player1\n";
const KEY_CAPS: &str = "7cdb000000000000 0\n";

struct Sys {
    _dir: tempfile::TempDir,
    root: PathBuf,
    record_dir: PathBuf,
}

impl Sys {
    fn new() -> Sys {
        let dir = tempfile::Builder::new()
            .prefix("builtin-identity.")
            .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .expect("tempdir");
        let root = dir.path().join("sys");
        let record_dir = dir.path().join("run/pocketforge-input");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&record_dir).unwrap();
        fs::set_permissions(&record_dir, fs::Permissions::from_mode(0o755)).unwrap();
        Sys {
            _dir: dir,
            root,
            record_dir,
        }
    }

    fn trust(&self) -> Trust {
        Trust::with_roots(&self.root, &self.record_dir, current_uid())
    }

    /// One input device at `devpath` (relative to the sysfs root) with an `eventN` child whose char
    /// device is 13:`minor`. `phys` is what the creator set (empty string = unset, as sysfs shows).
    fn device(&self, devpath: &str, event: &str, minor: u32, phys: &str) {
        let dev = self.root.join(devpath);
        fs::create_dir_all(dev.join("id")).unwrap();
        fs::create_dir_all(dev.join("capabilities")).unwrap();
        fs::write(dev.join("name"), NAME).unwrap();
        fs::write(dev.join("phys"), format!("{phys}\n")).unwrap();
        fs::write(dev.join("id/bustype"), "0003\n").unwrap();
        fs::write(dev.join("id/vendor"), "045e\n").unwrap();
        fs::write(dev.join("id/product"), "028e\n").unwrap();
        fs::write(dev.join("id/version"), "0110\n").unwrap();
        fs::write(dev.join("capabilities/key"), KEY_CAPS).unwrap();
        fs::write(dev.join("capabilities/abs"), "3003b\n").unwrap();
        fs::create_dir_all(dev.join(event)).unwrap();
        fs::write(
            dev.join(event).join("dev"),
            format!("{INPUT_MAJOR}:{minor}\n"),
        )
        .unwrap();
        let chr = self.root.join("dev/char");
        fs::create_dir_all(&chr).unwrap();
        // Relative, like the kernel's own links: ../../devices/...
        symlink(
            Path::new("../..").join(devpath).join(event),
            chr.join(format!("{INPUT_MAJOR}:{minor}")),
        )
        .unwrap();
    }

    fn record(&self, content: &str) {
        let path = self.record_dir.join(builtin::RECORD_NAME);
        fs::write(&path, content).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    }

    fn identity_files(&self, devpath: &str) -> Vec<String> {
        let dev = self.root.join(devpath);
        [
            "name",
            "id/bustype",
            "id/vendor",
            "id/product",
            "id/version",
            "capabilities/key",
            "capabilities/abs",
        ]
        .iter()
        .map(|f| fs::read_to_string(dev.join(f)).unwrap())
        .collect()
    }
}

fn current_uid() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

fn chr(minor: u32) -> (u32, u64) {
    (libc::S_IFCHR | 0o600, libc::makedev(INPUT_MAJOR, minor))
}

fn identify(sys: &Sys, minor: u32) -> Verdict {
    let (mode, rdev) = chr(minor);
    sys.trust()
        .identify_rdev(mode, rdev)
        .unwrap_or_else(|e| panic!("13:{minor}: identification errored: {e}"))
}

const REAL: &str = "devices/virtual/input/input5";
const CLONE: &str = "devices/virtual/input/input9";
const USB: &str = "devices/platform/soc/5200000.usb/usb1/1-1/1-1:1.0/input/input11";

/// The scenario: the decoder's device, a uinput clone and a USB lookalike are all present.
fn scenario(clone_phys: &str) -> Sys {
    let sys = Sys::new();
    sys.device(REAL, "event3", 67, builtin::PHYS);
    sys.device(CLONE, "event4", 68, clone_phys);
    sys.device(USB, "event5", 69, builtin::PHYS);
    sys.record("input5\n");
    sys
}

#[test]
fn clones_are_byte_identical_by_the_old_name_and_id_rule() {
    // Negative control for the old rule: name, bus/vid/pid/version and capabilities agree, so
    // anything keyed on them (udev ATTRS{name}, the broker's ExpectedIdentity) accepts the clone.
    let sys = scenario("");
    assert_eq!(sys.identity_files(REAL), sys.identity_files(CLONE));
    assert_eq!(sys.identity_files(REAL), sys.identity_files(USB));
}

#[test]
fn real_decoder_device_is_built_in_and_clones_are_not() {
    let sys = scenario("");
    assert_eq!(
        identify(&sys, 67),
        Verdict::BuiltIn {
            sysname: "input5".into()
        },
        "positive control: the decoder's own device"
    );
    assert_eq!(
        identify(&sys, 68),
        Verdict::NotBuiltIn(Reason::PhysMismatch)
    );
    assert!(matches!(
        identify(&sys, 69),
        Verdict::NotBuiltIn(Reason::NotVirtual { .. })
    ));
}

#[test]
fn uinput_clone_that_also_copies_phys_is_not_built_in() {
    // phys is public in sysfs, so a uinput holder could copy it. The record pins one instance.
    let sys = scenario(builtin::PHYS);
    assert_eq!(
        identify(&sys, 67),
        Verdict::BuiltIn {
            sysname: "input5".into()
        }
    );
    assert_eq!(
        identify(&sys, 68),
        Verdict::NotBuiltIn(Reason::SysnameMismatch {
            device: "input9".into(),
            record: "input5".into()
        })
    );
}

#[test]
fn without_a_record_nothing_is_built_in() {
    let sys = Sys::new();
    sys.device(REAL, "event3", 67, builtin::PHYS);
    assert_eq!(
        identify(&sys, 67),
        Verdict::NotBuiltIn(Reason::RecordAbsent)
    );
    sys.record("input5\n");
    assert!(matches!(identify(&sys, 67), Verdict::BuiltIn { .. }));
}

#[test]
fn non_input_and_non_char_devices_are_not_built_in() {
    let sys = scenario("");
    let trust = sys.trust();
    // Same minor as the real device but another major (e.g. a tty 4:67).
    let v = trust
        .identify_rdev(libc::S_IFCHR | 0o600, libc::makedev(4, 67))
        .unwrap();
    assert_eq!(v, Verdict::NotBuiltIn(Reason::NotInputCharDevice));
    // A block device that happens to carry 13:67.
    let v = trust
        .identify_rdev(libc::S_IFBLK | 0o600, libc::makedev(INPUT_MAJOR, 67))
        .unwrap();
    assert_eq!(v, Verdict::NotBuiltIn(Reason::NotInputCharDevice));
    // Positive control in the same run.
    assert!(matches!(identify(&sys, 67), Verdict::BuiltIn { .. }));
}

#[test]
fn a_record_others_could_write_is_not_trusted() {
    let sys = scenario("");
    let record = sys.record_dir.join(builtin::RECORD_NAME);
    assert!(matches!(identify(&sys, 67), Verdict::BuiltIn { .. }));

    fs::set_permissions(&record, fs::Permissions::from_mode(0o664)).unwrap();
    assert!(matches!(
        identify(&sys, 67),
        Verdict::NotBuiltIn(Reason::RecordUntrusted(_))
    ));
    fs::set_permissions(&record, fs::Permissions::from_mode(0o646)).unwrap();
    assert!(matches!(
        identify(&sys, 67),
        Verdict::NotBuiltIn(Reason::RecordUntrusted(_))
    ));
    fs::set_permissions(&record, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(identify(&sys, 67), Verdict::BuiltIn { .. }));

    // A writable directory lets anyone swap the record.
    fs::set_permissions(&sys.record_dir, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(matches!(
        identify(&sys, 67),
        Verdict::NotBuiltIn(Reason::RecordUntrusted(_))
    ));
    fs::set_permissions(&sys.record_dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(identify(&sys, 67), Verdict::BuiltIn { .. }));
}

#[test]
fn a_record_owned_by_anyone_but_the_trusted_owner_is_not_trusted() {
    let sys = scenario("");
    // Production pins owner uid 0; this run owns the files as a normal user, so expecting another
    // uid is the same check failing.
    let other = Trust::with_roots(&sys.root, &sys.record_dir, current_uid().wrapping_add(1));
    let (mode, rdev) = chr(67);
    assert!(matches!(
        other.identify_rdev(mode, rdev).unwrap(),
        Verdict::NotBuiltIn(Reason::RecordUntrusted(_))
    ));
    assert!(matches!(identify(&sys, 67), Verdict::BuiltIn { .. }));
}

#[test]
fn a_symlinked_or_malformed_record_is_not_trusted() {
    let sys = scenario("");
    let record = sys.record_dir.join(builtin::RECORD_NAME);
    let elsewhere = sys.record_dir.parent().unwrap().join("forged");
    fs::write(&elsewhere, "input5\n").unwrap();
    fs::set_permissions(&elsewhere, fs::Permissions::from_mode(0o644)).unwrap();
    fs::remove_file(&record).unwrap();
    symlink(&elsewhere, &record).unwrap();
    assert!(matches!(
        identify(&sys, 67),
        Verdict::NotBuiltIn(Reason::RecordUntrusted(_))
    ));
    fs::remove_file(&record).unwrap();

    for bad in [
        "",
        "input5",
        "input5\ninput9\n",
        "event3\n",
        "input\n",
        "input5 \n",
        "../input5\n",
    ] {
        sys.record(bad);
        assert!(
            matches!(
                identify(&sys, 67),
                Verdict::NotBuiltIn(Reason::RecordUntrusted(_))
            ),
            "record {bad:?} must not be trusted"
        );
    }
    sys.record("input5\n");
    assert!(matches!(identify(&sys, 67), Verdict::BuiltIn { .. }));
}

#[test]
fn a_dangling_char_link_is_an_error_not_a_negative() {
    // The device vanished between open and identify: an I/O failure, which callers fail closed on.
    let sys = scenario("");
    let (mode, rdev) = chr(99);
    let err = sys.trust().identify_rdev(mode, rdev).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    assert!(matches!(identify(&sys, 67), Verdict::BuiltIn { .. }));
}

#[test]
fn the_decoder_record_round_trips_through_the_helper() {
    let sys = Sys::new();
    sys.device(REAL, "event3", 67, builtin::PHYS);
    builtin::write_record(&sys.record_dir, "input5").unwrap();
    let meta = fs::metadata(sys.record_dir.join(builtin::RECORD_NAME)).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o644);
    assert!(matches!(identify(&sys, 67), Verdict::BuiltIn { .. }));
    // A restarted decoder gets a new inputN; the record follows it and the old one stops matching.
    sys.device("devices/virtual/input/input12", "event6", 70, builtin::PHYS);
    builtin::write_record(&sys.record_dir, "input12").unwrap();
    assert!(matches!(identify(&sys, 70), Verdict::BuiltIn { .. }));
    assert!(matches!(
        identify(&sys, 67),
        Verdict::NotBuiltIn(Reason::SysnameMismatch { .. })
    ));
    assert!(builtin::write_record(&sys.record_dir, "../input5").is_err());
}

#[test]
fn decoder_start_clears_a_stale_record_before_creating_a_device() {
    let sys = scenario(builtin::PHYS);
    assert!(matches!(identify(&sys, 67), Verdict::BuiltIn { .. }));

    builtin::clear_record(&sys.record_dir).unwrap();
    assert_eq!(
        identify(&sys, 67),
        Verdict::NotBuiltIn(Reason::RecordAbsent),
        "the prior device must stop being trusted before a replacement can reuse its input number"
    );
    builtin::clear_record(&sys.record_dir).unwrap();
}
