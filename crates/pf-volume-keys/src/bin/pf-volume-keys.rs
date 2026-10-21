//! `pf-volume-keys` — always-on hardware volume-key handler (tsp-ihne1.63).
//!
//! Usage: pf-volume-keys [--prefs-sock PATH] [--toast-sock PATH]   (prefs: flag, else $PF_PREFSD_SOCK)
//!
//! The input broker only runs inside an app session, so this is its own service: it works
//! while the shell, the first-run sheet or a menu is up, and while an app has focus.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_sig: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

fn main() -> Result<(), String> {
    let mut prefs: Option<PathBuf> = None;
    let mut toast: Option<PathBuf> = Some(PathBuf::from("/run/pocketforge/volume-osd.sock"));
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--prefs-sock" => prefs = Some(value.into()),
            "--toast-sock" => toast = Some(value.into()),
            _ => return Err(format!("unknown argument: {flag} (usage: pf-volume-keys [--prefs-sock PATH] [--toast-sock PATH])")),
        }
    }
    // SAFETY: the handler only stores to an atomic.
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
    let prefs = pf_volume_keys::host::resolve_prefs_sock(prefs, std::env::var_os("PF_PREFSD_SOCK"));
    pf_volume_keys::host::serve(&prefs, toast.as_deref(), &STOP);
    Ok(())
}
