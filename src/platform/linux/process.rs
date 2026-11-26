//! Linux process control: pidfds pin a process's identity, and `/proc` lists children and
//! describes processes.

use super::super::unix::process::Uid;
use std::{
    fs, io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
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

/// A pidfd: signals reach the process it was opened for, never a later process with its PID.
pub(crate) struct Pin {
    pidfd: OwnedFd,
}

impl Pin {
    /// Opens a pidfd for `pid`; `ESRCH` when it is gone.
    pub(crate) fn open(pid: libc::pid_t) -> io::Result<Self> {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self {
                pidfd: unsafe { OwnedFd::from_raw_fd(fd as libc::c_int) },
            })
        }
    }

    pub(crate) fn signal(&self, signal: libc::c_int) -> io::Result<()> {
        if unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.pidfd.as_raw_fd(),
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

    /// Whether the process exits within `wait` milliseconds (or already has).
    pub(crate) fn wait_exit(&self, wait: i32) -> bool {
        let mut poll = libc::pollfd {
            fd: self.pidfd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid `pollfd` that outlives the call.
        unsafe { libc::poll(&mut poll, 1, wait) > 0 }
    }
}

/// Waits briefly for every thread of `pid` to stop. This only narrows the
/// window in which the process can fork ahead of the walk, so a process that
/// is slow to stop is still killed and its children read.
pub(crate) fn wait_until_stopped(pid: libc::pid_t) {
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

pub(crate) fn children(pid: libc::pid_t) -> io::Result<Vec<u32>> {
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
    pub fn uid(&self, pid: u32) -> Option<Uid> {
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

    pub fn pids(&self) -> Vec<u32> {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
            .collect()
    }
}

#[cfg(test)]
mod tests {
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
}
