use std::{path::Path, process::Command, sync::atomic::AtomicBool};

use super::{OpenSsh, OpenSshError};
use crate::domain::probe::Route;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedConfig {
    pub hostname: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_files: Vec<String>,
    pub proxy_jump: Option<String>,
    pub proxy_command: Option<String>,
    /// `ProxyUseFdpass`: the `ProxyCommand` hands ssh its connected socket.
    pub proxy_use_fdpass: bool,
    pub known_hosts_files: Vec<String>,
    /// `IdentityAgent` as ssh printed it; `None` when absent.
    pub identity_agent: Option<String>,
    pub identities_only: bool,
    pub pkcs11_provider: Option<String>,
}

impl ResolvedConfig {
    /// How ssh reaches the host: through its jump host or proxy command, if either is set.
    pub fn route(&self) -> Route<'_> {
        fn set(value: &Option<String>) -> Option<&str> {
            value
                .as_deref()
                .filter(|value| !value.eq_ignore_ascii_case("none"))
        }
        let jump = set(&self.proxy_jump);
        if jump.is_none() && set(&self.proxy_command).is_none() {
            return Route::Direct;
        }
        Route::Proxied {
            jump,
            hostname: &self.hostname,
            port: self.port.unwrap_or(22),
            fd_passing: self.proxy_use_fdpass,
        }
    }
}

impl OpenSsh {
    /// The identity files OpenSSH tries when none is configured, read with no configuration
    /// file at all.
    pub fn default_identity_files(&self) -> Result<Vec<String>, OpenSshError> {
        let text = self.effective(Path::new("none"), "portico-defaults", None)?;
        Ok(text
            .lines()
            .filter_map(|line| line.split_once(char::is_whitespace))
            .filter(|(key, _)| key.eq_ignore_ascii_case("identityfile"))
            .map(|(_, value)| value.trim_start().to_owned())
            .collect())
    }

    pub fn validate(&self, entry: &Path, aliases: &[String]) -> Result<(), OpenSshError> {
        self.validate_inner(entry, aliases, None)
    }

    pub(crate) fn validate_cancellable(
        &self,
        entry: &Path,
        aliases: &[String],
        cancelled: &AtomicBool,
    ) -> Result<(), OpenSshError> {
        self.validate_inner(entry, aliases, Some(cancelled))
    }

    fn validate_inner(
        &self,
        entry: &Path,
        aliases: &[String],
        cancelled: Option<&AtomicBool>,
    ) -> Result<(), OpenSshError> {
        if aliases.is_empty() {
            self.validate_alias(entry, "portico-validation", cancelled)?;
        } else {
            for alias in aliases {
                self.validate_alias(entry, alias, cancelled)?;
            }
        }
        Ok(())
    }

    /// The raw `ssh -G` output for `alias` when reading `entry`, for comparing
    /// effective configurations.
    pub(crate) fn effective(
        &self,
        entry: &Path,
        alias: &str,
        cancelled: Option<&AtomicBool>,
    ) -> Result<String, OpenSshError> {
        let mut command = Command::new(&self.ssh);
        command.arg("-G").arg("-F").arg(entry);
        let output = self.checked_output(command.arg("--").arg(alias), cancelled)?;
        String::from_utf8(output.stdout)
            .map_err(|error| OpenSshError::InvalidOutput(error.to_string()))
    }

    pub fn resolve(&self, alias: &str) -> Result<ResolvedConfig, OpenSshError> {
        self.resolve_inner(alias, None)
    }

    pub(crate) fn resolve_cancellable(
        &self,
        alias: &str,
        cancelled: &AtomicBool,
    ) -> Result<ResolvedConfig, OpenSshError> {
        self.resolve_inner(alias, Some(cancelled))
    }

    fn resolve_inner(
        &self,
        alias: &str,
        cancelled: Option<&AtomicBool>,
    ) -> Result<ResolvedConfig, OpenSshError> {
        self.resolve_with(None, alias, cancelled)
    }

    /// Resolves `alias` with `ssh -G`, reading `entry` instead of the user's
    /// configuration when given.
    pub(crate) fn resolve_with(
        &self,
        entry: Option<&Path>,
        alias: &str,
        cancelled: Option<&AtomicBool>,
    ) -> Result<ResolvedConfig, OpenSshError> {
        let mut command = Command::new(&self.ssh);
        command.arg("-G");
        if let Some(entry) = entry {
            command.arg("-F").arg(entry);
        }
        let output = self.checked_output(command.arg("--").arg(alias), cancelled)?;
        let text = String::from_utf8(output.stdout)
            .map_err(|error| OpenSshError::InvalidOutput(error.to_string()))?;
        let mut hostname = None;
        let mut user = None;
        let mut port = None;
        let mut identity_files = Vec::new();
        let mut proxy_jump = None;
        let mut proxy_command = None;
        let mut proxy_use_fdpass = false;
        let mut known_hosts_files = Vec::new();
        let mut identity_agent = None;
        let mut identities_only = false;
        let mut pkcs11_provider = None;
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
            } else if key.eq_ignore_ascii_case("proxycommand") {
                proxy_command = Some(value.into());
            } else if key.eq_ignore_ascii_case("proxyusefdpass") {
                proxy_use_fdpass = value.eq_ignore_ascii_case("yes");
            } else if key.eq_ignore_ascii_case("userknownhostsfile") {
                known_hosts_files = value.split_whitespace().map(String::from).collect();
            } else if key.eq_ignore_ascii_case("identityagent") {
                identity_agent = Some(value.into());
            } else if key.eq_ignore_ascii_case("identitiesonly") {
                identities_only = value.eq_ignore_ascii_case("yes");
            } else if key.eq_ignore_ascii_case("pkcs11provider") {
                pkcs11_provider = Some(value.into());
            }
        }
        Ok(ResolvedConfig {
            hostname: hostname
                .ok_or_else(|| OpenSshError::InvalidOutput("missing hostname".into()))?,
            user,
            port,
            identity_files,
            proxy_jump,
            proxy_command,
            proxy_use_fdpass,
            known_hosts_files,
            identity_agent,
            identities_only,
            pkcs11_provider,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::Duration,
    };

    use super::*;
    use crate::domain::openssh::tests::{fake_failure, fake_resolver, fake_ssh, wait_for_contents};
    use crate::test_executable::write_executable;

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
        assert_eq!(
            resolved.route(),
            Route::Proxied {
                jump: Some("bastion"),
                hostname: "dev.example",
                port: 2222,
                fd_passing: false,
            }
        );
    }

    #[test]
    fn parses_identity_settings_and_defaults() {
        let fake = fake_resolver(
            "hostname h\nidentitiesonly yes\nidentityagent /home/me/.1password/agent.sock\n\
             pkcs11provider /usr/lib/p11.so\nidentityfile ~/.ssh/a.pub\nidentityfile ~/.ssh/b\n",
        );
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
        let resolved = openssh.resolve("h").unwrap();
        assert!(resolved.identities_only);
        assert_eq!(
            resolved.identity_agent.as_deref(),
            Some("/home/me/.1password/agent.sock")
        );
        assert_eq!(resolved.pkcs11_provider.as_deref(), Some("/usr/lib/p11.so"));
        assert_eq!(resolved.identity_files, ["~/.ssh/a.pub", "~/.ssh/b"]);

        assert_eq!(
            openssh.default_identity_files().unwrap(),
            ["~/.ssh/a.pub", "~/.ssh/b"]
        );
        assert_eq!(
            fs::read_to_string(fake.log()).unwrap(),
            "-G\n-F\nnone\n--\nportico-defaults\n"
        );
    }

    #[test]
    fn a_proxy_command_or_none_decides_the_route() {
        let resolve = |output: &str| {
            let fake = fake_resolver(output);
            OpenSsh::new(fake.path(), "xdg-terminal-exec".into())
                .resolve("dev")
                .unwrap()
        };
        let command = resolve("hostname h\nproxycommand nc %h %p\n");
        assert_eq!(command.proxy_command.as_deref(), Some("nc %h %p"));
        assert_eq!(
            command.route(),
            Route::Proxied {
                jump: None,
                hostname: "h",
                port: 22,
                fd_passing: false,
            }
        );
        let fd_passing = resolve("hostname h\nproxycommand nc %h %p\nproxyusefdpass yes\n");
        assert_eq!(
            fd_passing.route(),
            Route::Proxied {
                jump: None,
                hostname: "h",
                port: 22,
                fd_passing: true,
            }
        );
        let none = resolve("hostname h\nport 2200\nproxyjump none\nproxycommand none\n");
        assert_eq!(none.route(), Route::Direct);
        assert_eq!(resolve("hostname h\n").route(), Route::Direct);
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
    fn cancellation_stops_descendant_holding_pipes_after_resolver_exits() {
        let fake = fake_ssh();
        let marker = fake.log.with_file_name("parent-exited");
        write_executable(
            &fake.path,
            &format!(
                "#!/bin/sh\nif [ \"$1\" = '-G' ]; then\n  sleep 3 &\n  printf 'hostname dev.example\\n'\n  : > '{}'\nfi\n",
                marker.display(),
            ),
        );
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
        let cancelled = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&cancelled);
        let cancel = thread::spawn(move || {
            wait_for_contents(&marker, "");
            thread::sleep(Duration::from_millis(100));
            signal.store(true, Ordering::Relaxed);
        });
        let started = std::time::Instant::now();

        let result = openssh.resolve_cancellable("dev", &cancelled);

        cancel.join().unwrap();
        assert!(matches!(result, Err(OpenSshError::Cancelled)));
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "cancellation blocked for {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn resolve_with_reads_the_known_hosts_files() {
        let fake = fake_resolver(
            "hostname h\nuserknownhostsfile ~/.ssh/known_hosts ~/.ssh/known_hosts2\n",
        );
        let entry = fake.log().with_file_name("entry config");
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());

        let resolved = openssh.resolve_with(Some(&entry), "dev", None).unwrap();

        assert_eq!(
            resolved.known_hosts_files,
            ["~/.ssh/known_hosts", "~/.ssh/known_hosts2"]
        );
        assert_eq!(
            fs::read_to_string(fake.log()).unwrap(),
            format!("-G\n-F\n{}\n--\ndev\n", entry.display())
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

    #[test]
    fn effective_returns_raw_output_of_ssh_g_with_the_entry() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join("ssh");
        write_executable(&ssh, "#!/bin/sh\nprintf '%s\\n' \"$@\"\n");
        let entry = dir.path().join("config");
        let openssh = OpenSsh::new(ssh, "xdg-terminal-exec".into());

        let output = openssh.effective(&entry, "dev", None).unwrap();

        assert_eq!(output, format!("-G\n-F\n{}\n--\ndev\n", entry.display()));
    }
}
