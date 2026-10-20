use pf_volumed::{AlsaMixer, MixerBackend, Output};
use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

// Writing a fake script while a sibling test forks can hold the file open across that fork's exec
// (ETXTBSY); these tests spawn fake amixers, so they run one at a time.
static FAKE_AMIXER: std::sync::Mutex<()> = std::sync::Mutex::new(());

static SERIAL: AtomicUsize = AtomicUsize::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "pf-volumed-{}-{nonce}-{serial}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Ok(entries) = fs::read_dir(&self.0) {
            for entry in entries.flatten() {
                let _ = fs::remove_file(entry.path());
            }
        }
        let _ = fs::remove_dir(&self.0);
    }
}

fn write_fake(path: &Path, log: &Path, fail: &Path) {
    let script = format!(
        "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$*\" >> '{}'\nif [ -e '{}' ]; then echo injected-failure >&2; exit 9; fi\n",
        log.display(),
        fail.display(),
    );
    fs::write(path, script).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

#[test]
fn alsa_backend_maps_levels_rejects_unsafe_routes_and_surfaces_command_failure() {
    let _serial = FAKE_AMIXER
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let scratch = Scratch::new();
    let fake = scratch.0.join("amixer");
    let log = scratch.0.join("calls");
    let fail = scratch.0.join("fail");
    write_fake(&fake, &log, &fail);

    let mut mixer = AlsaMixer::with_amixer(&fake, "Codec");
    mixer.quiesce().unwrap();
    mixer.apply(Output::Speaker, 60, false).unwrap();
    mixer.apply(Output::Headphones, 40, true).unwrap();
    mixer.transition(Output::Headphones, 40, false).unwrap();

    let calls = fs::read_to_string(&log).unwrap();
    assert!(calls.contains("cset name=DAC Playback Volume 38"));
    assert!(calls.contains("cset name=DAC Playback Volume 0"));
    assert!(!calls.contains("DAC Playback Switch"));
    assert_eq!(
        calls.lines().next(),
        Some("-q -c Codec cset name=DAC Playback Volume 0")
    );
    // A route transition is minimum first, then the destination's stored level (40% -> 25).
    let lines: Vec<_> = calls.lines().collect();
    assert_eq!(
        &lines[lines.len() - 2..],
        [
            "-q -c Codec cset name=DAC Playback Volume 0",
            "-q -c Codec cset name=DAC Playback Volume 25"
        ]
    );
    assert_eq!(lines.len(), 5);

    File::create(&fail).unwrap();
    let error = mixer.apply(Output::Headphones, 40, true).unwrap_err();
    assert!(error.to_string().contains("injected-failure"));
}

fn write_flaky(path: &Path, log: &Path, counter: &Path, fail_first: usize) {
    let script = format!(
        "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$*\" >> '{log}'\nn=$(cat '{c}' 2>/dev/null || echo 0)\nn=$((n+1))\necho $n > '{c}'\nif [ $n -le {k} ]; then echo flaky >&2; exit 9; fi\n",
        log = log.display(),
        c = counter.display(),
        k = fail_first,
    );
    fs::write(path, script).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

#[test]
fn a_transient_quiesce_failure_is_retried_before_the_destination_level_is_applied() {
    let _serial = FAKE_AMIXER
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let scratch = Scratch::new();
    let (fake, log, counter) = (
        scratch.0.join("amixer"),
        scratch.0.join("calls"),
        scratch.0.join("n"),
    );
    write_flaky(&fake, &log, &counter, 2);
    let mut mixer = AlsaMixer::with_amixer(&fake, "Codec");
    let result = mixer.transition(Output::Headphones, 40, false);
    let calls = fs::read_to_string(&log).unwrap();
    result.unwrap_or_else(|e| panic!("{e} calls={calls:?} n={:?}", fs::read_to_string(&counter)));
    let lines: Vec<_> = calls.lines().collect();
    assert_eq!(lines.len(), 4, "{calls}");
    assert!(lines[..3].iter().all(|line| line.ends_with("Volume 0")));
    assert!(lines[3].ends_with("Volume 25"));
}

#[test]
fn a_quiesce_that_never_succeeds_fails_the_transition_without_applying_any_level() {
    let _serial = FAKE_AMIXER
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let scratch = Scratch::new();
    let (fake, log, counter) = (
        scratch.0.join("amixer"),
        scratch.0.join("calls"),
        scratch.0.join("n"),
    );
    write_flaky(&fake, &log, &counter, 1_000_000);
    let mut mixer = AlsaMixer::with_amixer(&fake, "Codec");
    assert!(mixer.transition(Output::Headphones, 40, false).is_err());
    let calls = fs::read_to_string(&log).unwrap();
    assert!(
        calls.lines().all(|line| line.ends_with("Volume 0")),
        "{calls}"
    );
}
