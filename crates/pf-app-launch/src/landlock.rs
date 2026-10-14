//! Landlock ABI gating and the Linux syscall adapter.

use crate::policy::{DowngradeMode, FsAccess, NetAccess, PolicyDecision};
use linux_raw_sys::general::{
    __NR_landlock_add_rule, __NR_landlock_create_ruleset, __NR_landlock_restrict_self,
};
use linux_raw_sys::landlock::landlock_rule_type;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io;
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

const CREATE_RULESET_VERSION: u32 = 1;

const FS_EXECUTE: u64 = 1 << 0;
const FS_WRITE_FILE: u64 = 1 << 1;
const FS_READ_FILE: u64 = 1 << 2;
const FS_READ_DIR: u64 = 1 << 3;
const FS_REMOVE_DIR: u64 = 1 << 4;
const FS_REMOVE_FILE: u64 = 1 << 5;
const FS_MAKE_CHAR: u64 = 1 << 6;
const FS_MAKE_DIR: u64 = 1 << 7;
const FS_MAKE_REG: u64 = 1 << 8;
const FS_MAKE_SOCK: u64 = 1 << 9;
const FS_MAKE_FIFO: u64 = 1 << 10;
const FS_MAKE_BLOCK: u64 = 1 << 11;
const FS_MAKE_SYM: u64 = 1 << 12;
const FS_REFER: u64 = 1 << 13;
const FS_TRUNCATE: u64 = 1 << 14;
const FS_IOCTL_DEV: u64 = 1 << 15;
// The vendored generated bindings predate the device's ABI-9/10 UAPI. These three values are
// audited against kernel-sunxi-7.x@45b87a7d include/uapi/linux/landlock.h:418,467-468.
const FS_RESOLVE_UNIX: u64 = 1 << 16;

const NET_BIND_TCP: u64 = 1 << 0;
const NET_CONNECT_TCP: u64 = 1 << 1;
const NET_BIND_UDP: u64 = 1 << 2;
const NET_CONNECT_SEND_UDP: u64 = 1 << 3;

const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
const SCOPE_SIGNAL: u64 = 1 << 1;

const FS_ABI1: u64 = FS_EXECUTE
    | FS_WRITE_FILE
    | FS_READ_FILE
    | FS_READ_DIR
    | FS_REMOVE_DIR
    | FS_REMOVE_FILE
    | FS_MAKE_CHAR
    | FS_MAKE_DIR
    | FS_MAKE_REG
    | FS_MAKE_SOCK
    | FS_MAKE_FIFO
    | FS_MAKE_BLOCK
    | FS_MAKE_SYM;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
    quiet_access_fs: u64,
    quiet_access_net: u64,
    quiet_scoped: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

#[repr(C)]
struct NetPortAttr {
    allowed_access: u64,
    port: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectiveFsRule {
    pub path: PathBuf,
    pub allowed_access: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectiveNetRule {
    pub port: u16,
    pub allowed_access: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectiveRuleset {
    pub abi: u32,
    pub handled_access_fs: u64,
    pub handled_access_net: u64,
    pub scoped: u64,
    pub filesystem: Vec<EffectiveFsRule>,
    pub network: Vec<EffectiveNetRule>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppliedPolicy {
    pub kernel_abi: u32,
    pub enforced: bool,
    pub ruleset: Option<EffectiveRuleset>,
}

#[derive(Debug)]
pub enum LandlockError {
    Unavailable(io::Error),
    AbiTooOld { actual: u32, required: u32 },
    Apply(io::Error),
}

impl fmt::Display for LandlockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(error) => write!(f, "Landlock unavailable: {error}"),
            Self::AbiTooOld { actual, required } => {
                write!(f, "Landlock ABI {actual} is below required ABI {required}")
            }
            Self::Apply(error) => write!(f, "apply Landlock policy: {error}"),
        }
    }
}

impl std::error::Error for LandlockError {}

pub trait LandlockKernel {
    fn abi(&mut self) -> io::Result<u32>;
    fn enforce(&mut self, ruleset: &EffectiveRuleset) -> io::Result<()>;
}

#[derive(Default)]
pub struct SystemLandlock;

impl LandlockKernel for SystemLandlock {
    fn abi(&mut self) -> io::Result<u32> {
        // SAFETY: the version query requires a null attribute and a zero size.
        let result = unsafe {
            libc::syscall(
                __NR_landlock_create_ruleset as libc::c_long,
                std::ptr::null::<libc::c_void>(),
                0usize,
                CREATE_RULESET_VERSION,
            )
        };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            u32::try_from(result).map_err(|_| io::Error::other("invalid Landlock ABI"))
        }
    }

    fn enforce(&mut self, ruleset: &EffectiveRuleset) -> io::Result<()> {
        let attr = RulesetAttr {
            handled_access_fs: ruleset.handled_access_fs,
            handled_access_net: ruleset.handled_access_net,
            scoped: ruleset.scoped,
            quiet_access_fs: 0,
            quiet_access_net: 0,
            quiet_scoped: 0,
        };
        // SAFETY: attr points to a live C-compatible structure for the duration of the call.
        let fd = unsafe {
            libc::syscall(
                __NR_landlock_create_ruleset as libc::c_long,
                &attr as *const RulesetAttr,
                ruleset_attr_size(ruleset.abi),
                0u32,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a successful create_ruleset syscall returns an owned file descriptor.
        let ruleset_file = unsafe { File::from_raw_fd(fd as i32) };

        for rule in &ruleset.filesystem {
            let path = match OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
                .open(&rule.path)
            {
                Ok(path) => path,
                // An absent hierarchy cannot be accessed and remains denied if later created.
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            let path_attr = PathBeneathAttr {
                allowed_access: rule.allowed_access,
                parent_fd: path.as_raw_fd(),
            };
            // SAFETY: both descriptors and the packed UAPI structure stay live for this call.
            let result = unsafe {
                libc::syscall(
                    __NR_landlock_add_rule as libc::c_long,
                    ruleset_file.as_raw_fd(),
                    landlock_rule_type::LANDLOCK_RULE_PATH_BENEATH as u32,
                    &path_attr as *const PathBeneathAttr,
                    0u32,
                )
            };
            if result < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        for rule in &ruleset.network {
            let net_attr = NetPortAttr {
                allowed_access: rule.allowed_access,
                port: u64::from(rule.port),
            };
            // SAFETY: net_attr is a live C-compatible UAPI structure.
            let result = unsafe {
                libc::syscall(
                    __NR_landlock_add_rule as libc::c_long,
                    ruleset_file.as_raw_fd(),
                    landlock_rule_type::LANDLOCK_RULE_NET_PORT as u32,
                    &net_attr as *const NetPortAttr,
                    0u32,
                )
            };
            if result < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        // SAFETY: PR_SET_NO_NEW_PRIVS takes the documented scalar arguments.
        if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: ruleset_file is a valid ruleset descriptor and flags must be zero through ABI 10.
        let result = unsafe {
            libc::syscall(
                __NR_landlock_restrict_self as libc::c_long,
                ruleset_file.as_raw_fd(),
                0u32,
            )
        };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

const fn ruleset_attr_size(abi: u32) -> usize {
    if abi >= 10 {
        size_of::<RulesetAttr>()
    } else if abi >= 6 {
        3 * size_of::<u64>()
    } else if abi >= 4 {
        2 * size_of::<u64>()
    } else {
        size_of::<u64>()
    }
}

pub fn apply_landlock<K: LandlockKernel>(
    policy: &PolicyDecision,
    kernel: &mut K,
) -> Result<AppliedPolicy, LandlockError> {
    let abi = match kernel.abi() {
        Ok(abi) => abi,
        Err(_error) if policy.landlock.downgrade_mode == DowngradeMode::BestEffort => {
            return Ok(AppliedPolicy {
                kernel_abi: 0,
                enforced: false,
                ruleset: None,
            });
        }
        Err(error) => return Err(LandlockError::Unavailable(error)),
    };
    if policy.landlock.downgrade_mode == DowngradeMode::Required
        && abi < policy.landlock.required_abi
    {
        return Err(LandlockError::AbiTooOld {
            actual: abi,
            required: policy.landlock.required_abi,
        });
    }
    if abi == 0 {
        return Err(LandlockError::Unavailable(io::Error::other(
            "kernel returned ABI 0",
        )));
    }
    let ruleset = effective_ruleset(policy, abi);
    kernel.enforce(&ruleset).map_err(LandlockError::Apply)?;
    Ok(AppliedPolicy {
        kernel_abi: abi,
        enforced: true,
        ruleset: Some(ruleset),
    })
}

fn effective_ruleset(policy: &PolicyDecision, abi: u32) -> EffectiveRuleset {
    let supported_fs = supported_fs(abi);
    let supported_net = supported_net(abi);
    let handled_access_fs = if policy.landlock.filesystem.is_empty() {
        0
    } else {
        supported_fs
    };
    let handled_access_net = if policy.landlock.handles_network {
        supported_net
    } else {
        0
    };
    let scoped = if abi >= 6 {
        (if policy.landlock.scope_abstract_unix {
            SCOPE_ABSTRACT_UNIX_SOCKET
        } else {
            0
        }) | (if policy.landlock.scope_signal {
            SCOPE_SIGNAL
        } else {
            0
        })
    } else {
        0
    };
    let filesystem = policy
        .landlock
        .filesystem
        .iter()
        .filter_map(|rule| {
            let allowed_access = fs_access(rule.access) & supported_fs;
            (allowed_access != 0).then(|| EffectiveFsRule {
                path: rule.path.clone(),
                allowed_access,
            })
        })
        .collect();
    let network = policy
        .landlock
        .network
        .iter()
        .filter_map(|rule| {
            let allowed_access = match rule.access {
                NetAccess::ConnectTcp => NET_CONNECT_TCP,
                NetAccess::BindUdpEphemeral => NET_BIND_UDP,
                NetAccess::ConnectSendUdp => NET_CONNECT_SEND_UDP,
            } & supported_net;
            (allowed_access != 0).then_some(EffectiveNetRule {
                port: rule.port,
                allowed_access,
            })
        })
        .collect();
    EffectiveRuleset {
        abi,
        handled_access_fs,
        handled_access_net,
        scoped,
        filesystem,
        network,
    }
}

const fn supported_fs(abi: u32) -> u64 {
    let mut mask = FS_ABI1;
    if abi >= 2 {
        mask |= FS_REFER;
    }
    if abi >= 3 {
        mask |= FS_TRUNCATE;
    }
    if abi >= 5 {
        mask |= FS_IOCTL_DEV;
    }
    if abi >= 9 {
        mask |= FS_RESOLVE_UNIX;
    }
    mask
}

const fn supported_net(abi: u32) -> u64 {
    let mut mask = 0;
    if abi >= 4 {
        mask |= NET_BIND_TCP | NET_CONNECT_TCP;
    }
    if abi >= 10 {
        mask |= NET_BIND_UDP | NET_CONNECT_SEND_UDP;
    }
    mask
}

const fn fs_access(access: FsAccess) -> u64 {
    match access {
        FsAccess::ReadExecute => FS_EXECUTE | FS_READ_FILE | FS_READ_DIR,
        FsAccess::ReadOnly => FS_READ_FILE | FS_READ_DIR,
        FsAccess::ReadWriteNoExec => {
            (FS_ABI1 | FS_REFER | FS_TRUNCATE | FS_RESOLVE_UNIX) & !FS_EXECUTE
        }
        FsAccess::DeviceReadOnly => FS_READ_FILE | FS_IOCTL_DEV,
        FsAccess::DeviceReadWrite => FS_READ_FILE | FS_WRITE_FILE | FS_IOCTL_DEV,
        FsAccess::UnixConnect => FS_RESOLVE_UNIX,
    }
}
