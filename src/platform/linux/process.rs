//! Linux process control: killing process trees through pidfds, reading `/proc`, and stopping a
//! recognised process.

use std::{
    ffi::OsStr,
    fs, io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::ffi::OsStrExt,
    },
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

/// Whether `child` has exited, leaving it unreaped so its PID, and so its
/// process group, cannot be reused before the group is killed.
pub fn has_exited(child: &std::process::Child) -> io::Result<bool> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::waitid(
            libc::P_PID,
            child.id(),
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { info.si_pid() } != 0)
}

pub fn terminate_process_group(child: &mut std::process::Child) {
    kill_process_group(child);
    let _ = child.wait();
}

pub fn terminate_process_tree(child: &mut std::process::Child) -> io::Result<()> {
    if let Err(error) = kill_process_tree(child.id()) {
        let _ = kill_direct_child(child);
        let _ = reap_child_bounded(child);
        return Err(error);
    }
    match reap_child_bounded(child) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = kill_direct_child(child);
            let _ = reap_child_bounded(child);
            Err(error)
        }
    }
}

pub fn kill_direct_child(child: &mut std::process::Child) -> io::Result<()> {
    match child.kill() {
        Ok(()) => Ok(()),
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(()),
        Err(error) => Err(error),
    }
}

pub fn reap_child_bounded(child: &mut std::process::Child) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_millis(250);
    loop {
        match child.try_wait()? {
            Some(_) => return Ok(()),
            None if Instant::now() >= deadline => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out reaping child process",
                ));
            }
            None => thread::sleep(Duration::from_millis(5)),
        }
    }
}

/// Stops, then kills, `root` and all of its descendants.
///
/// Every process is stopped before its children are read so it cannot fork
/// faster than the walk, and nothing is killed until the whole tree has been
/// collected. Failures never abandon the walk: each process that could be
/// reached is still killed, and the first error is reported afterwards. Only
/// failures that can leave a process alive are errors: a slow stop or a
/// missing pidfd does not prevent the kill, so neither is reported.
pub fn kill_process_tree(root: u32) -> io::Result<()> {
    let mut first_error = None;
    let mut stopped = Vec::new();
    let mut pending = vec![(root, None)];
    while let Some((pid, parent)) = pending.pop() {
        let Some(process) = stop_tree_member(pid, parent, &mut first_error) else {
            continue;
        };
        let children = note(&mut first_error, process_children(process.pid));
        pending.extend(
            children
                .into_iter()
                .flatten()
                .map(|child| (child, Some(process.pid))),
        );
        stopped.push(process);
    }
    for process in &stopped {
        match process.signal(libc::SIGKILL) {
            Err(error) if error.raw_os_error() != Some(libc::ESRCH) => {
                first_error.get_or_insert(error);
            }
            _ => {}
        }
    }
    first_error.map_or(Ok(()), Err)
}

/// Records the error of a best-effort step and yields its value on success.
fn note<T>(first_error: &mut Option<io::Error>, result: io::Result<T>) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(error) => {
            first_error.get_or_insert(error);
            None
        }
    }
}

struct Process {
    pid: libc::pid_t,
    /// Pins the identity of the process; absent when no descriptor was available.
    pidfd: Option<OwnedFd>,
}

impl Process {
    fn signal(&self, signal: libc::c_int) -> io::Result<()> {
        match &self.pidfd {
            Some(pidfd) => signal_pidfd(pidfd, signal),
            // Without a descriptor the PID is still safe to signal: the
            // process is `root`, which the caller has not reaped, or a child
            // of a stopped process that therefore cannot have reaped it.
            None if unsafe { libc::kill(self.pid, signal) } == 0 => Ok(()),
            None => Err(io::Error::last_os_error()),
        }
    }
}

/// Stops `pid` and returns a handle to kill it, or `None` when it is gone or
/// (for a descendant) no longer a child of `parent`. Errors that can leave
/// the process alive are recorded.
fn stop_tree_member(
    pid: u32,
    parent: Option<libc::pid_t>,
    first_error: &mut Option<io::Error>,
) -> Option<Process> {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        first_error.get_or_insert(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid process ID",
        ));
        return None;
    };
    let pidfd = match open_pidfd(pid) {
        Ok(pidfd) => Some(pidfd),
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => return None,
        Err(_) => None,
    };
    let process = Process { pid, pidfd };
    if let Some(parent) = parent
        && !note(first_error, process_children(parent))?.contains(&(pid as u32))
    {
        return None;
    }
    match process.signal(libc::SIGSTOP) {
        Ok(()) => {}
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => return None,
        Err(error) => {
            first_error.get_or_insert(error);
            return None;
        }
    }
    wait_until_stopped(pid);
    Some(process)
}

pub fn open_pidfd(pid: libc::pid_t) -> io::Result<OwnedFd> {
    #[cfg(test)]
    if FAIL_PIDFD_OPEN.with(|countdown| match countdown.get() {
        Some(0) => {
            countdown.set(None);
            true
        }
        Some(remaining) => {
            countdown.set(Some(remaining - 1));
            false
        }
        None => false,
    }) {
        PIDFD_OPEN_FAILED.with(|failed| failed.set(true));
        return Err(io::Error::from_raw_os_error(libc::EMFILE));
    }
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(fd as libc::c_int) })
    }
}

#[cfg(test)]
thread_local! {
    static FAIL_PIDFD_OPEN: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
    static PIDFD_OPEN_FAILED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Makes the pidfd open after `skipped` successful ones fail with `EMFILE`.
#[cfg(test)]
pub fn fail_pidfd_open_after(skipped: u32) {
    PIDFD_OPEN_FAILED.with(|failed| failed.set(false));
    FAIL_PIDFD_OPEN.with(|countdown| countdown.set(Some(skipped)));
}

/// Whether the failure `fail_pidfd_open_after` armed has been injected, so a test can prove it
/// exercised the path without a pidfd rather than passing through the normal one.
#[cfg(test)]
pub fn pidfd_open_failure_injected() -> bool {
    PIDFD_OPEN_FAILED.with(std::cell::Cell::get)
}

pub fn signal_pidfd(pidfd: &OwnedFd, signal: libc::c_int) -> io::Result<()> {
    if unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Waits briefly for every thread of `pid` to stop. This only narrows the
/// window in which the process can fork ahead of the walk, so a process that
/// is slow to stop is still killed and its children read.
fn wait_until_stopped(pid: libc::pid_t) {
    let deadline = Instant::now() + Duration::from_millis(100);
    loop {
        let Ok(tasks) = fs::read_dir(format!("/proc/{pid}/task")) else {
            return;
        };
        let stopped = tasks
            .flatten()
            .filter(|task| task.file_name().to_string_lossy().parse::<u32>().is_ok())
            .all(|task| {
                let state = fs::read_to_string(task.path().join("status"))
                    .ok()
                    .and_then(|status| {
                        status
                            .lines()
                            .find_map(|line| line.strip_prefix("State:").map(str::trim_start))
                            .and_then(|state| state.chars().next())
                    });
                matches!(state, Some('T' | 't' | 'Z' | 'X'))
            });
        if stopped || Instant::now() >= deadline {
            return;
        }
        thread::sleep(Duration::from_millis(1));
    }
}

fn process_children(pid: libc::pid_t) -> io::Result<Vec<u32>> {
    process_children_in(Path::new("/proc"), pid)
}

/// Reads the children of `pid` from the `proc` filesystem mounted at `proc`.
///
/// The per-task `children` files are exact but exist only on kernels built
/// with `CONFIG_PROC_CHILDREN`. A task directory without one means the kernel
/// lacks it (or the task just exited), so the parents of all processes are
/// scanned instead: reporting no children would silently miss descendants.
fn process_children_in(proc: &Path, pid: libc::pid_t) -> io::Result<Vec<u32>> {
    let task_dir = proc.join(pid.to_string()).join("task");
    let tasks = match fs::read_dir(&task_dir) {
        Ok(tasks) => tasks,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut children = Vec::new();
    for task in tasks {
        let task = task?;
        match fs::read_to_string(task.path().join("children")) {
            Ok(contents) => children.extend(
                contents
                    .split_whitespace()
                    .filter_map(|child| child.parse::<u32>().ok()),
            ),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if task.path().exists() {
                    return children_by_parent_scan(proc, pid);
                }
            }
            Err(error) => return Err(error),
        }
    }
    children.sort_unstable();
    children.dedup();
    Ok(children)
}

fn children_by_parent_scan(proc: &Path, parent: libc::pid_t) -> io::Result<Vec<u32>> {
    let mut children = Vec::new();
    for entry in fs::read_dir(proc)? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let stat = match fs::read_to_string(entry.path().join("stat")) {
            Ok(stat) => stat,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        // The command name may contain spaces and parentheses; the fields
        // after the last `)` are the state and then the parent PID.
        let ppid = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(1))
            .and_then(|ppid| ppid.parse::<libc::pid_t>().ok());
        if ppid == Some(parent) {
            children.push(pid);
        }
    }
    children.sort_unstable();
    Ok(children)
}

pub fn kill_process_group(child: &mut std::process::Child) {
    let Ok(pid) = libc::pid_t::try_from(child.id()) else {
        let _ = child.kill();
        return;
    };
    // The child was placed in a process group whose ID is its PID above.
    if unsafe { libc::kill(-pid, libc::SIGKILL) } != 0 {
        let _ = child.kill();
    }
}

/// A process as `/proc/<pid>/stat` describes it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Stat {
    pub start: u64,
    pub state: char,
    /// The session ID (field 6). `ssh -f` calls `setsid`, so a backgrounded tunnel leads its
    /// own session, while the foreground ssh that is still connecting does not.
    pub session: u32,
}

/// Reads processes from a `proc` filesystem, normally `/proc`; tests pass a fake one.
#[derive(Clone, Debug)]
pub struct Proc {
    root: PathBuf,
}

impl Proc {
    pub fn system() -> Self {
        Self::at("/proc")
    }

    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn boot_id(&self) -> Option<String> {
        let id = fs::read_to_string(self.root.join("sys/kernel/random/boot_id")).ok()?;
        Some(id.trim().to_owned()).filter(|id| !id.is_empty())
    }

    /// Boot time in Unix seconds (`btime` in `/proc/stat`).
    fn boot_time(&self) -> Option<u64> {
        fs::read_to_string(self.root.join("stat"))
            .ok()?
            .lines()
            .find_map(|line| line.strip_prefix("btime "))
            .and_then(|value| value.trim().parse().ok())
    }

    /// Unix seconds at which a process with `start` (clock ticks after boot) started.
    pub fn started_at(&self, start: u64) -> Option<u64> {
        // SAFETY: `sysconf` has no preconditions.
        let ticks = u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) })
            .ok()
            .filter(|ticks| *ticks > 0)?;
        Some(self.boot_time()? + start / ticks)
    }

    pub fn stat(&self, pid: u32) -> Option<Stat> {
        let text = fs::read_to_string(self.root.join(format!("{pid}/stat"))).ok()?;
        // The command name is in parentheses and may contain anything, so read after the last.
        let rest = &text[text.rfind(')')? + 1..];
        let mut fields = rest.split_whitespace();
        let state = fields.next()?.chars().next()?;
        // `rest` starts at field 3 (state); the session is field 6 and the start time field 22.
        let session = fields.nth(2)?.parse().ok()?;
        let start = fields.nth(15)?.parse().ok()?;
        Some(Stat {
            start,
            state,
            session,
        })
    }

    /// The real UID of `pid`.
    pub fn uid(&self, pid: u32) -> Option<u32> {
        fs::read_to_string(self.root.join(format!("{pid}/status")))
            .ok()?
            .lines()
            .find_map(|line| line.strip_prefix("Uid:"))
            .and_then(|ids| ids.split_whitespace().next())
            .and_then(|uid| uid.parse().ok())
    }

    pub fn cmdline(&self, pid: u32) -> Option<Vec<Vec<u8>>> {
        let bytes = fs::read(self.root.join(format!("{pid}/cmdline"))).ok()?;
        let mut args: Vec<Vec<u8>> = bytes.split(|byte| *byte == 0).map(<[u8]>::to_vec).collect();
        if args.last().is_some_and(Vec::is_empty) {
            args.pop();
        }
        Some(args)
    }

    fn pids(&self) -> Vec<u32> {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
            .collect()
    }
}

impl Proc {
    /// Every live process of `uid` that leads its own session (as `ssh -f` does once it has put
    /// itself in the background) and whose command line has `-E <path>`: what a status listing
    /// searches for tunnels without a record, read once however many tunnels it lists.
    pub fn scan(&self, uid: u32) -> Vec<Logged> {
        self.pids()
            .into_iter()
            .filter_map(|pid| {
                // The command line rejects nearly every process, so it is read first.
                let args = self.cmdline(pid)?;
                let log = args
                    .windows(2)
                    .find(|pair| pair[0] == b"-E")
                    .map(|pair| PathBuf::from(OsStr::from_bytes(&pair[1])))?;
                let stat = self.stat(pid)?;
                // Only a backgrounded ssh counts: a foreground one may still fail to connect.
                (!matches!(stat.state, 'Z' | 'X' | 'x')
                    && stat.session == pid
                    && self.uid(pid) == Some(uid))
                .then_some(Logged {
                    pid,
                    start: stat.start,
                    log,
                })
            })
            .collect()
    }
}

/// A process that logs to a file with `-E`, as every started tunnel does.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Logged {
    pub pid: u32,
    pub start: u64,
    pub log: PathBuf,
}

/// The current user's real UID.
pub fn current_uid() -> u32 {
    // SAFETY: `getuid` has no preconditions.
    unsafe { libc::getuid() }
}

/// How long a tunnel has to exit after SIGTERM before it is killed.
pub const TERM_GRACE_MS: i32 = 3000;

/// Stops `pid` if `verify` still recognises it once a pidfd pins it. Returns whether a signal
/// was sent. Because the pidfd is opened before `verify` runs, and a PID is not reused while its
/// process lives, a process that passes `verify` is the one the pidfd refers to.
pub fn stop_process(pid: u32, verify: impl FnOnce() -> bool) -> io::Result<bool> {
    let Ok(raw) = libc::pid_t::try_from(pid) else {
        return Ok(false);
    };
    let pidfd = match open_pidfd(raw) {
        Ok(pidfd) => pidfd,
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => return Ok(false),
        Err(error) => return Err(error),
    };
    if !verify() {
        return Ok(false);
    }
    for (signal, wait) in [(libc::SIGTERM, TERM_GRACE_MS), (libc::SIGKILL, 2000)] {
        match signal_pidfd(&pidfd, signal) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => return Ok(true),
            Err(error) => return Err(error),
        }
        let mut poll = libc::pollfd {
            fd: pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid `pollfd` that outlives the call.
        if unsafe { libc::poll(&mut poll, 1, wait) } > 0 {
            return Ok(true);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    fn fake_proc_task(proc: &Path, pid: u32, children: Option<&str>) {
        let task = proc
            .join(pid.to_string())
            .join("task")
            .join(pid.to_string());
        fs::create_dir_all(&task).unwrap();
        if let Some(children) = children {
            fs::write(task.join("children"), children).unwrap();
        }
    }

    fn fake_proc_stat(proc: &Path, pid: u32, command: &str, parent: u32) {
        fs::create_dir_all(proc.join(pid.to_string())).unwrap();
        fs::write(
            proc.join(pid.to_string()).join("stat"),
            format!("{pid} ({command}) S {parent} {pid} {pid} 0 -1 0\n"),
        )
        .unwrap();
    }

    #[test]
    fn children_come_from_the_task_children_files() {
        let proc = tempfile::tempdir().unwrap();
        fake_proc_task(proc.path(), 100, Some("300 200\n"));

        assert_eq!(process_children_in(proc.path(), 100).unwrap(), [200, 300]);
    }

    #[test]
    fn children_are_found_by_parent_scan_without_proc_children_support() {
        let proc = tempfile::tempdir().unwrap();
        fake_proc_task(proc.path(), 100, None);
        fake_proc_stat(proc.path(), 100, "sh", 1);
        fake_proc_stat(proc.path(), 200, "a) S 1 (b", 100);
        fake_proc_stat(proc.path(), 300, "sleep", 200);
        fake_proc_stat(proc.path(), 400, "other", 100);
        fs::create_dir(proc.path().join("self")).unwrap();

        assert_eq!(process_children_in(proc.path(), 100).unwrap(), [200, 400]);
    }

    #[test]
    fn a_vanished_process_has_no_children() {
        let proc = tempfile::tempdir().unwrap();

        assert!(process_children_in(proc.path(), 100).unwrap().is_empty());
    }

    #[test]
    fn stop_process_signals_only_a_verified_process() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        assert!(!stop_process(child.id(), || false).unwrap());
        assert!(
            child.try_wait().unwrap().is_none(),
            "an unverified process was signalled"
        );
        assert!(stop_process(child.id(), || true).unwrap());
        let status = child.wait().unwrap();
        assert!(!status.success());
    }
}
