//! What Portico knows about the identities a host offers, kept apart from the keys themselves.
//!
//! Pure functions and plain data: paths, fingerprints and comments, never key material. The
//! process side (running `ssh-add -l` and `ssh-keygen -l`, checking files) lives in
//! `app::identities`. The type, field and variant names here are the contract a machine-readable
//! output will serialize, so they are kept stable.

use std::path::{Path, PathBuf};

/// A public key as `ssh-add -l` or `ssh-keygen -l` describes it: no key blob, only what
/// identifies it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicKey {
    /// `ED25519`, `RSA`, `ED25519-SK`, …, as OpenSSH names it.
    pub key_type: String,
    pub bits: u32,
    /// `SHA256:…`.
    pub fingerprint: String,
    pub comment: String,
}

/// Who runs an agent, guessed from its socket path for display only.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Provider {
    OnePassword,
    GpgAgent,
    GnomeKeyring,
    Generic,
}

impl Provider {
    pub fn name(self) -> &'static str {
        match self {
            Provider::OnePassword => "1Password",
            Provider::GpgAgent => "gpg-agent",
            Provider::GnomeKeyring => "GNOME Keyring",
            Provider::Generic => "SSH agent",
        }
    }
}

/// Why an agent's keys could not be listed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentProblem {
    /// Nothing exists at the socket path.
    NoSuchSocket,
    /// Something other than a socket exists at the path.
    NotASocket,
    /// `ssh-add` could not list the keys; its message.
    Refused(String),
    /// The listing did not finish before the deadline; the agent may be waiting to be unlocked.
    NoAnswer,
}

/// The agent a host uses, and what it holds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Agent {
    /// `IdentityAgent none`.
    Disabled,
    /// The host takes its socket from an environment variable (`SSH_AUTH_SOCK`, or another
    /// one named by `IdentityAgent $NAME`) that is not set.
    NotConfigured { variable: String },
    Unavailable {
        socket: PathBuf,
        provider: Provider,
        reason: AgentProblem,
    },
    Listed {
        socket: PathBuf,
        provider: Provider,
        keys: Vec<PublicKey>,
    },
}

impl Agent {
    /// The keys the agent listed; none when it was not listed.
    pub fn keys(&self) -> Option<&[PublicKey]> {
        match self {
            Agent::Listed { keys, .. } => Some(keys),
            _ => None,
        }
    }

    pub fn socket(&self) -> Option<&Path> {
        match self {
            Agent::Unavailable { socket, .. } | Agent::Listed { socket, .. } => Some(socket),
            _ => None,
        }
    }

    /// One line naming the agent and its state, with paths under `home` shown with `~`.
    pub fn describe(&self, home: &Path) -> String {
        match self {
            Agent::Disabled => "none (IdentityAgent none)".into(),
            Agent::NotConfigured { variable } => format!("none ({variable} is not set)"),
            Agent::Unavailable {
                socket,
                provider,
                reason,
            } => format!(
                "{}, {}, unavailable: {}",
                provider.name(),
                crate::home::tilde(socket, home),
                reason.describe()
            ),
            Agent::Listed {
                socket,
                provider,
                keys,
            } => format!(
                "{}, {}, {}",
                provider.name(),
                crate::home::tilde(socket, home),
                count(keys.len(), "key")
            ),
        }
    }
}

impl AgentProblem {
    pub fn describe(&self) -> String {
        match self {
            AgentProblem::NoSuchSocket => "no such socket".into(),
            AgentProblem::NotASocket => "not a socket".into(),
            AgentProblem::Refused(message) => message.clone(),
            AgentProblem::NoAnswer => {
                "did not answer in time (it may be waiting to be unlocked)".into()
            }
        }
    }
}

/// Why an identity is missing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MissingReason {
    /// Neither the key file nor its public key exists.
    NoSuchFile,
    /// Only the public key exists, and the agent does not hold that key.
    NotInAgent,
    /// Only the public key exists, and the agent could not be listed.
    AgentUnavailable,
}

/// Whether an identity can be used right now.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Availability {
    /// The host's agent holds the key.
    InAgent,
    /// The private key file exists. Without its public key file the agent was not checked.
    KeyFile {
        public_key_found: bool,
    },
    Missing(MissingReason),
    /// The path uses a token Portico does not expand.
    Unchecked {
        token: String,
    },
}

impl Availability {
    pub fn word(&self) -> &'static str {
        match self {
            Availability::InAgent => "in agent",
            Availability::KeyFile { .. } => "key file",
            Availability::Missing(_) => "missing",
            Availability::Unchecked { .. } => "unchecked",
        }
    }

    /// `✓`, `✗` or `?`, beside the word in the TUI.
    pub fn symbol(&self) -> char {
        match self {
            Availability::InAgent | Availability::KeyFile { .. } => '✓',
            Availability::Missing(_) => '✗',
            Availability::Unchecked { .. } => '?',
        }
    }

    /// Why, when the word alone does not say.
    pub fn reason(&self) -> Option<String> {
        match self {
            Availability::InAgent
            | Availability::KeyFile {
                public_key_found: true,
            } => None,
            Availability::KeyFile {
                public_key_found: false,
            } => Some("agent not checked: no public key file".into()),
            Availability::Missing(MissingReason::NoSuchFile) => Some("no such file".into()),
            Availability::Missing(MissingReason::NotInAgent) => {
                Some("public key only, and the agent does not hold it".into())
            }
            Availability::Missing(MissingReason::AgentUnavailable) => {
                Some("public key only; agent unavailable".into())
            }
            Availability::Unchecked { token } => Some(format!("Portico does not expand {token}")),
        }
    }
}

/// One IdentityFile of a host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Identity {
    /// The value as `ssh -G` printed it.
    pub configured: String,
    /// The expanded path, when every token in it could be expanded.
    pub path: Option<PathBuf>,
    /// The public key that pairs with it, from its `.pub` file.
    pub public_key: Option<PublicKey>,
    pub availability: Availability,
}

impl Identity {
    /// `<status> (<details>)`: the availability, its reason and the key, as `portico show`
    /// prints them.
    pub fn describe(&self) -> String {
        let details = self
            .availability
            .reason()
            .into_iter()
            .chain(self.public_key.as_ref().map(describe_key))
            .collect::<Vec<_>>();
        if details.is_empty() {
            self.availability.word().into()
        } else {
            format!("{} ({})", self.availability.word(), details.join("; "))
        }
    }
}

/// `ED25519 SHA256:… "comment"`.
pub fn describe_key(key: &PublicKey) -> String {
    format!("{} {} \"{}\"", key.key_type, key.fingerprint, key.comment)
}

/// Everything Portico knows about the identities one host offers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostIdentities {
    pub agent: Agent,
    /// The IdentityFile list is OpenSSH's built-in default list.
    pub defaults: bool,
    /// The host's identities in order; with `defaults`, only the default files that exist.
    pub identities: Vec<Identity>,
    pub identities_only: bool,
    /// Agent keys ssh offers besides `identities`; empty with `identities_only`.
    pub also_offered: Vec<PublicKey>,
    pub pkcs11_provider: Option<String>,
}

/// An agent in the identity inventory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InventoryAgent {
    pub agent: Agent,
    /// It is the agent `SSH_AUTH_SOCK` names.
    pub environment: bool,
    /// The aliases whose configuration uses it.
    pub used_by: Vec<String>,
}

/// An identity file in the identity inventory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InventoryFile {
    /// The first spelling seen, with `$HOME` shown as `~`.
    pub configured: String,
    pub path: Option<PathBuf>,
    pub public_key: Option<PublicKey>,
    /// Checked against every agent in the inventory.
    pub availability: Availability,
    /// The aliases that use it; empty for a key found only in `~/.ssh`.
    pub used_by: Vec<String>,
}

/// Every identity Portico knows of: the agents, and the identity files hosts reference or
/// `~/.ssh` holds.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IdentityInventory {
    pub agents: Vec<InventoryAgent>,
    pub files: Vec<InventoryFile>,
    /// Hosts that could not be resolved, as `alias: reason`.
    pub warnings: Vec<String>,
}

/// What `%` tokens, `~` and `${VAR}` expand to for one host.
pub struct Expansion<'a> {
    pub home: &'a Path,
    pub local_user: &'a str,
    pub uid: u32,
    pub hostname: &'a str,
    pub remote_user: &'a str,
    pub port: u16,
    pub alias: &'a str,
    pub env: &'a dyn Fn(&str) -> Option<String>,
}

/// Expands `value` the way ssh expands an IdentityFile. Returns the first token Portico does
/// not expand (`%C`, `${UNSET}`, `~other`) as the error.
pub fn expand(value: &str, context: &Expansion<'_>) -> Result<PathBuf, String> {
    let mut expanded = String::new();
    let mut rest = value;
    if let Some(after) = rest.strip_prefix('~') {
        if after.is_empty() || after.starts_with('/') {
            expanded.push_str(&context.home.to_string_lossy());
            rest = after;
        } else {
            let user = after.split('/').next().unwrap_or(after);
            return Err(format!("~{user}"));
        }
    }
    let mut chars = rest.char_indices().peekable();
    while let Some((index, character)) = chars.next() {
        match character {
            '%' => {
                let Some((_, token)) = chars.next() else {
                    return Err("%".into());
                };
                match token {
                    '%' => expanded.push('%'),
                    'd' => expanded.push_str(&context.home.to_string_lossy()),
                    'u' => expanded.push_str(context.local_user),
                    'i' => expanded.push_str(&context.uid.to_string()),
                    'h' => expanded.push_str(context.hostname),
                    'r' => expanded.push_str(context.remote_user),
                    'p' => expanded.push_str(&context.port.to_string()),
                    'n' => expanded.push_str(context.alias),
                    other => return Err(format!("%{other}")),
                }
            }
            '$' if rest[index + 1..].starts_with('{') => {
                let start = index + 2;
                let Some(length) = rest[start..].find('}') else {
                    return Err(rest[index..].into());
                };
                let name = &rest[start..start + length];
                match (context.env)(name) {
                    Some(value) => expanded.push_str(&value),
                    None => return Err(format!("${{{name}}}")),
                }
                while chars.next_if(|(at, _)| *at <= start + length).is_some() {}
            }
            other => expanded.push(other),
        }
    }
    Ok(PathBuf::from(expanded))
}

/// Where a host's agent listens, from its `identityagent` value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Socket {
    Disabled,
    /// The environment variable that should name the socket is not set.
    NotConfigured {
        variable: String,
    },
    Path(PathBuf),
}

/// The socket `identity_agent` (as `ssh -G` printed it, or `None` when absent) names: the
/// environment's `SSH_AUTH_SOCK` when absent or `SSH_AUTH_SOCK`, another variable for `$NAME`,
/// nothing for `none`, and otherwise the value itself. `ssh -G` prints IdentityAgent already
/// expanded (`~`, `%` tokens and `${VAR}`), except `$NAME`, so the value is never expanded again.
pub fn agent_socket(identity_agent: Option<&str>, env: &dyn Fn(&str) -> Option<String>) -> Socket {
    let from_env = |name: &str| {
        env(name).filter(|value| !value.is_empty()).map_or_else(
            || Socket::NotConfigured {
                variable: name.into(),
            },
            |value| Socket::Path(value.into()),
        )
    };
    match identity_agent {
        None | Some("SSH_AUTH_SOCK") => from_env("SSH_AUTH_SOCK"),
        Some(value) if value.eq_ignore_ascii_case("none") => Socket::Disabled,
        Some(value) if value.starts_with('$') && !value.starts_with("${") => from_env(&value[1..]),
        Some(value) => Socket::Path(value.into()),
    }
}

/// The agent's friendly name, from its socket path.
pub fn provider(socket: &Path) -> Provider {
    let text = socket.to_string_lossy();
    let name = socket.file_name().map(|name| name.to_string_lossy());
    if text.contains("/.1password/") || text.contains("com.1password") {
        Provider::OnePassword
    } else if name.as_deref() == Some("S.gpg-agent.ssh") {
        Provider::GpgAgent
    } else if text.contains("/keyring/") {
        Provider::GnomeKeyring
    } else {
        Provider::Generic
    }
}

/// The keys in `ssh-add -l` or `ssh-keygen -l` output: `<bits> <fingerprint> <comment> (<TYPE>)`
/// per line. Other lines (`The agent has no identities.`) are skipped.
pub fn parse_listing(text: &str) -> Vec<PublicKey> {
    text.lines().filter_map(parse_key_line).collect()
}

fn parse_key_line(line: &str) -> Option<PublicKey> {
    let line = line.trim();
    let (bits, rest) = line.split_once(char::is_whitespace)?;
    let bits = bits.parse().ok()?;
    let rest = rest.trim_start();
    let (fingerprint, rest) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    if !fingerprint.contains(':') {
        return None;
    }
    let rest = rest.trim();
    let open = rest.rfind('(')?;
    let key_type = rest[open + 1..].strip_suffix(')')?;
    if key_type.is_empty() {
        return None;
    }
    Some(PublicKey {
        key_type: key_type.into(),
        bits,
        fingerprint: fingerprint.into(),
        comment: rest[..open].trim_end().into(),
    })
}

/// Whether `line`, the first line of a file, starts like an OpenSSH public key: its first word
/// is a key type such as `ssh-ed25519`, `ecdsa-sha2-nistp256` or `sk-ssh-ed25519@openssh.com`.
/// A private key's `-----BEGIN` header never does.
pub fn looks_like_public_key(line: &[u8]) -> bool {
    let word = line
        .split(|byte| byte.is_ascii_whitespace())
        .next()
        .unwrap_or_default();
    ["ssh-", "ecdsa-", "sk-", "webauthn-"]
        .iter()
        .any(|prefix| word.starts_with(prefix.as_bytes()) && word.len() > prefix.len())
        && line.iter().any(|byte| byte.is_ascii_whitespace())
}

/// Whether an IdentityFile names a public key, which selects an agent key and cannot sign.
pub fn is_public(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension == "pub")
}

/// The public key file that pairs with an IdentityFile: itself for a `.pub` path, otherwise the
/// path with `.pub` appended.
pub fn public_key_path(path: &Path) -> PathBuf {
    if is_public(path) {
        path.to_owned()
    } else {
        let mut name = path.as_os_str().to_owned();
        name.push(".pub");
        name.into()
    }
}

/// What was found on disk for one identity: whether its private key file exists, and its
/// public key file's key, when there is one.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FileFacts {
    pub private_exists: bool,
    pub public_key: Option<PublicKey>,
}

/// Whether an identity is available, given what is on disk and the keys of the agent it would
/// use (`None` when that agent was not listed).
pub fn availability(
    public: bool,
    facts: &FileFacts,
    agent_keys: Option<&[PublicKey]>,
) -> Availability {
    if let Some(key) = &facts.public_key {
        match agent_keys {
            Some(keys) if keys.iter().any(|held| held.fingerprint == key.fingerprint) => {
                return Availability::InAgent;
            }
            _ if facts.private_exists && !public => {
                return Availability::KeyFile {
                    public_key_found: true,
                };
            }
            Some(_) => return Availability::Missing(MissingReason::NotInAgent),
            None => return Availability::Missing(MissingReason::AgentUnavailable),
        }
    }
    if facts.private_exists && !public {
        Availability::KeyFile {
            public_key_found: false,
        }
    } else {
        Availability::Missing(MissingReason::NoSuchFile)
    }
}

/// Puts a host's identities together. With `defaults`, identities whose files do not exist are
/// dropped, as ssh skips them silently. Agent keys not matched by an identity are offered too,
/// unless `identities_only`.
pub fn assemble(
    agent: Agent,
    defaults: bool,
    identities: Vec<Identity>,
    identities_only: bool,
    pkcs11_provider: Option<String>,
) -> HostIdentities {
    let identities: Vec<Identity> = identities
        .into_iter()
        .filter(|identity| {
            !defaults || identity.availability != Availability::Missing(MissingReason::NoSuchFile)
        })
        .collect();
    let also_offered = if identities_only {
        Vec::new()
    } else {
        agent
            .keys()
            .unwrap_or_default()
            .iter()
            .filter(|key| {
                !identities.iter().any(|identity| {
                    identity
                        .public_key
                        .as_ref()
                        .is_some_and(|own| own.fingerprint == key.fingerprint)
                })
            })
            .cloned()
            .collect()
    };
    HostIdentities {
        agent,
        defaults,
        identities,
        identities_only,
        also_offered,
        pkcs11_provider: pkcs11_provider.filter(|provider| !provider.eq_ignore_ascii_case("none")),
    }
}

/// `SHA256:` and the first 8 characters of the hash, then `…`.
pub fn short_fingerprint(fingerprint: &str) -> String {
    let (prefix, hash) = fingerprint.split_once(':').unwrap_or(("", fingerprint));
    if hash.chars().count() <= 8 {
        return fingerprint.into();
    }
    let kept: String = hash.chars().take(8).collect();
    if prefix.is_empty() {
        format!("{kept}…")
    } else {
        format!("{prefix}:{kept}…")
    }
}

/// `1 key`, `3 keys`.
pub fn count(number: usize, noun: &str) -> String {
    if number == 1 {
        format!("1 {noun}")
    } else {
        format!("{number} {noun}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(name: &str) -> Option<String> {
        match name {
            "SSH_AUTH_SOCK" => Some("/run/agent.sock".into()),
            "KEYS" => Some("/keys".into()),
            "EMPTY" => Some(String::new()),
            _ => None,
        }
    }

    fn context() -> Expansion<'static> {
        Expansion {
            home: Path::new("/home/me"),
            local_user: "me",
            uid: 1000,
            hostname: "dev.example",
            remote_user: "deploy",
            port: 2222,
            alias: "dev",
            env: &env,
        }
    }

    fn key(fingerprint: &str) -> PublicKey {
        PublicKey {
            key_type: "ED25519".into(),
            bits: 256,
            fingerprint: fingerprint.into(),
            comment: "c".into(),
        }
    }

    #[test]
    fn expands_home_tokens_and_variables() {
        let context = context();
        assert_eq!(
            expand("~/.ssh/id", &context).unwrap(),
            Path::new("/home/me/.ssh/id")
        );
        assert_eq!(expand("~", &context).unwrap(), Path::new("/home/me"));
        assert_eq!(
            expand("%d/k/%u-%i-%h-%r-%p-%n-100%%", &context).unwrap(),
            Path::new("/home/me/k/me-1000-dev.example-deploy-2222-dev-100%")
        );
        assert_eq!(
            expand("${KEYS}/a${KEYS}", &context).unwrap(),
            Path::new("/keys/a/keys")
        );
        assert_eq!(expand("/abs/id", &context).unwrap(), Path::new("/abs/id"));
        assert_eq!(expand("a$b", &context).unwrap(), Path::new("a$b"));
    }

    #[test]
    fn leaves_unknown_tokens_unchecked() {
        let context = context();
        assert_eq!(expand("~/.ssh/%C", &context).unwrap_err(), "%C");
        assert_eq!(expand("%k", &context).unwrap_err(), "%k");
        assert_eq!(expand("${UNSET}/id", &context).unwrap_err(), "${UNSET}");
        assert_eq!(expand("${OPEN", &context).unwrap_err(), "${OPEN");
        assert_eq!(expand("~bob/.ssh/id", &context).unwrap_err(), "~bob");
        assert_eq!(expand("trailing%", &context).unwrap_err(), "%");
    }

    #[test]
    fn resolves_the_agent_socket() {
        let path = |value: &str| Socket::Path(value.into());
        let unset = |variable: &str| Socket::NotConfigured {
            variable: variable.into(),
        };
        assert_eq!(agent_socket(None, &env), path("/run/agent.sock"));
        assert_eq!(
            agent_socket(Some("SSH_AUTH_SOCK"), &env),
            path("/run/agent.sock")
        );
        assert_eq!(agent_socket(Some("none"), &env), Socket::Disabled);
        assert_eq!(agent_socket(Some("$KEYS"), &env), path("/keys"));
        assert_eq!(agent_socket(Some("$EMPTY"), &env), unset("EMPTY"));
        assert_eq!(agent_socket(Some("$UNSET"), &env), unset("UNSET"));
        // ssh -G has already expanded everything else: `%%` printed as `%` stays as it is.
        assert_eq!(
            agent_socket(Some("/run/a%b.sock"), &env),
            path("/run/a%b.sock")
        );
        assert_eq!(
            agent_socket(Some("/home/me/.1password/agent.sock"), &env),
            path("/home/me/.1password/agent.sock")
        );
        let nothing = |_: &str| None;
        assert_eq!(agent_socket(None, &nothing), unset("SSH_AUTH_SOCK"));
    }

    #[test]
    fn names_known_providers() {
        assert_eq!(
            provider(Path::new("/home/me/.1password/agent.sock")),
            Provider::OnePassword
        );
        assert_eq!(
            provider(Path::new(
                "/Users/me/Library/Group Containers/2BUA8C4S2C.com.1password/t/agent.sock"
            )),
            Provider::OnePassword
        );
        assert_eq!(
            provider(Path::new("/run/user/1000/gnupg/S.gpg-agent.ssh")),
            Provider::GpgAgent
        );
        assert_eq!(
            provider(Path::new("/run/user/1000/keyring/ssh")),
            Provider::GnomeKeyring
        );
        assert_eq!(
            provider(Path::new("/tmp/ssh-XXXX/agent.123")),
            Provider::Generic
        );
    }

    #[test]
    fn parses_listings() {
        let text = "256 SHA256:abc my key with spaces (ED25519)\n\
                    The agent has no identities.\n\
                    3072 SHA256:def  (RSA)\n\
                    256 SHA256:ghi user@host (ED25519-SK)\n\
                    256 SHA256:jkl hybrid (MLDSA44-ED25519)\n\
                    garbage line\n";
        let keys = parse_listing(text);
        assert_eq!(keys.len(), 4);
        assert_eq!(keys[0].comment, "my key with spaces");
        assert_eq!(keys[0].fingerprint, "SHA256:abc");
        assert_eq!(keys[0].bits, 256);
        assert_eq!(keys[1].comment, "");
        assert_eq!(keys[1].key_type, "RSA");
        assert_eq!(keys[2].key_type, "ED25519-SK");
        assert_eq!(keys[3].key_type, "MLDSA44-ED25519");
        assert!(parse_listing("The agent has no identities.\n").is_empty());
    }

    #[test]
    fn recognises_public_key_lines() {
        for line in [
            "ssh-ed25519 AAAAC3Nz me@host",
            "ssh-rsa AAAAB3",
            "ecdsa-sha2-nistp256 AAAAE2",
            "sk-ssh-ed25519@openssh.com AAAA",
            "ssh-mldsa44-ed25519 AAAA",
        ] {
            assert!(looks_like_public_key(line.as_bytes()), "{line}");
        }
        for line in [
            "-----BEGIN OPENSSH PRIVATE KEY-----",
            "-----BEGIN RSA PRIVATE KEY-----",
            "",
            "ssh-",
            "ssh-ed25519",
            "garbage",
        ] {
            assert!(!looks_like_public_key(line.as_bytes()), "{line}");
        }
    }

    #[test]
    fn pairs_public_key_paths() {
        assert_eq!(public_key_path(Path::new("/k/id")), Path::new("/k/id.pub"));
        assert_eq!(
            public_key_path(Path::new("/k/gh.pub")),
            Path::new("/k/gh.pub")
        );
        assert!(is_public(Path::new("/k/gh.pub")));
        assert!(!is_public(Path::new("/k/id")));
    }

    #[test]
    fn classifies_each_availability() {
        let held = [key("SHA256:a")];
        let with_key = |private_exists| FileFacts {
            private_exists,
            public_key: Some(key("SHA256:a")),
        };
        let other_key = FileFacts {
            private_exists: false,
            public_key: Some(key("SHA256:b")),
        };
        assert_eq!(
            availability(false, &with_key(true), Some(&held)),
            Availability::InAgent
        );
        assert_eq!(
            availability(true, &with_key(false), Some(&held)),
            Availability::InAgent
        );
        assert_eq!(
            availability(false, &with_key(true), None),
            Availability::KeyFile {
                public_key_found: true
            }
        );
        assert_eq!(
            availability(
                false,
                &FileFacts {
                    private_exists: true,
                    public_key: None
                },
                Some(&held)
            ),
            Availability::KeyFile {
                public_key_found: false
            }
        );
        assert_eq!(
            availability(true, &other_key, Some(&held)),
            Availability::Missing(MissingReason::NotInAgent)
        );
        assert_eq!(
            availability(true, &other_key, None),
            Availability::Missing(MissingReason::AgentUnavailable)
        );
        assert_eq!(
            availability(false, &FileFacts::default(), Some(&held)),
            Availability::Missing(MissingReason::NoSuchFile)
        );
        // A public key alone can never sign, even when the "private" path is itself a file.
        assert_eq!(
            availability(
                true,
                &FileFacts {
                    private_exists: true,
                    public_key: None
                },
                None
            ),
            Availability::Missing(MissingReason::NoSuchFile)
        );
    }

    fn identity(configured: &str, availability: Availability, key: Option<PublicKey>) -> Identity {
        Identity {
            configured: configured.into(),
            path: Some(configured.into()),
            public_key: key,
            availability,
        }
    }

    fn listed(keys: Vec<PublicKey>) -> Agent {
        Agent::Listed {
            socket: "/run/agent.sock".into(),
            provider: Provider::Generic,
            keys,
        }
    }

    #[test]
    fn assembles_defaults_and_offered_keys() {
        let identities = vec![
            identity(
                "~/.ssh/id_rsa",
                Availability::Missing(MissingReason::NoSuchFile),
                None,
            ),
            identity(
                "~/.ssh/id_ed25519",
                Availability::InAgent,
                Some(key("SHA256:a")),
            ),
        ];
        let agent = listed(vec![key("SHA256:a"), key("SHA256:b")]);
        let host = assemble(agent.clone(), true, identities.clone(), false, None);
        assert_eq!(host.identities.len(), 1);
        assert_eq!(host.identities[0].configured, "~/.ssh/id_ed25519");
        assert_eq!(host.also_offered, [key("SHA256:b")]);

        let configured = assemble(
            agent.clone(),
            false,
            identities.clone(),
            false,
            Some("none".into()),
        );
        assert_eq!(configured.identities.len(), 2);
        assert_eq!(configured.pkcs11_provider, None);

        let only = assemble(agent, false, identities, true, Some("/p11.so".into()));
        assert!(only.also_offered.is_empty());
        assert_eq!(only.pkcs11_provider.as_deref(), Some("/p11.so"));

        let disabled = assemble(Agent::Disabled, false, Vec::new(), false, None);
        assert!(disabled.also_offered.is_empty());
    }

    #[test]
    fn describes_identities_and_agents() {
        let home = Path::new("/home/me");
        assert_eq!(
            identity("x", Availability::InAgent, Some(key("SHA256:a"))).describe(),
            "in agent (ED25519 SHA256:a \"c\")"
        );
        assert_eq!(
            identity("x", Availability::Missing(MissingReason::NoSuchFile), None).describe(),
            "missing (no such file)"
        );
        assert_eq!(
            identity(
                "x",
                Availability::KeyFile {
                    public_key_found: true
                },
                Some(key("SHA256:a"))
            )
            .describe(),
            "key file (ED25519 SHA256:a \"c\")"
        );
        assert_eq!(
            identity("x", Availability::Unchecked { token: "%C".into() }, None).describe(),
            "unchecked (Portico does not expand %C)"
        );
        assert_eq!(
            Agent::Listed {
                socket: "/home/me/.1password/agent.sock".into(),
                provider: Provider::OnePassword,
                keys: vec![key("SHA256:a")],
            }
            .describe(home),
            "1Password, ~/.1password/agent.sock, 1 key"
        );
        assert_eq!(
            Agent::Unavailable {
                socket: "/run/a".into(),
                provider: Provider::Generic,
                reason: AgentProblem::NoSuchSocket,
            }
            .describe(home),
            "SSH agent, /run/a, unavailable: no such socket"
        );
        assert_eq!(Agent::Disabled.describe(home), "none (IdentityAgent none)");
        assert_eq!(
            Agent::NotConfigured {
                variable: "FOO".into()
            }
            .describe(home),
            "none (FOO is not set)"
        );
    }

    #[test]
    fn shortens_fingerprints() {
        assert_eq!(
            short_fingerprint("SHA256:NQlIU/kfZu3WkN47"),
            "SHA256:NQlIU/kf…"
        );
        assert_eq!(short_fingerprint("SHA256:abc"), "SHA256:abc");
        assert_eq!(count(1, "key"), "1 key");
        assert_eq!(count(0, "key"), "0 keys");
    }
}
