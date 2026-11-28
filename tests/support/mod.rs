#![allow(dead_code)]

#[path = "executable.rs"]
mod executable;
pub mod pty;

use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use tempfile::TempDir;

#[cfg(windows)]
#[allow(
    unused_imports,
    reason = "only some test binaries compile programs of their own"
)]
pub use executable::compile_program;
use executable::write_executable;

pub struct TestEnv {
    _dir: TempDir,
    home: PathBuf,
    bin: PathBuf,
}

pub struct RunResult(pub Output);

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

        // A machine's own podman (CI runners have one) must not answer for the tests: this
        // one fails as a missing podman does, until a test installs another.
        write_executable(
            bin.join("podman"),
            "#!/bin/sh\necho 'sh: 2: exec: podman: not found' >&2\nexit 127\n",
        );

        Self {
            _dir: dir,
            home,
            bin,
        }
    }

    /// Replaces a program on the test `PATH` with the given script.
    pub fn install_program(&self, name: &str, script: &str) {
        write_executable(self.bin.join(name), script);
    }

    pub fn run<I, S>(&self, args: I) -> RunResult
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        RunResult(self.command(args).output().unwrap())
    }

    /// Runs Portico with `input` on its standard input.
    pub fn run_with_stdin<I, S>(&self, args: I, input: &str) -> RunResult
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        use std::io::Write;
        let mut child = self
            .command(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        // Portico may exit without reading; a closed pipe is fine.
        let _ = stdin.write_all(input.as_bytes());
        drop(stdin);
        RunResult(child.wait_with_output().unwrap())
    }

    /// Starts Portico with stdin, stdout and stderr all piped.
    pub fn spawn_piped<I, S>(&self, args: I) -> std::process::Child
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.command(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    }

    /// Starts Portico without waiting for it, its output piped.
    pub fn spawn<I, S>(&self, args: I) -> std::process::Child
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.command(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap()
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
        let mut command = Command::new(env!("CARGO_BIN_EXE_portico"));
        command.args(args).env("PATH", self.path());
        for (key, value) in self.vars() {
            match value {
                Some(value) => command.env(key, value),
                None => command.env_remove(key),
            };
        }
        command
    }

    /// The environment Portico runs in, beyond `PATH`: a variable set, or removed (`None`).
    pub fn vars(&self) -> Vec<(&'static str, Option<PathBuf>)> {
        self.vars_in(&self.home)
    }

    /// `vars` with `home` as the home directory.
    pub fn vars_in(&self, home: &Path) -> Vec<(&'static str, Option<PathBuf>)> {
        vec![
            ("HOME", Some(home.to_owned())),
            // Windows' home and settings directory. Settings land where the other systems put
            // them, so tests find them at one path.
            ("USERPROFILE", Some(home.to_owned())),
            ("APPDATA", Some(home.join(".config"))),
            // The developer's own agent must never be asked (a locked 1Password would prompt).
            ("SSH_AUTH_SOCK", None),
            // Tunnel records stay in the temporary home, never in the real runtime directory.
            ("XDG_RUNTIME_DIR", None),
            // Settings come from the temporary home, never the developer's own.
            ("XDG_CONFIG_HOME", None),
            // macOS and Windows run the fake launcher named here; Linux finds it on `PATH`.
            (
                "PORTICO_TERMINAL",
                Some(exe(self.bin.join("xdg-terminal-exec"))),
            ),
            ("TERM_PROGRAM", None),
        ]
    }

    /// `PATH` with the fake programs first.
    pub fn path(&self) -> std::ffi::OsString {
        let inherited = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![self.bin.clone()];
        paths.extend(std::env::split_paths(&inherited));
        std::env::join_paths(paths).unwrap()
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

/// OpenSSH's client program `name` as Portico runs it: the one on `PATH`, or on Windows the one
/// Windows ships, since Git for Windows' own ssh, often first on `PATH`, reads a Windows path
/// as a relative one.
pub fn openssh(name: &str) -> PathBuf {
    if cfg!(windows) {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
        Path::new(&root)
            .join(r"System32\OpenSSH")
            .join(format!("{name}.exe"))
    } else {
        name.into()
    }
}

/// `path` as an OpenSSH configuration file takes it: Windows' OpenSSH reads `/` in a Windows
/// path, where a `\` may start an escape.
pub fn config_path(path: &Path) -> String {
    let text = path.display().to_string();
    if cfg!(windows) {
        text.replace('\\', "/")
    } else {
        text
    }
}

/// `path`, written with `/`, as Portico shows it to people: with the system's separator.
pub fn shown(path: &str) -> String {
    path.replace('/', std::path::MAIN_SEPARATOR_STR)
}

/// The file a fake program written at `path` runs from: `path` itself, or `<path>.exe` on
/// Windows, where a program named by its full path must exist under that name.
pub fn exe(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    if cfg!(windows) {
        let mut name = path.as_os_str().to_owned();
        name.push(".exe");
        PathBuf::from(name)
    } else {
        path.to_owned()
    }
}

/// Takes reading the file at `path` away from the user running the tests, until `allow_read`:
/// its mode on Unix, an access rule denying it on Windows.
pub fn deny_read(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
    }
    #[cfg(windows)]
    icacls(path, &["/deny", &format!("{}:(R)", windows_user())]);
}

/// Gives back what `deny_read` took (on Unix, the private mode `0o600`).
pub fn allow_read(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    #[cfg(windows)]
    icacls(path, &["/remove:d", &windows_user()]);
}

#[cfg(windows)]
fn windows_user() -> String {
    std::env::var("USERNAME").expect("USERNAME names the user running the tests")
}

/// Changes the access rules of `path` with `icacls`.
#[cfg(windows)]
pub fn icacls(path: &Path, args: &[&str]) {
    let output = Command::new("icacls")
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "icacls {} {args:?}: {}{}",
        path.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
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

    /// Asserts the command exited with `code` and printed `expected` on stdout.
    pub fn assert_exit_code_and_stdout_contains(self, code: i32, expected: &str) {
        let stdout = String::from_utf8_lossy(&self.0.stdout);
        assert!(
            self.0.status.code() == Some(code) && stdout.contains(expected),
            "status: {}\nstdout:\n{stdout}\nstderr:\n{}",
            self.0.status,
            String::from_utf8_lossy(&self.0.stderr),
        );
    }

    /// Asserts the command exited with `code` and printed `expected` on stderr.
    pub fn assert_exit_code_and_stderr_contains(self, code: i32, expected: &str) {
        let stderr = String::from_utf8_lossy(&self.0.stderr);
        assert!(
            self.0.status.code() == Some(code) && stderr.contains(expected),
            "status: {}\nstdout:\n{}\nstderr:\n{stderr}",
            self.0.status,
            String::from_utf8_lossy(&self.0.stdout),
        );
    }

    /// Asserts the command exited with `code` and printed exactly `expected` on stderr.
    pub fn assert_exit_code_and_stderr(self, code: i32, expected: &str) {
        let stderr = String::from_utf8_lossy(&self.0.stderr);
        assert!(
            self.0.status.code() == Some(code) && stderr == expected,
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

/// A fake receives its arguments and `HOME` as given, its standard input, and exits with the
/// script's code, also on Windows, where it runs through the launcher, and `sh` parses its own
/// command line and rewrites `HOME`.
#[test]
fn shim_passes_arguments_and_exit_code() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let fake = dir.path().join("fake");
    write_executable(
        &fake,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" \"$HOME\"\ncat\nexit 7\n",
    );
    let args = ["a b", "c", "*", "it's", "\"q\"", "", "trail\\", "@x"];
    let mut child = Command::new(&fake)
        .args(args)
        .env("HOME", dir.path())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"in\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!(
            "a b\nc\n*\nit's\n\"q\"\n\ntrail\\\n@x\n{}\nin\n",
            dir.path().display()
        )
    );
    assert_eq!(output.status.code(), Some(7));
}

/// A fake `ssh`: `-G` answers like OpenSSH (refusing the alias `rejected`), and a tunnel start
/// (`-f`, or `-N` where ssh cannot background itself) logs its arguments and leaves a
/// background stand-in that carries them, as the real ssh's background process does (or, with
/// `-N`, stays in the foreground authenticated), unless `fail-start` exists, when it fails as
/// ssh would on a busy remote port, or `exit-after-auth`, when it does so half a second after
/// authenticating. A start also appends `start-log`, when it exists, to its log. Any other call
/// runs `remote-calls`' script when it exists, so a test can answer a remote command. Returns the log of the tunnel starts.
pub fn install_tunnel_ssh(env: &TestEnv) -> PathBuf {
    let log = env.home().parent().unwrap().join("ssh-calls");
    let fail = env.home().parent().unwrap().join("fail-start");
    let exit_after_auth = env.home().parent().unwrap().join("exit-after-auth");
    let start_log = env.home().parent().unwrap().join("start-log");
    env.install_program(
        "ssh",
        &format!(
            r#"#!/bin/sh
if [ "$1" = -G ]; then
  for alias do :; done
  if [ "$alias" = rejected ] && [ "$2" = -o ]; then
    echo "Bad local forwarding specification '1:x'" >&2
    exit 255
  fi
  printf 'hostname resolved.example\nuser resolved\nport 22\n'
  exit 0
fi
if [ "$1" = -f ] || [ "$1" = -N ]; then
  printf '%s\n' "$*" >> '{log}'
  # The log: `-E <log>`, or on Windows standard error, which Portico sends to the log.
  elog=/dev/stderr
  prev=
  for arg do
    [ "$prev" = -E ] && elog=$arg
    prev=$arg
  done
  if [ -f '{start_log}' ]; then cat '{start_log}' >> "$elog"; fi
  if [ -f '{fail}' ]; then
    printf '\033[31mError: remote port forwarding failed for listen port 8080\033[0m\a\n' >> "$elog"
    exit 255
  fi
  if [ -f '{exit_after_auth}' ]; then
    printf 'Authenticated to prod.example ([192.0.2.1]:22) using "publickey".\n' >> "$elog"
    sleep 0.5
    printf 'Error: remote port forwarding failed for listen port 8080\n' >> "$elog"
    exit 255
  fi
  if [ "$1" = -N ]; then
    printf 'Authenticated to prod.example ([192.0.2.1]:22) using "publickey".\n' >> "$elog"
    {hold}
    exit 0
  fi
  # Like ssh -f, the background process leads its own session. macOS has no setsid command,
  # so Perl forks it there, and returns only once it leads its session and carries the
  # arguments (the exec closes the pipe), as the setsid command has done by the time Portico
  # looks.
  if command -v setsid >/dev/null; then
    setsid sh -c 'sleep 60; exit 0' sh "$@" </dev/null >/dev/null 2>&1 &
  else
    perl -MPOSIX -e 'pipe(my $r, my $w) or die; defined(my $pid = fork) or die;
      if ($pid) {{ close $w; sysread($r, my $b, 1); exit 0 }}
      close $r; POSIX::setsid(); exec @ARGV or die' \
      sh -c 'sleep 60; exit 0' sh "$@" </dev/null >/dev/null 2>&1
  fi
  exit 0
fi
if [ -f '{remote}' ]; then . '{remote}'; fi
exit 0
"#,
            log = log.display(),
            fail = fail.display(),
            exit_after_auth = exit_after_auth.display(),
            start_log = start_log.display(),
            remote = env.home().parent().unwrap().join("remote-calls").display(),
            hold = hold_forward(),
        ),
    );
    log
}

/// What the fake `ssh -N` does once authenticated: stay up, holding the listen address of a
/// local or dynamic forward, by which Windows' Portico sees the forward up.
fn hold_forward() -> String {
    #[cfg(windows)]
    return format!(
        "exec '{}' \"$@\"",
        compile_program("tests/support/listen.rs").display()
    );
    #[cfg(unix)]
    "sleep 60".into()
}
