//! A throwaway OpenSSH server for one test.
//!
//! The server runs as the current user (so it can only log that user in) on a
//! random port of `127.0.0.1`, with every file in a temporary directory.
//! Interactive sessions never reach the user's login shell: `ForceCommand`
//! runs a script that prints a banner, echoes one line back and exits.

use std::{
    ffi::CStr,
    fs,
    net::{Ipv4Addr, TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use tempfile::TempDir;

const SSHD: &str = "/usr/bin/sshd";
const KEYGEN: &str = "/usr/bin/ssh-keygen";

/// The passphrase of [`Sshd::locked_key`].
pub const LOCKED_KEY_PASSPHRASE: &str = "pw";

/// Fails the test, rather than skipping it, when OpenSSH's server is missing:
/// a suite that skipped everything would pass CI without testing anything.
pub fn require_sshd() {
    assert!(
        Path::new(SSHD).exists(),
        "{SSHD} is missing: install OpenSSH's server to run the end-to-end tests"
    );
}

/// The user's name and home directory from the password database, which is
/// where OpenSSH looks, whatever `$HOME` says.
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

/// A port nothing listens on (as long as nothing grabs it in between).
pub fn closed_port() -> u16 {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

pub fn keygen(path: &Path, passphrase: &str) {
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(path.with_extension("pub"));
    let status = Command::new(KEYGEN)
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
        fs::write(
            path.join("authorized_keys"),
            fs::read_to_string(path.join("client_key.pub")).unwrap()
                + &fs::read_to_string(path.join("locked_key.pub")).unwrap(),
        )
        .unwrap();
        let session = path.join("session.sh");
        fs::write(
            &session,
            "#!/bin/sh\necho portico-e2e ready\nread line\necho \"portico-e2e got $line\"\n",
        )
        .unwrap();
        fs::set_permissions(&session, fs::Permissions::from_mode(0o755)).unwrap();
        let (pubkey, password) = if options.password_only {
            ("no", "yes")
        } else {
            ("yes", "no")
        };
        fs::write(
            path.join("sshd_config.base"),
            format!(
                "ListenAddress 127.0.0.1\nHostKey {host}\nPidFile {pid}\n\
                 AuthorizedKeysFile {authorized}\nStrictModes no\nUsePAM no\n\
                 PubkeyAuthentication {pubkey}\nPasswordAuthentication {password}\n\
                 KbdInteractiveAuthentication no\nForceCommand {session}\nLogLevel ERROR\n",
                host = path.join("host_key").display(),
                pid = path.join("sshd.pid").display(),
                authorized = path.join("authorized_keys").display(),
                session = session.display(),
            ),
        )
        .unwrap();

        // The port is free when chosen but another process may take it before
        // sshd binds it; a server that cannot listen is retried on a new port.
        let mut failures = Vec::new();
        for _ in 0..3 {
            let port = closed_port();
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
        fs::write(self.dir.path().join("authorized_keys"), "").unwrap();
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
    let mut child = Command::new(SSHD)
        .args(["-D", "-e", "-f"])
        .arg(&config)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(fs::File::create(&log).unwrap())
        .spawn()
        .unwrap();
    let started = Instant::now();
    loop {
        if TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_ok() {
            return Ok(child);
        }
        let failure = if let Some(status) = child.try_wait().unwrap() {
            Some(format!("sshd exited ({status}) before listening on {port}"))
        } else if started.elapsed() > Duration::from_secs(10) {
            let _ = child.kill();
            let _ = child.wait();
            Some(format!("sshd did not listen on {port} within 10 s"))
        } else {
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
