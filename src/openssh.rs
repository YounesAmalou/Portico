use std::{
    collections::VecDeque,
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    os::unix::fs::OpenOptionsExt,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Output, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use thiserror::Error;

use crate::explain::Explanation;
use crate::managed::write_unique;
use crate::probe::{
    Authenticated, Failure, ProbeResult, Route, TEST_DEADLINE, TEST_STDERR_LIMIT, authenticated,
    classify, diagnose, fetch_key_args, test_args,
};

/// How long a new-terminal launcher may run before its launch counts as requested.
const LAUNCH_GRACE: Duration = Duration::from_millis(500);

#[derive(Clone, Debug)]
pub struct OpenSsh {
    ssh: PathBuf,
    terminal: PathBuf,
    keygen: PathBuf,
    /// Portico's own executable, run in a new terminal so a failure can be explained there.
    portico: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedConfig {
    pub hostname: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_files: Vec<String>,
    pub proxy_jump: Option<String>,
    pub proxy_command: Option<String>,
    /// `ProxyUseFdpass`: the `ProxyCommand` hands ssh its connected socket.
    pub proxy_use_fdpass: bool,
    pub known_hosts_files: Vec<String>,
}

impl ResolvedConfig {
    /// How ssh reaches the host: through its jump host or proxy command, if either is set.
    pub fn route(&self) -> Route<'_> {
        fn set(value: &Option<String>) -> Option<&str> {
            value
                .as_deref()
                .filter(|value| !value.eq_ignore_ascii_case("none"))
        }
        let jump = set(&self.proxy_jump);
        if jump.is_none() && set(&self.proxy_command).is_none() {
            return Route::Direct;
        }
        Route::Proxied {
            jump,
            hostname: &self.hostname,
            port: self.port.unwrap_or(22),
            fd_passing: self.proxy_use_fdpass,
        }
    }
}

/// One line of `ssh-keygen -l`: `<bits> <fingerprint> <host> (<TYPE>)`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyFingerprint {
    pub host: String,
    pub fingerprint: String,
    pub key_type: String,
}

impl KeyFingerprint {
    /// The line that shows this key to the user. A hashed known-hosts name is unreadable, so the
    /// tested `alias` is named in its place.
    pub fn describe(&self, alias: &str) -> String {
        let host = if self.host.starts_with("|1|") {
            alias
        } else {
            &self.host
        };
        format!(
            "{host} presents an {} key with fingerprint {}",
            self.key_type, self.fingerprint
        )
    }
}

/// The known-hosts lines a server presented, with their fingerprints.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PresentedKey {
    pub lines: Vec<u8>,
    pub fingerprints: Vec<KeyFingerprint>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KeyFetch {
    Presented(PresentedKey),
    Failed(Failure),
}

/// How a watched ssh run ended.
enum Watched {
    Authenticated(Authenticated),
    Ended {
        stderr: String,
        deadline_reached: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConnectionOutcome {
    /// ssh ran in this terminal and exited; a failure Portico recognised is explained.
    Exited {
        /// The alias as the inventory spells it, which the explanation uses too.
        alias: String,
        status: ExitStatus,
        explanation: Option<Explanation>,
    },
    LaunchRequested,
    LaunchFailed(ExitStatus),
}

/// The most of a connection's stderr kept for diagnosis, counted from the end.
pub const CONNECT_TAIL_LIMIT: usize = 64 * 1024;

/// How long the stderr copier may keep running after ssh exits: anything ssh wrote is already in
/// the pipe, so only a descendant still holding the pipe open makes it wait this long.
const MIRROR_DRAIN: Duration = Duration::from_secs(1);

/// A connection whose stderr was mirrored: ssh's exit status and the newest bytes it wrote there.
#[derive(Debug)]
pub struct Mirrored {
    pub status: ExitStatus,
    pub tail: Vec<u8>,
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
        Self::new("ssh".into(), "xdg-terminal-exec".into()).with_portico(
            // A replaced binary reports a path that no longer exists.
            std::env::current_exe().ok().filter(|path| path.exists()),
        )
    }

    pub fn new(ssh: PathBuf, terminal: PathBuf) -> Self {
        Self {
            ssh,
            terminal,
            keygen: "ssh-keygen".into(),
            portico: None,
        }
    }

    pub fn with_portico(self, portico: Option<PathBuf>) -> Self {
        Self { portico, ..self }
    }

    pub fn with_keygen(self, keygen: PathBuf) -> Self {
        Self { keygen, ..self }
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
        self.resolve_with(None, alias, cancelled)
    }

    /// Resolves `alias` with `ssh -G`, reading `entry` instead of the user's
    /// configuration when given.
    pub(crate) fn resolve_with(
        &self,
        entry: Option<&Path>,
        alias: &str,
        cancelled: Option<&AtomicBool>,
    ) -> Result<ResolvedConfig, OpenSshError> {
        let mut command = Command::new(&self.ssh);
        command.arg("-G");
        if let Some(entry) = entry {
            command.arg("-F").arg(entry);
        }
        let output = self.checked_output(command.arg("--").arg(alias), cancelled)?;
        let text = String::from_utf8(output.stdout)
            .map_err(|error| OpenSshError::InvalidOutput(error.to_string()))?;
        let mut hostname = None;
        let mut user = None;
        let mut port = None;
        let mut identity_files = Vec::new();
        let mut proxy_jump = None;
        let mut proxy_command = None;
        let mut proxy_use_fdpass = false;
        let mut known_hosts_files = Vec::new();
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
            } else if key.eq_ignore_ascii_case("proxycommand") {
                proxy_command = Some(value.into());
            } else if key.eq_ignore_ascii_case("proxyusefdpass") {
                proxy_use_fdpass = value.eq_ignore_ascii_case("yes");
            } else if key.eq_ignore_ascii_case("userknownhostsfile") {
                known_hosts_files = value.split_whitespace().map(String::from).collect();
            }
        }
        Ok(ResolvedConfig {
            hostname: hostname
                .ok_or_else(|| OpenSshError::InvalidOutput("missing hostname".into()))?,
            user,
            port,
            identity_files,
            proxy_jump,
            proxy_command,
            proxy_use_fdpass,
            known_hosts_files,
        })
    }

    /// Tries to log in to `alias` without a terminal, stopping as soon as
    /// OpenSSH reports success; a failure comes with ssh's own lines. `route` says whether the alias goes through a
    /// jump host or proxy, which decides the success line to wait for and lets
    /// a failure at the jump host be told apart from one at the target.
    pub(crate) fn probe(
        &self,
        entry: Option<&Path>,
        alias: &str,
        route: Route<'_>,
        cancelled: &AtomicBool,
    ) -> Result<(ProbeResult, Vec<String>), OpenSshError> {
        self.probe_with_deadline(entry, alias, route, cancelled, TEST_DEADLINE)
    }

    fn probe_with_deadline(
        &self,
        entry: Option<&Path>,
        alias: &str,
        route: Route<'_>,
        cancelled: &AtomicBool,
        deadline: Duration,
    ) -> Result<(ProbeResult, Vec<String>), OpenSshError> {
        Ok(
            match self.watch_ssh(test_args(entry, alias), route, cancelled, deadline)? {
                Watched::Authenticated(success) => {
                    (ProbeResult::Authenticated(success), Vec::new())
                }
                Watched::Ended {
                    stderr,
                    deadline_reached,
                } => {
                    let diagnosis = diagnose(&stderr, deadline_reached, route);
                    (ProbeResult::Failed(diagnosis.failure), diagnosis.raw)
                }
            },
        )
    }

    /// Connects to `alias` with every login method disabled, letting OpenSSH
    /// record the server's host key in a private temporary file, and returns
    /// that key with its fingerprints. Nothing is written to the user's own
    /// known-hosts files.
    pub(crate) fn fetch_host_key(
        &self,
        entry: Option<&Path>,
        alias: &str,
        route: Route<'_>,
        cancelled: &AtomicBool,
    ) -> Result<KeyFetch, OpenSshError> {
        let io_error = |source| OpenSshError::Io {
            program: self.ssh.clone(),
            source,
        };
        let known_hosts = write_unique(&std::env::temp_dir(), "portico-host-key", b"", 0o600)
            .map_err(io_error)?;
        let args = fetch_key_args(entry, known_hosts.path(), alias);
        // Every login method is off, so the run can only end or time out.
        let (stderr, deadline_reached) =
            match self.watch_ssh(args, route, cancelled, TEST_DEADLINE)? {
                Watched::Authenticated(_) => (String::new(), false),
                Watched::Ended {
                    stderr,
                    deadline_reached,
                } => (stderr, deadline_reached),
            };
        let lines = fs::read(known_hosts.path()).map_err(io_error)?;
        if lines.is_empty() {
            return Ok(KeyFetch::Failed(classify(
                &stderr,
                deadline_reached,
                route.jump(),
            )));
        }
        let output = self.checked_output(
            Command::new(&self.keygen)
                .arg("-l")
                .arg("-f")
                .arg(known_hosts.path()),
            Some(cancelled),
        )?;
        let fingerprints = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(parse_fingerprint)
            .collect::<Result<_, _>>()?;
        Ok(KeyFetch::Presented(PresentedKey {
            lines,
            fingerprints,
        }))
    }

    /// Runs ssh with `args` in its own process group, reading stderr line by
    /// line until a success line, its exit, `deadline` or cancellation. The
    /// whole group is killed whichever way the run ends.
    fn watch_ssh(
        &self,
        args: Vec<OsString>,
        route: Route<'_>,
        cancelled: &AtomicBool,
        deadline: Duration,
    ) -> Result<Watched, OpenSshError> {
        let mut child = Command::new(&self.ssh)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .map_err(|source| OpenSshError::Io {
                program: self.ssh.clone(),
                source,
            })?;
        let lines = read_lines(child.stderr.take().expect("stderr is piped"));
        let deadline = Instant::now() + deadline;
        let mut stderr = String::new();
        let deadline_reached = loop {
            if cancelled.load(Ordering::Relaxed) {
                terminate_process_group(&mut child);
                return Err(OpenSshError::Cancelled);
            }
            match lines.recv_timeout(Duration::from_millis(20)) {
                Ok(line) => {
                    if let Some(success) = authenticated(&line, route) {
                        terminate_process_group(&mut child);
                        return Ok(Watched::Authenticated(success));
                    }
                    push_capped(&mut stderr, &line, TEST_STDERR_LIMIT);
                    // A steady trickle of output must not hold off the deadline.
                    if Instant::now() < deadline {
                        continue;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    thread::sleep(Duration::from_millis(20));
                }
            }
            match has_exited(&child) {
                Ok(true) => break false,
                Ok(false) if Instant::now() >= deadline => break true,
                Ok(false) => {}
                Err(source) => {
                    terminate_process_group(&mut child);
                    return Err(OpenSshError::Io {
                        program: self.ssh.clone(),
                        source,
                    });
                }
            }
        };
        // Leftover processes in the group (a ProxyCommand, say) would hold
        // stderr open; once they are gone the rest of it can be read. ssh is
        // not reaped yet, so its PID still names this group.
        terminate_process_group(&mut child);
        let drain_deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if cancelled.load(Ordering::Relaxed) {
                return Err(OpenSshError::Cancelled);
            }
            match lines.recv_timeout(Duration::from_millis(20)) {
                Ok(line) => {
                    // ssh may log success just before the server drops it.
                    if let Some(success) = authenticated(&line, route) {
                        return Ok(Watched::Authenticated(success));
                    }
                    push_capped(&mut stderr, &line, TEST_STDERR_LIMIT);
                }
                Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() < drain_deadline => {}
                Err(_) => break,
            }
        }
        Ok(Watched::Ended {
            stderr,
            deadline_reached,
        })
    }

    /// Asks `xdg-terminal-exec` to open the connection in a new terminal: `portico connect
    /// --hold -- <alias>` when Portico's path is known, so a failure is explained and held on
    /// screen, otherwise `ssh -- <alias>`.
    ///
    /// The launcher's own exit status is the only signal available: it exits
    /// non-zero when it cannot find or start a terminal, but a terminal that
    /// stays open (or hands the window to a running instance later) is
    /// indistinguishable from one that is still starting. A launcher that
    /// fails within `LAUNCH_GRACE` is reported as `LaunchFailed`; anything
    /// else is `LaunchRequested`, which promises only that the request was
    /// accepted, never that the SSH session itself succeeded.
    pub fn launch_in_new_terminal(&self, alias: &str) -> Result<ConnectionOutcome, OpenSshError> {
        self.launch_new_terminal_with(
            &["connect".into(), "--hold".into(), "--".into(), alias.into()],
            &[self.ssh.clone().into(), "--".into(), alias.into()],
        )
    }

    /// Asks the terminal launcher to run `<portico> <portico_args…>` when Portico's path is
    /// known, otherwise `fallback`, with the same launch semantics as `launch_in_new_terminal`.
    pub fn launch_new_terminal_with(
        &self,
        portico_args: &[OsString],
        fallback: &[OsString],
    ) -> Result<ConnectionOutcome, OpenSshError> {
        // The command follows the launcher's own arguments directly: older
        // launchers have no option parsing and would take `--` as the command.
        let mut command = Command::new(&self.terminal);
        match &self.portico {
            Some(portico) => command.arg(portico).args(portico_args),
            None => command.args(fallback),
        };
        let mut child = command
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

    /// Runs `ssh -- <alias>` with stdin and stdout inherited and stderr piped through a copier
    /// thread that writes every chunk to `sink` as soon as it is read and keeps the newest
    /// `CONNECT_TAIL_LIMIT` bytes.
    ///
    /// Setting `cancelled` stops ssh and every process it started.
    pub(crate) fn connect_mirrored(
        &self,
        alias: &str,
        sink: Box<dyn Write + Send>,
        cancelled: Option<&AtomicBool>,
    ) -> Result<Mirrored, OpenSshError> {
        self.connect_mirrored_command(alias, None, sink, cancelled)
    }

    /// Like `connect_mirrored`, but with `Some(command)` runs `ssh -t -o RemoteCommand=none --
    /// <alias> <command>`, forcing a terminal for the remote command.
    pub(crate) fn connect_mirrored_command(
        &self,
        alias: &str,
        command: Option<&str>,
        sink: Box<dyn Write + Send>,
        cancelled: Option<&AtomicBool>,
    ) -> Result<Mirrored, OpenSshError> {
        let io_error = |source| OpenSshError::Io {
            program: self.ssh.clone(),
            source,
        };
        let mut ssh = Command::new(&self.ssh);
        if command.is_some() {
            ssh.args(["-t", "-o", "RemoteCommand=none"]);
        }
        ssh.arg("--").arg(alias);
        ssh.args(command);
        let mut child = ssh.stderr(Stdio::piped()).spawn().map_err(io_error)?;
        let copier = Copier::start(child.stderr.take().expect("stderr is piped"), sink);
        let status = loop {
            if cancelled.is_some_and(|cancelled| cancelled.load(Ordering::Relaxed)) {
                let stopped = terminate_process_tree(&mut child);
                copier.finish();
                stopped.map_err(io_error)?;
                return Err(OpenSshError::Cancelled);
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => thread::sleep(Duration::from_millis(20)),
                Err(source) => {
                    let stopped = terminate_process_tree(&mut child);
                    copier.finish();
                    stopped.map_err(io_error)?;
                    return Err(io_error(source));
                }
            }
        };
        Ok(Mirrored {
            status,
            tail: copier.finish(),
        })
    }

    /// The `ssh` program this instance runs.
    pub fn ssh_program(&self) -> &Path {
        &self.ssh
    }

    /// Runs `ssh -o RemoteCommand=none -- <alias> <command>` and returns its output whatever
    /// the exit status.
    ///
    /// With `batch`, ssh never prompts (`BatchMode=yes`, stdin closed) and `cancelled` stops it
    /// and everything it started. Without, stdin is inherited and ssh stays in Portico's
    /// process group so it can prompt on the terminal; `cancelled` is ignored then.
    pub(crate) fn run_remote(
        &self,
        alias: &str,
        command: &str,
        batch: bool,
        cancelled: Option<&AtomicBool>,
    ) -> Result<Output, OpenSshError> {
        let mut ssh = Command::new(&self.ssh);
        ssh.args(["-o", "RemoteCommand=none"]);
        if batch {
            ssh.args(["-o", "BatchMode=yes"]).stdin(Stdio::null());
        } else {
            ssh.stdin(Stdio::inherit());
        }
        ssh.arg("--").arg(alias).arg(command);
        match cancelled {
            Some(cancelled) if batch => self.cancellable_output(&mut ssh, cancelled),
            _ => ssh.output().map_err(|source| OpenSshError::Io {
                program: self.ssh.clone(),
                source,
            }),
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
        let program = PathBuf::from(command.get_program());
        let output = if let Some(cancelled) = cancelled {
            self.cancellable_output(command, cancelled)?
        } else {
            command.output().map_err(|source| OpenSshError::Io {
                program: program.clone(),
                source,
            })?
        };
        if output.status.success() {
            Ok(output)
        } else {
            Err(OpenSshError::Failed {
                program,
                status: output.status,
                stderr: String::from_utf8_lossy(&output.stderr).trim().into(),
            })
        }
    }

    /// Runs `command` in its own process group with stdout and stderr captured; setting
    /// `cancelled` stops it and everything it started.
    pub(crate) fn cancellable_output(
        &self,
        command: &mut Command,
        cancelled: &AtomicBool,
    ) -> Result<Output, OpenSshError> {
        let program = PathBuf::from(command.get_program());
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        command.process_group(0);
        let mut child = command.spawn().map_err(|source| OpenSshError::Io {
            program: program.clone(),
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
                    return Err(OpenSshError::Io { program, source });
                }
            }
        };

        Ok(Output {
            status,
            stdout: finish_pipe_cancellable(stdout, &mut child, cancelled, &program)?,
            stderr: finish_pipe_cancellable(stderr, &mut child, cancelled, &program)?,
        })
    }
}

/// Whether `child` has exited, leaving it unreaped so its PID, and so its
/// process group, cannot be reused before the group is killed.
fn has_exited(child: &std::process::Child) -> io::Result<bool> {
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

/// Copies a child's stderr to a sink chunk by chunk, keeping the newest bytes.
struct Copier {
    thread: thread::JoinHandle<()>,
    done: mpsc::Receiver<()>,
    detached: Arc<AtomicBool>,
    tail: Arc<Mutex<VecDeque<u8>>>,
}

impl Copier {
    fn start(mut source: impl Read + Send + 'static, mut sink: Box<dyn Write + Send>) -> Self {
        let (finished, done) = mpsc::channel();
        let detached = Arc::new(AtomicBool::new(false));
        let tail = Arc::new(Mutex::new(VecDeque::new()));
        let thread = thread::spawn({
            let detached = Arc::clone(&detached);
            let tail = Arc::clone(&tail);
            move || {
                let mut buffer = [0; 8 * 1024];
                loop {
                    let read = match source.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(read) => read,
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    };
                    // Once Portico has moved on, a leftover descendant's output is not ssh's.
                    if detached.load(Ordering::Relaxed) {
                        break;
                    }
                    let chunk = &buffer[..read];
                    // A sink that fails must not stop the reading, or ssh would block on a full
                    // pipe; the tail still gets every byte.
                    let _ = sink.write_all(chunk).and_then(|()| sink.flush());
                    let mut tail = tail.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    tail.extend(chunk);
                    let excess = tail.len().saturating_sub(CONNECT_TAIL_LIMIT);
                    tail.drain(..excess);
                }
                let _ = finished.send(());
            }
        });
        Self {
            thread,
            done,
            detached,
            tail,
        }
    }

    /// Waits for the copy to reach the end of the pipe, which comes when ssh and everything it
    /// started have exited; a descendant that still holds the pipe after `MIRROR_DRAIN` is left
    /// behind, and the copier stops at its next write. Returns the tail.
    fn finish(self) -> Vec<u8> {
        if self.done.recv_timeout(MIRROR_DRAIN).is_ok() {
            let _ = self.thread.join();
        } else {
            self.detached.store(true, Ordering::Relaxed);
        }
        let tail = self
            .tail
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        tail.iter().copied().collect()
    }
}

fn parse_fingerprint(line: &str) -> Result<KeyFingerprint, OpenSshError> {
    let invalid = || OpenSshError::InvalidOutput(format!("invalid ssh-keygen line: {line}"));
    let mut fields = line.split_whitespace();
    let (_bits, fingerprint) = (fields.next(), fields.next().ok_or_else(invalid)?);
    let rest: Vec<&str> = fields.collect();
    let (key_type, host) = rest.split_last().ok_or_else(invalid)?;
    let key_type = key_type
        .strip_prefix('(')
        .and_then(|key_type| key_type.strip_suffix(')'))
        .ok_or_else(invalid)?;
    if host.is_empty() {
        return Err(invalid());
    }
    Ok(KeyFingerprint {
        host: host.join(" "),
        fingerprint: fingerprint.into(),
        key_type: key_type.into(),
    })
}

/// Appends `lines` to the known-hosts file at `path`, starting on a new line
/// and creating the file private to the user when it is missing.
pub(crate) fn append_known_hosts(path: &Path, lines: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)?;
    let mut contents = Vec::with_capacity(lines.len() + 1);
    if file.metadata()?.len() > 0 {
        let mut last = [0];
        file.seek(SeekFrom::End(-1))?;
        file.read_exact(&mut last)?;
        if last != *b"\n" {
            contents.push(b'\n');
        }
    }
    contents.extend_from_slice(lines);
    file.write_all(&contents)?;
    file.sync_all()
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

/// Appends `line`, then drops whole lines from the front until at most `limit` bytes remain,
/// so a chatty ssh keeps its newest output. A single line over the limit keeps its newest
/// `limit` bytes instead.
fn push_capped(stderr: &mut String, line: &str, limit: usize) {
    stderr.push_str(line);
    stderr.push('\n');
    if let Some(excess) = stderr.len().checked_sub(limit) {
        let whole_lines = stderr.as_bytes()[excess..]
            .iter()
            .position(|&byte| byte == b'\n')
            .map(|at| excess + at + 1)
            .filter(|&cut| cut < stderr.len());
        let cut = whole_lines.unwrap_or_else(|| {
            (excess..=stderr.len())
                .find(|&at| stderr.is_char_boundary(at))
                .unwrap_or(stderr.len())
        });
        stderr.drain(..cut);
    }
}

/// Sends each line of `pipe` as it arrives; the channel closes at end of file.
fn read_lines(pipe: impl Read + Send + 'static) -> mpsc::Receiver<String> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = BufReader::new(pipe);
        let mut line = Vec::new();
        while reader
            .read_until(b'\n', &mut line)
            .is_ok_and(|read| read > 0)
        {
            let text = String::from_utf8_lossy(&line);
            if sender
                .send(text.trim_end_matches(['\n', '\r']).to_string())
                .is_err()
            {
                return;
            }
            line.clear();
        }
    });
    receiver
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
    use crate::probe::{Authenticated, KeySource, OfferedKey, test_args};
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
        let mirrored = openssh
            .connect_mirrored("-external", Box::new(io::sink()), None)
            .unwrap();

        assert!(mirrored.status.success());
        assert_eq!(fs::read_to_string(fake.log()).unwrap(), "--\n-external\n");
    }

    #[test]
    fn run_remote_passes_remote_command_none_and_batch() {
        let fake = fake_ssh();
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
        let output = openssh.run_remote("dev", "docker ps", true, None).unwrap();

        assert!(output.status.success());
        assert_eq!(
            fs::read_to_string(fake.log()).unwrap(),
            "-o\nRemoteCommand=none\n-o\nBatchMode=yes\n--\ndev\ndocker ps\n"
        );

        let cancelled = AtomicBool::new(true);
        assert!(matches!(
            openssh.run_remote("dev", "docker ps", true, Some(&cancelled)),
            Err(OpenSshError::Cancelled)
        ));
        cancelled.store(false, Ordering::Relaxed);
        openssh
            .run_remote("dev", "docker ps", true, Some(&cancelled))
            .unwrap();

        openssh.run_remote("dev", "docker ps", false, None).unwrap();
        assert_eq!(
            fs::read_to_string(fake.log()).unwrap(),
            "-o\nRemoteCommand=none\n--\ndev\ndocker ps\n"
        );

        write_executable(&fake.path, "#!/bin/sh\nprintf oops >&2\nexit 3\n");
        let output = openssh.run_remote("dev", "docker ps", true, None).unwrap();
        assert_eq!(output.status.code(), Some(3));
        assert_eq!(output.stderr, b"oops");
    }

    #[test]
    fn run_remote_inherits_stdin_unless_batch() {
        let fake = fake_ssh();
        write_executable(
            &fake.path,
            &format!(
                "#!/bin/sh\nreadlink /proc/self/fd/0 > '{}'\n",
                fake.log().display()
            ),
        );
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
        let own = fs::read_link("/proc/self/fd/0").unwrap();
        let seen = || {
            PathBuf::from(
                fs::read_to_string(fake.log())
                    .unwrap()
                    .trim_end_matches('\n'),
            )
        };

        openssh.run_remote("dev", "docker ps", false, None).unwrap();
        assert_eq!(seen(), own);

        openssh.run_remote("dev", "docker ps", true, None).unwrap();
        assert_eq!(seen(), Path::new("/dev/null"));
    }

    #[test]
    fn connect_mirrored_command_requests_a_tty() {
        let fake = fake_ssh();
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
        let mirrored = openssh
            .connect_mirrored_command(
                "dev",
                Some("docker exec -it web sh"),
                Box::new(io::sink()),
                None,
            )
            .unwrap();

        assert!(mirrored.status.success());
        assert_eq!(
            fs::read_to_string(fake.log()).unwrap(),
            "-t\n-o\nRemoteCommand=none\n--\ndev\ndocker exec -it web sh\n"
        );
    }

    #[test]
    fn new_terminal_with_custom_portico_args() {
        let ssh = fake_ssh();
        let terminal = fake_ssh();
        let openssh =
            OpenSsh::new(ssh.path(), terminal.path()).with_portico(Some("/opt/portico".into()));
        let args = ["shell".into(), "--hold".into(), "x".into()];
        let fallback = ["fallback".into(), "y".into()];

        assert_eq!(
            openssh.launch_new_terminal_with(&args, &fallback).unwrap(),
            ConnectionOutcome::LaunchRequested
        );
        wait_for_contents(terminal.log(), "/opt/portico\nshell\n--hold\nx\n");

        let terminal = fake_ssh();
        let openssh = OpenSsh::new(ssh.path(), terminal.path()).with_portico(None);
        openssh.launch_new_terminal_with(&args, &fallback).unwrap();
        wait_for_contents(terminal.log(), "fallback\ny\n");
        assert_eq!(openssh.ssh_program(), ssh.path());
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
        assert_eq!(
            resolved.route(),
            Route::Proxied {
                jump: Some("bastion"),
                hostname: "dev.example",
                port: 2222,
                fd_passing: false,
            }
        );
    }

    #[test]
    fn a_proxy_command_or_none_decides_the_route() {
        let resolve = |output: &str| {
            let fake = fake_resolver(output);
            OpenSsh::new(fake.path(), "xdg-terminal-exec".into())
                .resolve("dev")
                .unwrap()
        };
        let command = resolve("hostname h\nproxycommand nc %h %p\n");
        assert_eq!(command.proxy_command.as_deref(), Some("nc %h %p"));
        assert_eq!(
            command.route(),
            Route::Proxied {
                jump: None,
                hostname: "h",
                port: 22,
                fd_passing: false,
            }
        );
        let fd_passing = resolve("hostname h\nproxycommand nc %h %p\nproxyusefdpass yes\n");
        assert_eq!(
            fd_passing.route(),
            Route::Proxied {
                jump: None,
                hostname: "h",
                port: 22,
                fd_passing: true,
            }
        );
        let none = resolve("hostname h\nport 2200\nproxyjump none\nproxycommand none\n");
        assert_eq!(none.route(), Route::Direct);
        assert_eq!(resolve("hostname h\n").route(), Route::Direct);
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
            openssh.launch_in_new_terminal("-external").unwrap(),
            ConnectionOutcome::LaunchRequested
        );
        wait_for_contents(
            terminal.log(),
            &format!("{}\n--\n-external\n", ssh.path().display()),
        );
    }

    #[test]
    fn a_new_terminal_runs_portico_when_its_path_is_known() {
        let ssh = fake_ssh();
        let terminal = fake_ssh();
        let openssh =
            OpenSsh::new(ssh.path(), terminal.path()).with_portico(Some("/opt/portico".into()));

        assert_eq!(
            openssh.launch_in_new_terminal("dev").unwrap(),
            ConnectionOutcome::LaunchRequested
        );
        wait_for_contents(terminal.log(), "/opt/portico\nconnect\n--hold\n--\ndev\n");

        let terminal = fake_ssh();
        let openssh = OpenSsh::new(ssh.path(), terminal.path()).with_portico(None);
        openssh.launch_in_new_terminal("dev").unwrap();
        wait_for_contents(
            terminal.log(),
            &format!("{}\n--\ndev\n", ssh.path().display()),
        );
    }

    #[test]
    fn early_launcher_failure_is_reported() {
        let ssh = fake_ssh();
        let terminal = fake_ssh();
        write_executable(&terminal.path, "#!/bin/sh\nexit 7\n");
        let openssh = OpenSsh::new(ssh.path(), terminal.path());

        assert!(matches!(
            openssh.launch_in_new_terminal("dev").unwrap(),
            ConnectionOutcome::LaunchFailed(status) if status.code() == Some(7)
        ));
    }

    #[test]
    fn missing_launcher_is_an_error() {
        let openssh = OpenSsh::new("ssh".into(), "/nonexistent/xdg-terminal-exec".into());

        assert!(matches!(
            openssh.launch_in_new_terminal("dev"),
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
            openssh.launch_in_new_terminal("dev").unwrap(),
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

        let result = openssh.connect_mirrored("dev", Box::new(io::sink()), Some(&cancelled));

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

        let result = openssh.connect_mirrored("dev", Box::new(io::sink()), Some(&cancelled));

        cancel.join().unwrap();
        assert!(matches!(result, Err(OpenSshError::Cancelled)));
        for pid in fs::read_to_string(marker).unwrap().split_whitespace() {
            assert_dies(pid);
        }
    }

    /// Asserts that `pid` is gone or a zombie, allowing it time to die. Teardown sends SIGKILL to
    /// every process in the tree but cannot wait for descendants, and a process with SIGKILL
    /// pending is still runnable for a moment, longer on a loaded machine.
    fn assert_dies(pid: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let state = fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| {
                    stat.rsplit(')')
                        .next()
                        .map(|rest| rest.trim_start().to_owned())
                });
            if state.is_none_or(|state| state.starts_with('Z')) {
                return;
            }
            assert!(Instant::now() < deadline, "descendant {pid} survived");
            thread::sleep(Duration::from_millis(5));
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

    fn fixture_path(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/ssh")
            .join(format!("{name}.txt"))
    }

    /// A fake ssh that starts `sleep 30` in its group, records the sleep's pid
    /// at `<dir>/pid`, then prints `fixture` (if any) to stderr and waits.
    ///
    /// The sleep records its own pid just before it becomes `sleep`, so if the
    /// pid file is missing, no sleep was ever started that could outlive ssh.
    fn fake_hanging_ssh(fixture: Option<&str>) -> (FakeProgram, PathBuf) {
        let fake = fake_ssh();
        let pid = fake.log.with_file_name("pid");
        let print = fixture.map_or(String::new(), |name| {
            format!("cat '{}' >&2\n", fixture_path(name).display())
        });
        write_executable(
            &fake.path,
            &format!(
                "#!/bin/sh\nsh -c 'printf %s \"$$\" > \"$1.tmp\" && mv \"$1.tmp\" \"$1\" && exec sleep 30' sh '{0}' &\nwhile [ ! -s '{0}' ]; do sleep 0.01; done\n{print}wait\n",
                pid.display()
            ),
        );
        (fake, pid)
    }

    /// Checks that whatever a fake ssh recorded at `pid` is gone, once the run
    /// that a deadline ended has returned.
    ///
    /// A deadline counts from the spawn, so on a loaded machine it can end the
    /// fake before the fake records anything. The fakes record a pid before
    /// starting anything that could outlive them, so then there is nothing to
    /// check.
    fn assert_recorded_process_dies(pid: &Path) {
        if let Ok(pid) = fs::read_to_string(pid) {
            assert_dies(&pid);
        }
    }

    fn wait_for_file(path: &Path) -> String {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(contents) = fs::read_to_string(path)
                && !contents.is_empty()
            {
                return contents;
            }
            assert!(Instant::now() < deadline, "missing {}", path.display());
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn probe_stops_at_the_success_line_and_kills_the_group() {
        let (fake, pid) = fake_hanging_ssh(Some("authenticated"));
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
        let started = Instant::now();

        let result = openssh
            .probe(None, "dev", Route::Direct, &AtomicBool::new(false))
            .unwrap()
            .0;

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "probe blocked for {:?}",
            started.elapsed()
        );
        assert_eq!(
            result,
            ProbeResult::Authenticated(Authenticated {
                host: "127.0.0.1".into(),
                address: "127.0.0.1".into(),
                port: 22999,
                method: "publickey".into(),
                via: None,
            })
        );
        assert_dies(&fs::read_to_string(pid).unwrap());
    }

    #[test]
    fn a_proxied_probe_stops_at_the_targets_line_not_the_jump_hosts() {
        let (fake, pid) = fake_hanging_ssh(Some("authenticated-via-verbose-jump"));
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
        let route = Route::Proxied {
            jump: Some("jumpv"),
            hostname: "127.0.0.1",
            port: 2222,
            fd_passing: false,
        };

        let result = openssh
            .probe(None, "dev", route, &AtomicBool::new(false))
            .unwrap()
            .0;

        assert_eq!(
            result,
            ProbeResult::Authenticated(Authenticated {
                host: "127.0.0.1".into(),
                address: "127.0.0.1".into(),
                port: 2222,
                method: "publickey".into(),
                via: Some("jumpv".into()),
            })
        );
        assert_dies(&wait_for_file(&pid));
    }

    #[test]
    fn a_multi_hop_probe_waits_for_the_targets_line() {
        let (fake, pid) = fake_hanging_ssh(Some("authenticated-via-two-jumps"));
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
        let route = Route::Proxied {
            jump: Some("hopa,hopb"),
            hostname: "127.0.0.3",
            port: 22999,
            fd_passing: false,
        };

        let result = openssh
            .probe(None, "dev", route, &AtomicBool::new(false))
            .unwrap()
            .0;

        assert!(
            matches!(&result, ProbeResult::Authenticated(success) if success.host == "127.0.0.3"),
            "{result:?}"
        );
        assert_dies(&wait_for_file(&pid));
    }

    #[test]
    fn a_fd_passing_probe_succeeds_instead_of_timing_out() {
        let (fake, pid) = fake_hanging_ssh(Some("authenticated-via-fdpass"));
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
        let route = Route::Proxied {
            jump: None,
            hostname: "127.0.0.3",
            port: 22999,
            fd_passing: true,
        };
        let started = Instant::now();

        let result = openssh
            .probe(None, "dev", route, &AtomicBool::new(false))
            .unwrap()
            .0;

        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(
            matches!(&result, ProbeResult::Authenticated(success) if success.address == "127.0.0.3"),
            "{result:?}"
        );
        assert_dies(&wait_for_file(&pid));
    }

    #[test]
    fn a_hops_success_before_the_targets_failure_is_a_failure() {
        let refused = fs::read_to_string(fixture_path("auth-refused")).unwrap();
        let fake = fake_failure(&format!(
            "Authenticated to 127.0.0.2 (via proxy) using \"publickey\".\n{refused}"
        ));
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
        let route = Route::Proxied {
            jump: Some("hopa,hopb"),
            hostname: "127.0.0.3",
            port: 22999,
            fd_passing: false,
        };

        let result = openssh
            .probe(None, "dev", route, &AtomicBool::new(false))
            .unwrap()
            .0;

        assert!(
            matches!(result, ProbeResult::Failed(Failure::AuthRefused { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn a_jump_hosts_own_success_is_not_the_targets() {
        let fake =
            fake_failure("Authenticated to 127.0.0.1 ([127.0.0.1]:22999) using \"publickey\".\n");
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
        let route = Route::Proxied {
            jump: Some("jumpv"),
            hostname: "127.0.0.1",
            port: 22999,
            fd_passing: false,
        };

        let result = openssh
            .probe(None, "dev", route, &AtomicBool::new(false))
            .unwrap()
            .0;

        assert!(matches!(result, ProbeResult::Failed(_)), "{result:?}");
    }

    #[test]
    fn probe_passes_the_exact_options() {
        let fake = fake_ssh();
        write_executable(
            &fake.path,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nexit 255\n",
                fake.log.display()
            ),
        );
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());

        openssh
            .probe(None, "dev", Route::Direct, &AtomicBool::new(false))
            .unwrap();

        let expected: String = test_args(None, "dev")
            .iter()
            .map(|arg| format!("{}\n", arg.to_string_lossy()))
            .collect();
        assert_eq!(fs::read_to_string(fake.log()).unwrap(), expected);
    }

    #[test]
    fn probe_classifies_output_when_ssh_exits() {
        let fake = fake_ssh();
        write_executable(
            &fake.path,
            &format!(
                "#!/bin/sh\ncat '{}' >&2\nexit 255\n",
                fixture_path("auth-refused").display()
            ),
        );
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());

        assert_eq!(
            openssh
                .probe(None, "dev", Route::Direct, &AtomicBool::new(false))
                .unwrap()
                .0,
            ProbeResult::Failed(Failure::AuthRefused {
                methods: "publickey".into(),
                offered: vec![OfferedKey {
                    path: "/tmp/portico-fixture/wrong".into(),
                    fingerprint: "SHA256:DEPp8rJWkCfro2273hHcjabe3DX963MQEi2ggf+UeW4".into(),
                    source: KeySource::File,
                }],
                unusable: Vec::new(),
            })
        );
    }

    #[test]
    fn captured_stderr_keeps_the_newest_whole_lines_within_the_limit() {
        let mut stderr = String::new();
        let line = "x".repeat(1023);
        for n in 0..400 {
            push_capped(&mut stderr, &format!("{n}{line}"), TEST_STDERR_LIMIT);
        }
        assert!(stderr.len() <= TEST_STDERR_LIMIT);
        assert!(stderr.ends_with(&format!("399{line}\n")));
        assert!(stderr.lines().all(|l| l.ends_with(&line)));
        assert!(!stderr.starts_with("0x"));
    }

    #[test]
    fn a_line_over_the_limit_keeps_its_newest_bytes() {
        let mut stderr = String::from("old\n");
        push_capped(&mut stderr, &format!("{}tail", "\u{e9}".repeat(20)), 11);
        assert!(stderr.len() <= 11);
        assert!(stderr.ends_with("tail\n"));
        assert!(!stderr.contains("old"));
    }

    #[test]
    fn probe_reports_a_timeout_at_the_deadline() {
        let (fake, pid) = fake_hanging_ssh(None);
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());

        let probe = thread::spawn(move || {
            openssh.probe_with_deadline(
                None,
                "dev",
                Route::Direct,
                &AtomicBool::new(false),
                Duration::from_secs(1),
            )
        });
        let result = probe.join().unwrap().unwrap().0;

        assert_eq!(
            result,
            ProbeResult::Failed(Failure::TimedOut { deadline: true })
        );
        assert_recorded_process_dies(&pid);
    }

    #[test]
    fn the_deadline_holds_while_stderr_keeps_arriving() {
        let fake = fake_ssh();
        let pid = fake.log.with_file_name("pid");
        write_executable(
            &fake.path,
            &format!(
                "#!/bin/sh\nprintf '%s' \"$$\" > '{0}.tmp'\nmv '{0}.tmp' '{0}'\nwhile :; do echo still trying >&2; sleep 0.01; done\n",
                pid.display()
            ),
        );
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
        let (sender, results) = mpsc::channel();
        thread::spawn(move || {
            let _ = sender.send(openssh.probe_with_deadline(
                None,
                "dev",
                Route::Direct,
                &AtomicBool::new(false),
                Duration::from_millis(300),
            ));
        });

        let result = results
            .recv_timeout(Duration::from_secs(5))
            .expect("the probe ran past its deadline")
            .unwrap()
            .0;

        assert_eq!(
            result,
            ProbeResult::Failed(Failure::TimedOut { deadline: true })
        );
        assert_recorded_process_dies(&pid);
    }

    #[test]
    fn probe_cancellation_kills_ssh() {
        let (fake, pid) = fake_hanging_ssh(None);
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
        let cancelled = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&cancelled);
        let cancel = thread::spawn({
            let pid = pid.clone();
            move || {
                wait_for_file(&pid);
                thread::sleep(Duration::from_millis(100));
                signal.store(true, Ordering::Relaxed);
            }
        });
        let started = Instant::now();

        let result = openssh.probe(None, "dev", Route::Direct, &cancelled);

        cancel.join().unwrap();
        assert!(matches!(result, Err(OpenSshError::Cancelled)));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "cancellation blocked for {:?}",
            started.elapsed()
        );
        assert_dies(&fs::read_to_string(pid).unwrap());
    }

    /// A fake ssh that records the temporary known-hosts path at `<dir>/args`,
    /// runs `body` with it in `$f`, and exits 255.
    fn fake_fetching_ssh(body: &str) -> FakeProgram {
        let fake = fake_ssh();
        write_executable(
            &fake.path,
            &format!(
                "#!/bin/sh\nfor a in \"$@\"; do case \"$a\" in UserKnownHostsFile=*) f=\"${{a#UserKnownHostsFile=}}\";; esac; done\nprintf '%s' \"$f\" > '{}'\n{body}\nexit 255\n",
                fake.log.display()
            ),
        );
        fake
    }

    #[test]
    fn fetch_reads_the_key_written_to_the_temporary_file_and_removes_it() {
        let ssh = fake_fetching_ssh("printf '[h]:22 ssh-ed25519 AAAA\\n' > \"$f\"");
        let keygen = fake_ssh();
        write_executable(
            &keygen.path,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf '256 SHA256:abc [h]:22 (ED25519)\\n'\n",
                keygen.log.display()
            ),
        );
        let openssh =
            OpenSsh::new(ssh.path(), "xdg-terminal-exec".into()).with_keygen(keygen.path());

        let result = openssh
            .fetch_host_key(None, "dev", Route::Direct, &AtomicBool::new(false))
            .unwrap();

        assert_eq!(
            result,
            KeyFetch::Presented(PresentedKey {
                lines: b"[h]:22 ssh-ed25519 AAAA\n".to_vec(),
                fingerprints: vec![KeyFingerprint {
                    host: "[h]:22".into(),
                    fingerprint: "SHA256:abc".into(),
                    key_type: "ED25519".into(),
                }],
            })
        );
        let temporary = fs::read_to_string(ssh.log()).unwrap();
        assert!(temporary.starts_with(&std::env::temp_dir().display().to_string()));
        assert_eq!(
            fs::read_to_string(keygen.log()).unwrap(),
            format!("-l\n-f\n{temporary}\n")
        );
        assert!(!Path::new(&temporary).exists());
    }

    #[test]
    fn fetch_with_nothing_written_reports_why() {
        let ssh = fake_fetching_ssh(&format!(
            "cat '{}' >&2",
            fixture_path("connection-refused").display()
        ));
        let openssh = OpenSsh::new(ssh.path(), "xdg-terminal-exec".into())
            .with_keygen("/nonexistent/ssh-keygen".into());

        assert_eq!(
            openssh
                .fetch_host_key(None, "dev", Route::Direct, &AtomicBool::new(false))
                .unwrap(),
            KeyFetch::Failed(Failure::ConnectionRefused)
        );
        assert!(!Path::new(&fs::read_to_string(ssh.log()).unwrap()).exists());
    }

    #[test]
    fn append_adds_a_separating_newline_and_creates_missing_files_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("known_hosts");
        fs::write(&existing, "a b c").unwrap();
        append_known_hosts(&existing, b"x y z\n").unwrap();
        assert_eq!(fs::read_to_string(&existing).unwrap(), "a b c\nx y z\n");

        let missing = dir.path().join("new_hosts");
        append_known_hosts(&missing, b"x y z\n").unwrap();
        assert_eq!(fs::read(&missing).unwrap(), b"x y z\n");
        assert_eq!(
            fs::metadata(&missing).unwrap().permissions().mode() & 0o777,
            0o600
        );

        assert!(append_known_hosts(&dir.path().join("absent/known_hosts"), b"x y z\n").is_err());
    }

    #[test]
    fn resolve_with_reads_the_known_hosts_files() {
        let fake = fake_resolver(
            "hostname h\nuserknownhostsfile ~/.ssh/known_hosts ~/.ssh/known_hosts2\n",
        );
        let entry = fake.log().with_file_name("entry config");
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());

        let resolved = openssh.resolve_with(Some(&entry), "dev", None).unwrap();

        assert_eq!(
            resolved.known_hosts_files,
            ["~/.ssh/known_hosts", "~/.ssh/known_hosts2"]
        );
        assert_eq!(
            fs::read_to_string(fake.log()).unwrap(),
            format!("-G\n-F\n{}\n--\ndev\n", entry.display())
        );
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

    /// A sink the tests can read while a connection still writes to it.
    #[derive(Clone, Default)]
    struct SharedSink(Arc<std::sync::Mutex<Vec<u8>>>);

    impl SharedSink {
        fn contents(&self) -> Vec<u8> {
            self.0.lock().unwrap().clone()
        }
    }

    impl Write for SharedSink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn mirrored_ssh(script: &str) -> FakeProgram {
        let fake = fake_ssh();
        write_executable(&fake.path, &format!("#!/bin/sh\n{script}\n"));
        fake
    }

    #[test]
    fn mirrored_bytes_arrive_unchanged_and_in_order() {
        let fake = fake_ssh();
        let release = fake.log.with_file_name("release");
        write_executable(
            &fake.path,
            &format!(
                "#!/bin/sh\nprintf 'Password:' >&2\ni=0\nwhile [ ! -f '{}' ] && [ \"$i\" -lt 1000 ]; do i=$((i + 1)); sleep 0.01; done\nprintf '\\377\\r\\nbye\\n' >&2\nexit 0\n",
                release.display()
            ),
        );
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
        let sink = SharedSink::default();
        let connection = thread::spawn({
            let sink = sink.clone();
            move || openssh.connect_mirrored("dev", Box::new(sink), None)
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        while sink.contents() != b"Password:" {
            assert!(Instant::now() < deadline, "the prompt was not mirrored");
            thread::sleep(Duration::from_millis(5));
        }
        assert!(!connection.is_finished(), "ssh exited before its release");
        fs::write(&release, "").unwrap();
        let mirrored = connection.join().unwrap().unwrap();

        assert!(mirrored.status.success());
        assert_eq!(sink.contents(), b"Password:\xff\r\nbye\n");
        assert_eq!(mirrored.tail, b"Password:\xff\r\nbye\n");
    }

    #[test]
    fn the_tail_keeps_the_newest_bytes() {
        let fake = mirrored_ssh("head -c 102400 /dev/zero | tr '\\000' a >&2\nprintf 'END\\n' >&2");
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
        let sink = SharedSink::default();

        let mirrored = openssh
            .connect_mirrored("dev", Box::new(sink.clone()), None)
            .unwrap();

        assert_eq!(sink.contents().len(), 102400 + 4);
        assert_eq!(mirrored.tail.len(), CONNECT_TAIL_LIMIT);
        assert!(mirrored.tail.ends_with(b"aaaEND\n"));
    }

    #[test]
    fn the_exit_status_passes_through() {
        let fake = mirrored_ssh("exit 3");
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());

        let mirrored = openssh
            .connect_mirrored("dev", Box::new(SharedSink::default()), None)
            .unwrap();

        assert_eq!(mirrored.status.code(), Some(3));
        assert!(mirrored.tail.is_empty());
    }

    #[test]
    fn a_descendant_holding_stderr_open_does_not_block_the_return() {
        let fake = fake_ssh();
        let pid = fake.log.with_file_name("pid");
        write_executable(
            &fake.path,
            &format!(
                "#!/bin/sh\nprintf 'gone\\n' >&2\nsleep 30 &\nprintf '%s' \"$!\" > '{}'\nexit 255\n",
                pid.display()
            ),
        );
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
        let sink = SharedSink::default();
        let started = Instant::now();

        let mirrored = openssh
            .connect_mirrored("dev", Box::new(sink.clone()), None)
            .unwrap();

        assert!(
            started.elapsed() < Duration::from_secs(10),
            "waited {:?} for a descendant",
            started.elapsed()
        );
        assert_eq!(mirrored.status.code(), Some(255));
        assert_eq!(mirrored.tail, b"gone\n");
        assert_eq!(sink.contents(), b"gone\n");
        let pid: libc::pid_t = fs::read_to_string(pid).unwrap().parse().unwrap();
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
}
