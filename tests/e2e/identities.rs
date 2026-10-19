//! Identity awareness against a real `ssh-agent`: `portico show` and `portico identities`
//! report which identities are in the agent, on disk or missing, and never open a private key.

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use tempfile::TempDir;

use crate::client::{Client, RealHome, text};

/// A real `ssh-agent` listening in a short temporary directory: a Unix socket path is limited
/// to 108 bytes.
struct Agent {
    dir: TempDir,
    socket: PathBuf,
    child: Child,
}

impl Agent {
    fn start() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("pa")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = dir.path().join("a.sock");
        let child = Command::new("ssh-agent")
            .arg("-D")
            .arg("-a")
            .arg(&socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("ssh-agent is installed");
        let started = Instant::now();
        while !socket.exists() {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "ssh-agent did not start"
            );
            thread::sleep(Duration::from_millis(20));
        }
        Self { dir, socket, child }
    }

    fn add(&self, key: &Path) {
        let output = Command::new("ssh-add")
            .arg(key)
            .env("SSH_AUTH_SOCK", &self.socket)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(output.status.success(), "ssh-add: {}", text(&output));
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Creates an unencrypted ed25519 key pair at `path`: test set-up, not anything Portico does.
fn keygen(path: &Path, comment: &str) {
    let output = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", comment, "-f"])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success(), "ssh-keygen: {}", text(&output));
}

fn stdout(client: &Client, args: &[&str]) -> String {
    String::from_utf8(client.run_ok(args).stdout).unwrap()
}

#[test]
#[ignore = "end-to-end: run with scripts/ci-checks.sh e2e"]
fn identities_with_a_real_agent() {
    let real_home = RealHome::snapshot();
    let client = Client::new();
    client.init();
    let ssh = client.home().join(".ssh");
    let agent = Agent::start();
    keygen(&ssh.join("held"), "held key");
    keygen(&ssh.join("file_only"), "file only");
    keygen(&ssh.join("vault"), "vault key");
    agent.add(&ssh.join("held"));
    agent.add(&ssh.join("vault"));
    // The 1Password pattern: the agent holds the key, and only its public half is on disk.
    fs::remove_file(ssh.join("vault")).unwrap();
    let removed = agent.dir.path().join("removed.sock");
    client.append_config(&format!(
        "\nHost withagent\n  HostName 127.0.0.1\n  IdentityAgent {socket}\n  IdentitiesOnly yes\n  \
         IdentityFile ~/.ssh/held\n  IdentityFile ~/.ssh/file_only\n  IdentityFile ~/.ssh/vault.pub\n  \
         IdentityFile ~/.ssh/gone\n\
         \nHost noagent\n  HostName 127.0.0.1\n  IdentityAgent none\n  IdentityFile ~/.ssh/vault.pub\n\
         \nHost deadagent\n  HostName 127.0.0.1\n  IdentityAgent {removed}\n  IdentityFile ~/.ssh/held\n",
        socket = agent.socket.display(),
        removed = removed.display(),
    ));

    let shown = stdout(&client, &["show", "withagent"]);
    for expected in [
        format!("Agent: SSH agent, {}, 2 keys\n", agent.socket.display()),
        "Identity: ~/.ssh/held: in agent (ED25519 SHA256:".into(),
        "Identity: ~/.ssh/file_only: key file (ED25519 SHA256:".into(),
        "Identity: ~/.ssh/vault.pub: in agent (ED25519 SHA256:".into(),
        "\"vault key\")\n".into(),
        "Identity: ~/.ssh/gone: missing (no such file)\n".into(),
        "IdentitiesOnly: yes\n".into(),
    ] {
        assert!(shown.contains(&expected), "{expected:?} in:\n{shown}");
    }
    let noagent = stdout(&client, &["show", "noagent"]);
    assert!(
        noagent.contains("Agent: none (IdentityAgent none)\n")
            && noagent.contains(
                "Identity: ~/.ssh/vault.pub: missing (public key only; agent unavailable;"
            ),
        "{noagent}"
    );
    let dead = stdout(&client, &["show", "deadagent"]);
    assert!(
        dead.contains("unavailable: no such socket\n")
            && dead.contains("Identity: ~/.ssh/held: key file ("),
        "{dead}"
    );

    let inventory = stdout(&client, &["identities"]);
    for expected in [
        format!(
            "  SSH agent  {}  2 keys  (used by 1 host)\n",
            agent.socket.display()
        ),
        "held key   ~/.ssh/held\n".into(),
        "vault key  ~/.ssh/vault.pub\n".into(),
        format!(
            "  SSH agent  {}  unavailable: no such socket",
            removed.display()
        ),
        "  ~/.ssh/file_only  key file  used by: withagent\n".into(),
        "  ~/.ssh/gone       missing   used by: withagent\n".into(),
        "  ~/.ssh/held       in agent  used by: deadagent, withagent\n".into(),
        "  ~/.ssh/vault.pub  in agent  used by: noagent, withagent\n".into(),
    ] {
        assert!(
            inventory.contains(&expected),
            "{expected:?} in:\n{inventory}"
        );
    }

    // Portico never opens a private key: making them unreadable changes nothing.
    for name in ["held", "file_only"] {
        fs::set_permissions(ssh.join(name), fs::Permissions::from_mode(0o000)).unwrap();
    }
    assert_eq!(stdout(&client, &["show", "withagent"]), shown);
    assert_eq!(stdout(&client, &["identities"]), inventory);
    real_home.assert_unchanged();
}

#[test]
#[ignore = "end-to-end: run with scripts/ci-checks.sh e2e"]
fn the_environment_agent_and_openssh_defaults() {
    let client = Client::new();
    client.init();
    let ssh = client.home().join(".ssh");
    let agent = Agent::start();
    keygen(&ssh.join("id_ed25519"), "default key");
    keygen(&ssh.join("other"), "other key");
    agent.add(&ssh.join("other"));
    client.append_config("\nHost plain\n  HostName 127.0.0.1\n");
    let output = client
        .command(&["show", "plain"])
        .env("SSH_AUTH_SOCK", &agent.socket)
        .output()
        .unwrap();
    let shown = text(&output);
    // The e2e ssh wrapper ends the configuration with `IdentityAgent none`, so the host's agent
    // is disabled even though the environment names one; `portico identities` still lists it.
    assert!(
        shown.contains("Agent: none (IdentityAgent none)\n")
            && shown.contains(
                "Identity: ~/.ssh/id_ed25519 (OpenSSH default): key file (ED25519 SHA256:"
            )
            && shown.contains("Identity: other OpenSSH defaults: none present\n"),
        "{shown}"
    );
    let output = client
        .command(&["identities"])
        .env("SSH_AUTH_SOCK", &agent.socket)
        .output()
        .unwrap();
    let listed = text(&output);
    assert!(
        listed.contains(&format!(
            "  SSH agent  {}  1 key  (SSH_AUTH_SOCK)\n",
            agent.socket.display()
        )) && listed.contains("other key  ~/.ssh/other\n")
            && listed.contains("  ~/.ssh/other       in agent  used by: -\n"),
        "{listed}"
    );
}
