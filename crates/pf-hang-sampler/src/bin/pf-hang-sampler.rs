use pf_hang_sampler::server::Sampler;
use pf_hang_sampler::transport;
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::os::fd::AsFd;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};

fn decode_hex(text: &str) -> Result<Vec<u8>, String> {
    if !(16..=64).contains(&text.len())
        || text.len() % 2 != 0
        || !text.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("build ids must be 8..32 bytes of hexadecimal".to_owned());
    }
    text.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            std::str::from_utf8(pair)
                .ok()
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                .ok_or_else(|| "invalid build id".to_owned())
        })
        .collect()
}

fn prepare_socket(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            if transport::connect(path).is_ok() {
                Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "another sampler is already listening",
                ))
            } else {
                fs::remove_file(path)
            }
        }
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "sampler socket path exists and is not a socket",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn parse_args() -> Result<(PathBuf, BTreeSet<Vec<u8>>), String> {
    let mut args = std::env::args().skip(1);
    let mut socket = None;
    let mut build_ids = BTreeSet::new();
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--socket" => socket = Some(PathBuf::from(value)),
            "--allowed-build-id" => {
                build_ids.insert(decode_hex(&value)?);
            }
            _ => return Err(format!("unknown argument: {flag}")),
        }
    }
    Ok((socket.ok_or("--socket is required")?, build_ids))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (socket, build_ids) = parse_args()?;
    prepare_socket(&socket)?;
    let listener = transport::bind(&socket)?;
    let mut sampler = Sampler::new(build_ids);
    loop {
        let stream = transport::accept(listener.as_fd())?;
        let (outcome, _completed) = sampler.handle_stream(&stream);
        // The completed safe frame set stays in process until pf-crashd consumes this crate's
        // canonical report API. Never print request data, paths, registers, stack words, or frames.
        eprintln!("{}", outcome.line());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_id_allowlist_is_exact_hex() {
        assert_eq!(decode_hex("0011223344556677").unwrap().len(), 8);
        assert!(decode_hex("0011").is_err());
        assert!(decode_hex("00112233445566zz").is_err());
    }
}
