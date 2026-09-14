use std::{
    fs,
    io::{self, Read},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Output, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

use thiserror::Error;

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
            let mut command = Command::new(&self.terminal);
            command
                .arg("--")
                .arg(&self.ssh)
                .arg("--")
                .arg(alias)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let mut child = command.spawn().map_err(|source| OpenSshError::Io {
                program: self.terminal.clone(),
                source,
            })?;
            let deadline = Instant::now() + Duration::from_millis(100);
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
                        terminate_process_tree(&mut child);
                        return Err(OpenSshError::Io {
                            program: self.terminal.clone(),
                            source,
                        });
                    }
                }
            }
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
                terminate_process_tree(&mut child);
                return Err(OpenSshError::Cancelled);
            }
            match child.try_wait() {
                Ok(Some(status)) => return Ok(status),
                Ok(None) => thread::sleep(Duration::from_millis(20)),
                Err(source) => {
                    terminate_process_tree(&mut child);
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

fn terminate_process_tree(child: &mut std::process::Child) {
    kill_process_tree(child.id());
    let _ = child.wait();
}

fn kill_process_tree(pid: u32) {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return;
    };
    unsafe {
        libc::kill(pid, libc::SIGSTOP);
    }
    if let Ok(children) = fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")) {
        for child in children
            .split_whitespace()
            .filter_map(|child| child.parse().ok())
        {
            kill_process_tree(child);
        }
    }
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
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
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        sync::Arc,
    };

    use tempfile::TempDir;

    use super::*;

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
        fs::write(
            &path,
            format!(
                "#!/bin/sh\nprintf '%s' '{}' >&2\nexit 23\n",
                message.replace('\'', "'\\''")
            ),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        wait_until_executable(&path);
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
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        wait_until_executable(&path);
        FakeProgram {
            _dir: dir,
            path,
            log,
        }
    }

    fn wait_until_executable(path: &Path) {
        loop {
            match std::process::Command::new(path)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
            {
                Ok(_) => return,
                Err(error) if error.raw_os_error() == Some(26) => std::thread::yield_now(),
                Err(error) => panic!("could not execute fake program: {error}"),
            }
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
            &format!("--\n{}\n--\n-external\n", ssh.path().display()),
        );
    }

    #[test]
    fn new_terminal_connection_returns_after_launch() {
        let ssh = fake_ssh();
        let terminal = fake_ssh();
        let release = terminal.log.with_file_name("release");
        let done = terminal.log.with_file_name("done");
        fs::write(
            &terminal.path,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\ni=0\nwhile [ ! -f '{}' ] && [ \"$i\" -lt 200 ]; do i=$((i + 1)); sleep 0.01; done\n: > '{}'\n",
                terminal.log.display(),
                release.display(),
                done.display(),
            ),
        )
        .unwrap();
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
            &format!("--\n{}\n--\ndev\n", ssh.path().display()),
        );
        fs::write(release, "").unwrap();
        wait_for_contents(&done, "");
    }

    #[test]
    fn cancellation_stops_descendant_holding_pipes_after_resolver_exits() {
        let fake = fake_ssh();
        let marker = fake.log.with_file_name("parent-exited");
        fs::write(
            &fake.path,
            format!(
                "#!/bin/sh\nif [ \"$1\" = '-G' ]; then\n  sleep 3 &\n  printf 'hostname dev.example\\n'\n  : > '{}'\nfi\n",
                marker.display(),
            ),
        )
        .unwrap();
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
