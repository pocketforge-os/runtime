use pf_prefsd::Client;
use pf_volumed::jack::{discover, EvdevJack};
use pf_volumed::{load_preferences, AlsaMixer, MixerBackend, SyncError, VolumeService};
use std::path::{Path, PathBuf};
use std::time::Duration;

const SYSFS_INPUT: &str = "/sys/class/input";
const DEV_INPUT: &str = "/dev/input";

struct Args {
    prefs_socket: PathBuf,
    card: String,
    poll: Duration,
}

fn open_jack() -> Result<Option<EvdevJack>, pf_volumed::jack::RouteError> {
    match discover(Path::new(SYSFS_INPUT), Path::new(DEV_INPUT))? {
        Some(path) => EvdevJack::open(&path).map(Some),
        None => Ok(None),
    }
}

fn report(last: &mut String, message: String) {
    if *last != message {
        eprintln!("pf-volumed: {message}");
        *last = message;
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args(std::env::args().skip(1))?;
    let client = Client::new(args.prefs_socket);
    let mut mixer = AlsaMixer::new(args.card);
    // Nothing is exposed until the route is known: hold the shared DAC at its minimum, then let
    // the headphone-jack switch (EVIOCGSW at start, events after) drive VolumeService's atomic
    // transition. If the switch device is absent or unreadable the DAC simply stays quiesced.
    mixer.quiesce()?;
    let mut service = VolumeService::unrouted(mixer, load_preferences(&client)?);
    let mut jack: Option<EvdevJack> = None;
    let mut last = String::new();

    loop {
        if jack.is_none() {
            match open_jack() {
                Ok(found) => {
                    if found.is_none() {
                        report(
                            &mut last,
                            String::from("headphone-jack switch absent; volume held at minimum"),
                        );
                    }
                    jack = found;
                }
                Err(error) => report(
                    &mut last,
                    format!("headphone-jack switch unusable: {error}"),
                ),
            }
        }
        let Some(source) = jack.as_mut() else {
            std::thread::sleep(args.poll);
            if let Err(error) = load_preferences(&client) {
                report(&mut last, error.to_string());
            }
            continue;
        };
        match load_preferences(&client) {
            Ok(preferences) => match service.sync_route(source, preferences) {
                Ok(()) => last.clear(),
                Err(SyncError::Route(error)) => {
                    report(
                        &mut last,
                        format!("route unknown, volume held at minimum: {error}"),
                    );
                    jack = None;
                    continue;
                }
                Err(SyncError::Mixer(error)) => {
                    report(&mut last, error.to_string());
                    if !service.route_known() {
                        // A failed transition may have left the previous level exposed: confirm
                        // the minimum or stop (systemd restarts the unit) rather than continue.
                        service.route_lost()?;
                    }
                }
            },
            Err(error) => report(&mut last, error.to_string()),
        }
        if let Err(error) = source.wait(args.poll) {
            report(
                &mut last,
                format!("route unknown, volume held at minimum: {error}"),
            );
            jack = None;
            service.route_lost()?;
        }
    }
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut prefs_socket = PathBuf::from("/run/pocketforge/prefsd.sock");
    let mut card = String::from("Codec");
    let mut poll_ms = 250_u64;
    while let Some(flag) = args.next() {
        if flag == "--help" || flag == "-h" {
            return Err(String::from(
                "usage: pf-volumed [--prefs-socket PATH] [--card ID] [--poll-ms N]",
            ));
        }
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--prefs-socket" => prefs_socket = value.into(),
            "--card" => card = value,
            "--poll-ms" => {
                poll_ms = value
                    .parse()
                    .map_err(|_| format!("invalid --poll-ms value: {value}"))?;
                if poll_ms == 0 {
                    return Err(String::from("--poll-ms must be greater than zero"));
                }
            }
            _ => return Err(format!("unknown argument: {flag}")),
        }
    }
    Ok(Args {
        prefs_socket,
        card,
        poll: Duration::from_millis(poll_ms),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_invalid_poll_are_distinct() {
        let args = parse_args(std::iter::empty()).unwrap();
        assert_eq!(args.card, "Codec");
        assert_eq!(args.poll, Duration::from_millis(250));
        assert!(parse_args(["--poll-ms".into(), "0".into()].into_iter()).is_err());
    }
}
