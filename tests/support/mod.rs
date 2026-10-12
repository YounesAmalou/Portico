#![allow(dead_code)]

use std::{
    ffi::OsStr,
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output},
};

use tempfile::TempDir;

pub struct TestEnv {
    _dir: TempDir,
    home: PathBuf,
    bin: PathBuf,
}

pub struct RunResult(Output);

impl TestEnv {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let bin = dir.path().join("bin");
        let log = dir.path().join("connections");
        let rejected_alias = dir.path().join("rejected-alias");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&bin).unwrap();

        write_executable(
            bin.join("ssh"),
            &format!(
                "#!/bin/sh\nif [ \"$1\" = '-G' ]; then\n  for alias do :; done\n  if [ -f '{}' ] && [ \"$(cat '{}')\" = \"$alias\" ]; then\n    printf 'rejected alias: %s\\n' \"$alias\" >&2\n    exit 23\n  fi\n  printf 'hostname resolved.example\\nuser resolved\\nport 22\\nidentityfile ~/.ssh/id_default\\nproxyjump none\\n'\n  exit 0\nfi\nprintf '%s\\n' \"$@\" >> '{}'\n[ \"$2\" = 'fail' ] && exit 23\nexit 0\n",
                rejected_alias.display(),
                rejected_alias.display(),
                log.display()
            ),
        );
        write_executable(
            bin.join("xdg-terminal-exec"),
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\n[ \"$3\" = 'fail' ] && exit 23\nexit 0\n",
                log.display()
            ),
        );

        Self {
            _dir: dir,
            home,
            bin,
        }
    }

    pub fn run<I, S>(&self, args: I) -> RunResult
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        RunResult(self.command(args).output().unwrap())
    }

    pub fn run_with_env<I, S>(&self, args: I, key: &str, value: &OsStr) -> RunResult
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        RunResult(self.command(args).env(key, value).output().unwrap())
    }

    fn command<I, S>(&self, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let inherited = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![self.bin.clone()];
        paths.extend(std::env::split_paths(&inherited));
        let path = std::env::join_paths(paths).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_portico"));
        command.args(args).env("HOME", &self.home).env("PATH", path);
        command
    }

    pub fn ssh_config(&self) -> PathBuf {
        self.home.join(".ssh/config")
    }

    pub fn home(&self) -> &std::path::Path {
        &self.home
    }

    pub fn bin(&self) -> &std::path::Path {
        &self.bin
    }

    pub fn managed_config(&self) -> PathBuf {
        self.home.join(".ssh/portico/config")
    }

    pub fn connection_log(&self) -> PathBuf {
        self._dir.path().join("connections")
    }

    pub fn reject_alias(&self, alias: &str) {
        fs::write(self._dir.path().join("rejected-alias"), alias).unwrap();
    }
}

impl RunResult {
    pub fn assert_success(self) {
        assert!(
            self.0.status.success(),
            "status: {}\nstdout:\n{}\nstderr:\n{}",
            self.0.status,
            String::from_utf8_lossy(&self.0.stdout),
            String::from_utf8_lossy(&self.0.stderr),
        );
    }

    pub fn assert_failure(self) {
        assert!(!self.0.status.success(), "command unexpectedly succeeded");
    }

    pub fn assert_stdout_contains(self, expected: &str) {
        let stdout = String::from_utf8_lossy(&self.0.stdout);
        assert!(
            self.0.status.success() && stdout.contains(expected),
            "status: {}\nstdout:\n{stdout}\nstderr:\n{}",
            self.0.status,
            String::from_utf8_lossy(&self.0.stderr),
        );
    }

    pub fn assert_failure_stderr_contains(self, expected: &str) {
        let stderr = String::from_utf8_lossy(&self.0.stderr);
        assert!(
            !self.0.status.success() && stderr.contains(expected),
            "status: {}\nstdout:\n{}\nstderr:\n{stderr}",
            self.0.status,
            String::from_utf8_lossy(&self.0.stdout),
        );
    }

    pub fn assert_exit_code(self, expected: i32) {
        assert_eq!(self.0.status.code(), Some(expected));
    }

    pub fn assert_success_without_output(self, unexpected: &str) {
        let stdout = String::from_utf8_lossy(&self.0.stdout);
        let stderr = String::from_utf8_lossy(&self.0.stderr);
        assert!(
            self.0.status.success() && !stdout.contains(unexpected) && !stderr.contains(unexpected),
            "status: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            self.0.status,
        );
    }
}

fn write_executable(path: PathBuf, contents: &str) {
    fs::write(&path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
