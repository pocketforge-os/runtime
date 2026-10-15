use crate::Frame;
use serde_json::{json, Value};
use std::io;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HangHow {
    NoReplyMenu,
    StopTimeout,
}

impl HangHow {
    const fn as_str(self) -> &'static str {
        match self {
            Self::NoReplyMenu => "no-reply-menu",
            Self::StopTimeout => "stop-timeout",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReportMetadata {
    pub day: String,
    pub build: String,
    pub model: String,
    pub app_id: String,
    pub app_version: String,
    pub app_origin: String,
    pub uptime: String,
    pub session: String,
    pub memory_pressure: bool,
    pub deletion_token: [u8; 32],
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Produce one canonical problem-report-v1 payload. Empty attempts are omitted from `samples`;
/// recovery is handled before this function and therefore produces no payload at all.
pub fn encode_hang_report(
    metadata: &ReportMetadata,
    how: HangHow,
    attempts: &[Vec<Frame>],
) -> io::Result<Vec<u8>> {
    let successful = attempts
        .iter()
        .filter(|frames| !frames.is_empty())
        .take(2)
        .collect::<Vec<_>>();
    let mut frames = Vec::new();
    let mut samples = Vec::new();
    for sample in successful {
        let remaining = 48_usize.saturating_sub(frames.len());
        let count = sample.len().min(remaining);
        if count == 0 {
            break;
        }
        samples.push(count);
        frames.extend(sample.iter().take(count).map(|frame| {
            match (&frame.build_id, frame.offset) {
                (Some(build_id), Some(offset)) => json!([hex(build_id), offset]),
                _ => json!([null, null]),
            }
        }));
    }
    let report: Value = json!({
        "v": 1,
        "kind": "hang",
        "how": how.as_str(),
        "day": metadata.day,
        "build": metadata.build,
        "model": metadata.model,
        "app": {
            "id": metadata.app_id,
            "ver": metadata.app_version,
            "origin": metadata.app_origin,
        },
        "uptime": metadata.uptime,
        "session": metadata.session,
        "mem": {"pressure": metadata.memory_pressure, "oom": false},
        "frames": frames,
        "samples": samples,
        "del": hex(&metadata.deletion_token),
    });
    pf_problem_report::encode(&report).map_err(io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata() -> ReportMetadata {
        ReportMetadata {
            day: "2026-10-15".to_owned(),
            build: "123456abcdef".to_owned(),
            model: "tsp-a133".to_owned(),
            app_id: "org.example.hung".to_owned(),
            app_version: "1.0".to_owned(),
            app_origin: "catalog".to_owned(),
            uptime: "10-60m".to_owned(),
            session: "1-10m".to_owned(),
            memory_pressure: false,
            deletion_token: [7; 32],
        }
    }

    #[test]
    fn report_round_trip_caps_samples_and_omits_empty_attempts() {
        let known = Frame {
            build_id: Some(vec![1; 20]),
            offset: Some(42),
        };
        let encoded = encode_hang_report(
            &metadata(),
            HangHow::NoReplyMenu,
            &[Vec::new(), vec![known.clone()], vec![known]],
        )
        .unwrap();
        let decoded = pf_problem_report::decode(&encoded).unwrap();
        assert_eq!(decoded["samples"], json!([1, 1]));
        assert_eq!(decoded["frames"].as_array().unwrap().len(), 2);
    }
}
