use pf_volumed::{AlsaMixer, MixerBackend, Output};
use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("pf-volumed-{}-{nonce}", std::process::id()));
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
