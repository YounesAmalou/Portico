use std::{
    ffi::OsString,
    fmt,
    path::{Path, PathBuf},
    time::Duration,
};

pub const CONNECT_TIMEOUT_SECS: u32 = 5;
pub const TEST_DEADLINE: Duration = Duration::from_secs(15);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Authenticated {
    pub host: String,
    pub address: String,
    pub port: u16,
    pub method: String,
    /// The jump host the login went through, or `proxy` for a `ProxyCommand`.
    pub via: Option<String>,
}

/// How ssh reaches the host, as `ssh -G` resolves it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Route<'a> {
    Direct,
    /// Through `ProxyJump` (`jump` names it) or a `ProxyCommand` (`jump` is `None`), to the
    /// resolved `hostname` and `port`. `fd_passing` is set when the `ProxyCommand` hands ssh its
    /// connected socket (`ProxyUseFdpass`).
    Proxied {
        jump: Option<&'a str>,
        hostname: &'a str,
        port: u16,
        fd_passing: bool,
    },
}

impl<'a> Route<'a> {
    /// The jump host, whose failures are told apart from the target's.
    pub fn jump(self) -> Option<&'a str> {
        match self {
            Route::Direct => None,
            Route::Proxied { jump, .. } => jump,
        }
    }
}

/// Where an offered key came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeySource {
    File,
    Agent,
}

/// A public key ssh offered the server, as its debug log names it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OfferedKey {
    pub path: String,
    pub fingerprint: String,
    pub source: KeySource,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Failure {
    HostKeyUnknown,
    /// `known_hosts` is the file and line OpenSSH names as holding the old key.
    HostKeyChanged {
        known_hosts: Option<(String, u32)>,
    },
    /// `unusable` lists keys the server would accept but ssh could not use (a passphrase, say).
    AuthRefused {
        methods: String,
        offered: Vec<OfferedKey>,
        unusable: Vec<String>,
    },
    NameUnresolved {
        host: String,
    },
    ConnectionRefused,
    Unreachable,
    /// `deadline` is true when Portico's own deadline fired rather than ssh timing out.
    TimedOut {
        deadline: bool,
    },
    JumpHostFailed {
        jump: String,
        cause: Box<Failure>,
    },
    /// The session was established and then dropped.
    ConnectionLost {
        line: String,
    },
    /// ssh refused the configuration before connecting.
    ConfigRejected(crate::config_error::ConfigRejection),
    Other(String),
}

/// A failure with every line of ssh's own message (debug output left out) behind it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Diagnosis {
    pub failure: Failure,
    pub raw: Vec<String>,
}

/// The most ssh stderr a test keeps, counted from the end.
pub const TEST_STDERR_LIMIT: usize = 256 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProbeResult {
    Authenticated(Authenticated),
    Failed(Failure),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::HostKeyUnknown => write!(f, "host key not trusted"),
            Failure::HostKeyChanged { .. } => write!(f, "host key changed"),
            Failure::AuthRefused { methods, .. } => {
                write!(f, "authentication refused (server accepts: {methods})")
            }
            Failure::NameUnresolved { host } => write!(f, "host name {host} does not resolve"),
            Failure::ConnectionRefused => write!(f, "connection refused"),
            Failure::Unreachable => write!(f, "host unreachable"),
            Failure::TimedOut { .. } => write!(f, "timed out"),
            Failure::JumpHostFailed { jump, cause } => {
                write!(f, "jump host {jump} failed: {cause}")
            }
            Failure::ConnectionLost { .. } => write!(f, "connection lost"),
            Failure::ConfigRejected(rejection) => write!(f, "{rejection}"),
            Failure::Other(line) => write!(f, "{line}"),
        }
    }
}

fn base_args(entry: Option<&Path>) -> Vec<OsString> {
    let mut args: Vec<OsString> = ["-N", "-n"].map(OsString::from).to_vec();
    for option in [
        "BatchMode=yes".to_string(),
        format!("ConnectTimeout={CONNECT_TIMEOUT_SECS}"),
        "LogLevel=DEBUG1".to_string(),
        "ClearAllForwardings=yes".to_string(),
        "ControlMaster=no".to_string(),
        "ControlPath=none".to_string(),
        "RemoteCommand=none".to_string(),
        "PermitLocalCommand=no".to_string(),
        "ForwardAgent=no".to_string(),
        "ForwardX11=no".to_string(),
    ] {
        args.push("-o".into());
        args.push(option.into());
    }
    if let Some(entry) = entry {
        args.push("-F".into());
        args.push(entry.into());
    }
    args
}

pub fn test_args(entry: Option<&Path>, alias: &str) -> Vec<OsString> {
    let mut args = base_args(entry);
    args.push("--".into());
    args.push(alias.into());
    args
}

pub fn fetch_key_args(entry: Option<&Path>, known_hosts: &Path, alias: &str) -> Vec<OsString> {
    let mut args = base_args(None);
    for option in [
        "PubkeyAuthentication=no",
        "PasswordAuthentication=no",
        "KbdInteractiveAuthentication=no",
        "GSSAPIAuthentication=no",
        "HostbasedAuthentication=no",
        "StrictHostKeyChecking=accept-new",
    ] {
        args.push("-o".into());
        args.push(option.into());
    }
    let mut known = OsString::from("UserKnownHostsFile=");
    known.push(known_hosts);
    args.push("-o".into());
    args.push(known);
    if let Some(entry) = entry {
        args.push("-F".into());
        args.push(entry.into());
    }
    args.push("--".into());
    args.push(alias.into());
    args
}

/// Parses the target's success line. A login on a socket ssh holds itself logs
/// `Authenticated to <host> ([<address>]:<port>) using "<method>".`, one through a proxy's pipe
/// `Authenticated to <host> (via proxy) using "<method>".`; only the form `route` produces is
/// accepted, so a first jump host's own success line, logged when its configuration is verbose,
/// is never taken for the target's. A `ProxyCommand` that passes ssh its socket
/// (`ProxyUseFdpass`) produces the socket form. A later hop is itself reached through a proxy,
/// so a proxied line must also name the target's resolved `hostname` (OpenSSH lowercases both).
/// A later verbose hop with the target's own `HostName` still cannot be told apart: its line
/// carries no port.
pub fn authenticated(line: &str, route: Route<'_>) -> Option<Authenticated> {
    let rest = line.trim().strip_prefix("Authenticated to ")?;
    let on_socket = |rest: &str| -> Option<Authenticated> {
        let (host, rest) = rest.split_once(" ([")?;
        let (address, rest) = rest.split_once("]:")?;
        let (port, rest) = rest.split_once(") using \"")?;
        let method = rest.strip_suffix("\".")?;
        Some(Authenticated {
            host: host.to_string(),
            address: address.to_string(),
            port: port.parse().ok()?,
            method: method.to_string(),
            via: None,
        })
    };
    match route {
        Route::Direct => on_socket(rest),
        Route::Proxied {
            jump,
            hostname,
            fd_passing: true,
            ..
        } => {
            let success = on_socket(rest)?;
            success
                .host
                .eq_ignore_ascii_case(hostname)
                .then(|| Authenticated {
                    via: Some(jump.unwrap_or("proxy").to_string()),
                    ..success
                })
        }
        Route::Proxied {
            jump,
            hostname,
            port,
            fd_passing: false,
        } => {
            let (host, rest) = rest.split_once(" (via proxy) using \"")?;
            if !host.eq_ignore_ascii_case(hostname) {
                return None;
            }
            let method = rest.strip_suffix("\".")?;
            Some(Authenticated {
                host: host.to_string(),
                address: hostname.to_string(),
                port,
                method: method.to_string(),
                via: Some(jump.unwrap_or("proxy").to_string()),
            })
        }
    }
}

/// Lines of the jump host's own hand-off, which say nothing about why it failed.
fn is_noise(line: &str) -> bool {
    line.starts_with("kex_exchange_identification")
        || line.starts_with("Connection closed by UNKNOWN")
        || line.starts_with("stdio forwarding failed")
}

/// A line of OpenSSH's own debug log, which the test's `LogLevel=DEBUG1` interleaves with the
/// messages that explain a failure; it is never one of those messages.
fn is_debug(line: &str) -> bool {
    line.starts_with("debug")
}

fn is_progress(line: &str) -> bool {
    line.starts_with("Authenticated to ")
        || line.starts_with("Transferred:")
        || line.starts_with("Bytes per second")
}

fn last_line(lines: &[&str]) -> String {
    lines
        .iter()
        .rev()
        .map(|line| line.trim())
        .find(|line| !line.is_empty())
        .unwrap_or("ssh failed without output")
        .to_string()
}

/// The messages OpenSSH prints when a running session is dropped; a server closing the
/// connection before login (`kex_exchange_identification: ...`) is not one of them.
fn is_session_drop(line: &str) -> bool {
    let line = line.trim();
    (line.starts_with("Connection to ") && line.ends_with("closed by remote host."))
        || (line.starts_with("client_loop:") && line.contains("Broken pipe"))
        || (line.starts_with("Timeout, server ") && line.ends_with("not responding."))
}

/// What ssh's debug log says about keys and `known_hosts`.
#[derive(Default)]
struct KeyFacts {
    offered: Vec<OfferedKey>,
    unusable: Vec<String>,
    known_hosts: Option<(String, u32)>,
}

/// Splits `<path> <type> <fingerprint> <flags…>`; the path may contain spaces.
fn key_fields(rest: &str) -> Option<(String, &str, Vec<&str>)> {
    let tokens: Vec<&str> = rest.split_whitespace().collect();
    let at = tokens
        .iter()
        .position(|token| token.starts_with("SHA256:") || token.starts_with("MD5:"))?;
    if at < 2 {
        return None;
    }
    Some((
        tokens[..at - 1].join(" "),
        tokens[at],
        tokens[at + 1..].to_vec(),
    ))
}

fn offered_key(line: &str) -> Option<OfferedKey> {
    let rest = line.trim().strip_prefix("debug1: Offering public key: ")?;
    let (path, fingerprint, flags) = key_fields(rest)?;
    Some(OfferedKey {
        path,
        fingerprint: fingerprint.to_string(),
        source: if flags.contains(&"agent") {
            KeySource::Agent
        } else {
            KeySource::File
        },
    })
}

fn accepted_key_path(line: &str) -> Option<String> {
    let rest = line.trim().strip_prefix("debug1: Server accepts key: ")?;
    Some(key_fields(rest)?.0)
}

/// `Offending <type> key in <file>:<line>`
fn offending_key(line: &str) -> Option<(String, u32)> {
    let rest = line.trim().strip_prefix("Offending ")?;
    let (_, location) = rest.split_once(" key in ")?;
    let (file, number) = location.rsplit_once(':')?;
    Some((file.to_string(), number.parse().ok()?))
}

fn key_facts(all: &[&str], route: Route<'_>) -> KeyFacts {
    // OpenSSH does not pass its log level to a ProxyJump child, so debug key lines are normally
    // the target's. A jump host the user configured verbose logs its own, ending in its own
    // `Authenticated to` line (any that is not the target's); only what follows the last such
    // line is the target's.
    let from = match route {
        Route::Direct => 0,
        Route::Proxied { .. } => all
            .iter()
            .rposition(|line| {
                line.trim().starts_with("Authenticated to ") && authenticated(line, route).is_none()
            })
            .map_or(0, |at| at + 1),
    };
    let lines = &all[from..];
    let mut facts = KeyFacts::default();
    for (at, line) in lines.iter().enumerate() {
        if let Some(key) = offered_key(line) {
            facts.offered.push(key);
        } else if let Some(path) = accepted_key_path(line) {
            let logged_in = lines[at + 1..]
                .iter()
                .any(|line| line.trim().starts_with("Authenticated to "));
            if !logged_in && !facts.unusable.contains(&path) {
                facts.unusable.push(path);
            }
        }
    }
    facts.known_hosts = all.iter().find_map(|line| offending_key(line));
    facts
}

fn classify_lines(lines: &[&str], deadline_reached: bool, facts: &KeyFacts) -> Failure {
    let text = lines.join("\n");
    if text.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") {
        return Failure::HostKeyChanged {
            known_hosts: facts.known_hosts.clone(),
        };
    }
    if lines
        .iter()
        .any(|l| l.trim() == "Host key verification failed.")
    {
        return Failure::HostKeyUnknown;
    }
    if let Some(methods) = lines.iter().find_map(|line| {
        let rest = line.split_once("Permission denied (")?.1;
        Some(rest.split_once(')')?.0.to_string())
    }) {
        return Failure::AuthRefused {
            methods,
            offered: facts.offered.clone(),
            unusable: facts.unusable.clone(),
        };
    }
    if let Some(host) = lines.iter().find_map(|line| {
        let rest = line.split_once("Could not resolve hostname ")?.1;
        Some(rest.split_once(':')?.0.to_string())
    }) {
        return Failure::NameUnresolved { host };
    }
    if text.contains("Connection refused") {
        return Failure::ConnectionRefused;
    }
    if text.contains("No route to host") || text.contains("Network is unreachable") {
        return Failure::Unreachable;
    }
    if let Some(line) = lines.iter().find(|line| is_session_drop(line)) {
        return Failure::ConnectionLost {
            line: line.trim().to_string(),
        };
    }
    if deadline_reached {
        return Failure::TimedOut { deadline: true };
    }
    if text.contains("Connection timed out") || text.contains("Operation timed out") {
        return Failure::TimedOut { deadline: false };
    }
    Failure::Other(last_line(lines))
}

fn host_part(jump: &str) -> &str {
    let jump = jump.split_once('@').map_or(jump, |(_, rest)| rest);
    if let Some(bracketed) = jump.strip_prefix('[') {
        return bracketed.split(']').next().unwrap_or(bracketed);
    }
    jump.split(':').next().unwrap_or(jump)
}

/// Whether `host` appears in `line` as a whole name, not inside a longer one.
fn mentions_host(line: &str, host: &str) -> bool {
    let is_name = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_');
    !host.is_empty()
        && line.match_indices(host).any(|(at, _)| {
            let before = line[..at].chars().next_back();
            let after = line[at + host.len()..].chars().next();
            !before.is_some_and(is_name) && !after.is_some_and(is_name)
        })
}

pub fn classify(stderr: &str, deadline_reached: bool, jump: Option<&str>) -> Failure {
    let route = match jump {
        None => Route::Direct,
        Some(_) => Route::Proxied {
            jump,
            hostname: "",
            port: 0,
            fd_passing: false,
        },
    };
    diagnose(stderr, deadline_reached, route).failure
}

pub fn diagnose(stderr: &str, deadline_reached: bool, route: Route<'_>) -> Diagnosis {
    let all: Vec<&str> = stderr.lines().collect();
    let lines: Vec<&str> = all.iter().copied().filter(|line| !is_debug(line)).collect();
    let facts = key_facts(&all, route);
    let failure = classify_route(&lines, deadline_reached, route.jump(), &facts);
    let raw = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.trim_end().to_string())
        .collect();
    Diagnosis { failure, raw }
}

fn classify_route(
    lines: &[&str],
    deadline_reached: bool,
    jump: Option<&str>,
    facts: &KeyFacts,
) -> Failure {
    let first = classify_lines(lines, deadline_reached, facts);
    let Some(jump) = jump else {
        return first;
    };
    let handed_off = lines.iter().any(|line| line.contains("UNKNOWN port 65535"));
    // OpenSSH names no hop for a key failure, but when the jump child fails it
    // is followed by "Connection closed by UNKNOWN port 65535", which a
    // failure of the final host's own key is not.
    if matches!(
        first,
        Failure::HostKeyChanged { .. } | Failure::HostKeyUnknown
    ) && !handed_off
    {
        return first;
    }
    let host = host_part(jump);
    let named = lines
        .iter()
        .any(|line| !is_progress(line) && mentions_host(line, host));
    let marked = handed_off
        || lines
            .iter()
            .any(|line| line.contains("channel 0: open failed"));
    if !named && !marked {
        return first;
    }
    let rest: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|line| !is_noise(line) && !is_progress(line))
        .collect();
    Failure::JumpHostFailed {
        jump: jump.to_string(),
        cause: Box::new(classify_lines(&rest, false, facts)),
    }
}

/// The first configured known-hosts file, with a leading `~/` expanded.
pub fn known_hosts_path(configured: &[String], home: &Path) -> Result<PathBuf, String> {
    let first = configured
        .first()
        .ok_or_else(|| "ssh reports no known hosts file".to_string())?;
    if first.contains('%') {
        return Err(format!(
            "the known hosts file {first} uses tokens, which Portico cannot expand"
        ));
    }
    Ok(match first.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(first),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture(name: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/ssh")
            .join(format!("{name}.txt"));
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    #[test]
    fn the_success_line_names_the_address_port_and_method() {
        let line = fixture("authenticated")
            .lines()
            .find_map(|line| authenticated(line, Route::Direct))
            .unwrap();
        assert_eq!(
            (line.address.as_str(), line.port, line.method.as_str()),
            ("127.0.0.1", 22999, "publickey")
        );
        assert_eq!(line.via, None);
        assert_eq!(
            authenticated("Transferred: sent 1, received 2 bytes", Route::Direct),
            None
        );
    }

    #[test]
    fn a_proxied_success_is_recognised_only_through_a_proxy() {
        let text = fixture("authenticated-via-proxy");
        let jump = Route::Proxied {
            jump: Some("bastion"),
            hostname: "127.0.0.1",
            port: 2222,
            fd_passing: false,
        };
        assert_eq!(
            text.lines().find_map(|line| authenticated(line, jump)),
            Some(Authenticated {
                host: "127.0.0.1".into(),
                address: "127.0.0.1".into(),
                port: 2222,
                method: "publickey".into(),
                via: Some("bastion".into()),
            })
        );
        let command = Route::Proxied {
            jump: None,
            hostname: "127.0.0.1",
            port: 22,
            fd_passing: false,
        };
        let success = text.lines().find_map(|line| authenticated(line, command));
        assert_eq!(success.and_then(|s| s.via), Some("proxy".into()));
        assert_eq!(
            text.lines()
                .find_map(|line| authenticated(line, Route::Direct)),
            None
        );
        assert_eq!(
            fixture("authenticated")
                .lines()
                .find_map(|line| authenticated(line, jump)),
            None
        );
    }

    #[test]
    fn a_verbose_jump_hosts_own_login_is_not_the_targets() {
        let jump = Route::Proxied {
            jump: Some("jumpv"),
            hostname: "127.0.0.1",
            port: 22999,
            fd_passing: false,
        };
        let text = fixture("authenticated-via-verbose-jump");
        let jump_only = text
            .lines()
            .find(|line| line.starts_with("Authenticated to "))
            .unwrap();
        assert!(jump_only.contains("([127.0.0.1]:22999)"));
        let success = text
            .lines()
            .find_map(|line| authenticated(line, jump))
            .unwrap();
        assert_eq!(success.via.as_deref(), Some("jumpv"));
        assert_eq!(authenticated(jump_only, jump), None);
    }

    #[test]
    fn a_later_hops_proxied_login_is_not_the_targets() {
        let route = Route::Proxied {
            jump: Some("hopa,hopb"),
            hostname: "127.0.0.3",
            port: 22999,
            fd_passing: false,
        };
        let text = fixture("authenticated-via-two-jumps");
        let hop = text
            .lines()
            .find(|line| line.contains("127.0.0.2 (via proxy)"))
            .unwrap();
        assert_eq!(authenticated(hop, route), None);
        let success = text
            .lines()
            .find_map(|line| authenticated(line, route))
            .unwrap();
        assert_eq!(success.host, "127.0.0.3");
        assert_eq!(success.via.as_deref(), Some("hopa,hopb"));
    }

    #[test]
    fn a_fd_passing_proxy_logs_the_direct_form() {
        let route = Route::Proxied {
            jump: None,
            hostname: "127.0.0.3",
            port: 2222,
            fd_passing: true,
        };
        let text = fixture("authenticated-via-fdpass");
        assert_eq!(
            text.lines().find_map(|line| authenticated(line, route)),
            Some(Authenticated {
                host: "127.0.0.3".into(),
                address: "127.0.0.3".into(),
                port: 22999,
                method: "publickey".into(),
                via: Some("proxy".into()),
            })
        );
        assert_eq!(
            authenticated(
                "Authenticated to 127.0.0.3 (via proxy) using \"publickey\".",
                route
            ),
            None
        );
        assert_eq!(
            authenticated(
                "Authenticated to 127.0.0.9 ([127.0.0.9]:22) using \"publickey\".",
                route
            ),
            None
        );
        let plain = Route::Proxied {
            jump: None,
            hostname: "127.0.0.3",
            port: 2222,
            fd_passing: false,
        };
        assert_eq!(
            text.lines().find_map(|line| authenticated(line, plain)),
            None
        );
    }

    #[test]
    fn the_targets_host_is_matched_ignoring_case() {
        let route = Route::Proxied {
            jump: None,
            hostname: "Db.Example",
            port: 22,
            fd_passing: false,
        };
        assert!(
            authenticated(
                "Authenticated to db.example (via proxy) using \"publickey\".",
                route
            )
            .is_some()
        );
    }

    fn refused() -> Failure {
        Failure::AuthRefused {
            methods: "publickey".into(),
            offered: Vec::new(),
            unusable: Vec::new(),
        }
    }

    #[test]
    fn every_recorded_failure_has_its_own_result() {
        for (name, expected) in [
            ("host-key-unknown", Failure::HostKeyUnknown),
            ("connection-refused", Failure::ConnectionRefused),
            (
                "name-unresolved",
                Failure::NameUnresolved {
                    host: "probe.invalid".into(),
                },
            ),
        ] {
            assert_eq!(classify(&fixture(name), false, None), expected, "{name}");
        }
        assert!(matches!(
            classify(&fixture("host-key-changed"), false, None),
            Failure::HostKeyChanged { .. }
        ));
        for name in ["auth-refused", "passphrase-key"] {
            assert!(
                matches!(
                    classify(&fixture(name), false, None),
                    Failure::AuthRefused { ref methods, .. } if methods == "publickey"
                ),
                "{name}"
            );
        }
    }

    #[test]
    fn failures_through_a_jump_host_name_it() {
        for (name, cause) in [
            ("jump-connection-refused", Failure::ConnectionRefused),
            ("jump-target-refused", Failure::ConnectionRefused),
        ] {
            assert_eq!(
                classify(&fixture(name), false, Some("127.0.0.1:22998")),
                Failure::JumpHostFailed {
                    jump: "127.0.0.1:22998".into(),
                    cause: Box::new(cause)
                },
                "{name}"
            );
        }
        for name in ["jump-host-key-unknown", "jump-host-key-changed"] {
            let Failure::JumpHostFailed { jump, cause } =
                classify(&fixture(name), false, Some("127.0.0.1:22999"))
            else {
                panic!("{name}");
            };
            assert_eq!(jump, "127.0.0.1:22999");
            assert!(
                matches!(
                    (name, *cause),
                    ("jump-host-key-unknown", Failure::HostKeyUnknown)
                        | ("jump-host-key-changed", Failure::HostKeyChanged { .. })
                ),
                "{name}"
            );
        }
        assert_eq!(
            classify(
                &fixture("target-key-unknown-via-jump"),
                false,
                Some("127.0.0.1:22999")
            ),
            Failure::HostKeyUnknown
        );
        for (line, jump) in [
            ("user@db1.example: Permission denied (publickey).\n", "db"),
            ("user@target: Permission denied (publickey).\n", "arg"),
        ] {
            assert_eq!(classify(line, false, Some(jump)), refused(), "{jump}");
        }
        assert!(matches!(
            classify(&fixture("auth-refused"), false, Some("jump.example")),
            Failure::AuthRefused { .. }
        ));
    }

    #[test]
    fn offered_keys_and_their_source_are_read_from_debug_lines() {
        let Failure::AuthRefused {
            offered,
            unusable,
            methods,
        } = classify(&fixture("auth-refused"), false, None)
        else {
            panic!()
        };
        assert_eq!(methods, "publickey");
        assert_eq!(offered.len(), 1);
        assert_eq!(offered[0].source, KeySource::File);
        assert!(offered[0].fingerprint.starts_with("SHA256:"));
        assert!(unusable.is_empty());
        let line = "debug1: Offering public key: me@laptop ED25519 SHA256:abc agent";
        assert_eq!(offered_key(line).unwrap().source, KeySource::Agent);
    }

    #[test]
    fn a_key_the_server_accepts_but_ssh_cannot_use_is_unusable() {
        let Failure::AuthRefused { unusable, .. } =
            classify(&fixture("passphrase-key"), false, None)
        else {
            panic!()
        };
        assert_eq!(unusable, ["/tmp/portico-fixture/pass"]);
    }

    #[test]
    fn password_only_and_missing_identity_are_refusals_with_their_facts() {
        assert!(matches!(
            classify(&fixture("password-only"), false, None),
            Failure::AuthRefused { ref methods, .. } if !methods.contains("publickey")
        ));
        assert!(matches!(
            classify(&fixture("missing-identity"), false, None),
            Failure::AuthRefused { ref offered, .. } if offered.is_empty()
        ));
    }

    #[test]
    fn a_changed_key_names_the_offending_known_hosts_line() {
        let Failure::HostKeyChanged {
            known_hosts: Some((file, line)),
        } = classify(&fixture("host-key-changed"), false, None)
        else {
            panic!()
        };
        assert!(file.ends_with("kh_bad") && line >= 1);
    }

    #[test]
    fn a_jump_failure_carries_the_jump_hosts_own_cause() {
        let jump = Route::Proxied {
            jump: Some("jump"),
            hostname: "127.0.0.1",
            port: 22997,
            fd_passing: false,
        };
        let failure = diagnose(&fixture("jump-target-key-refused"), false, jump).failure;
        // The jump host logged in; the target refused the key, so this is the target's refusal,
        // and only keys offered after the jump's own `Authenticated to` line are listed.
        let Failure::AuthRefused { offered, .. } = failure else {
            panic!("{failure:?}")
        };
        assert!(!offered.is_empty());
        assert!(offered.iter().all(|key| key.path.ends_with("wrong")));
        assert_eq!(
            classify(
                &fixture("jump-connection-refused"),
                false,
                Some("127.0.0.1:22998")
            ),
            Failure::JumpHostFailed {
                jump: "127.0.0.1:22998".into(),
                cause: Box::new(Failure::ConnectionRefused)
            }
        );
    }

    #[test]
    fn keys_after_a_verbose_jump_hosts_login_are_the_targets() {
        let jump = Route::Proxied {
            jump: Some("jump"),
            hostname: "127.0.0.1",
            port: 22997,
            fd_passing: false,
        };
        let text = "debug1: Offering public key: /k/jump ED25519 SHA256:j explicit\n\
                    Authenticated to jump ([127.0.0.1]:22999) using \"publickey\".\n\
                    debug1: Offering public key: /k/target ED25519 SHA256:t explicit\n\
                    user@target: Permission denied (publickey).\n";
        let Failure::AuthRefused { offered, .. } = diagnose(text, false, jump).failure else {
            panic!()
        };
        assert_eq!(offered.len(), 1);
        assert_eq!(offered[0].path, "/k/target");
    }

    #[test]
    fn keys_after_a_later_hops_proxied_login_are_the_targets() {
        let route = Route::Proxied {
            jump: Some("hopa,hopb"),
            hostname: "127.0.0.3",
            port: 22999,
            fd_passing: false,
        };
        let text = "Authenticated to 127.0.0.1 ([127.0.0.1]:22999) using \"publickey\".\n\
                    debug1: Offering public key: /k/hopb ED25519 SHA256:b explicit\n\
                    Authenticated to 127.0.0.2 (via proxy) using \"publickey\".\n\
                    debug1: Offering public key: /k/target ED25519 SHA256:t explicit\n\
                    user@target: Permission denied (publickey).\n";
        let Failure::AuthRefused { offered, .. } = diagnose(text, false, route).failure else {
            panic!()
        };
        assert_eq!(offered.len(), 1);
        assert_eq!(offered[0].path, "/k/target");
    }

    #[test]
    fn a_dropped_session_is_lost_not_refused() {
        assert!(matches!(
            classify(&fixture("connection-lost"), false, None),
            Failure::ConnectionLost { .. }
        ));
        for line in [
            "client_loop: send disconnect: Broken pipe",
            "Timeout, server 10.0.0.1 not responding.",
            "Connection to 10.0.0.1 closed by remote host.",
        ] {
            assert!(
                matches!(classify(line, false, None), Failure::ConnectionLost { .. }),
                "{line}"
            );
        }
    }

    #[test]
    fn a_connection_closed_before_login_is_not_a_lost_session() {
        let line = "kex_exchange_identification: Connection closed by remote host";
        assert_eq!(
            classify(line, false, None),
            Failure::Other(line.to_string())
        );
    }

    #[test]
    fn raw_lines_leave_out_debug_output() {
        let raw = diagnose(&fixture("auth-refused"), false, Route::Direct).raw;
        assert!(raw.iter().all(|line| !line.starts_with("debug")));
        assert!(raw.last().unwrap().contains("Permission denied"));
    }

    #[test]
    fn debug_lines_never_name_the_jump_host() {
        let text = "debug1: Setting implicit ProxyCommand from ProxyJump: ssh -l u -p 22 -W '[%h]:%p' bastion\n\
                    user@target: Permission denied (publickey).\n";
        assert!(matches!(
            classify(text, false, Some("bastion")),
            Failure::AuthRefused { .. }
        ));
    }

    #[test]
    fn unreachable_and_timeouts_are_recognised() {
        assert_eq!(
            classify(
                "ssh: connect to host 10.0.0.1 port 22: No route to host\n",
                false,
                None
            ),
            Failure::Unreachable
        );
        assert_eq!(
            classify(
                "ssh: connect to host 10.0.0.1 port 22: Network is unreachable\n",
                false,
                None
            ),
            Failure::Unreachable
        );
        assert_eq!(
            classify(
                "ssh: connect to host 10.0.0.1 port 22: Connection timed out\n",
                false,
                None
            ),
            Failure::TimedOut { deadline: false }
        );
        assert_eq!(
            classify("", true, None),
            Failure::TimedOut { deadline: true }
        );
    }

    #[test]
    fn an_unrecognised_failure_reports_the_last_line() {
        let text = fixture("other");
        let last = text.lines().rev().find(|l| !l.trim().is_empty()).unwrap();
        assert_eq!(
            classify(&text, false, None),
            Failure::Other(last.trim().to_string())
        );
        assert_eq!(
            classify("", false, None),
            Failure::Other("ssh failed without output".into())
        );
    }

    #[test]
    fn test_arguments_are_exact() {
        let args = test_args(Some(Path::new("/tmp/entry")), "dev");
        assert_eq!(
            args,
            [
                "-N",
                "-n",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=5",
                "-o",
                "LogLevel=DEBUG1",
                "-o",
                "ClearAllForwardings=yes",
                "-o",
                "ControlMaster=no",
                "-o",
                "ControlPath=none",
                "-o",
                "RemoteCommand=none",
                "-o",
                "PermitLocalCommand=no",
                "-o",
                "ForwardAgent=no",
                "-o",
                "ForwardX11=no",
                "-F",
                "/tmp/entry",
                "--",
                "dev"
            ]
            .map(OsString::from)
        );
        assert!(!test_args(None, "dev").contains(&OsString::from("-F")));
    }

    #[test]
    fn fetch_arguments_disable_every_login_method_and_use_the_temporary_file() {
        let args = fetch_key_args(Some(Path::new("/tmp/entry")), Path::new("/tmp/kh"), "dev");
        for option in [
            "PubkeyAuthentication=no",
            "PasswordAuthentication=no",
            "KbdInteractiveAuthentication=no",
            "GSSAPIAuthentication=no",
            "HostbasedAuthentication=no",
            "StrictHostKeyChecking=accept-new",
            "UserKnownHostsFile=/tmp/kh",
        ] {
            let at = args.windows(2).any(|w| w[0] == "-o" && w[1] == option);
            assert!(at, "{option}");
        }
        assert!(args.starts_with(&test_args(None, "dev")[..test_args(None, "dev").len() - 2]));
        assert_eq!(
            args[args.len() - 4..],
            ["-F", "/tmp/entry", "--", "dev"].map(OsString::from)
        );
        let plain = fetch_key_args(None, Path::new("/tmp/kh"), "dev");
        assert!(!plain.contains(&OsString::from("-F")));
        assert_eq!(plain[plain.len() - 2..], ["--", "dev"].map(OsString::from));
    }

    #[test]
    fn known_hosts_path_expands_home_and_refuses_tokens() {
        let home = Path::new("/home/u");
        assert_eq!(
            known_hosts_path(
                &["~/.ssh/known_hosts".into(), "~/.ssh/known_hosts2".into()],
                home
            )
            .unwrap(),
            home.join(".ssh/known_hosts")
        );
        assert_eq!(
            known_hosts_path(&["/etc/kh".into()], home).unwrap(),
            PathBuf::from("/etc/kh")
        );
        assert!(known_hosts_path(&["~/.ssh/%u_hosts".into()], home).is_err());
        assert!(known_hosts_path(&[], home).is_err());
    }

    #[test]
    fn reasons_match_the_spec() {
        let refused = Failure::AuthRefused {
            methods: "publickey,password".into(),
            offered: Vec::new(),
            unusable: Vec::new(),
        };
        assert_eq!(
            refused.to_string(),
            "authentication refused (server accepts: publickey,password)"
        );
        assert_eq!(
            Failure::HostKeyChanged { known_hosts: None }.to_string(),
            "host key changed"
        );
        let jump = Failure::JumpHostFailed {
            jump: "bastion".into(),
            cause: Box::new(Failure::ConnectionRefused),
        };
        assert_eq!(
            jump.to_string(),
            "jump host bastion failed: connection refused"
        );
        assert_eq!(Failure::HostKeyUnknown.to_string(), "host key not trusted");
        assert_eq!(
            Failure::TimedOut { deadline: true }.to_string(),
            "timed out"
        );
        assert_eq!(Failure::Other("boom".into()).to_string(), "boom");
    }
}
