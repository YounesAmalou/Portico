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
    /// resolved `hostname` and `port`.
    Proxied {
        jump: Option<&'a str>,
        hostname: &'a str,
        port: u16,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Failure {
    HostKeyUnknown,
    HostKeyChanged,
    AuthRefused { methods: String },
    NameUnresolved { host: String },
    ConnectionRefused,
    Unreachable,
    TimedOut,
    JumpHostFailed { jump: String, detail: String },
    Other(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProbeResult {
    Authenticated(Authenticated),
    Failed(Failure),
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Failure::HostKeyUnknown => write!(f, "host key not trusted"),
            Failure::HostKeyChanged => write!(f, "host key changed"),
            Failure::AuthRefused { methods } => {
                write!(f, "authentication refused (server accepts: {methods})")
            }
            Failure::NameUnresolved { host } => write!(f, "host name {host} does not resolve"),
            Failure::ConnectionRefused => write!(f, "connection refused"),
            Failure::Unreachable => write!(f, "host unreachable"),
            Failure::TimedOut => write!(f, "timed out"),
            Failure::JumpHostFailed { jump, detail } => {
                write!(f, "jump host {jump} failed: {detail}")
            }
            Failure::Other(line) => write!(f, "{line}"),
        }
    }
}

impl Failure {
    pub fn hint(&self) -> Option<&'static str> {
        match self {
            Failure::HostKeyUnknown | Failure::Other(_) => None,
            Failure::HostKeyChanged => Some(
                "the server's host key differs from the one you trusted, which can mean an attack; confirm the new key with the server's administrator before changing known_hosts",
            ),
            Failure::AuthRefused { .. } => Some(
                "check the user and the key; a key with a passphrase must be loaded in an agent",
            ),
            Failure::NameUnresolved { .. } => Some("check the host name"),
            Failure::ConnectionRefused => {
                Some("check the port, and that an SSH server is running there")
            }
            Failure::Unreachable => Some("check the address and your network"),
            Failure::TimedOut => Some(
                "the host did not answer in time; check the address, the port and any firewall",
            ),
            Failure::JumpHostFailed { .. } => Some("test the jump host on its own first"),
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

/// Parses the target's success line. A direct login logs
/// `Authenticated to <host> ([<address>]:<port>) using "<method>".`, one through a proxy
/// `Authenticated to <host> (via proxy) using "<method>".`; only the form `route` produces is
/// accepted, so a jump host's own success line, logged when its configuration is verbose, is
/// never taken for the target's.
pub fn authenticated(line: &str, route: Route<'_>) -> Option<Authenticated> {
    let rest = line.trim().strip_prefix("Authenticated to ")?;
    match route {
        Route::Direct => {
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
        }
        Route::Proxied {
            jump,
            hostname,
            port,
        } => {
            let (host, rest) = rest.split_once(" (via proxy) using \"")?;
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

fn classify_lines(lines: &[&str], deadline_reached: bool) -> Failure {
    let text = lines.join("\n");
    if text.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") {
        return Failure::HostKeyChanged;
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
        return Failure::AuthRefused { methods };
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
    if deadline_reached
        || text.contains("Connection timed out")
        || text.contains("Operation timed out")
    {
        return Failure::TimedOut;
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
    let lines: Vec<&str> = stderr.lines().filter(|line| !is_debug(line)).collect();
    let first = classify_lines(&lines, deadline_reached);
    let Some(jump) = jump else {
        return first;
    };
    let handed_off = lines.iter().any(|line| line.contains("UNKNOWN port 65535"));
    // OpenSSH names no hop for a key failure, but when the jump child fails it
    // is followed by "Connection closed by UNKNOWN port 65535", which a
    // failure of the final host's own key is not.
    if matches!(first, Failure::HostKeyChanged | Failure::HostKeyUnknown) && !handed_off {
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
    let detail = match classify_lines(&rest, false) {
        Failure::Other(_) => last_line(&rest),
        other => other.to_string(),
    };
    Failure::JumpHostFailed {
        jump: jump.to_string(),
        detail,
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
            hostname: "db.internal",
            port: 2222,
        };
        assert_eq!(
            text.lines().find_map(|line| authenticated(line, jump)),
            Some(Authenticated {
                host: "127.0.0.1".into(),
                address: "db.internal".into(),
                port: 2222,
                method: "publickey".into(),
                via: Some("bastion".into()),
            })
        );
        let command = Route::Proxied {
            jump: None,
            hostname: "db.internal",
            port: 22,
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
    fn every_recorded_failure_has_its_own_result() {
        for (name, expected) in [
            ("host-key-unknown", Failure::HostKeyUnknown),
            ("host-key-changed", Failure::HostKeyChanged),
            (
                "auth-refused",
                Failure::AuthRefused {
                    methods: "publickey".into(),
                },
            ),
            (
                "passphrase-key",
                Failure::AuthRefused {
                    methods: "publickey".into(),
                },
            ),
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
    }

    #[test]
    fn failures_through_a_jump_host_name_it() {
        for (name, detail) in [
            ("jump-connection-refused", "connection refused"),
            ("jump-target-refused", "connection refused"),
        ] {
            assert_eq!(
                classify(&fixture(name), false, Some("127.0.0.1:22998")),
                Failure::JumpHostFailed {
                    jump: "127.0.0.1:22998".into(),
                    detail: detail.into()
                },
                "{name}"
            );
        }
        for (name, detail) in [
            ("jump-host-key-unknown", "host key not trusted"),
            ("jump-host-key-changed", "host key changed"),
        ] {
            assert_eq!(
                classify(&fixture(name), false, Some("127.0.0.1:22999")),
                Failure::JumpHostFailed {
                    jump: "127.0.0.1:22999".into(),
                    detail: detail.into()
                },
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
            assert_eq!(
                classify(line, false, Some(jump)),
                Failure::AuthRefused {
                    methods: "publickey".into()
                },
                "{jump}"
            );
        }
        assert_eq!(
            classify(&fixture("auth-refused"), false, Some("jump.example")),
            Failure::AuthRefused {
                methods: "publickey".into()
            }
        );
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
            Failure::TimedOut
        );
        assert_eq!(classify("", true, None), Failure::TimedOut);
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
    fn reasons_and_hints_match_the_spec() {
        let refused = Failure::AuthRefused {
            methods: "publickey,password".into(),
        };
        assert_eq!(
            refused.to_string(),
            "authentication refused (server accepts: publickey,password)"
        );
        assert_eq!(
            refused.hint(),
            Some("check the user and the key; a key with a passphrase must be loaded in an agent")
        );
        assert_eq!(Failure::HostKeyChanged.to_string(), "host key changed");
        assert_eq!(
            Failure::HostKeyChanged.hint(),
            Some(
                "the server's host key differs from the one you trusted, which can mean an attack; confirm the new key with the server's administrator before changing known_hosts"
            )
        );
        let jump = Failure::JumpHostFailed {
            jump: "bastion".into(),
            detail: "connection refused".into(),
        };
        assert_eq!(
            jump.to_string(),
            "jump host bastion failed: connection refused"
        );
        assert_eq!(jump.hint(), Some("test the jump host on its own first"));
        assert_eq!(Failure::HostKeyUnknown.to_string(), "host key not trusted");
        assert_eq!(Failure::HostKeyUnknown.hint(), None);
        assert_eq!(Failure::Other("boom".into()).to_string(), "boom");
    }
}
