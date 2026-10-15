use crate::Frame;
use framehop::{ExplicitModuleSectionInfo, Module, Unwinder};
use object::{Object, ObjectSection};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::ops::Range;

const MAX_FRAMES: usize = 48;

#[derive(Clone)]
struct LoadedModule {
    avma: Range<u64>,
    base_avma: u64,
    build_id: Option<Vec<u8>>,
    sections: ExplicitModuleSectionInfo<Vec<u8>>,
}

pub(crate) struct PreparedUnwind {
    modules: Vec<LoadedModule>,
    mappings: Vec<MappingIdentity>,
}

#[derive(Default)]
struct MapGroup {
    start: u64,
    end: u64,
    base: u64,
    executable: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MappingIdentity {
    start: u64,
    end: u64,
    offset: u64,
    permissions: String,
    device: String,
    inode: u64,
    path: String,
}

struct ProcessMappings {
    files: Vec<(String, MapGroup)>,
    identity: Vec<MappingIdentity>,
}

fn parse_hex(text: &str) -> io::Result<u64> {
    u64::from_str_radix(text, 16)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid process map"))
}

fn mapped_files(pid: i32) -> io::Result<ProcessMappings> {
    let maps = fs::read_to_string(format!("/proc/{pid}/maps"))?;
    let mut groups = BTreeMap::<String, MapGroup>::new();
    let mut mappings = Vec::new();
    for line in maps.lines() {
        let mut fields = line.split_whitespace();
        let range = fields.next().unwrap_or("");
        let permissions = fields.next().unwrap_or("");
        let offset = fields.next().unwrap_or("");
        let device = fields.next().unwrap_or("");
        let inode = fields.next().unwrap_or("");
        let path = fields.collect::<Vec<_>>().join(" ");
        if !path.starts_with('/') || path.ends_with(" (deleted)") {
            continue;
        }
        let (start, end) = range
            .split_once('-')
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid process map"))?;
        let start = parse_hex(start)?;
        let end = parse_hex(end)?;
        let offset = parse_hex(offset)?;
        let inode = inode
            .parse::<u64>()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid process map"))?;
        mappings.push(MappingIdentity {
            start,
            end,
            offset,
            permissions: permissions.to_owned(),
            device: device.to_owned(),
            inode,
            path: path.clone(),
        });
        let base = start.checked_sub(offset).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "invalid process map base")
        })?;
        let group = groups.entry(path).or_insert(MapGroup {
            start,
            end,
            base,
            executable: false,
        });
        group.start = group.start.min(start);
        group.end = group.end.max(end);
        group.base = group.base.min(base);
        group.executable |= permissions.as_bytes().get(2) == Some(&b'x');
    }
    Ok(ProcessMappings {
        files: groups
            .into_iter()
            .filter(|(_, group)| group.executable)
            .collect(),
        identity: mappings,
    })
}

fn section(file: &object::File<'_>, name: &str) -> Option<(Range<u64>, Vec<u8>)> {
    let section = file.section_by_name(name)?;
    let data = section.data().ok()?.to_vec();
    let start = section.address();
    Some((start..start.checked_add(section.size())?, data))
}

fn load_modules(pid: i32, files: Vec<(String, MapGroup)>) -> Vec<LoadedModule> {
    let mut modules = Vec::new();
    for (path, group) in files {
        let rooted = format!("/proc/{pid}/root{path}");
        let Ok(bytes) = fs::read(rooted) else {
            continue;
        };
        let Ok(file) = object::File::parse(bytes.as_slice()) else {
            continue;
        };
        if !file.is_little_endian() {
            continue;
        }
        let (text_svma, text) = section(&file, ".text")
            .map(|(range, data)| (Some(range), Some(data)))
            .unwrap_or_default();
        let (eh_frame_svma, eh_frame) = section(&file, ".eh_frame")
            .map(|(range, data)| (Some(range), Some(data)))
            .unwrap_or_default();
        let (eh_frame_hdr_svma, eh_frame_hdr) = section(&file, ".eh_frame_hdr")
            .map(|(range, data)| (Some(range), Some(data)))
            .unwrap_or_default();
        let (_, debug_frame) = section(&file, ".debug_frame")
            .map(|(range, data)| (Some(range), Some(data)))
            .unwrap_or_default();
        let (got_svma, _) = section(&file, ".got")
            .map(|(range, data)| (Some(range), Some(data)))
            .unwrap_or_default();
        let build_id = file.build_id().ok().flatten().map(<[u8]>::to_vec);
        modules.push(LoadedModule {
            avma: group.start..group.end,
            base_avma: group.base,
            build_id,
            sections: ExplicitModuleSectionInfo {
                base_svma: file.relative_address_base(),
                text_svma,
                text,
                got_svma,
                eh_frame_svma,
                eh_frame,
                eh_frame_hdr_svma,
                eh_frame_hdr,
                debug_frame,
                ..Default::default()
            },
        });
    }
    modules
}

pub(crate) fn prepare(pid: i32) -> io::Result<PreparedUnwind> {
    let mappings = mapped_files(pid)?;
    Ok(PreparedUnwind {
        modules: load_modules(pid, mappings.files),
        mappings: mappings.identity,
    })
}

impl PreparedUnwind {
    fn validate_process_image(&self, pid: i32) -> io::Result<()> {
        let current = mapped_files(pid)?;
        if current.identity != self.mappings {
            return Err(io::Error::other("prepared process image changed"));
        }
        Ok(())
    }
}

fn read_word(pid: i32, address: u64) -> Result<u64, ()> {
    let mut word = 0_u64;
    let local = libc::iovec {
        iov_base: (&mut word as *mut u64).cast(),
        iov_len: std::mem::size_of::<u64>(),
    };
    let remote = libc::iovec {
        iov_base: (address as usize as *mut libc::c_void),
        iov_len: std::mem::size_of::<u64>(),
    };
    // SAFETY: local points to one writable word; remote is read only by process_vm_readv.
    let read = unsafe { libc::process_vm_readv(pid, &local, 1, &remote, 1, 0) };
    if read == std::mem::size_of::<u64>() as isize {
        Ok(word)
    } else {
        Err(())
    }
}

fn map_frames(
    addresses: impl IntoIterator<Item = u64>,
    modules: &[LoadedModule],
    allowlist: &BTreeSet<Vec<u8>>,
) -> Vec<Frame> {
    addresses
        .into_iter()
        .take(MAX_FRAMES)
        .map(|address| {
            let allowed = modules.iter().find(|module| module.avma.contains(&address));
            let Some(module) = allowed else {
                return Frame::unknown();
            };
            let Some(build_id) = module
                .build_id
                .as_ref()
                .filter(|id| (8..=32).contains(&id.len()) && allowlist.contains(*id))
            else {
                return Frame::unknown();
            };
            Frame {
                build_id: Some(build_id.clone()),
                offset: address.checked_sub(module.base_avma),
            }
        })
        .collect()
}

#[cfg(target_arch = "x86_64")]
pub fn unwind_stopped(pid: i32, allowlist: &BTreeSet<Vec<u8>>) -> io::Result<Vec<Frame>> {
    let prepared = prepare(pid)?;
    unwind_prepared(pid, &prepared, allowlist)
}

#[cfg(target_arch = "x86_64")]
pub(crate) fn unwind_prepared(
    pid: i32,
    prepared: &PreparedUnwind,
    allowlist: &BTreeSet<Vec<u8>>,
) -> io::Result<Vec<Frame>> {
    use framehop::x86_64::{CacheX86_64, UnwindRegsX86_64, UnwinderX86_64};

    prepared.validate_process_image(pid)?;
    let modules = &prepared.modules;
    let mut unwinder = UnwinderX86_64::<Vec<u8>>::new();
    for module in modules {
        unwinder.add_module(Module::new(
            String::new(),
            module.avma.clone(),
            module.base_avma,
            module.sections.clone(),
        ));
    }
    let mut regs: libc::user_regs_struct = unsafe { std::mem::zeroed() };
    if unsafe { libc::ptrace(libc::PTRACE_GETREGS, pid, 0, &mut regs) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut cache = CacheX86_64::new();
    let mut reader = |address| read_word(pid, address);
    let mut iterator = unwinder.iter_frames(
        regs.rip,
        UnwindRegsX86_64::new(regs.rip, regs.rsp, regs.rbp),
        &mut cache,
        &mut reader,
    );
    let mut addresses = Vec::new();
    loop {
        match iterator.next() {
            Ok(Some(frame)) if addresses.len() < MAX_FRAMES => addresses.push(frame.address()),
            Ok(Some(_)) | Ok(None) => break,
            Err(_) => return Err(io::Error::other("unwind failed")),
        }
    }
    Ok(map_frames(addresses, modules, allowlist))
}

#[cfg(target_arch = "aarch64")]
pub fn unwind_stopped(pid: i32, allowlist: &BTreeSet<Vec<u8>>) -> io::Result<Vec<Frame>> {
    let prepared = prepare(pid)?;
    unwind_prepared(pid, &prepared, allowlist)
}

#[cfg(target_arch = "aarch64")]
pub(crate) fn unwind_prepared(
    pid: i32,
    prepared: &PreparedUnwind,
    allowlist: &BTreeSet<Vec<u8>>,
) -> io::Result<Vec<Frame>> {
    use framehop::aarch64::{CacheAarch64, PtrAuthMask, UnwindRegsAarch64, UnwinderAarch64};

    prepared.validate_process_image(pid)?;
    let modules = &prepared.modules;
    let mut unwinder = UnwinderAarch64::<Vec<u8>>::new();
    for module in modules {
        unwinder.add_module(Module::new(
            String::new(),
            module.avma.clone(),
            module.base_avma,
            module.sections.clone(),
        ));
    }
    #[repr(C)]
    struct UserRegs {
        x: [u64; 31],
        sp: u64,
        pc: u64,
        pstate: u64,
    }
    let mut regs: UserRegs = unsafe { std::mem::zeroed() };
    let mut iov = libc::iovec {
        iov_base: (&mut regs as *mut UserRegs).cast(),
        iov_len: std::mem::size_of::<UserRegs>(),
    };
    if unsafe { libc::ptrace(libc::PTRACE_GETREGSET, pid, 1, &mut iov) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut cache = CacheAarch64::new();
    let mut reader = |address| read_word(pid, address);
    let mask = PtrAuthMask::from_max_known_address(unwinder.max_known_code_address());
    let mut iterator = unwinder.iter_frames(
        regs.pc,
        UnwindRegsAarch64::new_with_ptr_auth_mask(mask, regs.x[30], regs.sp, regs.x[29]),
        &mut cache,
        &mut reader,
    );
    let mut addresses = Vec::new();
    loop {
        match iterator.next() {
            Ok(Some(frame)) if addresses.len() < MAX_FRAMES => addresses.push(frame.address()),
            Ok(Some(_)) | Ok(None) => break,
            Err(_) => return Err(io::Error::other("unwind failed")),
        }
    }
    Ok(map_frames(addresses, modules, allowlist))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framehop_offline_fallback_covers_x86_64_and_aarch64() {
        use framehop::aarch64::{CacheAarch64, UnwindRegsAarch64, UnwinderAarch64};
        use framehop::x86_64::{CacheX86_64, UnwindRegsX86_64, UnwinderX86_64};

        let x86_stack = BTreeMap::from([(0x2000, 0_u64), (0x2008, 0x1234_u64)]);
        let mut x86_read = |address| x86_stack.get(&address).copied().ok_or(());
        let x86 = UnwinderX86_64::<Vec<u8>>::new();
        let mut x86_cache = CacheX86_64::new();
        let mut x86_iter = x86.iter_frames(
            0x1111,
            UnwindRegsX86_64::new(0x1111, 0x1000, 0x2000),
            &mut x86_cache,
            &mut x86_read,
        );
        assert_eq!(x86_iter.next().unwrap().unwrap().address(), 0x1111);
        assert_eq!(x86_iter.next().unwrap().unwrap().address(), 0x1234);

        let arm_stack = BTreeMap::from([
            (0x3000, 0x4000_u64),
            (0x3008, 0x5678_u64),
            (0x4000, 0_u64),
            (0x4008, 0_u64),
        ]);
        let mut arm_read = |address| arm_stack.get(&address).copied().ok_or(());
        let arm = UnwinderAarch64::<Vec<u8>>::new();
        let mut arm_cache = CacheAarch64::new();
        let mut arm_iter = arm.iter_frames(
            0x4444,
            UnwindRegsAarch64::new(0x5678, 0x1000, 0x3000),
            &mut arm_cache,
            &mut arm_read,
        );
        assert_eq!(arm_iter.next().unwrap().unwrap().address(), 0x4444);
        assert_eq!(arm_iter.next().unwrap().unwrap().address(), 0x5678);
    }
}
