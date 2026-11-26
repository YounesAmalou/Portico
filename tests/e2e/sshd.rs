//! A throwaway OpenSSH server for one test.
//!
//! The server runs as the current user (so it can only log that user in) on a
//! random port of `127.0.0.1`, with every file in a temporary directory.
//! Interactive sessions never reach the user's login shell: `ForceCommand`
//! runs a script that prints a banner, echoes one line back and exits.
//!
//! On Windows the server is Win32-OpenSSH's `sshd.exe`, which serves its own
//! user's key logins without running as a service. It runs `ForceCommand`
//! through the user's default shell, `cmd.exe`, so there the script is a batch
//! file. Its keys and its `authorized_keys` are readable by the user alone, as
//! Win32-OpenSSH requires.

#[cfg(unix)]
use std::{
    ffi::CStr,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::fs::PermissionsExt,
    },
};
use std::{
    fs,
    net::{Ipv4Addr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use tempfile::TempDir;

/// Where OpenSSH's server is when `SSHD` does not say: `scripts/ci-checks.sh e2e` sets `SSHD`
/// to the same default.
#[cfg(target_os = "linux")]
const DEFAULT_SSHD: &str = "/usr/bin/sshd";
#[cfg(all(unix, not(target_os = "linux")))]
const DEFAULT_SSHD: &str = "/usr/sbin/sshd";
#[cfg(windows)]
const DEFAULT_SSHD: &str = r"C:\Windows\System32\OpenSSH\sshd.exe";

/// The passphrase of [`Sshd::locked_key`].
pub const LOCKED_KEY_PASSPHRASE: &str = "pw";

/// OpenSSH's server: `$SSHD`, or the system's usual place.
pub fn sshd_program() -> PathBuf {
    std::env::var_os("SSHD")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SSHD))
}

/// OpenSSH's client program `name` (`ssh`, `ssh-keygen`, `ssh-add`).
#[cfg(unix)]
pub fn openssh_program(name: &str) -> PathBuf {
    Path::new("/usr/bin").join(name)
}

/// OpenSSH's client program `name` (`ssh`, `ssh-keygen`, `ssh-add`): Win32-OpenSSH installs
/// its client beside its server, and Git for Windows' own OpenSSH, often first on `PATH`,
/// is not the one Portico supports.
#[cfg(windows)]
pub fn openssh_program(name: &str) -> PathBuf {
    sshd_program()
        .parent()
        .expect("the server's path has a directory")
        .join(format!("{name}.exe"))
}

/// Fails the test, rather than skipping it, when OpenSSH's server is missing:
/// a suite that skipped everything would pass CI without testing anything.
pub fn require_sshd() {
    let sshd = sshd_program();
    assert!(
        sshd.exists(),
        "{} is missing: install OpenSSH's server to run the end-to-end tests",
        sshd.display()
    );
}

/// The user's name and home directory from the password database, which is
/// where OpenSSH looks, whatever `$HOME` says.
#[cfg(unix)]
pub fn current_user() -> (String, PathBuf) {
    // SAFETY: `getpwuid` returns null or a pointer to a static entry, read
    // before any other call can overwrite it.
    unsafe {
        let entry = libc::getpwuid(libc::getuid());
        assert!(!entry.is_null(), "no password database entry for this user");
        let name = CStr::from_ptr((*entry).pw_name)
            .to_str()
            .unwrap()
            .to_owned();
        let home = CStr::from_ptr((*entry).pw_dir).to_str().unwrap().to_owned();
        (name, PathBuf::from(home))
    }
}

/// The user's name and profile directory, where Win32-OpenSSH looks.
#[cfg(windows)]
pub fn current_user() -> (String, PathBuf) {
    let name = std::env::var("USERNAME").expect("USERNAME is set");
    let home = std::env::var_os("USERPROFILE").expect("USERPROFILE is set");
    (name, PathBuf::from(home))
}

/// A port of `127.0.0.1` that refuses connections for as long as this value
/// lives. On Linux it is bound but never listened on, so no other test's
/// server can be given it in the meantime. macOS lets a connection to such a
/// port time out instead of refusing it, so there, as on Windows, the port is
/// released at once and only refuses until something else takes it.
pub struct ClosedPort {
    #[cfg(unix)]
    _socket: Option<OwnedFd>,
    port: u16,
}

impl ClosedPort {
    pub fn port(&self) -> u16 {
        self.port
    }
}

#[cfg(windows)]
pub fn closed_port() -> ClosedPort {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    ClosedPort {
        port: listener.local_addr().unwrap().port(),
    }
}

#[cfg(unix)]
pub fn closed_port() -> ClosedPort {
    // SAFETY: plain socket calls on a descriptor this function owns, with
    // properly sized `sockaddr_in` buffers.
    unsafe {
        // macOS has no SOCK_CLOEXEC; the flag is set straight after there.
        #[cfg(target_os = "linux")]
        let kind = libc::SOCK_STREAM | libc::SOCK_CLOEXEC;
        #[cfg(not(target_os = "linux"))]
        let kind = libc::SOCK_STREAM;
        let fd = libc::socket(libc::AF_INET, kind, 0);
        assert!(fd >= 0, "socket: {}", std::io::Error::last_os_error());
        #[cfg(not(target_os = "linux"))]
        assert_eq!(libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC), 0);
        let socket = OwnedFd::from_raw_fd(fd);
        let mut address: libc::sockaddr_in = std::mem::zeroed();
        address.sin_family = libc::AF_INET as libc::sa_family_t;
        address.sin_addr.s_addr = u32::from(Ipv4Addr::LOCALHOST).to_be();
        let mut length = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
        let bound = libc::bind(socket.as_raw_fd(), (&raw const address).cast(), length);
        assert_eq!(bound, 0, "bind: {}", std::io::Error::last_os_error());
        let named = libc::getsockname(socket.as_raw_fd(), (&raw mut address).cast(), &mut length);
        assert_eq!(named, 0, "getsockname: {}", std::io::Error::last_os_error());
        ClosedPort {
            _socket: cfg!(target_os = "linux").then_some(socket),
            port: u16::from_be(address.sin_port),
        }
    }
}

pub fn keygen(path: &Path, passphrase: &str) {
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(path.with_extension("pub"));
    let status = Command::new(openssh_program("ssh-keygen"))
        .args([
            "-q",
            "-t",
            "ed25519",
            "-N",
            passphrase,
            "-C",
            "portico-e2e",
            "-f",
        ])
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success(), "ssh-keygen failed");
    restrict_to_user(path);
}

/// Makes `path` readable by the current user alone; OpenSSH refuses private keys, and
/// Win32-OpenSSH's server the files it reads, that others may read. A new file in a temporary
/// directory is already private on Unix.
#[cfg(unix)]
fn restrict_to_user(_path: &Path) {}

#[cfg(windows)]
fn restrict_to_user(path: &Path) {
    let output = Command::new("icacls")
        .arg(path)
        .args(["/inheritance:r", "/grant:r"])
        .arg(format!("{}:F", current_user().0))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "icacls {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stdout)
    );
}

/// `path` as OpenSSH's configuration files take it: Win32-OpenSSH reads `/` in a Windows path,
/// where a `\` may start an escape.
fn config_path(path: &Path) -> String {
    let text = path.display().to_string();
    if cfg!(windows) {
        text.replace('\\', "/")
    } else {
        text
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SshdOptions {
    /// Offer only password authentication, which no test can complete.
    pub password_only: bool,
}

/// A running server; it is killed when dropped, however the test ends.
pub struct Sshd {
    dir: TempDir,
    port: u16,
    child: Child,
}

impl Sshd {
    pub fn start() -> Self {
        Self::start_with(SshdOptions::default())
    }

    pub fn start_with(options: SshdOptions) -> Self {
        require_sshd();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        keygen(&path.join("host_key"), "");
        keygen(&path.join("client_key"), "");
        keygen(&path.join("locked_key"), LOCKED_KEY_PASSPHRASE);
        write_authorized_keys(
            &path.join("authorized_keys"),
            &(fs::read_to_string(path.join("client_key.pub")).unwrap()
                + &fs::read_to_string(path.join("locked_key.pub")).unwrap()),
        );
        let session = write_session(path);
        let (pubkey, password) = if options.password_only {
            ("no", "yes")
        } else {
            ("yes", "no")
        };
        // Win32-OpenSSH has no PAM.
        let pam = if cfg!(windows) { "" } else { "UsePAM no\n" };
        fs::write(
            path.join("sshd_config.base"),
            format!(
                "ListenAddress 127.0.0.1\nHostKey {host}\nPidFile {pid}\n\
                 AuthorizedKeysFile {authorized}\nStrictModes no\n{pam}\
                 PubkeyAuthentication {pubkey}\nPasswordAuthentication {password}\n\
                 KbdInteractiveAuthentication no\nForceCommand {session}\nLogLevel ERROR\n",
                host = config_path(&path.join("host_key")),
                pid = config_path(&path.join("sshd.pid")),
                authorized = config_path(&path.join("authorized_keys")),
                session = config_path(&session),
            ),
        )
        .unwrap();

        // The port is free when chosen but another process may take it before
        // sshd binds it; a server that cannot listen is retried on a new port.
        let mut failures = Vec::new();
        for _ in 0..3 {
            // Released just before sshd binds it, hence the retries.
            let port = closed_port().port();
            match spawn(path, port) {
                Ok(child) => return Self { dir, port, child },
                Err(error) => failures.push(error),
            }
        }
        panic!("sshd never listened:\n{}", failures.join("\n---\n"));
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn client_key(&self) -> PathBuf {
        self.dir.path().join("client_key")
    }

    /// A key protected by [`LOCKED_KEY_PASSPHRASE`].
    pub fn locked_key(&self) -> PathBuf {
        self.dir.path().join("locked_key")
    }

    /// Makes the server refuse every client key.
    pub fn revoke_all_keys(&self) {
        write_authorized_keys(&self.dir.path().join("authorized_keys"), "");
    }

    /// Gives the server a new host key and restarts it on the same port.
    pub fn replace_host_key(&mut self) {
        self.stop();
        keygen(&self.dir.path().join("host_key"), "");
        self.child = spawn(self.dir.path(), self.port).unwrap_or_else(|error| panic!("{error}"));
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Writes the server's `authorized_keys`, readable by the user alone.
fn write_authorized_keys(path: &Path, keys: &str) {
    fs::write(path, keys).unwrap();
    restrict_to_user(path);
}

/// Writes the session script `ForceCommand` runs in `dir` and returns its path: it prints a
/// banner, reads one line and echoes it back.
#[cfg(unix)]
fn write_session(dir: &Path) -> PathBuf {
    let session = dir.join("session.sh");
    fs::write(
        &session,
        "#!/bin/sh\necho portico-e2e ready\nread line\necho \"portico-e2e got $line\"\n",
    )
    .unwrap();
    fs::set_permissions(&session, fs::Permissions::from_mode(0o755)).unwrap();
    session
}

/// Writes the session script `ForceCommand` runs in `dir` and returns its path: a batch file,
/// as Win32-OpenSSH's server runs the command with `cmd.exe /c`. It prints a banner, reads one
/// line and echoes it back.
#[cfg(windows)]
fn write_session(dir: &Path) -> PathBuf {
    let session = dir.join("session.cmd");
    fs::write(
        &session,
        "@echo off\r\necho portico-e2e ready\r\nset /p line=\r\necho portico-e2e got %line%\r\n",
    )
    .unwrap();
    session
}

impl Drop for Sshd {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Starts sshd on `port` and waits until it accepts connections, or returns
/// why it did not.
fn spawn(dir: &Path, port: u16) -> Result<Child, String> {
    let config = dir.join("sshd_config");
    fs::write(
        &config,
        format!(
            "Port {port}\n{}",
            fs::read_to_string(dir.join("sshd_config.base")).unwrap()
        ),
    )
    .unwrap();
    let log = dir.join("sshd.log");
    // sshd writes its PID file only once it listens, and a killed one leaves
    // its file behind: the file holding this child's PID is the proof that
    // the server answering on the port is this one.
    let pid_file = dir.join("sshd.pid");
    let _ = fs::remove_file(&pid_file);
    let mut child = Command::new(sshd_program())
        .args(["-D", "-e", "-f"])
        .arg(&config)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(fs::File::create(&log).unwrap())
        .spawn()
        .unwrap();
    let started = Instant::now();
    loop {
        let failure = if let Some(status) = child.try_wait().unwrap() {
            Some(format!("sshd exited ({status}) before listening on {port}"))
        } else if started.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            let _ = child.wait();
            Some(format!("sshd did not listen on {port} within 10 s"))
        } else {
            let listening =
                fs::read_to_string(&pid_file).is_ok_and(|pid| pid.trim() == child.id().to_string());
            if listening && TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_ok() {
                return Ok(child);
            }
            None
        };
        if let Some(failure) = failure {
            return Err(format!(
                "{failure}:\n{}",
                fs::read_to_string(&log).unwrap_or_default()
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
#[ignore = "end-to-end: run with scripts/ci-checks.sh e2e"]
fn a_server_that_cannot_listen_is_not_mistaken_for_another_one() {
    let sshd = Sshd::start();
    // Something else already listens on the port: our server cannot bind it,
    // and the other listener's answers must not count as ours.
    let taken = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = taken.local_addr().unwrap().port();
    let result = spawn(sshd.dir.path(), port);
    assert!(
        result.is_err(),
        "a server that never listened was reported ready"
    );
}

#[test]
#[ignore = "end-to-end: run with scripts/ci-checks.sh e2e"]
fn a_closed_port_refuses_connections_and_stays_reserved() {
    let closed = closed_port();
    let error = TcpStream::connect((Ipv4Addr::LOCALHOST, closed.port())).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::ConnectionRefused);
    // Only Linux keeps the port reserved (see `ClosedPort`).
    if cfg!(target_os = "linux") {
        assert!(
            TcpListener::bind((Ipv4Addr::LOCALHOST, closed.port())).is_err(),
            "another server could take the closed port"
        );
    }
}
