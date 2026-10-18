//! An isolated home for the real `portico` binary and the real OpenSSH client.
//!
//! OpenSSH resolves `~` from the password database rather than `$HOME`, so a
//! temporary `$HOME` alone would still read and write the real `~/.ssh`. A
//! wrapper `ssh` first on `PATH` therefore hands OpenSSH a flattened copy of
//! the temporary `~/.ssh/config` with `~/` spelled out, and ends it with a
//! `Host *` block that keeps known hosts in the temporary home and leaves the
//! agent alone. Those defaults live in the file rather than on the command
//! line so that Portico's own `-o` options still win.

use std::{
    ffi::OsString,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use tempfile::TempDir;

use crate::{
    executable::write_executable,
    sshd::{Sshd, current_user},
};

pub struct Client {
    _dir: TempDir,
    home: PathBuf,
    bin: PathBuf,
}

impl Client {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let bin = dir.path().join("bin");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&bin).unwrap();
        assert_ne!(
            home,
            current_user().1,
            "the test home must not be the real home"
        );
        write_executable(
            bin.join("ssh"),
            r#"#!/bin/sh
for arg do
  [ "$arg" = -F ] && exec /usr/bin/ssh "$@"
done
flat="$HOME/.ssh/e2e-flat-config"
mkdir -p "$HOME/.ssh"
if [ -f "$HOME/.ssh/config" ]; then
  sed "s|~/|$HOME/|g" "$HOME/.ssh/config" > "$flat"
else
  : > "$flat"
fi
printf '\nHost *\n  UserKnownHostsFile %s\n  GlobalKnownHostsFile /dev/null\n  IdentityAgent none\n  IdentitiesOnly yes\n' \
  "$HOME/.ssh/known_hosts" >> "$flat"
exec /usr/bin/ssh -F "$flat" "$@"
"#,
        );
        write_executable(
            bin.join("ssh-keygen"),
            "#!/bin/sh\nexec /usr/bin/ssh-keygen \"$@\"\n",
        );
        Self {
            _dir: dir,
            home,
            bin,
        }
    }

    pub fn binary() -> &'static str {
        env!("CARGO_BIN_EXE_portico")
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn known_hosts(&self) -> PathBuf {
        self.home.join(".ssh/known_hosts")
    }

    pub fn managed_config(&self) -> PathBuf {
        self.home.join(".ssh/portico/config")
    }

    /// The environment Portico runs with: the temporary home and the
    /// wrappers first on `PATH`.
    pub fn env(&self) -> Vec<(OsString, OsString)> {
        let inherited = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![self.bin.clone()];
        paths.extend(std::env::split_paths(&inherited));
        vec![
            ("HOME".into(), self.home.clone().into()),
            ("PATH".into(), std::env::join_paths(paths).unwrap()),
        ]
    }

    pub fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(Self::binary());
        command.args(args).envs(self.env()).stdin(Stdio::null());
        command
    }

    pub fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    /// Runs Portico and fails the test unless it succeeds.
    pub fn run_ok(&self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "portico {args:?}: {}\n{}",
            output.status,
            text(&output)
        );
        output
    }

    pub fn init(&self) {
        self.run_ok(&["init", "-y"]);
    }

    /// Adds a managed host that logs into `sshd` with `key`.
    pub fn add_host(&self, alias: &str, sshd: &Sshd, key: &Path) {
        let port = sshd.port().to_string();
        let user = current_user().0;
        self.run_ok(&[
            "add",
            alias,
            "-H",
            "127.0.0.1",
            "-p",
            &port,
            "-u",
            &user,
            "-i",
            key.to_str().unwrap(),
        ]);
    }

    /// Trusts `alias`'s host key the way a user would: read the fingerprint
    /// `portico test` reports, then accept exactly that one.
    pub fn trust(&self, alias: &str) {
        let first = self.run(&["test", alias]);
        let first_text = text(&first);
        let fingerprint = first_text
            .split_whitespace()
            .find(|word| word.starts_with("SHA256:"))
            .unwrap_or_else(|| panic!("no fingerprint in:\n{first_text}"))
            .trim_end_matches(['.', ',', ')']);
        // The login itself may still fail (a password-only server): trusting
        // is done once the key is recorded.
        let accepted = self.run(&["test", alias, "--accept-host-key", fingerprint]);
        let known = fs::read_to_string(self.known_hosts()).unwrap_or_default();
        assert!(
            !known.trim().is_empty(),
            "the host key of {alias} was not recorded:\n{}",
            text(&accepted)
        );
    }

    pub fn append_config(&self, text: &str) {
        fs::OpenOptions::new()
            .append(true)
            .open(self.home.join(".ssh/config"))
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
    }
}

/// Everything a command printed, stdout then stderr.
pub fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// The real user's `~/.ssh/known_hosts`, which no test may change.
pub struct RealHome {
    known_hosts: PathBuf,
    before: Option<Vec<u8>>,
}

impl RealHome {
    pub fn snapshot() -> Self {
        let known_hosts = current_user().1.join(".ssh/known_hosts");
        let before = fs::read(&known_hosts).ok();
        Self {
            known_hosts,
            before,
        }
    }

    pub fn assert_unchanged(&self) {
        assert_eq!(
            fs::read(&self.known_hosts).ok(),
            self.before,
            "the real {} changed",
            self.known_hosts.display()
        );
    }
}
