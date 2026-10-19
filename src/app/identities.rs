//! The process side of identity awareness: asking agents for their key lists with `ssh-add -l`,
//! fingerprinting public key files with `ssh-keygen -l`, and checking that private key files
//! exist. A private key file is only ever passed to `stat`; nothing here opens one.

use std::{
    collections::HashMap,
    ffi::OsString,
    fs,
    os::unix::fs::FileTypeExt,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{
        Arc, Condvar, Mutex, OnceLock, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use super::{App, AppError};
use crate::{
    identity::{
        self, Agent, AgentProblem, Expansion, FileFacts, HostIdentities, Identity,
        IdentityInventory, InventoryAgent, InventoryFile, PublicKey, Socket,
    },
    openssh::{OpenSsh, OpenSshError, ResolvedConfig},
};

/// How long `ssh-add` or `ssh-keygen` may run before it is killed.
pub(crate) const TOOL_DEADLINE: Duration = Duration::from_secs(3);

/// What one `App` has learnt about identities: the defaults once, each agent's listing once,
/// and each public key file's fingerprint once, until `App::forget_identities`.
pub(crate) struct IdentityState {
    /// Replaces the environment's `SSH_AUTH_SOCK` when set. Unit tests start with no agent, so
    /// the developer's own agent never shows up in their results.
    auth_sock: Option<Option<String>>,
    defaults: OnceLock<Option<Vec<String>>>,
    /// One slot per socket. Its own lock is held while the agent is asked, so a second caller
    /// waits for that answer instead of asking again.
    agents: Mutex<HashMap<PathBuf, Arc<Mutex<Option<Agent>>>>>,
    fingerprints: Mutex<HashMap<PathBuf, Option<PublicKey>>>,
    deadline: Option<Duration>,
}

impl Default for IdentityState {
    fn default() -> Self {
        Self {
            auth_sock: cfg!(test).then_some(None),
            defaults: OnceLock::new(),
            agents: Mutex::default(),
            fingerprints: Mutex::default(),
            deadline: None,
        }
    }
}

impl IdentityState {
    fn env(&self, name: &str) -> Option<String> {
        match &self.auth_sock {
            Some(socket) if name == "SSH_AUTH_SOCK" => socket.clone(),
            _ => std::env::var(name).ok(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_deadline(deadline: Duration) -> Self {
        Self {
            deadline: Some(deadline),
            ..Self::default()
        }
    }

    fn deadline(&self) -> Duration {
        self.deadline.unwrap_or(TOOL_DEADLINE)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How a tool run with a deadline ended.
enum Ran {
    Finished(Output),
    TimedOut,
    Failed(String),
}

/// Runs `command` with stdin null in its own process group, killing and reaping the group when
/// `deadline` passes.
fn run_with_deadline(openssh: &OpenSsh, command: &mut Command, deadline: Duration) -> Ran {
    command.stdin(Stdio::null());
    let expired = AtomicBool::new(false);
    let finished = (Mutex::new(false), Condvar::new());
    thread::scope(|scope| {
        scope.spawn(|| {
            let (done, signal) = &finished;
            let (done, _) = signal
                .wait_timeout_while(lock(done), deadline, |done| !*done)
                .unwrap_or_else(PoisonError::into_inner);
            if !*done {
                expired.store(true, Ordering::Relaxed);
            }
        });
        let result = openssh.cancellable_output(command, &expired);
        *lock(&finished.0) = true;
        finished.1.notify_all();
        match result {
            Ok(output) => Ran::Finished(output),
            Err(OpenSshError::Cancelled) => Ran::TimedOut,
            Err(error) => Ran::Failed(error.to_string()),
        }
    })
}

/// Lists the keys of the agent at `socket` with `ssh-add -l`, in an environment where it can
/// never ask for anything.
fn list_agent(openssh: &OpenSsh, socket: &Path, deadline: Duration) -> Agent {
    let provider = identity::provider(socket);
    let unavailable = |reason| Agent::Unavailable {
        socket: socket.to_owned(),
        provider,
        reason,
    };
    match fs::metadata(socket) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return unavailable(AgentProblem::NoSuchSocket);
        }
        Err(error) => return unavailable(AgentProblem::Refused(error.to_string())),
        Ok(metadata) if !metadata.file_type().is_socket() => {
            return unavailable(AgentProblem::NotASocket);
        }
        Ok(_) => {}
    }
    let mut command = Command::new(openssh.ssh_add_program());
    command
        .args(["-l", "-E", "sha256"])
        .env("SSH_AUTH_SOCK", socket)
        .env_remove("SSH_ASKPASS")
        .env("SSH_ASKPASS_REQUIRE", "never");
    match run_with_deadline(openssh, &mut command, deadline) {
        Ran::Finished(output) if matches!(output.status.code(), Some(0 | 1)) => Agent::Listed {
            socket: socket.to_owned(),
            provider,
            keys: identity::parse_listing(&String::from_utf8_lossy(&output.stdout)),
        },
        Ran::Finished(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            unavailable(AgentProblem::Refused(if stderr.is_empty() {
                format!("ssh-add failed ({})", output.status)
            } else {
                stderr
            }))
        }
        Ran::TimedOut => unavailable(AgentProblem::NoAnswer),
        Ran::Failed(message) => unavailable(AgentProblem::Refused(message)),
    }
}

/// The key in the public key file at `path`, read by `ssh-keygen -l`. Only a regular file named
/// `*.pub` whose first line reads as a public key is ever passed on, so a private key that is
/// misnamed, or linked to as `.pub`, never reaches `ssh-keygen`.
fn fingerprint(openssh: &OpenSsh, path: &Path, deadline: Duration) -> Option<PublicKey> {
    if !identity::is_public(path) || !is_file(path) || !starts_like_a_public_key(path) {
        return None;
    }
    let mut command = Command::new(openssh.keygen_program());
    command.args(["-l", "-E", "sha256", "-f"]).arg(path);
    match run_with_deadline(openssh, &mut command, deadline) {
        Ran::Finished(output) if output.status.success() => {
            identity::parse_listing(&String::from_utf8_lossy(&output.stdout))
                .into_iter()
                .next()
        }
        _ => None,
    }
}

/// Whether the file at `path`, which claims to be a public key, starts with a public key type.
/// Only its first word is read, one byte at a time and at most 64 bytes, and reading stops at a
/// leading `-`: of a private key file (`-----BEGIN …`), only that first byte is ever read.
fn starts_like_a_public_key(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let mut start = Vec::new();
    let mut byte = [0];
    while start.len() < 64 {
        match file.read(&mut byte) {
            Ok(1) => {
                start.push(byte[0]);
                if byte[0].is_ascii_whitespace() || start == b"-" {
                    break;
                }
            }
            _ => break,
        }
    }
    identity::looks_like_public_key(&start)
}

/// Whether a regular file exists at `path`, by `stat` alone.
fn is_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
}

fn local_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_default()
}

impl App {
    /// Uses `socket` as the agent `SSH_AUTH_SOCK` names, instead of the environment's.
    pub fn with_auth_sock(mut self, socket: Option<PathBuf>) -> Self {
        self.identities.auth_sock = Some(socket.map(|path| path.to_string_lossy().into_owned()));
        self
    }

    /// Forgets every agent listing and fingerprint, so the next look asks again.
    pub fn forget_identities(&self) {
        lock(&self.identities.agents).clear();
        lock(&self.identities.fingerprints).clear();
    }

    fn defaults(&self) -> Option<&Vec<String>> {
        self.identities
            .defaults
            .get_or_init(|| self.openssh.default_identity_files().ok())
            .as_ref()
    }

    /// The agent at `socket`, asked at most once until `forget_identities`.
    fn agent(&self, socket: Socket) -> Agent {
        let path = match socket {
            Socket::Disabled => return Agent::Disabled,
            Socket::NotConfigured { variable } => return Agent::NotConfigured { variable },
            Socket::Path(path) => path,
        };
        let slot = Arc::clone(
            lock(&self.identities.agents)
                .entry(path.clone())
                .or_default(),
        );
        let mut slot = lock(&slot);
        slot.get_or_insert_with(|| list_agent(&self.openssh, &path, self.identities.deadline()))
            .clone()
    }

    fn file_facts(&self, path: &Path) -> FileFacts {
        let public_path = identity::public_key_path(path);
        let public_key = lock(&self.identities.fingerprints)
            .get(&public_path)
            .cloned();
        let public_key = public_key.unwrap_or_else(|| {
            let key = fingerprint(&self.openssh, &public_path, self.identities.deadline());
            lock(&self.identities.fingerprints).insert(public_path, key.clone());
            key
        });
        FileFacts {
            private_exists: !identity::is_public(path) && is_file(path),
            public_key,
        }
    }

    /// What `alias`'s identities are, as `resolved` configures them, and whether each is
    /// available now.
    pub(crate) fn host_identities(&self, alias: &str, resolved: &ResolvedConfig) -> HostIdentities {
        let user = local_user();
        let remote_user = resolved.user.clone().unwrap_or_else(|| user.clone());
        let context = Expansion {
            home: self.home(),
            local_user: &user,
            // SAFETY: `getuid` has no preconditions and cannot fail.
            uid: unsafe { libc::getuid() },
            hostname: &resolved.hostname,
            remote_user: &remote_user,
            port: resolved.port.unwrap_or(22),
            alias,
            env: &|name| self.identities.env(name),
        };
        let agent = self.agent(identity::agent_socket(
            resolved.identity_agent.as_deref(),
            &|name| self.identities.env(name),
        ));
        let defaults = !resolved.identity_files.is_empty()
            && self.defaults() == Some(&resolved.identity_files);
        let identities = resolved
            .identity_files
            .iter()
            .filter(|value| !value.eq_ignore_ascii_case("none"))
            .map(|configured| match identity::expand(configured, &context) {
                Ok(path) => {
                    let facts = self.file_facts(&path);
                    Identity {
                        configured: configured.clone(),
                        availability: identity::availability(
                            identity::is_public(&path),
                            &facts,
                            agent.keys(),
                        ),
                        public_key: facts.public_key,
                        path: Some(path),
                    }
                }
                Err(token) => Identity {
                    configured: configured.clone(),
                    path: None,
                    public_key: None,
                    availability: identity::Availability::Unchecked { token },
                },
            })
            .collect();
        identity::assemble(
            agent,
            defaults,
            identities,
            resolved.identities_only,
            resolved.pkcs11_provider.clone(),
        )
    }

    /// Every identity Portico knows of: each host's (through `ssh -G`), every key pair in
    /// `~/.ssh`, and every agent those hosts or `SSH_AUTH_SOCK` name.
    pub fn identity_inventory(&self) -> Result<IdentityInventory, AppError> {
        self.identity_inventory_inner(None)
    }

    pub(crate) fn identity_inventory_cancellable(
        &self,
        cancelled: &AtomicBool,
    ) -> Result<IdentityInventory, AppError> {
        self.identity_inventory_inner(Some(cancelled))
    }

    fn identity_inventory_inner(
        &self,
        cancelled: Option<&AtomicBool>,
    ) -> Result<IdentityInventory, AppError> {
        let hosts = self.hosts()?;
        let home = self.home().to_owned();
        let mut inventory = IdentityInventory::default();
        let environment = self
            .identities
            .env("SSH_AUTH_SOCK")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        if let Some(socket) = &environment {
            inventory.agents.push(InventoryAgent {
                agent: self.agent(Socket::Path(socket.clone())),
                environment: true,
                used_by: Vec::new(),
            });
        }
        let mut files: Vec<(Option<PathBuf>, InventoryFile, bool)> = Vec::new();
        for host in &hosts.hosts {
            let resolved = match cancelled {
                Some(cancelled) => self.openssh.resolve_cancellable(&host.alias, cancelled),
                None => self.openssh.resolve(&host.alias),
            };
            let resolved = match resolved {
                Ok(resolved) => resolved,
                Err(OpenSshError::Cancelled) => return Err(OpenSshError::Cancelled.into()),
                Err(error) => {
                    inventory
                        .warnings
                        .push(format!("{}: could not resolve: {error}", host.alias));
                    continue;
                }
            };
            let identities = self.host_identities(&host.alias, &resolved);
            if let Some(socket) = identities.agent.socket() {
                match inventory
                    .agents
                    .iter_mut()
                    .find(|known| known.agent.socket() == Some(socket))
                {
                    Some(known) => known.used_by.push(host.alias.clone()),
                    None => inventory.agents.push(InventoryAgent {
                        agent: identities.agent.clone(),
                        environment: false,
                        used_by: vec![host.alias.clone()],
                    }),
                }
            }
            for identity in identities.identities {
                let key = identity.path.clone();
                let public = identity.path.as_deref().is_some_and(identity::is_public);
                match files.iter_mut().find(|(path, file, _)| match (&key, path) {
                    (Some(key), Some(path)) => key == path,
                    (None, None) => file.configured == identity.configured,
                    _ => false,
                }) {
                    Some((_, file, _)) => {
                        if !file.used_by.contains(&host.alias) {
                            file.used_by.push(host.alias.clone());
                        }
                    }
                    None => files.push((
                        key,
                        InventoryFile {
                            configured: crate::home::tilde_text(&identity.configured, &home),
                            path: identity.path,
                            public_key: identity.public_key,
                            availability: identity.availability,
                            used_by: vec![host.alias.clone()],
                        },
                        public,
                    )),
                }
            }
        }
        for path in key_files_in(&home.join(".ssh")) {
            let public_path = identity::public_key_path(&path);
            // A host may name the key, its private file or its public file.
            let known = files.iter().any(|(known, _, _)| {
                known.as_ref().is_some_and(|known| {
                    *known == path
                        || *known == public_path
                        || identity::public_key_path(known) == path
                })
            });
            if known {
                continue;
            }
            let facts = self.file_facts(&path);
            files.push((
                Some(path.clone()),
                InventoryFile {
                    configured: crate::home::tilde(&path, &home),
                    path: Some(path.clone()),
                    public_key: facts.public_key,
                    availability: identity::Availability::Missing(
                        identity::MissingReason::NoSuchFile,
                    ),
                    used_by: Vec::new(),
                },
                identity::is_public(&path),
            ));
        }
        // A file is available when any agent Portico knows holds its key.
        let listed: Vec<&Agent> = inventory
            .agents
            .iter()
            .map(|known| &known.agent)
            .filter(|agent| agent.keys().is_some())
            .collect();
        let held: Vec<PublicKey> = listed
            .iter()
            .flat_map(|agent| agent.keys().unwrap_or_default())
            .cloned()
            .collect();
        for (path, file, public) in &mut files {
            if let Some(path) = path {
                let facts = FileFacts {
                    private_exists: !*public && is_file(path),
                    public_key: file.public_key.clone(),
                };
                file.availability = identity::availability(
                    *public,
                    &facts,
                    (!listed.is_empty()).then_some(held.as_slice()),
                );
            }
        }
        inventory.files = files.into_iter().map(|(_, file, _)| file).collect();
        inventory
            .files
            .sort_by(|a, b| a.configured.cmp(&b.configured));
        Ok(inventory)
    }
}

/// The key pairs and lone public keys directly in `dir`: a private key's path when it exists
/// beside its `.pub`, otherwise the `.pub` path itself. Only `stat` touches the private files.
pub(crate) fn key_files_in(dir: &Path) -> Vec<PathBuf> {
    public_keys_in(dir)
        .into_iter()
        .map(|public| {
            let private = public.with_extension("");
            if is_file(&private) { private } else { public }
        })
        .collect()
}

/// The `*.pub` files directly in `dir`, certificates excluded, sorted.
fn public_keys_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut keys: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            let name: OsString = path.file_name().unwrap_or_default().to_owned();
            let name = name.to_string_lossy();
            name.ends_with(".pub") && !name.ends_with("-cert.pub") && is_file(path)
        })
        .collect();
    keys.sort();
    keys
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        identity::{Availability, MissingReason},
        managed::{ConfigPaths, ManagedStore},
        test_executable::write_executable,
    };
    use std::os::unix::net::UnixListener;
    use std::time::Instant;
    use tempfile::TempDir;

    struct Fixture {
        dir: TempDir,
        app: App,
    }

    impl Fixture {
        fn path(&self, name: &str) -> PathBuf {
            self.dir.path().join(name)
        }
    }

    /// An app whose `ssh -G` prints `resolved`, whose `ssh-add` runs `ssh_add`, and whose
    /// `ssh-keygen` prints one key named after the file it is given.
    fn fixture(resolved: &str, ssh_add: &str) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        fs::create_dir_all(home.join(".ssh")).unwrap();
        let ssh = dir.path().join("ssh");
        write_executable(
            &ssh,
            &format!(
                "#!/bin/sh\nif [ \"$2\" = -F ]; then printf 'identityfile ~/.ssh/id_rsa\\nidentityfile ~/.ssh/id_ed25519\\n'; exit 0; fi\nprintf '%s' '{}'\n",
                resolved.replace('\'', "'\\''")
            ),
        );
        let add = dir.path().join("ssh-add");
        write_executable(&add, &format!("#!/bin/sh\n{ssh_add}\n"));
        let keygen = dir.path().join("ssh-keygen");
        write_executable(
            &keygen,
            "#!/bin/sh\nname=$(basename \"$5\" .pub)\necho \"256 SHA256:$name $name (ED25519)\"\n",
        );
        let openssh = OpenSsh::new(ssh, "xdg-terminal-exec".into())
            .with_ssh_add(add)
            .with_keygen(keygen);
        let mut app = App::new(ManagedStore::new(ConfigPaths::from_home(&home)), openssh);
        app.identities = IdentityState::with_deadline(Duration::from_millis(500));
        Fixture { dir, app }
    }

    /// What a public key file starts with.
    const PUBLIC: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5 test\n";

    fn resolved(text: &str) -> ResolvedConfig {
        let mut config = ResolvedConfig {
            hostname: "h".into(),
            user: Some("deploy".into()),
            port: None,
            identity_files: Vec::new(),
            proxy_jump: None,
            proxy_command: None,
            proxy_use_fdpass: false,
            known_hosts_files: Vec::new(),
            identity_agent: None,
            identities_only: false,
            pkcs11_provider: None,
        };
        for line in text.lines() {
            let (key, value) = line.split_once(' ').unwrap();
            match key {
                "identityfile" => config.identity_files.push(value.into()),
                "identityagent" => config.identity_agent = Some(value.into()),
                "identitiesonly" => config.identities_only = value == "yes",
                _ => panic!("{key}"),
            }
        }
        config
    }

    fn socket(fixture: &Fixture) -> (PathBuf, UnixListener) {
        let path = fixture.path("agent.sock");
        let listener = UnixListener::bind(&path).unwrap();
        (path, listener)
    }

    #[test]
    fn classifies_a_hosts_identities_against_its_agent() {
        let log = tempfile::NamedTempFile::new().unwrap();
        let fixture = fixture(
            "",
            &format!(
                "echo \"$SSH_AUTH_SOCK|${{SSH_ASKPASS-unset}}|$SSH_ASKPASS_REQUIRE|$*\" >> '{}'\necho '256 SHA256:held held key (ED25519)'",
                log.path().display()
            ),
        );
        let (sock, _listener) = socket(&fixture);
        let ssh_dir = fixture.app.home().join(".ssh");
        fs::write(ssh_dir.join("held.pub"), PUBLIC).unwrap();
        fs::write(ssh_dir.join("file"), "").unwrap();
        fs::write(ssh_dir.join("file.pub"), PUBLIC).unwrap();
        fs::write(ssh_dir.join("lonely.pub"), PUBLIC).unwrap();
        let config = resolved(&format!(
            "identityagent {}\nidentityfile ~/.ssh/held.pub\nidentityfile ~/.ssh/file\nidentityfile ~/.ssh/lonely.pub\nidentityfile ~/.ssh/gone\nidentityfile ~/.ssh/%C",
            sock.display()
        ));
        let identities = fixture.app.host_identities("h", &config);
        let availability: Vec<_> = identities
            .identities
            .iter()
            .map(|identity| identity.availability.clone())
            .collect();
        assert_eq!(
            availability,
            [
                Availability::InAgent,
                Availability::KeyFile {
                    public_key_found: true
                },
                Availability::Missing(MissingReason::NotInAgent),
                Availability::Missing(MissingReason::NoSuchFile),
                Availability::Unchecked { token: "%C".into() },
            ]
        );
        assert!(identities.also_offered.is_empty());
        assert_eq!(
            fs::read_to_string(log.path()).unwrap(),
            format!("{}|unset|never|-l -E sha256\n", sock.display())
        );
    }

    #[test]
    fn agent_listing_is_asked_once_per_socket_until_forgotten() {
        let log = tempfile::NamedTempFile::new().unwrap();
        let fixture = fixture(
            "",
            &format!(
                "echo asked >> '{}'\necho 'The agent has no identities.'\nexit 1",
                log.path().display()
            ),
        );
        let (sock, _listener) = socket(&fixture);
        let config = resolved(&format!("identityagent {}", sock.display()));
        for _ in 0..3 {
            let identities = fixture.app.host_identities("a", &config);
            assert_eq!(identities.agent.keys(), Some(&[][..]));
        }
        assert_eq!(fs::read_to_string(log.path()).unwrap(), "asked\n");
        fixture.app.forget_identities();
        fixture.app.host_identities("b", &config);
        assert_eq!(fs::read_to_string(log.path()).unwrap(), "asked\nasked\n");
    }

    #[test]
    fn a_listing_past_the_deadline_is_killed() {
        let pid = tempfile::NamedTempFile::new().unwrap();
        let fixture = fixture(
            "",
            &format!("echo $$ > '{}'\nexec sleep 30", pid.path().display()),
        );
        let (sock, _listener) = socket(&fixture);
        let started = Instant::now();
        let identities = fixture
            .app
            .host_identities("h", &resolved(&format!("identityagent {}", sock.display())));
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(matches!(
            identities.agent,
            Agent::Unavailable {
                reason: AgentProblem::NoAnswer,
                ..
            }
        ));
        let pid: i32 = fs::read_to_string(pid.path())
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // SAFETY: signal 0 only checks whether the process exists.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        assert!(!alive, "ssh-add {pid} is still running");
    }

    #[test]
    fn reports_missing_sockets_and_refusals_without_listing() {
        let log = tempfile::NamedTempFile::new().unwrap();
        let fixture = fixture(
            "",
            &format!(
                "echo asked >> '{}'\necho 'Error connecting to agent: refused' >&2\nexit 2",
                log.path().display()
            ),
        );
        let missing = fixture
            .app
            .host_identities("h", &resolved("identityagent /nonexistent/agent.sock"));
        assert!(matches!(
            missing.agent,
            Agent::Unavailable {
                reason: AgentProblem::NoSuchSocket,
                ..
            }
        ));
        fs::write(fixture.path("plain"), "").unwrap();
        let plain = fixture.app.host_identities(
            "h",
            &resolved(&format!(
                "identityagent {}",
                fixture.path("plain").display()
            )),
        );
        assert!(matches!(
            plain.agent,
            Agent::Unavailable {
                reason: AgentProblem::NotASocket,
                ..
            }
        ));
        assert!(!log.path().exists() || fs::read_to_string(log.path()).unwrap().is_empty());
        let (sock, _listener) = socket(&fixture);
        let refused = fixture
            .app
            .host_identities("h", &resolved(&format!("identityagent {}", sock.display())));
        assert_eq!(
            refused.agent,
            Agent::Unavailable {
                socket: sock,
                provider: identity::Provider::Generic,
                reason: AgentProblem::Refused("Error connecting to agent: refused".into()),
            }
        );
        let disabled = fixture
            .app
            .host_identities("h", &resolved("identityagent none"));
        assert_eq!(disabled.agent, Agent::Disabled);
    }

    #[test]
    fn collapses_openssh_defaults() {
        let fixture = fixture("", "exit 2");
        fs::write(fixture.app.home().join(".ssh/id_ed25519"), "").unwrap();
        let identities = fixture.app.host_identities(
            "h",
            &resolved(
                "identityagent none\nidentityfile ~/.ssh/id_rsa\nidentityfile ~/.ssh/id_ed25519",
            ),
        );
        assert!(identities.defaults);
        assert_eq!(identities.identities.len(), 1);
        assert_eq!(identities.identities[0].configured, "~/.ssh/id_ed25519");
        assert_eq!(
            identities.identities[0].availability,
            Availability::KeyFile {
                public_key_found: false
            }
        );
    }

    #[test]
    fn never_fingerprints_a_private_key() {
        let log = tempfile::NamedTempFile::new().unwrap();
        let fixture = fixture("", "exit 2");
        let keygen = fixture.path("ssh-keygen");
        write_executable(
            &keygen,
            &format!("#!/bin/sh\necho \"$5\" >> '{}'\n", log.path().display()),
        );
        let ssh_dir = fixture.app.home().join(".ssh");
        fs::write(ssh_dir.join("only_private"), "").unwrap();
        fs::write(ssh_dir.join("pair"), "").unwrap();
        fs::write(ssh_dir.join("pair.pub"), PUBLIC).unwrap();
        // A private key misnamed `.pub`, and a `.pub` link to a private key, are not public keys.
        fs::write(
            ssh_dir.join("misnamed.pub"),
            "-----BEGIN OPENSSH PRIVATE KEY-----\nsecret\n",
        )
        .unwrap();
        fs::write(
            ssh_dir.join("linked"),
            "-----BEGIN OPENSSH PRIVATE KEY-----\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(ssh_dir.join("linked"), ssh_dir.join("linked.pub")).unwrap();
        let identities = fixture.app.host_identities(
            "h",
            &resolved(
                "identityagent none\nidentityfile ~/.ssh/only_private\nidentityfile ~/.ssh/pair\n\
                 identityfile ~/.ssh/misnamed.pub\nidentityfile ~/.ssh/linked",
            ),
        );
        assert_eq!(
            identities.identities[2].availability,
            Availability::Missing(MissingReason::NoSuchFile)
        );
        assert_eq!(
            identities.identities[3].availability,
            Availability::KeyFile {
                public_key_found: false
            }
        );
        assert_eq!(
            fs::read_to_string(log.path()).unwrap(),
            format!("{}\n", ssh_dir.join("pair.pub").display())
        );
    }

    #[test]
    fn inventory_merges_hosts_ssh_keys_and_agents() {
        let fixture = fixture(
            "hostname h\nidentityfile ~/.ssh/used\nidentityfile ~/.ssh/held.pub\nidentityfile ~/.ssh/vault\n",
            "echo '256 SHA256:held held (ED25519)'",
        );
        let (sock, _listener) = socket(&fixture);
        fixture.app.initialize(true).unwrap();
        let entry = fixture.app.home().join(".ssh/config");
        let mut config = fs::read_to_string(&entry).unwrap();
        config.push_str("\nHost one\n  HostName h\n\nHost two\n  HostName h\n");
        fs::write(&entry, config).unwrap();
        let ssh_dir = fixture.app.home().join(".ssh");
        fs::write(ssh_dir.join("used"), "").unwrap();
        fs::write(ssh_dir.join("held.pub"), PUBLIC).unwrap();
        fs::write(ssh_dir.join("spare"), "").unwrap();
        fs::write(ssh_dir.join("spare.pub"), PUBLIC).unwrap();
        fs::write(ssh_dir.join("spare-cert.pub"), "").unwrap();
        // `vault` is named by a host, but only its public half is on disk.
        fs::write(ssh_dir.join("vault.pub"), PUBLIC).unwrap();
        let Fixture { dir: _dir, app } = fixture;
        let app = app.with_auth_sock(Some(sock));
        let inventory = app.identity_inventory().unwrap();
        assert_eq!(inventory.agents.len(), 1);
        assert!(inventory.agents[0].environment);
        assert_eq!(inventory.agents[0].used_by, ["one", "two"]);
        let files: Vec<(&str, &str, Vec<String>)> = inventory
            .files
            .iter()
            .map(|file| {
                (
                    file.configured.as_str(),
                    file.availability.word(),
                    file.used_by.clone(),
                )
            })
            .collect();
        assert_eq!(
            files,
            [
                (
                    "~/.ssh/held.pub",
                    "in agent",
                    vec!["one".into(), "two".into()]
                ),
                ("~/.ssh/spare", "key file", vec![]),
                ("~/.ssh/used", "key file", vec!["one".into(), "two".into()]),
                ("~/.ssh/vault", "missing", vec!["one".into(), "two".into()]),
            ]
        );
    }
}
