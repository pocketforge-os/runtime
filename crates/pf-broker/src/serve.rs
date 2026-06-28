//! Serving the enforcing backend over the `.2` PFW1 Unix socket, with an `SO_PEERCRED`
//! peer-credential check at accept time. In the real (substrate) deployment the supervisor
//! launches each app into its own namespace with a per-app socket bind-mounted in, and
//! `SO_PEERCRED` confirms the connecting peer is the app's uid (defense against a different
//! local user connecting to a socket it can see). v0 is non-namespaced: the check is the same
//! mechanism, validated here; the namespace bind-mount is the substrate-gated leg.

use std::io;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use pocketforge::server::serve_connection;
use pocketforge::Backend;

/// The peer's kernel-attested credentials (`SO_PEERCRED`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerCred {
    pub pid: i32,
    pub uid: u32,
    pub gid: u32,
}

/// Read the connecting peer's `SO_PEERCRED` (kernel-attested pid/uid/gid — unforgeable by the peer).
pub fn peer_cred(stream: &UnixStream) -> io::Result<PeerCred> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt writes a ucred of `len` bytes into `cred`; fd is a valid socket.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(PeerCred { pid: cred.pid, uid: cred.uid, gid: cred.gid })
}

/// Serve `backend` on `listener` forever (one thread per connection), refusing any peer whose
/// uid does not match `allowed_uid` (when `Some`). Blocks the calling thread.
pub fn serve_enforcing(
    listener: UnixListener,
    backend: Arc<dyn Backend>,
    allowed_uid: Option<u32>,
) -> io::Result<()> {
    for stream in listener.incoming() {
        let stream = stream?;
        if !admit(&stream, allowed_uid) {
            continue;
        }
        let b = backend.clone();
        std::thread::spawn(move || {
            let _ = serve_connection(&*b, stream);
        });
    }
    Ok(())
}

/// As [`serve_enforcing`] but stops accepting once `stop` is set (poll-driven; used by tests and
/// for clean daemon shutdown).
pub fn serve_enforcing_until(
    listener: UnixListener,
    backend: Arc<dyn Backend>,
    allowed_uid: Option<u32>,
    stop: &AtomicBool,
) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                if !admit(&stream, allowed_uid) {
                    continue;
                }
                stream.set_nonblocking(false)?;
                let b = backend.clone();
                std::thread::spawn(move || {
                    let _ = serve_connection(&*b, stream);
                });
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Accept-time admission: enforce the `SO_PEERCRED` uid match (when configured). A refused peer is
/// logged (audit) and the connection dropped.
fn admit(stream: &UnixStream, allowed_uid: Option<u32>) -> bool {
    let cred = match peer_cred(stream) {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "pf-broker: SO_PEERCRED failed, refusing: {e}");
            return false;
        }
    };
    if let Some(uid) = allowed_uid {
        if cred.uid != uid {
            let _ = writeln!(
                std::io::stderr(),
                "pf-broker: REFUSED peer pid={} uid={} (expected uid={uid})",
                cred.pid, cred.uid
            );
            return false;
        }
    }
    true
}
