//! Which input device is the built-in controller (`tsp-ihne1.8`).
//!
//! **This module is the only way to answer that question.** The session input router, the
//! broker's `--require-builtin` source check and the new-keyboard confirm card all call
//! [`Trust::identify_fd`]; nothing may decide it from the device name, the vid/pid or the
//! capabilities, because a USB device or another `uinput` creator can copy all of them.
//!
//! A device is the built-in controller only when all three hold:
//!
//! 1. **Virtual-device path.** Its char device is `13:<minor>` and `/sys/dev/char/13:<minor>`
//!    resolves to `/sys/devices/virtual/input/input<N>/event<M>`. Devices on a real bus (USB,
//!    Bluetooth, platform) never live under `devices/virtual`.
//! 2. **Decoder-set property.** `input<N>/phys` is [`PHYS`], which `pf-input-decode` sets with
//!    `UI_SET_PHYS`. The kernel fills `phys` for real devices from their topology.
//! 3. **Decoder record.** `input<N>` is the sysname `pf-input-decode` wrote to [`RECORD_NAME`] in
//!    [`RECORD_DIR`] right after creating the device. The record and its directory must be owned by
//!    root and writable by nobody else. `phys` is readable by everyone, so another `uinput` holder
//!    could copy it; the record binds trust to the one kernel device the decoder created.
//!    Linux may reuse `input<N>` after a device disappears, so the decoder clears any stale record
//!    before creating its device. Its systemd `RuntimeDirectory` is also removed when the service
//!    stops. Trust therefore exists only after this decoder instance publishes its new record.
//!
//! Apps cannot open `/dev/uinput` at all (the image's `pf-app@.service` hides it), so in practice
//! only root services can create virtual input devices; (3) keeps a bug in one of those from
//! being enough.

use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

/// The `phys` string `pf-input-decode` sets on its virtual gamepad.
pub const PHYS: &str = "pf-input-decode/builtin-pad";
/// The decoder's record directory (systemd `RuntimeDirectory=pocketforge-input`, root 0755).
/// Not `/run/pocketforge`: that directory belongs to the session user.
pub const RECORD_DIR: &str = "/run/pocketforge-input";
/// The record file inside [`RECORD_DIR`]: one line, `input<N>\n`.
pub const RECORD_NAME: &str = "builtin-pad";

const INPUT_MAJOR: u32 = 13;

/// The verdict for one device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The decoder's own virtual gamepad, `input<N>` = `sysname`.
    BuiltIn { sysname: String },
    /// Anything else, with the first check that failed.
    NotBuiltIn(Reason),
}

/// Why a device is not the built-in controller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// Not an input char device (major 13).
    NotInputCharDevice,
    /// Not under `devices/virtual/input/input<N>/event<M>`; `devpath` is where it lives.
    NotVirtual { devpath: String },
    /// `phys` is not [`PHYS`].
    PhysMismatch,
    /// No record: the decoder is not running (or has not created its device yet).
    RecordAbsent,
    /// The record exists but is not trustworthy (owner, mode, symlink, content).
    RecordUntrusted(String),
    /// A virtual device with the right `phys`, but not the instance the decoder recorded.
    SysnameMismatch { device: String, record: String },
}

/// Where to look, and whose record to trust. [`Trust::system`] in production.
#[derive(Debug, Clone)]
pub struct Trust {
    sys_root: PathBuf,
    record_dir: PathBuf,
    owner_uid: u32,
}

impl Trust {
    /// The real `/sys` and [`RECORD_DIR`], trusting only records owned by root.
    pub fn system() -> Trust {
        Trust::with_roots("/sys", RECORD_DIR, 0)
    }

    /// Explicit roots and record owner, for hermetic tests against a fake sysfs tree.
    pub fn with_roots(
        sys_root: impl Into<PathBuf>,
        record_dir: impl Into<PathBuf>,
        owner_uid: u32,
    ) -> Trust {
        Trust {
            sys_root: sys_root.into(),
            record_dir: record_dir.into(),
            owner_uid,
        }
    }

    /// Identify the device behind an opened fd. Checking the fd, not a path, means the answer is
    /// about the node the caller actually holds.
    pub fn identify_fd(&self, fd: BorrowedFd<'_>) -> io::Result<Verdict> {
        // SAFETY: fstat on a borrowed (valid) fd into a zeroed stat buffer.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } < 0 {
            return Err(io::Error::last_os_error());
        }
        self.identify_rdev(st.st_mode, st.st_rdev)
    }

    /// Identify a device from its `st_mode` and `st_rdev`.
    pub fn identify_rdev(&self, mode: libc::mode_t, rdev: libc::dev_t) -> io::Result<Verdict> {
        if mode & libc::S_IFMT != libc::S_IFCHR || libc::major(rdev) != INPUT_MAJOR {
            return Ok(Verdict::NotBuiltIn(Reason::NotInputCharDevice));
        }
        let link = self
            .sys_root
            .join("dev/char")
            .join(format!("{INPUT_MAJOR}:{}", libc::minor(rdev)));
        let target = fs::canonicalize(&link)?;
        let root = fs::canonicalize(&self.sys_root)?;
        let rel = target.strip_prefix(&root).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} resolves outside sysfs", link.display()),
            )
        })?;
        let parts: Vec<&str> = rel
            .components()
            .map(|c| match c {
                Component::Normal(s) => s.to_str().unwrap_or(""),
                _ => "",
            })
            .collect();
        let sysname = match parts.as_slice() {
            ["devices", "virtual", "input", input, event]
                if is_numbered(input, "input") && is_numbered(event, "event") =>
            {
                (*input).to_string()
            }
            _ => {
                return Ok(Verdict::NotBuiltIn(Reason::NotVirtual {
                    devpath: rel.display().to_string(),
                }))
            }
        };
        let phys = fs::read(
            root.join("devices/virtual/input")
                .join(&sysname)
                .join("phys"),
        )?;
        if phys.strip_suffix(b"\n").unwrap_or(&phys) != PHYS.as_bytes() {
            return Ok(Verdict::NotBuiltIn(Reason::PhysMismatch));
        }
        let record = match self.read_record()? {
            Ok(record) => record,
            Err(reason) => return Ok(Verdict::NotBuiltIn(reason)),
        };
        if record != sysname {
            return Ok(Verdict::NotBuiltIn(Reason::SysnameMismatch {
                device: sysname,
                record,
            }));
        }
        Ok(Verdict::BuiltIn { sysname })
    }

    /// The recorded sysname, or why there is no trustworthy one. I/O failures other than "absent"
    /// are errors.
    fn read_record(&self) -> io::Result<Result<String, Reason>> {
        let untrusted = |why: String| Ok(Err(Reason::RecordUntrusted(why)));
        let dir = match fs::symlink_metadata(&self.record_dir) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Err(Reason::RecordAbsent)),
            Err(e) => return Err(e),
        };
        if !dir.is_dir() {
            return untrusted(format!("{} is not a directory", self.record_dir.display()));
        }
        if let Some(why) = self.ownership_problem(&dir) {
            return untrusted(format!("directory {why}"));
        }
        let path = self.record_dir.join(RECORD_NAME);
        let file = match fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Err(Reason::RecordAbsent)),
            Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
                return untrusted("record is a symlink".into())
            }
            Err(e) => return Err(e),
        };
        let meta = file.metadata()?;
        if !meta.is_file() {
            return untrusted("record is not a regular file".into());
        }
        if let Some(why) = self.ownership_problem(&meta) {
            return untrusted(format!("record {why}"));
        }
        // A valid record is a few bytes; read one past the longest sane line to detect junk.
        let mut buf = Vec::new();
        file.take(64).read_to_end(&mut buf)?;
        match std::str::from_utf8(&buf)
            .ok()
            .and_then(|s| s.strip_suffix('\n'))
        {
            Some(name) if is_numbered(name, "input") => Ok(Ok(name.to_string())),
            _ => untrusted(format!(
                "record content {:?} is not one input<N> line",
                String::from_utf8_lossy(&buf)
            )),
        }
    }

    fn ownership_problem(&self, meta: &fs::Metadata) -> Option<String> {
        if meta.uid() != self.owner_uid {
            return Some(format!(
                "owned by uid {} (want {})",
                meta.uid(),
                self.owner_uid
            ));
        }
        if meta.mode() & 0o022 != 0 {
            return Some(format!(
                "mode {:o} is group- or other-writable",
                meta.mode() & 0o7777
            ));
        }
        None
    }
}

/// `input12`, `event3`: the prefix followed by one or more ASCII digits and nothing else.
fn is_numbered(s: &str, prefix: &str) -> bool {
    s.strip_prefix(prefix)
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Remove the prior decoder record before creating a new virtual device. Linux can reuse an
/// `input<N>` sysname after device teardown, so leaving an old record in place during creation
/// could transiently trust the wrong instance. Absence is already the desired fail-closed state.
pub fn clear_record(dir: &Path) -> io::Result<()> {
    match fs::remove_file(dir.join(RECORD_NAME)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Write the record for `sysname` into `dir` (the decoder's call). The file is replaced
/// atomically, mode 0644, so a reader never sees a partial line.
pub fn write_record(dir: &Path, sysname: &str) -> io::Result<()> {
    if !is_numbered(sysname, "input") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not an input sysname: {sysname:?}"),
        ));
    }
    let tmp = dir.join(format!(".{RECORD_NAME}.tmp"));
    match fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)?;
    // create_new applies the umask; set the mode the reader checks explicitly.
    file.set_permissions(fs::Permissions::from_mode(0o644))?;
    file.write_all(format!("{sysname}\n").as_bytes())?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, dir.join(RECORD_NAME))
}
