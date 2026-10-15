//! Bounded, authenticated hang sampling primitives.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{self, Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::time::Duration;

pub mod client;
pub mod protocol;
pub mod report;
pub mod server;
pub mod transport;
mod unwind;

pub const SAMPLE_DEADLINE: Duration = Duration::from_millis(300);
const MAX_RETAINED_EPISODES: usize = 128;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct EpisodeKey {
    pub unit: String,
    pub pid: i32,
    pub start_time: u64,
    pub hang_nonce: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Frame {
    pub build_id: Option<Vec<u8>>,
    pub offset: Option<u64>,
}

impl Frame {
    pub fn unknown() -> Self {
        Self {
            build_id: None,
            offset: None,
        }
    }
}

#[derive(Default)]
pub struct EpisodeStore {
    completed: BTreeMap<EpisodeKey, Vec<Vec<Frame>>>,
    recovered: BTreeSet<EpisodeKey>,
    finalized: BTreeSet<EpisodeKey>,
    order: VecDeque<EpisodeKey>,
}

impl EpisodeStore {
    fn track_new(&mut self, key: EpisodeKey) {
        self.order.push_back(key);
        while self.order.len() > MAX_RETAINED_EPISODES {
            let Some(expired) = self.order.pop_front() else {
                break;
            };
            self.completed.remove(&expired);
            self.recovered.remove(&expired);
            self.finalized.remove(&expired);
        }
    }

    pub fn complete(&mut self, key: EpisodeKey, frames: Vec<Frame>) {
        if self.recovered.contains(&key) {
            return;
        }
        if !self.completed.contains_key(&key) && !self.finalized.contains(&key) {
            self.track_new(key.clone());
        }
        let samples = self.completed.entry(key).or_default();
        if samples.len() < 2 {
            samples.push(frames);
        }
    }

    pub fn recover(&mut self, key: &EpisodeKey) {
        if !self.contains(key) && !self.finalized.contains(key) {
            self.track_new(key.clone());
        }
        self.recovered.insert(key.clone());
        self.completed.remove(key);
    }

    pub fn samples(&self, key: &EpisodeKey) -> Option<&[Vec<Frame>]> {
        self.completed.get(key).map(Vec::as_slice)
    }

    fn contains(&self, key: &EpisodeKey) -> bool {
        self.completed.contains_key(key) || self.recovered.contains(key)
    }

    fn active_for_target(&self, unit: &str, pid: i32, start_time: u64) -> Option<EpisodeKey> {
        self.completed
            .keys()
            .find(|key| key.unit == unit && key.pid == pid && key.start_time == start_time)
            .cloned()
    }

    fn finalize(&mut self, key: &EpisodeKey) -> Option<Vec<Vec<Frame>>> {
        if !self.finalized.insert(key.clone()) || self.recovered.contains(key) {
            return None;
        }
        self.completed.get(key).cloned()
    }
}

pub fn target_app_id_from_cgroup(cgroup: &str) -> Option<String> {
    pf_peer_identity::service_unit_from_cgroup(cgroup)
        .as_deref()
        .and_then(pf_peer_identity::app_id_from_unit)
}

fn arm_sigkill_timer(timeout: Duration) -> io::Result<libc::timer_t> {
    let mut timer: libc::timer_t = std::ptr::null_mut();
    let mut event: libc::sigevent = unsafe { std::mem::zeroed() };
    event.sigev_notify = libc::SIGEV_SIGNAL;
    event.sigev_signo = libc::SIGKILL;
    // SAFETY: timer and event point to initialized process-local storage.
    if unsafe { libc::timer_create(libc::CLOCK_MONOTONIC, &mut event, &mut timer) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let value = libc::itimerspec {
        it_interval: libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        },
        it_value: libc::timespec {
            tv_sec: timeout
                .as_secs()
                .try_into()
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "timeout is too large"))?,
            tv_nsec: timeout.subsec_nanos() as libc::c_long,
        },
    };
    // SAFETY: timer was created above and value is fully initialized.
    if unsafe { libc::timer_settime(timer, 0, &value, std::ptr::null_mut()) } != 0 {
        let error = io::Error::last_os_error();
        unsafe { libc::timer_delete(timer) };
        return Err(error);
    }
    Ok(timer)
}

fn wait_for_ptrace_stop(pid: i32) -> io::Result<()> {
    let mut status = 0;
    loop {
        // SAFETY: status points to writable storage and pid names the seized tracee.
        let result = unsafe { libc::waitpid(pid, &mut status, libc::__WALL) };
        if result == pid && libc::WIFSTOPPED(status) {
            return Ok(());
        }
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if result == pid {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "sample target exited",
            ));
        }
    }
}

fn deadline_worker(pid: i32, stall: Duration) -> io::Result<()> {
    // SAFETY: process-local prctl with a scalar signal argument.
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let _timer = arm_sigkill_timer(SAMPLE_DEADLINE)?;
    // SAFETY: PTRACE_SEIZE does not stop the tracee and passes no pointer arguments here.
    if unsafe { libc::ptrace(libc::PTRACE_SEIZE, pid, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pid is now this worker's tracee.
    if unsafe { libc::ptrace(libc::PTRACE_INTERRUPT, pid, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    wait_for_ptrace_stop(pid)?;
    std::thread::sleep(stall);
    // PTRACE_O_EXITKILL is deliberately absent: if the timer fires, Linux detaches and resumes
    // the live tracee when this worker dies.
    if unsafe { libc::ptrace(libc::PTRACE_DETACH, pid, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

struct TraceeGuard(i32);

impl Drop for TraceeGuard {
    fn drop(&mut self) {
        // SAFETY: best-effort detach of the tracee owned by this worker. ESRCH means it died.
        unsafe { libc::ptrace(libc::PTRACE_DETACH, self.0, 0, 0) };
    }
}

fn sampling_worker(
    pid: i32,
    allowlist: &BTreeSet<Vec<u8>>,
    output: &mut impl Write,
) -> io::Result<()> {
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let timer = arm_sigkill_timer(SAMPLE_DEADLINE)?;
    if unsafe { libc::ptrace(libc::PTRACE_SEIZE, pid, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let guard = TraceeGuard(pid);
    if unsafe { libc::ptrace(libc::PTRACE_INTERRUPT, pid, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    wait_for_ptrace_stop(pid)?;
    let frames = unwind::unwind_stopped(pid, allowlist)?;
    drop(guard);
    // Disarm before sending the completed result so an already-detached worker cannot lose a
    // valid sample at the boundary.
    let disarmed: libc::itimerspec = unsafe { std::mem::zeroed() };
    if unsafe { libc::timer_settime(timer, 0, &disarmed, std::ptr::null_mut()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe { libc::timer_delete(timer) };
    let encoded = serde_json::to_vec(&frames).map_err(io::Error::other)?;
    output.write_all(&encoded)
}

/// Take one bounded sample in a short-lived tracer worker.
///
/// An empty vector is the only externally visible outcome for timeout, target exit, ptrace
/// refusal, partial reads, unwind failure, worker crash, or malformed worker output.
pub fn sample_target(pid: i32, allowlist: &BTreeSet<Vec<u8>>) -> Vec<Frame> {
    let mut pipe = [0; 2];
    if unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Vec::new();
    }
    let read_end = unsafe { OwnedFd::from_raw_fd(pipe[0]) };
    let write_end = unsafe { OwnedFd::from_raw_fd(pipe[1]) };
    let worker = unsafe { libc::fork() };
    if worker < 0 {
        return Vec::new();
    }
    if worker == 0 {
        drop(read_end);
        let mut output = std::fs::File::from(write_end);
        let code = if sampling_worker(pid, allowlist, &mut output).is_ok() {
            0
        } else {
            70
        };
        unsafe { libc::_exit(code) };
    }
    drop(write_end);
    let mut encoded = Vec::new();
    let input = std::fs::File::from(read_end);
    if input.take(64 * 1024).read_to_end(&mut encoded).is_err() {
        encoded.clear();
    }
    let mut status = 0;
    loop {
        let result = unsafe { libc::waitpid(worker, &mut status, 0) };
        if result == worker {
            break;
        }
        if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return Vec::new();
        }
    }
    if !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 0 {
        return Vec::new();
    }
    serde_json::from_slice::<Vec<Frame>>(&encoded).unwrap_or_default()
}

pub fn seize_interrupt_and_expire(pid: i32, stall: Duration) -> io::Result<()> {
    // SAFETY: fork creates one short-lived tracer worker. The child runs only the isolated worker
    // routine and `_exit`; it does not return into the caller.
    let worker = unsafe { libc::fork() };
    if worker < 0 {
        return Err(io::Error::last_os_error());
    }
    if worker == 0 {
        let code = if deadline_worker(pid, stall).is_ok() {
            0
        } else {
            70
        };
        unsafe { libc::_exit(code) };
    }
    let mut status = 0;
    loop {
        // SAFETY: status is writable and worker names this process's child.
        let result = unsafe { libc::waitpid(worker, &mut status, 0) };
        if result == worker {
            break;
        }
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
    }
    if (libc::WIFSIGNALED(status) && libc::WTERMSIG(status) == libc::SIGKILL)
        || (libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0)
    {
        Ok(())
    } else if libc::WIFEXITED(status) {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "ptrace worker refused with status {}",
                libc::WEXITSTATUS(status)
            ),
        ))
    } else {
        Err(io::Error::other("ptrace worker ended unexpectedly"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};
    use std::thread;
    use std::time::Instant;

    fn key(nonce: u64) -> EpisodeKey {
        EpisodeKey {
            unit: "pf-app@org.example.hung.service".to_owned(),
            pid: 123,
            start_time: 456,
            hang_nonce: nonce,
        }
    }

    #[test]
    fn recovery_tombstone_discards_completed_sample() {
        let mut store = EpisodeStore::default();
        let recovered = key(1);
        store.complete(recovered.clone(), vec![]);
        store.recover(&recovered);
        store.complete(recovered.clone(), vec![]);
        assert!(store.samples(&recovered).is_none());

        let hung = key(2);
        store.complete(hung.clone(), vec![]);
        assert_eq!(store.samples(&hung).unwrap().len(), 1);
    }

    #[test]
    fn episode_store_bounds_completed_recovered_and_finalized_state() {
        let mut store = EpisodeStore::default();
        for nonce in 0..256 {
            store.complete(key(nonce), vec![]);
        }
        for nonce in 256..512 {
            store.recover(&key(nonce));
        }
        for nonce in 512..768 {
            let episode = key(nonce);
            store.complete(episode.clone(), vec![]);
            assert!(store.finalize(&episode).is_some());
        }

        assert!(store.completed.len() <= 128);
        assert!(store.recovered.len() <= 128);
        assert!(store.finalized.len() <= 128);
    }

    #[test]
    fn input_units_are_never_sample_targets() {
        let app = "0::/system.slice/system-pf\\x2dapp.slice/pf-app@org.example.hung.service\n";
        assert_eq!(
            target_app_id_from_cgroup(app).as_deref(),
            Some("org.example.hung")
        );
        for unit in [
            "pf-osk.service",
            "pf-fcitx5.service",
            "pf-input-broker.service",
            "pf-menu.service",
            "input-lookalike.service",
        ] {
            let cgroup = format!("0::/system.slice/{unit}\n");
            assert_eq!(target_app_id_from_cgroup(&cgroup), None, "{unit}");
        }
        let nested = "0::/system.slice/system-pf\\x2dapp.slice/pf-app@org.example.hung.service/child.service\n";
        assert_eq!(target_app_id_from_cgroup(nested), None);
    }

    fn spawn_live_target() -> Child {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "tests::live_target_helper",
                "--nocapture",
            ])
            .env("PF_HANG_TARGET_HELPER", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut reader = BufReader::new(child.stdout.take().unwrap());
        loop {
            let mut line = String::new();
            assert_ne!(
                reader.read_line(&mut line).unwrap(),
                0,
                "target exited early"
            );
            if line.trim() == "TARGET_READY" {
                break;
            }
        }
        child
    }

    #[test]
    #[ignore]
    fn live_target_helper() {
        if std::env::var_os("PF_HANG_TARGET_HELPER").is_none() {
            return;
        }
        // Permit the sibling tracer under Yama scope 1 without adding a container capability.
        assert_eq!(
            unsafe { libc::prctl(libc::PR_SET_PTRACER, libc::PR_SET_PTRACER_ANY) },
            0
        );
        println!("TARGET_READY");
        loop {
            std::hint::spin_loop();
        }
    }

    #[test]
    fn worker_sigkill_deadline_detaches_live_target() {
        let mut target = spawn_live_target();
        let started = Instant::now();
        seize_interrupt_and_expire(target.id() as i32, Duration::from_secs(5)).unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));

        thread::sleep(Duration::from_millis(20));
        assert!(
            target.try_wait().unwrap().is_none(),
            "target must remain alive"
        );
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", target.id())).unwrap();
        let state = stat.rsplit_once(") ").unwrap().1.as_bytes()[0];
        assert_ne!(state, b't', "target remained ptrace-stopped");
        target.kill().unwrap();
        target.wait().unwrap();
    }
}
