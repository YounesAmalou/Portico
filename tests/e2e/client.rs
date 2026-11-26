//! An isolated home for the real `portico` binary and the real OpenSSH client.
//!
//! OpenSSH resolves `~` from the password database rather than `$HOME`, so a
//! temporary `$HOME` alone would still read and write the real `~/.ssh`. A
//! wrapper `ssh` first on `PATH` therefore hands OpenSSH a flattened copy of
//! the temporary `~/.ssh/config` with `~/` spelled out, and ends it with a
//! `Host *` block that keeps known hosts in the temporary home and leaves the
//! agent alone. Those defaults live in the file rather than on the command
//! line so that Portico's own `-o` options still win.
//!
//! Windows' OpenSSH gets the same flattened configuration, as its `~` need not follow
//! `USERPROFILE`. Windows runs only `.exe` files, so there the wrapper is a small program
//! (`ssh_wrapper.rs`) that does what the script does, and Win32-OpenSSH's directory comes next
//! on `PATH` for `ssh-keygen` and `ssh-add`.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    ffi::OsString,
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use tempfile::TempDir;

use crate::sshd::{Sshd, current_user};

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
        let runtime = dir.path().join("run");
        fs::create_dir_all(&runtime).unwrap();
        #[cfg(unix)]
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        assert_ne!(
            home,
            current_user().1,
            "the test home must not be the real home"
        );
        write_wrappers(&bin);
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

    /// The runtime directory Portico runs with.
    pub fn runtime(&self) -> PathBuf {
        self._dir.path().join("run")
    }

    /// Where Portico keeps the records and logs of started tunnels.
    pub fn tunnel_runtime(&self) -> PathBuf {
        if cfg!(target_os = "linux") {
            self.runtime().join("portico/tunnels")
        } else {
            // macOS and Windows have no runtime directory: records stay under the temporary
            // home.
            self.home.join(".ssh/portico/run/tunnels")
        }
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
        if cfg!(windows) {
            let ssh = crate::sshd::openssh_program("ssh");
            paths.push(ssh.parent().unwrap().to_owned());
        }
        paths.extend(std::env::split_paths(&inherited));
        let mut env: Vec<(OsString, OsString)> = vec![
            ("HOME".into(), self.home.clone().into()),
            ("PATH".into(), std::env::join_paths(paths).unwrap()),
            // Tunnel records go to a temporary runtime directory, never the real one.
            ("XDG_RUNTIME_DIR".into(), self.runtime().into()),
            // Settings go to the temporary home, never the developer's own.
            ("XDG_CONFIG_HOME".into(), self.home.join(".config").into()),
        ];
        if cfg!(windows) {
            env.extend([
                // Windows' home and settings directory, the settings where the other systems
                // put them.
                ("USERPROFILE".into(), self.home.clone().into()),
                ("APPDATA".into(), self.home.join(".config").into()),
                // The real ssh the wrapper runs.
                (
                    "PORTICO_E2E_SSH".into(),
                    crate::sshd::openssh_program("ssh").into(),
                ),
            ]);
        }
        env
    }

    pub fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(Self::binary());
        // The developer's own agent never shows up in a test's results.
        command
            .args(args)
            .envs(self.env())
            .env_remove("SSH_AUTH_SOCK")
            .stdin(Stdio::null());
        command
    }

    pub fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    /// Runs Portico and fails the test unless it succeeds, with the tunnels' ssh logs.
    pub fn run_ok(&self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "portico {args:?}: {}\n{}{}",
            output.status,
            text(&output),
            self.tunnel_logs()
        );
        output
    }

    /// Every tunnel's ssh log, for a failure message.
    pub fn tunnel_logs(&self) -> String {
        let Ok(entries) = fs::read_dir(self.tunnel_runtime()) else {
            return String::new();
        };
        let mut logs = String::new();
        for path in entries.flatten().map(|entry| entry.path()) {
            if path.extension().is_some_and(|extension| extension == "log") {
                // Escaped, so that a stray byte or a failed read shows.
                let text = match fs::read(&path) {
                    Ok(bytes) => String::from_utf8_lossy(&bytes).escape_debug().to_string(),
                    Err(error) => format!("<cannot read: {error}>"),
                };
                logs.push_str(&format!("--- {} ---\n{text}\n", path.display()));
            }
        }
        logs
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

/// Writes the wrappers that run the real OpenSSH client into `bin`.
#[cfg(unix)]
fn write_wrappers(bin: &Path) {
    use crate::executable::write_executable;
    write_executable(
        bin.join("ssh"),
        r#"#!/bin/sh
for arg do
  [ "$arg" = -F ] && exec /usr/bin/ssh "$@"
done
# Portico runs several ssh processes at once: each writes its own copy and
# renames it into place, so none reads a copy another one is writing.
flat="$HOME/.ssh/e2e-flat-config"
partial="$flat.$$"
mkdir -p "$HOME/.ssh"
if [ -f "$HOME/.ssh/config" ]; then
  sed "s|~/|$HOME/|g" "$HOME/.ssh/config" > "$partial"
else
  : > "$partial"
fi
printf '\nHost *\n  UserKnownHostsFile %s\n  GlobalKnownHostsFile /dev/null\n  IdentityAgent none\n  IdentitiesOnly yes\n' \
  "$HOME/.ssh/known_hosts" >> "$partial"
mv -f "$partial" "$flat"
exec /usr/bin/ssh -F "$flat" "$@"
"#,
    );
    write_executable(
        bin.join("ssh-keygen"),
        "#!/bin/sh\nexec /usr/bin/ssh-keygen \"$@\"\n",
    );
    write_executable(
        bin.join("ssh-add"),
        "#!/bin/sh\nexec /usr/bin/ssh-add \"$@\"\n",
    );
}

/// Puts the compiled `ssh` wrapper into `bin`; `Client::env` names the real ssh and puts
/// Win32-OpenSSH's directory, with `ssh-keygen` and `ssh-add`, after `bin` on `PATH`.
#[cfg(windows)]
fn write_wrappers(bin: &Path) {
    use std::sync::OnceLock;
    static WRAPPER: OnceLock<PathBuf> = OnceLock::new();
    let wrapper =
        WRAPPER.get_or_init(|| crate::executable::compile_program("tests/e2e/ssh_wrapper.rs"));
    fs::copy(wrapper, bin.join("ssh.exe")).unwrap();
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
