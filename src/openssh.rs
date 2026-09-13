use std::{
    io,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Output},
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
}

impl OpenSsh {
    pub fn system() -> Self {
        Self::new("ssh".into(), "xdg-terminal-exec".into())
    }

    pub fn new(ssh: PathBuf, terminal: PathBuf) -> Self {
        Self { ssh, terminal }
    }

    pub fn validate(&self, entry: &Path, aliases: &[String]) -> Result<(), OpenSshError> {
        if aliases.is_empty() {
            self.validate_alias(entry, "portico-validation")?;
        } else {
            for alias in aliases {
                self.validate_alias(entry, alias)?;
            }
        }
        Ok(())
    }

    pub fn resolve(&self, alias: &str) -> Result<ResolvedConfig, OpenSshError> {
        let output = self.checked_output(Command::new(&self.ssh).args(["-G", "--", alias]))?;
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

    pub fn connect(&self, alias: &str, new_terminal: bool) -> Result<ExitStatus, OpenSshError> {
        let (program, mut command) = if new_terminal {
            let mut command = Command::new(&self.terminal);
            command.arg("--").arg(&self.ssh).arg("--").arg(alias);
            (&self.terminal, command)
        } else {
            let mut command = Command::new(&self.ssh);
            command.arg("--").arg(alias);
            (&self.ssh, command)
        };
        command.status().map_err(|source| OpenSshError::Io {
            program: program.clone(),
            source,
        })
    }

    fn validate_alias(&self, entry: &Path, alias: &str) -> Result<(), OpenSshError> {
        self.checked_output(
            Command::new(&self.ssh)
                .arg("-G")
                .arg("-F")
                .arg(entry)
                .arg("--")
                .arg(alias),
        )?;
        Ok(())
    }

    fn checked_output(&self, command: &mut Command) -> Result<Output, OpenSshError> {
        let output = command.output().map_err(|source| OpenSshError::Io {
            program: self.ssh.clone(),
            source,
        })?;
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
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
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

    #[test]
    fn passes_alias_as_one_argument_without_a_shell() {
        let fake = fake_ssh();
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
        let status = openssh.connect("-external", false).unwrap();

        assert!(status.success());
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

        assert!(openssh.connect("-external", true).unwrap().success());
        assert_eq!(
            fs::read_to_string(terminal.log()).unwrap(),
            format!("--\n{}\n--\n-external\n", ssh.path().display())
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
