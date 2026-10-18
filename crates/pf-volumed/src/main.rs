use pf_prefsd::Client;
use pf_volumed::{load_preferences, AlsaMixer, MixerBackend};
use std::path::PathBuf;
use std::time::Duration;

struct Args {
    prefs_socket: PathBuf,
    card: String,
    poll: Duration,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args(std::env::args().skip(1))?;
    let client = Client::new(args.prefs_socket);
    let mut mixer = AlsaMixer::new(args.card);
    // This board has no headphone-detect event, so neither endpoint can be established safely.
    // Quiesce the shared DAC before any preference I/O and leave it there until an event-driven
    // route owner can perform VolumeService's atomic transition contract.
    mixer.quiesce()?;
    load_preferences(&client)?;

    loop {
        std::thread::sleep(args.poll);
        match load_preferences(&client) {
            Ok(_) => {}
            Err(error) => eprintln!("pf-volumed: {error}"),
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
