//! macOS process control: kqueue exit watches pin a process's identity, and `sysctl` and
//! libproc list children and describe processes.

use std::{
    cell::Cell,
    ffi::{CStr, c_void},
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    thread,
    time::{Duration, Instant},
};

/// A kqueue watching one process for its exit. The watch belongs to the process, not to its
/// number: once that process exits the watch fires, even if the PID is reused afterwards, and
/// a fired watch is never followed by a signal.
pub(crate) struct Pin {
    pid: libc::pid_t,
    kqueue: OwnedFd,
    exited: Cell<bool>,
}

impl Pin {
    /// Watches `pid` for its exit; `ESRCH` when it is gone (or already a zombie).
    pub(crate) fn open(pid: libc::pid_t) -> io::Result<Self> {
        // SAFETY: `kqueue` has no preconditions; a valid descriptor is owned at once.
        let fd = unsafe { libc::kqueue() };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let kqueue = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: `fcntl` on the descriptor owned here.
        if unsafe { libc::fcntl(kqueue.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
            return Err(io::Error::last_os_error());
        }
        let change = libc::kevent {
            ident: pid as libc::uintptr_t,
            filter: libc::EVFILT_PROC,
            flags: libc::EV_ADD | libc::EV_ONESHOT,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        // SAFETY: one valid change, no events to receive.
        if unsafe {
            libc::kevent(
                kqueue.as_raw_fd(),
                &change,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        } == -1
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            pid,
            kqueue,
            exited: Cell::new(false),
        })
    }

    /// Signals the process unless its exit has been seen, which is `ESRCH`.
    pub(crate) fn signal(&self, signal: libc::c_int) -> io::Result<()> {
        if self.wait_exit(0) {
            return Err(io::Error::from_raw_os_error(libc::ESRCH));
        }
        // SAFETY: `kill` has no memory preconditions.
        if unsafe { libc::kill(self.pid, signal) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    /// Whether the process exits within `wait` milliseconds (or already has).
    pub(crate) fn wait_exit(&self, wait: i32) -> bool {
        if self.exited.get() {
            return true;
        }
        let wait = wait.max(0);
        let timeout = libc::timespec {
            tv_sec: libc::time_t::from(wait / 1000),
            tv_nsec: libc::c_long::from(wait % 1000) * 1_000_000,
        };
        // SAFETY: an all-zero `kevent` is a valid value to be overwritten.
        let mut event: libc::kevent = unsafe { std::mem::zeroed() };
        // SAFETY: room for one event and a valid timeout, both outliving the call.
        let ready = unsafe {
            libc::kevent(
                self.kqueue.as_raw_fd(),
                std::ptr::null(),
                0,
                &mut event,
                1,
                &timeout,
            )
        };
        if ready > 0 {
            self.exited.set(true);
        }
        self.exited.get()
    }
}

/// Whether `child` has exited, leaving it unreaped so its PID, and so its process group,
/// cannot be reused before the group is killed. An unreaped child that has exited is a zombie,
/// which libproc no longer describes; `waitid` with `WNOWAIT` is not relied on here.
pub fn has_exited(child: &std::process::Child) -> io::Result<bool> {
    let pid = libc::pid_t::try_from(child.id())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid process ID"))?;
    Ok(bsd_info(pid).is_none_or(|info| info.pbi_status == libc::SZOMB))
}

/// Fills a buffer of PIDs from a libproc listing call, which returns how many it wrote.
fn list_pids(list: impl Fn(*mut c_void, libc::c_int) -> libc::c_int) -> io::Result<Vec<u32>> {
    let needed = list(std::ptr::null_mut(), 0);
    if needed < 0 {
        return Err(io::Error::last_os_error());
    }
    // Room for processes started between the two calls.
    let mut pids: Vec<libc::pid_t> = vec![0; usize::try_from(needed).unwrap_or(0) + 64];
    let bytes = libc::c_int::try_from(pids.len() * size_of::<libc::pid_t>()).unwrap_or(0);
    let count = list(pids.as_mut_ptr().cast(), bytes);
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    pids.truncate(usize::try_from(count).unwrap_or(0).min(pids.len()));
    let mut pids: Vec<u32> = pids
        .into_iter()
        .filter_map(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid != 0)
        .collect();
    pids.sort_unstable();
    pids.dedup();
    Ok(pids)
}

/// The children of `pid`.
pub(crate) fn children(pid: libc::pid_t) -> io::Result<Vec<u32>> {
    // SAFETY: the buffer and its size come from `list_pids`.
    list_pids(|buffer, size| unsafe { libc::proc_listchildpids(pid, buffer, size) })
}

/// Waits briefly for `pid` to stop. This only narrows the window in which the process can
/// fork ahead of the walk, so a process that is slow to stop is still killed and its children
/// read.
pub(crate) fn wait_until_stopped(pid: libc::pid_t) {
    let deadline = Instant::now() + Duration::from_millis(100);
    loop {
        match bsd_info(pid) {
            Some(info) if info.pbi_status != libc::SSTOP => {}
            _ => return,
        }
        if Instant::now() >= deadline {
            return;
        }
        thread::sleep(Duration::from_millis(1));
    }
}

/// libproc's description of `pid`; `None` when it is gone or a zombie.
fn bsd_info(pid: libc::pid_t) -> Option<libc::proc_bsdinfo> {
    // SAFETY: an all-zero `proc_bsdinfo` is a valid value to be overwritten.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = libc::c_int::try_from(size_of::<libc::proc_bsdinfo>()).ok()?;
    // SAFETY: the buffer is a `proc_bsdinfo` of the size given.
    let written =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    (written == size).then_some(info)
}

/// `pbi_flags`: the process leads its session (`PROC_FLAG_SLEADER` in `<sys/proc_info.h>`).
const PROC_FLAG_SLEADER: u32 = 0x20;

/// A process as libproc describes it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Stat {
    /// Microseconds since the Unix epoch.
    pub start: u64,
    /// `R`, `S`, `T` or `I` (being created), as `/proc` spells them on Linux.
    pub state: char,
    /// The session ID. `ssh -f` calls `setsid`, so a backgrounded tunnel leads its own
    /// session, while the foreground ssh that is still connecting does not.
    pub session: u32,
}

/// Reads the process table through `sysctl` and libproc.
#[derive(Clone, Debug)]
pub struct Proc {
    _private: (),
}

impl Proc {
    pub fn system() -> Self {
        Self { _private: () }
    }

    /// The boot session UUID (`kern.bootsessionuuid`), new at every boot.
    pub fn boot_id(&self) -> Option<String> {
        let name = c"kern.bootsessionuuid";
        let mut buffer = [0u8; 64];
        let mut length = buffer.len();
        // SAFETY: a NUL-terminated name and a buffer whose length is passed with it.
        if unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                buffer.as_mut_ptr().cast(),
                &mut length,
                std::ptr::null_mut(),
                0,
            )
        } != 0
        {
            return None;
        }
        let id = CStr::from_bytes_until_nul(&buffer[..length.min(buffer.len())])
            .ok()?
            .to_str()
            .ok()?;
        Some(id.trim().to_owned()).filter(|id| !id.is_empty())
    }

    /// Unix seconds at which a process with `start` (microseconds since the epoch) started.
    pub fn started_at(&self, start: u64) -> Option<u64> {
        Some(start / 1_000_000)
    }

    pub fn stat(&self, pid: u32) -> Option<Stat> {
        let raw = libc::pid_t::try_from(pid).ok()?;
        let info = bsd_info(raw)?;
        let state = match info.pbi_status {
            libc::SRUN => 'R',
            libc::SSTOP => 'T',
            libc::SZOMB => 'Z',
            libc::SIDL => 'I',
            _ => 'S',
        };
        let session = if info.pbi_flags & PROC_FLAG_SLEADER != 0 {
            pid
        } else {
            // SAFETY: `getsid` has no memory preconditions.
            u32::try_from(unsafe { libc::getsid(raw) }).unwrap_or(0)
        };
        Some(Stat {
            start: info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec,
            state,
            session,
        })
    }

    /// The real UID of `pid`.
    pub fn uid(&self, pid: u32) -> Option<u32> {
        bsd_info(libc::pid_t::try_from(pid).ok()?).map(|info| info.pbi_ruid)
    }

    /// The arguments of `pid` (`KERN_PROCARGS2`); readable for the current user's processes.
    pub fn cmdline(&self, pid: u32) -> Option<Vec<Vec<u8>>> {
        let raw = libc::c_int::try_from(pid).ok()?;
        let mut buffer = vec![0u8; arg_max()];
        let mut length = buffer.len();
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, raw];
        // SAFETY: a valid MIB and a buffer whose length is passed with it.
        if unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                buffer.as_mut_ptr().cast(),
                &mut length,
                std::ptr::null_mut(),
                0,
            )
        } != 0
        {
            return None;
        }
        parse_procargs(&buffer[..length.min(buffer.len())])
    }

    pub fn pids(&self) -> Vec<u32> {
        // SAFETY: the buffer and its size come from `list_pids`.
        list_pids(|buffer, size| unsafe { libc::proc_listallpids(buffer, size) })
            .unwrap_or_default()
    }
}

/// The largest argument area of a process (`kern.argmax`).
fn arg_max() -> usize {
    let mut value: libc::c_int = 0;
    let mut length = size_of::<libc::c_int>();
    let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
    // SAFETY: a valid MIB and an `int` whose size is passed with it.
    let read = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            2,
            (&raw mut value).cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    match usize::try_from(value) {
        Ok(value) if read == 0 && value > 0 => value,
        _ => 1024 * 1024,
    }
}

/// The arguments in a `KERN_PROCARGS2` buffer: `argc` as a native `int`, the executable
/// path, NUL padding, then `argc` NUL-terminated arguments (and the environment, ignored).
fn parse_procargs(buffer: &[u8]) -> Option<Vec<Vec<u8>>> {
    let count_size = size_of::<libc::c_int>();
    let argc = libc::c_int::from_ne_bytes(buffer.get(..count_size)?.try_into().ok()?);
    let argc = usize::try_from(argc).ok()?;
    let rest = &buffer[count_size..];
    let path_end = rest.iter().position(|byte| *byte == 0)?;
    let rest = &rest[path_end..];
    let start = rest
        .iter()
        .position(|byte| *byte != 0)
        .unwrap_or(rest.len());
    let mut args = Vec::with_capacity(argc);
    let mut fields = rest[start..].split(|byte| *byte == 0);
    for _ in 0..argc {
        args.push(fields.next()?.to_vec());
    }
    Some(args)
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    #[test]
    fn procargs_are_parsed_without_the_path_or_the_environment() {
        let mut buffer = 3i32.to_ne_bytes().to_vec();
        buffer.extend_from_slice(b"/usr/bin/ssh\0\0\0\0ssh\0-E\0/tmp/a b.log\0HOME=/x\0\0");
        assert_eq!(
            parse_procargs(&buffer).unwrap(),
            [b"ssh".to_vec(), b"-E".to_vec(), b"/tmp/a b.log".to_vec()]
        );
        let mut short = 3i32.to_ne_bytes().to_vec();
        short.extend_from_slice(b"/usr/bin/ssh\0ssh\0");
        assert_eq!(parse_procargs(&short), None);
        assert_eq!(parse_procargs(b"\x01"), None);
    }

    #[test]
    fn an_exited_child_is_seen_before_it_is_reaped() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        assert!(!has_exited(&child).unwrap());
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGKILL) };
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !has_exited(&child).unwrap() {
            assert!(
                std::time::Instant::now() < deadline,
                "the exit was not seen"
            );
            thread::sleep(Duration::from_millis(5));
        }
        // Still unreaped: the wait collects it now.
        assert!(child.try_wait().unwrap().is_some());
    }

    #[test]
    fn a_live_process_is_described() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let proc = Proc::system();
        let stat = proc.stat(child.id()).unwrap();
        assert_ne!(stat.state, 'Z');
        assert_eq!(proc.uid(child.id()), Some(unsafe { libc::getuid() }));
        assert_eq!(
            proc.cmdline(child.id()).unwrap(),
            [b"sleep".to_vec(), b"30".to_vec()]
        );
        assert!(proc.pids().contains(&child.id()));
        let own = u32::try_from(unsafe { libc::getpid() }).unwrap();
        assert!(
            children(unsafe { libc::getpid() })
                .unwrap()
                .contains(&child.id())
        );
        assert!(proc.stat(own).unwrap().start <= stat.start);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(proc.started_at(stat.start).unwrap().abs_diff(now) < 60);
        assert!(proc.boot_id().is_some_and(|id| id.len() >= 32));
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(proc.stat(child.id()), None);
    }
}
