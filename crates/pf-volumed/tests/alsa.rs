use pf_volumed::{AlsaJackSource, AlsaMixer, MixerBackend, Output};
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

fn write_fake(path: &Path, log: &Path, state: &Path, fail: &Path) {
    let script = format!(
        "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$*\" >> '{}'\nif [ -e '{}' ]; then echo injected-failure >&2; exit 9; fi\ncase \"$*\" in\n  *cget*) printf '  : values=%s\\n' \"$(cat '{}')\" ;;\nesac\n",
        log.display(),
        fail.display(),
        state.display()
    );
    fs::write(path, script).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

#[test]
fn alsa_backend_maps_levels_reads_jack_and_surfaces_command_failure() {
    let scratch = Scratch::new();
    let fake = scratch.0.join("amixer");
    let log = scratch.0.join("calls");
    let state = scratch.0.join("jack");
    let fail = scratch.0.join("fail");
    fs::write(&state, "off\n").unwrap();
    write_fake(&fake, &log, &state, &fail);

    let mut mixer = AlsaMixer::with_amixer(&fake, "Codec");
    mixer.apply(Output::Speaker, 60, false).unwrap();
    let jack = AlsaJackSource::with_amixer(&fake, "Codec");
    assert!(!jack.headphones_plugged().unwrap());
    fs::write(&state, "on\n").unwrap();
    assert!(jack.headphones_plugged().unwrap());

    let calls = fs::read_to_string(&log).unwrap();
    assert!(calls.contains("cset name=DAC Playback Volume 38"));
    assert!(calls.contains("cset name=DAC Playback Switch on"));
    assert!(calls.contains("cget name=Headphone Jack"));

    File::create(&fail).unwrap();
    let error = mixer.apply(Output::Headphones, 40, true).unwrap_err();
    assert!(error.to_string().contains("injected-failure"));
}
