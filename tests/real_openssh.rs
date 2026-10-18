//! Opt-in end-to-end check of `portico test` against a real `sshd`.
//!
//! Run with `cargo test --test real_openssh -- --ignored`. Everything the
//! server and the client need lives in temporary directories. OpenSSH resolves
//! `~` from the password database rather than `$HOME`, so the test reaches the
//! client files through absolute paths and an `ssh` wrapper that passes `-F`,
//! and it proves at the end that the real user's `~/.ssh/known_hosts` was not
//! touched.

mod support;

use std::{
    ffi::CStr,
    fs,
    net::{Ipv4Addr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use support::TestEnv;

const SSHD: &str = "/usr/bin/sshd";

/// Kills the `sshd` when the test ends, however it ends.
struct Sshd(Child);

impl Drop for Sshd {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The user's name and home directory from the password database, which is
/// where OpenSSH looks, whatever `$HOME` says.
fn passwd_entry() -> (String, PathBuf) {
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

fn keygen(path: &Path) {
    let status = Command::new("/usr/bin/ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "portico-test", "-f"])
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success(), "ssh-keygen failed");
}

fn portico(env: &TestEnv, args: &[&str]) -> Output {
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![env.bin().to_path_buf()];
    paths.extend(std::env::split_paths(&inherited));
    Command::new(env!("CARGO_BIN_EXE_portico"))
        .args(args)
        .env("HOME", env.home())
        .env("PATH", std::env::join_paths(paths).unwrap())
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
#[ignore = "starts a real sshd; run with --ignored"]
fn portico_test_against_a_real_sshd() {
    if !Path::new(SSHD).exists() {
        eprintln!("skipping: {SSHD} is not installed");
        return;
    }
    let (user, real_home) = passwd_entry();
    let real_known_hosts = real_home.join(".ssh/known_hosts");
    let known_hosts_before = fs::read(&real_known_hosts).ok();

    let server = tempfile::tempdir().unwrap();
    let dir = server.path();
    keygen(&dir.join("host_key"));
    keygen(&dir.join("client_key"));
    let authorized = dir.join("authorized_keys");
    fs::copy(dir.join("client_key.pub"), &authorized).unwrap();

    let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let sshd_config = dir.join("sshd_config");
    fs::write(
        &sshd_config,
        format!(
            "Port {port}\nListenAddress 127.0.0.1\nHostKey {host}\nPidFile {pid}\n\
             AuthorizedKeysFile {authorized}\nStrictModes no\nUsePAM no\n\
             PasswordAuthentication no\nKbdInteractiveAuthentication no\n\
             PubkeyAuthentication yes\nLogLevel ERROR\n",
            host = dir.join("host_key").display(),
            pid = dir.join("sshd.pid").display(),
            authorized = authorized.display(),
        ),
    )
    .unwrap();
    let mut sshd = Sshd(
        Command::new(SSHD)
            .args(["-D", "-e", "-f"])
            .arg(&sshd_config)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let ready = Instant::now();
    loop {
        if TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_ok() {
            break;
        }
        assert!(
            sshd.0.try_wait().unwrap().is_none(),
            "sshd exited before it listened"
        );
        assert!(
            ready.elapsed() < Duration::from_secs(10),
            "sshd never listened"
        );
        thread::sleep(Duration::from_millis(50));
    }

    let env = TestEnv::new();
    assert_ne!(
        env.home(),
        real_home,
        "the test home must not be the real home"
    );
    assert!(portico(&env, &["init", "-y"]).status.success());
    let env_known_hosts = env.home().join(".ssh/known_hosts");
    // `realjump` reaches the same sshd through `real` as its jump host.
    let external = format!(
        "\nHost realjump\n  ProxyJump real\n\
         \nHost real realjump\n  HostName 127.0.0.1\n  Port {port}\n  User {user}\n  \
         IdentityFile {key}\n  IdentitiesOnly yes\n  IdentityAgent none\n  \
         UserKnownHostsFile {known}\n  GlobalKnownHostsFile /dev/null\n",
        key = dir.join("client_key").display(),
        known = env_known_hosts.display(),
    );
    let config = fs::read_to_string(env.ssh_config()).unwrap() + &external;
    fs::write(env.ssh_config(), &config).unwrap();
    let real_config = env.home().join(".ssh/real-config");
    fs::write(
        &real_config,
        config.replace(
            "~/.ssh/portico/config",
            env.managed_config().to_str().unwrap(),
        ),
    )
    .unwrap();
    env.install_program(
        "ssh",
        &format!(
            "#!/bin/sh\nexec /usr/bin/ssh -F '{}' \"$@\"\n",
            real_config.display()
        ),
    );
    env.install_program("ssh-keygen", "#!/bin/sh\nexec /usr/bin/ssh-keygen \"$@\"\n");

    // An unknown host key is reported with its fingerprint and trusted by nothing.
    let first = portico(&env, &["test", "real"]);
    let first_text = text(&first);
    assert_eq!(first.status.code(), Some(1), "{first_text}");
    let fingerprint = first_text
        .split_whitespace()
        .find(|word| word.starts_with("SHA256:"))
        .unwrap_or_else(|| panic!("no fingerprint in:\n{first_text}"))
        .trim_end_matches(['.', ',', ')'])
        .to_owned();
    assert!(!env_known_hosts.exists(), "nothing may be trusted yet");

    // Trusting exactly that fingerprint lets the login pass.
    let accepted = portico(&env, &["test", "real", "--accept-host-key", &fingerprint]);
    let accepted_text = text(&accepted);
    assert_eq!(accepted.status.code(), Some(0), "{accepted_text}");
    assert!(accepted_text.contains("authenticated"), "{accepted_text}");
    assert!(
        !fs::read_to_string(&env_known_hosts)
            .unwrap()
            .trim()
            .is_empty(),
        "the host key was not recorded"
    );

    // A login through a jump host is recognised as soon as it happens, well
    // before the test's 15 second deadline.
    let started = Instant::now();
    let jumped = portico(&env, &["test", "realjump"]);
    let jumped_text = text(&jumped);
    assert_eq!(jumped.status.code(), Some(0), "{jumped_text}");
    assert!(
        jumped_text.contains(&format!(
            "authenticated as {user}@127.0.0.1:{port} via real (publickey)"
        )),
        "{jumped_text}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "the jump test took {:?}",
        started.elapsed()
    );

    // A key the server no longer accepts is an authentication failure.
    fs::write(&authorized, "").unwrap();
    let refused = portico(&env, &["test", "real"]);
    let refused_text = text(&refused);
    assert_eq!(refused.status.code(), Some(1), "{refused_text}");
    assert!(
        refused_text.contains("authentication refused"),
        "{refused_text}"
    );

    assert_eq!(
        fs::read(&real_known_hosts).ok(),
        known_hosts_before,
        "the real ~/.ssh/known_hosts changed"
    );
}
