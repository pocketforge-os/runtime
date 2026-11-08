//! Self-applied Landlock (design item 1, the ENFORCED floor), through pf-app-launch's
//! syscall adapter (`SystemLandlock` + `EffectiveRuleset`).
//!
//! Applied after the descriptor and CA bundle are read and before any socket:
//! - filesystem: read/write, never execute, beneath the app state directory only; read-only
//!   on the resolver files musl reads per lookup (`/etc/resolv.conf`, `/etc/hosts`,
//!   `/run/systemd/resolve`). Everything else, including every execute, is denied.
//! - network (ABI >= 4): TCP connect only to the scope's ports plus 53; no TCP bind. With
//!   ABI >= 10, UDP send only to 53 plus the ephemeral bind a resolver needs.
//! - scopes (ABI >= 6): no abstract-unix connects, no signals outside the domain.
//!
//! The release binary requires ABI >= 4 (TCP port rules) and fails closed otherwise; the
//! device kernel is ABI 10.

use pf_app_launch::landlock::{
    EffectiveFsRule, EffectiveNetRule, EffectiveRuleset, LandlockKernel, SystemLandlock,
};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const FS_EXECUTE: u64 = 1 << 0;
const FS_WRITE_FILE: u64 = 1 << 1;
const FS_READ_FILE: u64 = 1 << 2;
const FS_READ_DIR: u64 = 1 << 3;
const FS_REMOVE_DIR: u64 = 1 << 4;
const FS_REMOVE_FILE: u64 = 1 << 5;
const FS_MAKE_DIR: u64 = 1 << 7;
const FS_MAKE_REG: u64 = 1 << 8;
const FS_REFER: u64 = 1 << 13;
const FS_TRUNCATE: u64 = 1 << 14;
const FS_ABI1: u64 = (1 << 13) - 1;

const NET_BIND_TCP: u64 = 1 << 0;
const NET_CONNECT_TCP: u64 = 1 << 1;
const NET_BIND_UDP: u64 = 1 << 2;
const NET_CONNECT_SEND_UDP: u64 = 1 << 3;

const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
const SCOPE_SIGNAL: u64 = 1 << 1;

pub const REQUIRED_ABI: u32 = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    Off,
    BestEffort,
    Required,
}

fn handled_fs(abi: u32) -> u64 {
    let mut mask = FS_ABI1;
    if abi >= 2 {
        mask |= FS_REFER;
    }
    if abi >= 3 {
        mask |= FS_TRUNCATE;
    }
    if abi >= 5 {
        mask |= 1 << 15; // IOCTL_DEV
    }
    if abi >= 9 {
        mask |= 1 << 16; // RESOLVE_UNIX
    }
    mask
}

pub fn ruleset(abi: u32, state_dir: &Path, tcp_ports: &BTreeSet<u16>) -> EffectiveRuleset {
    let handled_access_fs = handled_fs(abi);
    let state_access = (FS_WRITE_FILE
        | FS_READ_FILE
        | FS_READ_DIR
        | FS_REMOVE_DIR
        | FS_REMOVE_FILE
        | FS_MAKE_DIR
        | FS_MAKE_REG
        | FS_REFER
        | FS_TRUNCATE)
        & handled_access_fs
        & !FS_EXECUTE;
    let read = (FS_READ_FILE | FS_READ_DIR) & handled_access_fs;
    let mut filesystem = vec![EffectiveFsRule {
        path: state_dir.to_path_buf(),
        allowed_access: state_access,
    }];
    for path in [
        "/etc/resolv.conf",
        "/etc/hosts",
        "/etc/nsswitch.conf",
        "/etc/gai.conf",
        "/run/systemd/resolve",
    ] {
        filesystem.push(EffectiveFsRule {
            path: PathBuf::from(path),
            allowed_access: read,
        });
    }
    let mut handled_access_net = 0;
    let mut network = Vec::new();
    if abi >= 4 {
        handled_access_net |= NET_BIND_TCP | NET_CONNECT_TCP;
        let mut ports = tcp_ports.clone();
        ports.insert(53);
        for port in ports {
            network.push(EffectiveNetRule {
                port,
                allowed_access: NET_CONNECT_TCP,
            });
        }
    }
    if abi >= 10 {
        handled_access_net |= NET_BIND_UDP | NET_CONNECT_SEND_UDP;
        network.push(EffectiveNetRule {
            port: 53,
            allowed_access: NET_CONNECT_SEND_UDP,
        });
        network.push(EffectiveNetRule {
            port: 0,
            allowed_access: NET_BIND_UDP,
        });
    }
    let scoped = if abi >= 6 {
        SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL
    } else {
        0
    };
    EffectiveRuleset {
        abi,
        handled_access_fs,
        handled_access_net,
        scoped,
        filesystem,
        network,
    }
}

/// Apply the ruleset to this process. `Ok(Some(abi))` when enforced, `Ok(None)` when skipped
/// (test modes only).
pub fn apply(
    mode: Mode,
    state_dir: &Path,
    tcp_ports: &BTreeSet<u16>,
) -> Result<Option<u32>, String> {
    if mode == Mode::Off {
        return Ok(None);
    }
    let mut kernel = SystemLandlock;
    let abi = match kernel.abi() {
        Ok(abi) if abi >= REQUIRED_ABI => abi,
        Ok(abi) if mode == Mode::BestEffort => {
            let _ = abi;
            return Ok(None);
        }
        Err(_) if mode == Mode::BestEffort => return Ok(None),
        Ok(abi) => return Err(format!("Landlock ABI {abi} < required {REQUIRED_ABI}")),
        Err(error) => return Err(format!("Landlock unavailable: {error}")),
    };
    kernel
        .enforce(&ruleset(abi, state_dir, tcp_ports))
        .map_err(|e| format!("apply Landlock: {e}"))?;
    Ok(Some(abi))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_dir_is_never_executable_and_ports_are_scoped() {
        let rules = ruleset(10, Path::new("/s"), &BTreeSet::from([443]));
        assert_eq!(rules.filesystem[0].allowed_access & FS_EXECUTE, 0);
        assert_ne!(rules.handled_access_fs & FS_EXECUTE, 0);
        let tcp: Vec<u16> = rules
            .network
            .iter()
            .filter(|r| r.allowed_access == NET_CONNECT_TCP)
            .map(|r| r.port)
            .collect();
        assert_eq!(tcp, vec![53, 443]);
        assert_ne!(rules.handled_access_net & NET_BIND_TCP, 0);
        let old = ruleset(4, Path::new("/s"), &BTreeSet::from([443]));
        assert_eq!(old.handled_access_net & NET_CONNECT_SEND_UDP, 0);
        assert_eq!(old.scoped, 0);
    }
}
