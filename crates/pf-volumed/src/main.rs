use pf_prefsd::Client;
use pf_volumed::{load_preferences, AlsaMixer, Output, VolumeService};
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
    let preferences = load_preferences(&client)?;
    // This board has no headphone-detect event. Keep the ALSA backend fixed on the supported
    // speaker route; a future event-driven route owner can use VolumeService::reconcile.
    let mut service = VolumeService::new(AlsaMixer::new(args.card), Output::Speaker, preferences);
    service.apply_current()?;

    loop {
        std::thread::sleep(args.poll);
        match load_preferences(&client) {
            Ok(preferences) => {
                if let Err(error) = service.reconcile(Output::Speaker, preferences) {
                    eprintln!("pf-volumed: apply preferences: {error}");
                }
            }
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
