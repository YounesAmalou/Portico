//! Unix process control (Linux and macOS): process groups, reaping, killing process trees and
//! stopping a recognised process. Each system provides the handle that pins a process's
//! identity (`Pin`: a pidfd on Linux, a kqueue exit watch on macOS), how to list a process's
//! children and how to wait for it to stop.

use std::{
    ffi::OsStr,
    io,
    os::unix::ffi::OsStrExt,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use super::super::os::process::{Pin, Proc, children, wait_until_stopped};

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
/// missing pin does not prevent the kill, so neither is reported.
pub fn kill_process_tree(root: u32) -> io::Result<()> {
    let mut first_error = None;
    let mut stopped = Vec::new();
    let mut pending = vec![(root, None)];
    while let Some((pid, parent)) = pending.pop() {
        let Some(process) = stop_tree_member(pid, parent, &mut first_error) else {
            continue;
        };
        let children = note(&mut first_error, children(process.pid));
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
    /// Pins the identity of the process; absent when no handle was available.
    pin: Option<Pin>,
}

impl Process {
    fn signal(&self, signal: libc::c_int) -> io::Result<()> {
        match &self.pin {
            Some(pin) => pin.signal(signal),
            // Without a pin the PID is still safe to signal: the process is
            // `root`, which the caller has not reaped, or a child of a stopped
            // process that therefore cannot have reaped it.
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
    let pin = match open_pin(pid) {
        Ok(pin) => Some(pin),
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => return None,
        Err(_) => None,
    };
    let process = Process { pid, pin };
    if let Some(parent) = parent
        && !note(first_error, children(parent))?.contains(&(pid as u32))
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

/// Pins the identity of `pid`; `ESRCH` when it is gone.
fn open_pin(pid: libc::pid_t) -> io::Result<Pin> {
    #[cfg(test)]
    if FAIL_PIN_OPEN.with(|countdown| match countdown.get() {
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
        PIN_OPEN_FAILED.with(|failed| failed.set(true));
        return Err(io::Error::from_raw_os_error(libc::EMFILE));
    }
    Pin::open(pid)
}

#[cfg(test)]
thread_local! {
    static FAIL_PIN_OPEN: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
    static PIN_OPEN_FAILED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Makes the pin open after `skipped` successful ones fail with `EMFILE`.
#[cfg(test)]
pub fn fail_pin_after(skipped: u32) {
    PIN_OPEN_FAILED.with(|failed| failed.set(false));
    FAIL_PIN_OPEN.with(|countdown| countdown.set(Some(skipped)));
}

/// Whether the failure `fail_pin_after` armed has been injected, so a test can prove it
/// exercised the path without a pin rather than passing through the normal one.
#[cfg(test)]
pub fn pin_failure_injected() -> bool {
    PIN_OPEN_FAILED.with(std::cell::Cell::get)
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

/// Stops `pid` if `verify` still recognises it once a pin holds it. Returns whether a signal
/// was sent. Because the pin is taken before `verify` runs, and a pinned process is never
/// signalled once it has exited, a process that passes `verify` is the one the pin refers to.
pub fn stop_process(pid: u32, verify: impl FnOnce() -> bool) -> io::Result<bool> {
    let Ok(raw) = libc::pid_t::try_from(pid) else {
        return Ok(false);
    };
    let pin = match open_pin(raw) {
        Ok(pin) => pin,
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => return Ok(false),
        Err(error) => return Err(error),
    };
    if !verify() {
        return Ok(false);
    }
    for (signal, wait) in [(libc::SIGTERM, TERM_GRACE_MS), (libc::SIGKILL, 2000)] {
        match pin.signal(signal) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => return Ok(true),
            Err(error) => return Err(error),
        }
        if pin.wait_exit(wait) {
            return Ok(true);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

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

    #[test]
    fn a_pin_never_signals_an_exited_process() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = libc::pid_t::try_from(child.id()).unwrap();
        let pin = Pin::open(pid).unwrap();
        assert!(!pin.wait_exit(0), "a running process counted as exited");
        child.kill().unwrap();
        child.wait().unwrap();
        // Reaped, the PID is free for another process; the pin still refers to the old one.
        assert!(pin.wait_exit(2000), "the exit was not seen");
        let error = pin.signal(libc::SIGTERM).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::ESRCH));
    }

    #[test]
    fn a_process_tree_is_killed() {
        // The shell forks a `sleep` and waits for it, so the tree has two levels.
        let mut child = Command::new("sh")
            .args(["-c", "sleep 30 & echo $! ; wait"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(child.stdout.as_mut().unwrap()),
            &mut line,
        )
        .unwrap();
        let grandchild: libc::pid_t = line.trim().parse().unwrap();
        terminate_process_tree(&mut child).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        // The orphaned `sleep` is left a zombie where nothing reaps orphans (a container's
        // PID 1), so only a running one counts.
        let grandchild = u32::try_from(grandchild).unwrap();
        while Proc::system()
            .stat(grandchild)
            .is_some_and(|stat| stat.state != 'Z')
        {
            assert!(Instant::now() < deadline, "the grandchild survived");
            thread::sleep(Duration::from_millis(10));
        }
    }
}
