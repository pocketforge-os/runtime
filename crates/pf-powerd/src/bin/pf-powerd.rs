use pf_powerd::{authorize_connection, SystemctlExecutor};
use std::fs;
use std::io;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const MAX_CLIENTS: usize = 4;

struct ClientSlot(Arc<AtomicUsize>);

impl Drop for ClientSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Release);
    }
}

fn try_client_slot(active: &Arc<AtomicUsize>) -> Option<ClientSlot> {
    active
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < MAX_CLIENTS).then_some(n + 1)
        })
        .ok()
        .map(|_| ClientSlot(Arc::clone(active)))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let socket = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/run/pocketforge-power/power.sock"));
    prepare_socket(&socket)?;
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o660))?;
    let allowed_uid = gamer_uid()?;
    let incarnation = fs::read_to_string("/proc/sys/kernel/random/uuid")?
        .trim()
        .to_owned();
    let menu = Arc::new(Mutex::new(pf_powerd::key::Menu::new(incarnation)));
    let _key_reader = pf_powerd::key::spawn(Arc::clone(&menu))?;
    let active = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let connection = match authorize_connection(stream, allowed_uid) {
                    Ok(connection) => connection,
                    Err(error) => {
                        eprintln!("pf-powerd: refused peer: {error}");
                        continue;
                    }
                };
                let Some(slot) = try_client_slot(&active) else {
                    eprintln!("pf-powerd: refused peer: client limit reached");
                    continue;
                };
                let menu = Arc::clone(&menu);
                if let Err(error) = std::thread::Builder::new().spawn(move || {
                    let _slot = slot;
                    if let Err(error) = connection.serve_with_menu(&SystemctlExecutor, &menu) {
                        eprintln!("pf-powerd: refused request: {error}");
                    }
                }) {
                    eprintln!("pf-powerd: client worker unavailable: {error}");
                }
            }
            Err(error) => eprintln!("pf-powerd: accept failed: {error}"),
        }
    }
    Ok(())
}

fn gamer_uid() -> io::Result<u32> {
    let passwd = fs::read_to_string("/etc/passwd")?;
    passwd
        .lines()
        .find_map(|line| {
            let mut fields = line.split(':');
            (fields.next()? == "gamer")
                .then(|| fields.nth(1)?.parse().ok())
                .flatten()
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "gamer user is absent"))
}

fn prepare_socket(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            if UnixStream::connect(path).is_ok() {
                Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "power daemon already listening",
                ))
            } else {
                fs::remove_file(path)
            }
        }
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "socket path is not a socket",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_slots_are_bounded_and_reusable() {
        let active = Arc::new(AtomicUsize::new(0));
        let mut slots: Vec<_> = (0..MAX_CLIENTS)
            .map(|_| try_client_slot(&active).unwrap())
            .collect();
        assert!(try_client_slot(&active).is_none());
        slots.pop();
        assert!(try_client_slot(&active).is_some());
    }
}
