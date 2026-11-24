use std::{
    io,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Output, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::Duration,
};

use thiserror::Error;

use crate::domain::explain::Explanation;
use crate::platform::launcher::DEFAULT_LAUNCHER;
use crate::platform::process::terminate_process_group;

mod config;
mod connect;
mod keys;
mod pipes;

pub use config::ResolvedConfig;
pub use connect::{CONNECT_TAIL_LIMIT, Mirrored};
pub(crate) use keys::append_known_hosts;
pub use keys::{KeyFetch, KeyFingerprint, PresentedKey};

use pipes::{finish_pipe_cancellable, read_pipe};

#[derive(Clone, Debug)]
pub struct OpenSsh {
    ssh: PathBuf,
    terminal: PathBuf,
    keygen: PathBuf,
    ssh_add: PathBuf,
    /// Portico's own executable, run in a new terminal so a failure can be explained there.
    portico: Option<PathBuf>,
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
        Self::new("ssh".into(), DEFAULT_LAUNCHER.into()).with_portico(
            // A replaced binary reports a path that no longer exists.
            std::env::current_exe().ok().filter(|path| path.exists()),
        )
    }

    pub fn new(ssh: PathBuf, terminal: PathBuf) -> Self {
        Self {
            ssh,
            terminal,
            keygen: "ssh-keygen".into(),
            ssh_add: "ssh-add".into(),
            portico: None,
        }
    }

    pub fn with_portico(self, portico: Option<PathBuf>) -> Self {
        Self { portico, ..self }
    }

    pub fn with_keygen(self, keygen: PathBuf) -> Self {
        Self { keygen, ..self }
    }

    pub fn with_ssh_add(self, ssh_add: PathBuf) -> Self {
        Self { ssh_add, ..self }
    }

    pub(crate) fn keygen_program(&self) -> &Path {
        &self.keygen
    }

    pub(crate) fn ssh_add_program(&self) -> &Path {
        &self.ssh_add
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

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicBool, Ordering},
        thread,
        time::Duration,
    };

    use tempfile::TempDir;

    use super::*;
    use crate::test_executable::write_executable;

    pub(super) struct FakeProgram {
        _dir: TempDir,
        pub(super) path: PathBuf,
        pub(super) log: PathBuf,
    }

    impl FakeProgram {
        pub(super) fn path(&self) -> PathBuf {
            self.path.clone()
        }

        pub(super) fn log(&self) -> &Path {
            &self.log
        }
    }

    pub(super) fn fake_ssh() -> FakeProgram {
        fake_program("", true)
    }

    pub(super) fn fake_resolver(output: &str) -> FakeProgram {
        fake_program(output, false)
    }

    pub(super) fn fake_failure(message: &str) -> FakeProgram {
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

    pub(super) fn wait_for_contents(path: &Path, expected: &str) {
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

    pub(super) fn fixture_path(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/ssh")
            .join(format!("{name}.txt"))
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

    /// Reads a descriptor's target from `/proc/self/fd`, which macOS lacks.
    #[cfg(target_os = "linux")]
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
}
