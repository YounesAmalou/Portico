use std::{
    fs,
    io::{self, Read},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Output, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

use thiserror::Error;

/// How long a new-terminal launcher may run before its launch counts as requested.
const LAUNCH_GRACE: Duration = Duration::from_millis(500);

#[derive(Clone, Debug)]
pub struct OpenSsh {
    ssh: PathBuf,
    terminal: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedConfig {
    pub hostname: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_files: Vec<String>,
    pub proxy_jump: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionOutcome {
    Exited(ExitStatus),
    LaunchRequested,
    LaunchFailed(ExitStatus),
}

#[derive(Debug, Error)]
pub enum OpenSshError {
    #[error("could not run {program}: {source}")]
    Io {
        program: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{program} exited with {status}: {stderr}")]
    Failed {
        program: PathBuf,
        status: ExitStatus,
        stderr: String,
    },
    #[error("invalid ssh configuration output: {0}")]
    InvalidOutput(String),
    #[error("SSH operation cancelled")]
    Cancelled,
}

impl OpenSsh {
    pub fn system() -> Self {
        Self::new("ssh".into(), "xdg-terminal-exec".into())
    }

    pub fn new(ssh: PathBuf, terminal: PathBuf) -> Self {
        Self { ssh, terminal }
    }

    pub fn validate(&self, entry: &Path, aliases: &[String]) -> Result<(), OpenSshError> {
        self.validate_inner(entry, aliases, None)
    }

    pub(crate) fn validate_cancellable(
        &self,
        entry: &Path,
        aliases: &[String],
        cancelled: &AtomicBool,
    ) -> Result<(), OpenSshError> {
        self.validate_inner(entry, aliases, Some(cancelled))
    }

    fn validate_inner(
        &self,
        entry: &Path,
        aliases: &[String],
        cancelled: Option<&AtomicBool>,
    ) -> Result<(), OpenSshError> {
        if aliases.is_empty() {
            self.validate_alias(entry, "portico-validation", cancelled)?;
        } else {
            for alias in aliases {
                self.validate_alias(entry, alias, cancelled)?;
            }
        }
        Ok(())
    }

    pub fn resolve(&self, alias: &str) -> Result<ResolvedConfig, OpenSshError> {
        self.resolve_inner(alias, None)
    }

    pub(crate) fn resolve_cancellable(
        &self,
        alias: &str,
        cancelled: &AtomicBool,
    ) -> Result<ResolvedConfig, OpenSshError> {
        self.resolve_inner(alias, Some(cancelled))
    }

    fn resolve_inner(
        &self,
        alias: &str,
        cancelled: Option<&AtomicBool>,
    ) -> Result<ResolvedConfig, OpenSshError> {
        let output =
            self.checked_output(Command::new(&self.ssh).args(["-G", "--", alias]), cancelled)?;
        let text = String::from_utf8(output.stdout)
            .map_err(|error| OpenSshError::InvalidOutput(error.to_string()))?;
        let mut hostname = None;
        let mut user = None;
        let mut port = None;
        let mut identity_files = Vec::new();
        let mut proxy_jump = None;
        for line in text.lines() {
            let Some((key, value)) = line.split_once(char::is_whitespace) else {
                continue;
            };
            let value = value.trim_start();
            if key.eq_ignore_ascii_case("hostname") {
                hostname = Some(value.into());
            } else if key.eq_ignore_ascii_case("user") {
                user = Some(value.into());
            } else if key.eq_ignore_ascii_case("port") {
                port =
                    Some(value.parse().map_err(|_| {
                        OpenSshError::InvalidOutput(format!("invalid port: {value}"))
                    })?);
            } else if key.eq_ignore_ascii_case("identityfile") {
                identity_files.push(value.into());
            } else if key.eq_ignore_ascii_case("proxyjump") {
                proxy_jump = Some(value.into());
            }
        }
        Ok(ResolvedConfig {
            hostname: hostname
                .ok_or_else(|| OpenSshError::InvalidOutput("missing hostname".into()))?,
            user,
            port,
            identity_files,
            proxy_jump,
        })
    }

    pub fn connect(
        &self,
        alias: &str,
        new_terminal: bool,
    ) -> Result<ConnectionOutcome, OpenSshError> {
        if new_terminal {
            return self.launch_in_new_terminal(alias);
        }

        Ok(ConnectionOutcome::Exited(
            Command::new(&self.ssh)
                .arg("--")
                .arg(alias)
                .status()
                .map_err(|source| OpenSshError::Io {
                    program: self.ssh.clone(),
                    source,
                })?,
        ))
    }

    /// Asks `xdg-terminal-exec` to open `ssh -- <alias>` in a new terminal.
    ///
    /// The launcher's own exit status is the only signal available: it exits
    /// non-zero when it cannot find or start a terminal, but a terminal that
    /// stays open (or hands the window to a running instance later) is
    /// indistinguishable from one that is still starting. A launcher that
    /// fails within `LAUNCH_GRACE` is reported as `LaunchFailed`; anything
    /// else is `LaunchRequested`, which promises only that the request was
    /// accepted, never that the SSH session itself succeeded.
    fn launch_in_new_terminal(&self, alias: &str) -> Result<ConnectionOutcome, OpenSshError> {
        // The command follows the launcher's own arguments directly: older
        // launchers have no option parsing and would take `--` as the command.
        let mut child = Command::new(&self.terminal)
            .arg(&self.ssh)
            .arg("--")
            .arg(alias)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|source| OpenSshError::Io {
                program: self.terminal.clone(),
                source,
            })?;
        let deadline = Instant::now() + LAUNCH_GRACE;
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => {
                    return Ok(ConnectionOutcome::LaunchRequested);
                }
                Ok(Some(status)) => return Ok(ConnectionOutcome::LaunchFailed(status)),
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Ok(None) => {
                    thread::spawn(move || {
                        let _ = child.wait();
                    });
                    return Ok(ConnectionOutcome::LaunchRequested);
                }
                Err(source) => {
                    let program = self.terminal.clone();
                    terminate_process_tree(&mut child).map_err(|source| OpenSshError::Io {
                        program: program.clone(),
                        source,
                    })?;
                    return Err(OpenSshError::Io { program, source });
                }
            }
        }
    }

    pub(crate) fn connect_cancellable(
        &self,
        alias: &str,
        cancelled: &AtomicBool,
    ) -> Result<ExitStatus, OpenSshError> {
        let mut child = Command::new(&self.ssh)
            .arg("--")
            .arg(alias)
            .spawn()
            .map_err(|source| OpenSshError::Io {
                program: self.ssh.clone(),
                source,
            })?;
        loop {
            if cancelled.load(Ordering::Relaxed) {
                terminate_process_tree(&mut child).map_err(|source| OpenSshError::Io {
                    program: self.ssh.clone(),
                    source,
                })?;
                return Err(OpenSshError::Cancelled);
            }
            match child.try_wait() {
                Ok(Some(status)) => return Ok(status),
                Ok(None) => thread::sleep(Duration::from_millis(20)),
                Err(source) => {
                    terminate_process_tree(&mut child).map_err(|source| OpenSshError::Io {
                        program: self.ssh.clone(),
                        source,
                    })?;
                    return Err(OpenSshError::Io {
                        program: self.ssh.clone(),
                        source,
                    });
                }
            }
        }
    }

    fn validate_alias(
        &self,
        entry: &Path,
        alias: &str,
        cancelled: Option<&AtomicBool>,
    ) -> Result<(), OpenSshError> {
        self.checked_output(
            Command::new(&self.ssh)
                .arg("-G")
                .arg("-F")
                .arg(entry)
                .arg("--")
                .arg(alias),
            cancelled,
        )?;
        Ok(())
    }

    fn checked_output(
        &self,
        command: &mut Command,
        cancelled: Option<&AtomicBool>,
    ) -> Result<Output, OpenSshError> {
        let output = if let Some(cancelled) = cancelled {
            self.cancellable_output(command, cancelled)?
        } else {
            command.output().map_err(|source| OpenSshError::Io {
                program: self.ssh.clone(),
                source,
            })?
        };
        if output.status.success() {
            Ok(output)
        } else {
            Err(OpenSshError::Failed {
                program: self.ssh.clone(),
                status: output.status,
                stderr: String::from_utf8_lossy(&output.stderr).trim().into(),
            })
        }
    }

    fn cancellable_output(
        &self,
        command: &mut Command,
        cancelled: &AtomicBool,
    ) -> Result<Output, OpenSshError> {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        command.process_group(0);
        let mut child = command.spawn().map_err(|source| OpenSshError::Io {
            program: self.ssh.clone(),
            source,
        })?;
        let stdout = read_pipe(child.stdout.take().expect("stdout is piped"));
        let stderr = read_pipe(child.stderr.take().expect("stderr is piped"));

        let status = loop {
            if cancelled.load(Ordering::Relaxed) {
                terminate_process_group(&mut child);
                return Err(OpenSshError::Cancelled);
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => thread::sleep(Duration::from_millis(20)),
                Err(source) => {
                    terminate_process_group(&mut child);
                    return Err(OpenSshError::Io {
                        program: self.ssh.clone(),
                        source,
                    });
                }
            }
        };

        Ok(Output {
            status,
            stdout: finish_pipe_cancellable(stdout, &mut child, cancelled, &self.ssh)?,
            stderr: finish_pipe_cancellable(stderr, &mut child, cancelled, &self.ssh)?,
        })
    }
}

fn terminate_process_group(child: &mut std::process::Child) {
    kill_process_group(child);
    let _ = child.wait();
}

fn terminate_process_tree(child: &mut std::process::Child) -> io::Result<()> {
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

fn kill_direct_child(child: &mut std::process::Child) -> io::Result<()> {
    match child.kill() {
        Ok(()) => Ok(()),
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(()),
        Err(error) => Err(error),
    }
}

fn reap_child_bounded(child: &mut std::process::Child) -> io::Result<()> {
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
fn kill_process_tree(root: u32) -> io::Result<()> {
    let mut first_error = None;
    let mut stopped = Vec::new();
    let mut pending = vec![(root, None)];
    while let Some((pid, parent)) = pending.pop() {
        let Some(process) = stop_process(pid, parent, &mut first_error) else {
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
fn stop_process(
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

fn open_pidfd(pid: libc::pid_t) -> io::Result<OwnedFd> {
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
}

/// Makes the pidfd open after `skipped` successful ones fail with `EMFILE`.
#[cfg(test)]
fn fail_pidfd_open_after(skipped: u32) {
    FAIL_PIDFD_OPEN.with(|countdown| countdown.set(Some(skipped)));
}

fn signal_pidfd(pidfd: &OwnedFd, signal: libc::c_int) -> io::Result<()> {
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

fn kill_process_group(child: &mut std::process::Child) {
    let Ok(pid) = libc::pid_t::try_from(child.id()) else {
        let _ = child.kill();
        return;
    };
    // The child was placed in a process group whose ID is its PID above.
    if unsafe { libc::kill(-pid, libc::SIGKILL) } != 0 {
        let _ = child.kill();
    }
}

fn read_pipe(mut pipe: impl Read + Send + 'static) -> thread::JoinHandle<io::Result<Vec<u8>>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        pipe.read_to_end(&mut bytes)?;
        Ok(bytes)
    })
}

fn finish_pipe(
    reader: thread::JoinHandle<io::Result<Vec<u8>>>,
    program: &Path,
) -> Result<Vec<u8>, OpenSshError> {
    reader
        .join()
        .map_err(|_| OpenSshError::Io {
            program: program.into(),
            source: io::Error::other("output reader panicked"),
        })?
        .map_err(|source| OpenSshError::Io {
            program: program.into(),
            source,
        })
}

fn finish_pipe_cancellable(
    reader: thread::JoinHandle<io::Result<Vec<u8>>>,
    child: &mut std::process::Child,
    cancelled: &AtomicBool,
    program: &Path,
) -> Result<Vec<u8>, OpenSshError> {
    while !reader.is_finished() {
        if cancelled.load(Ordering::Relaxed) {
            terminate_process_group(child);
            return Err(OpenSshError::Cancelled);
        }
        thread::sleep(Duration::from_millis(20));
    }
    match finish_pipe(reader, program) {
        Ok(output) => Ok(output),
        Err(error) => {
            terminate_process_group(child);
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::Arc,
    };

    use tempfile::TempDir;

    use super::*;
    use crate::test_executable::write_executable;

    struct FakeProgram {
        _dir: TempDir,
        path: PathBuf,
        log: PathBuf,
    }

    impl FakeProgram {
        fn path(&self) -> PathBuf {
            self.path.clone()
        }

        fn log(&self) -> &Path {
            &self.log
        }
    }

    fn fake_ssh() -> FakeProgram {
        fake_program("", true)
    }

    fn fake_resolver(output: &str) -> FakeProgram {
        fake_program(output, false)
    }

    fn fake_failure(message: &str) -> FakeProgram {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ssh");
        let log = dir.path().join("args");
        write_executable(
            &path,
            &format!(
                "#!/bin/sh\nprintf '%s' '{}' >&2\nexit 23\n",
                message.replace('\'', "'\\''")
            ),
        );
        FakeProgram {
            _dir: dir,
            path,
            log,
        }
    }

    fn fake_program(output: &str, record: bool) -> FakeProgram {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ssh");
        let log = dir.path().join("args");
        let body = if record {
            format!("printf '%s\\n' \"$@\" > '{}'", log.display())
        } else {
            format!(
                "printf '%s\\n' \"$@\" > '{}'\nif [ \"$1\" = '-G' ]; then printf '%s' '{}'; fi",
                log.display(),
                output.replace('\'', "'\\''")
            )
        };
        write_executable(&path, &format!("#!/bin/sh\n{body}\n"));
        FakeProgram {
            _dir: dir,
            path,
            log,
        }
    }

    fn wait_for_contents(path: &Path, expected: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if fs::read_to_string(path).is_ok_and(|contents| contents == expected) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "missing output at {}",
                path.display()
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn passes_alias_as_one_argument_without_a_shell() {
        let fake = fake_ssh();
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
        let outcome = openssh.connect("-external", false).unwrap();

        assert!(matches!(
            outcome,
            ConnectionOutcome::Exited(status) if status.success()
        ));
        assert_eq!(fs::read_to_string(fake.log()).unwrap(), "--\n-external\n");
    }

    #[test]
    fn parses_effective_configuration() {
        let fake = fake_resolver(
            "hostname dev.example\nuser deploy\nport 2222\nidentityfile ~/.ssh/id_dev\nproxyjump bastion\n",
        );
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
        let resolved = openssh.resolve("-external").unwrap();

        assert_eq!(
            fs::read_to_string(fake.log()).unwrap(),
            "-G\n--\n-external\n"
        );
        assert_eq!(resolved.hostname, "dev.example");
        assert_eq!(resolved.user.as_deref(), Some("deploy"));
        assert_eq!(resolved.port, Some(2222));
        assert_eq!(resolved.identity_files, ["~/.ssh/id_dev"]);
        assert_eq!(resolved.proxy_jump.as_deref(), Some("bastion"));
    }

    #[test]
    fn validates_each_alias_against_the_candidate_entry() {
        let fake = fake_ssh();
        let entry = fake.log().with_file_name("entry config");
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());

        openssh.validate(&entry, &["-external".into()]).unwrap();

        assert_eq!(
            fs::read_to_string(fake.log()).unwrap(),
            format!("-G\n-F\n{}\n--\n-external\n", entry.display())
        );
    }

    #[test]
    fn passes_terminal_arguments_without_a_shell() {
        let ssh = fake_ssh();
        let terminal = fake_ssh();
        let openssh = OpenSsh::new(ssh.path(), terminal.path());

        assert_eq!(
            openssh.connect("-external", true).unwrap(),
            ConnectionOutcome::LaunchRequested
        );
        wait_for_contents(
            terminal.log(),
            &format!("{}\n--\n-external\n", ssh.path().display()),
        );
    }

    #[test]
    fn early_launcher_failure_is_reported() {
        let ssh = fake_ssh();
        let terminal = fake_ssh();
        write_executable(&terminal.path, "#!/bin/sh\nexit 7\n");
        let openssh = OpenSsh::new(ssh.path(), terminal.path());

        assert!(matches!(
            openssh.connect("dev", true).unwrap(),
            ConnectionOutcome::LaunchFailed(status) if status.code() == Some(7)
        ));
    }

    #[test]
    fn missing_launcher_is_an_error() {
        let openssh = OpenSsh::new("ssh".into(), "/nonexistent/xdg-terminal-exec".into());

        assert!(matches!(
            openssh.connect("dev", true),
            Err(OpenSshError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound
        ));
    }

    #[test]
    fn new_terminal_connection_returns_after_launch() {
        let ssh = fake_ssh();
        let terminal = fake_ssh();
        let release = terminal.log.with_file_name("release");
        let done = terminal.log.with_file_name("done");
        write_executable(
            &terminal.path,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\ni=0\nwhile [ ! -f '{}' ] && [ \"$i\" -lt 200 ]; do i=$((i + 1)); sleep 0.01; done\n: > '{}'\n",
                terminal.log.display(),
                release.display(),
                done.display(),
            ),
        );
        let openssh = OpenSsh::new(ssh.path(), terminal.path());
        let started = std::time::Instant::now();

        assert_eq!(
            openssh.connect("dev", true).unwrap(),
            ConnectionOutcome::LaunchRequested
        );

        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "new-terminal launch blocked for {:?}",
            started.elapsed()
        );
        wait_for_contents(
            terminal.log(),
            &format!("{}\n--\ndev\n", ssh.path().display()),
        );
        fs::write(release, "").unwrap();
        wait_for_contents(&done, "");
    }

    #[test]
    fn cancellation_stops_descendant_holding_pipes_after_resolver_exits() {
        let fake = fake_ssh();
        let marker = fake.log.with_file_name("parent-exited");
        write_executable(
            &fake.path,
            &format!(
                "#!/bin/sh\nif [ \"$1\" = '-G' ]; then\n  sleep 3 &\n  printf 'hostname dev.example\\n'\n  : > '{}'\nfi\n",
                marker.display(),
            ),
        );
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
        let cancelled = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&cancelled);
        let cancel = thread::spawn(move || {
            wait_for_contents(&marker, "");
            thread::sleep(Duration::from_millis(100));
            signal.store(true, Ordering::Relaxed);
        });
        let started = std::time::Instant::now();

        let result = openssh.resolve_cancellable("dev", &cancelled);

        cancel.join().unwrap();
        assert!(matches!(result, Err(OpenSshError::Cancelled)));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "cancellation blocked for {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn cancellation_without_a_pidfd_still_kills_and_reports_cancelled() {
        let fake = fake_ssh();
        let marker = fake.log.with_file_name("connection-started");
        write_executable(
            &fake.path,
            &format!(
                "#!/bin/sh\nprintf '%s' \"$$\" > '{}'\nsleep 2\n",
                marker.display()
            ),
        );
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
        let cancelled = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&cancelled);
        let cancel = thread::spawn({
            let marker = marker.clone();
            move || {
                let deadline = Instant::now() + Duration::from_secs(1);
                while !fs::read_to_string(&marker).is_ok_and(|pid| !pid.is_empty()) {
                    assert!(Instant::now() < deadline, "connection did not start");
                    thread::sleep(Duration::from_millis(5));
                }
                signal.store(true, Ordering::Relaxed);
            }
        });
        fail_pidfd_open_after(0);
        let started = Instant::now();

        let result = openssh.connect_cancellable("dev", &cancelled);

        cancel.join().unwrap();
        let pid = fs::read_to_string(marker).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "cancellation blocked for {:?}",
            started.elapsed()
        );
        assert!(matches!(result, Err(OpenSshError::Cancelled)));
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
    }

    #[test]
    fn cancellation_without_a_descendant_pidfd_still_kills_the_whole_tree() {
        let fake = fake_ssh();
        let marker = fake.log.with_file_name("tree-started");
        write_executable(
            &fake.path,
            &format!(
                "#!/bin/sh\nsleep 30 &\nfirst=$!\nsleep 30 &\nprintf '%s %s' \"$first\" \"$!\" > '{}.tmp'\nmv '{0}.tmp' '{0}'\nwait\n",
                marker.display()
            ),
        );
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
        let cancelled = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&cancelled);
        let cancel = thread::spawn({
            let marker = marker.clone();
            move || {
                let deadline = Instant::now() + Duration::from_secs(2);
                while !marker.exists() {
                    assert!(Instant::now() < deadline, "connection did not start");
                    thread::sleep(Duration::from_millis(5));
                }
                signal.store(true, Ordering::Relaxed);
            }
        });
        // The root's descriptor opens; the first descendant's does not.
        fail_pidfd_open_after(1);

        let result = openssh.connect_cancellable("dev", &cancelled);

        cancel.join().unwrap();
        assert!(matches!(result, Err(OpenSshError::Cancelled)));
        for pid in fs::read_to_string(marker).unwrap().split_whitespace() {
            let state = fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| stat.rsplit(')').next().map(|rest| rest.trim_start().into()));
            assert!(
                state.is_none_or(|state: String| state.starts_with('Z')),
                "descendant {pid} survived"
            );
        }
    }

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
    fn reports_resolver_stderr() {
        let fake = fake_failure("bad option");
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());

        assert!(
            openssh
                .resolve("dev-vps")
                .unwrap_err()
                .to_string()
                .contains("bad option")
        );
    }
}
