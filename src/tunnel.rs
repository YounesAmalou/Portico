//! Saved tunnels: one TCP forward through a known host, kept in Portico's own tunnels file and
//! started on demand as an ordinary `ssh` process.
//!
//! This module is pure: the model, its validation, OpenSSH's forward syntax, the tunnels file,
//! collisions between saved tunnels, the status types every interface renders, and the ssh
//! arguments of a start. Processes and files live in `runtime` and `store`.

pub mod runtime;
pub mod store;

use std::{ffi::OsString, fmt, path::Path};

use thiserror::Error;

use crate::host::{HostField, parse_port};

/// The kind of forward a tunnel opens.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Kind {
    Local,
    Remote,
    Dynamic,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Local, Kind::Remote, Kind::Dynamic];

    /// The ssh option that opens this forward.
    pub fn flag(self) -> &'static str {
        match self {
            Kind::Local => "-L",
            Kind::Remote => "-R",
            Kind::Dynamic => "-D",
        }
    }

    pub fn from_flag(flag: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.flag() == flag)
    }

    /// The `ssh_config` directive of this forward, also used in the tunnels file.
    pub fn directive(self) -> &'static str {
        match self {
            Kind::Local => "LocalForward",
            Kind::Remote => "RemoteForward",
            Kind::Dynamic => "DynamicForward",
        }
    }

    fn from_directive(directive: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.directive().eq_ignore_ascii_case(directive))
    }

    /// The stable lowercase name.
    pub fn name(self) -> &'static str {
        match self {
            Kind::Local => "local",
            Kind::Remote => "remote",
            Kind::Dynamic => "dynamic",
        }
    }

    /// Whether the forward listens on this machine.
    pub fn listens_locally(self) -> bool {
        self != Kind::Remote
    }
}

/// Where a local or remote forward connects to, as the far side resolves it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Destination {
    pub host: String,
    pub port: u16,
}

/// One forward: what listens where, and for local and remote forwards where it connects to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Forward {
    pub kind: Kind,
    /// `None` leaves ssh's default (loopback unless `GatewayPorts`); `*` means every address.
    pub bind_address: Option<String>,
    pub bind_port: u16,
    /// Set exactly for local and remote forwards.
    pub destination: Option<Destination>,
}

/// A field of a tunnel, so forms can put an error next to the field it concerns.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TunnelField {
    Name,
    Host,
    Kind,
    BindAddress,
    BindPort,
    TargetHost,
    TargetPort,
}

impl fmt::Display for TunnelField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Name => "name",
            Self::Host => "host",
            Self::Kind => "kind",
            Self::BindAddress => "bind address",
            Self::BindPort => "bind port",
            Self::TargetHost => "target host",
            Self::TargetPort => "target port",
        })
    }
}

impl TunnelField {
    /// Whether `character` may appear in a value of this field, for forms to filter typing.
    pub fn permits(self, character: char) -> bool {
        if character.is_control() || character.is_whitespace() {
            return false;
        }
        match self {
            Self::Name => HostField::Alias.permits(character),
            Self::BindPort | Self::TargetPort => character.is_ascii_digit(),
            Self::BindAddress | Self::TargetHost => !"[]/".contains(character),
            Self::Host | Self::Kind => true,
        }
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum TunnelError {
    #[error("invalid tunnel {field}: {reason}")]
    Invalid {
        field: TunnelField,
        reason: &'static str,
    },
    #[error("invalid {} forward `{spec}`: {reason}", kind.name())]
    Spec {
        kind: Kind,
        spec: String,
        reason: &'static str,
    },
}

impl TunnelError {
    /// The field the error concerns.
    pub fn field(&self) -> TunnelField {
        match self {
            Self::Invalid { field, .. } => *field,
            Self::Spec { .. } => TunnelField::BindPort,
        }
    }
}

fn invalid(field: TunnelField, reason: &'static str) -> TunnelError {
    TunnelError::Invalid { field, reason }
}

/// Checks a single-word value: an address, a host name or a target host.
fn check_word(field: TunnelField, value: &str) -> Result<(), TunnelError> {
    if value.is_empty() {
        return Err(invalid(field, "must not be empty"));
    }
    if !value.chars().all(|character| field.permits(character)) {
        return Err(invalid(
            field,
            "must not contain whitespace, control characters, brackets or '/'",
        ));
    }
    Ok(())
}

fn check_port(field: TunnelField, value: &str) -> Result<u16, TunnelError> {
    parse_port(value).map_err(|_| invalid(field, "must be an integer from 1 through 65535"))
}

/// Writes `address` as OpenSSH expects it in a forward: IPv6 addresses in brackets.
fn bracketed(address: &str) -> String {
    if address.contains(':') {
        format!("[{address}]")
    } else {
        address.to_owned()
    }
}

/// Splits a forward spec on `:`, keeping bracketed parts (IPv6 addresses) whole and unbracketed.
fn split_spec(spec: &str) -> Option<Vec<String>> {
    let mut parts = Vec::new();
    let mut rest = spec;
    loop {
        let (part, after) = if let Some(inner) = rest.strip_prefix('[') {
            let end = inner.find(']')?;
            let after = &inner[end + 1..];
            if !(after.is_empty() || after.starts_with(':')) {
                return None;
            }
            (inner[..end].to_owned(), after)
        } else {
            match rest.find(':') {
                Some(end) => (rest[..end].to_owned(), &rest[end..]),
                None => (rest.to_owned(), ""),
            }
        };
        parts.push(part);
        match after.strip_prefix(':') {
            Some(next) => rest = next,
            None => return Some(parts),
        }
    }
}

impl Forward {
    /// Parses a forward in ssh's command-line syntax: `[bind_address:]port:host:hostport` for
    /// local and remote forwards, `[bind_address:]port` for dynamic ones.
    pub fn parse(kind: Kind, spec: &str) -> Result<Self, TunnelError> {
        let refuse = |reason| TunnelError::Spec {
            kind,
            spec: spec.into(),
            reason,
        };
        if spec.contains('/') {
            return Err(refuse("Unix-socket forwards are not supported"));
        }
        if spec.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(refuse("must not contain whitespace or control characters"));
        }
        let parts = split_spec(spec).ok_or_else(|| refuse("unbalanced brackets"))?;
        let (bind, port, destination) = match (kind, parts.as_slice()) {
            (Kind::Dynamic, [port]) => (None, port, None),
            (Kind::Dynamic, [bind, port]) => (Some(bind), port, None),
            (Kind::Remote, [_] | [_, _]) => {
                return Err(refuse("remote dynamic forwards are not supported"));
            }
            (Kind::Local | Kind::Remote, [port, host, target]) => {
                (None, port, Some((host, target)))
            }
            (Kind::Local | Kind::Remote, [bind, port, host, target]) => {
                (Some(bind), port, Some((host, target)))
            }
            (Kind::Dynamic, _) => return Err(refuse("expected [bind_address:]port")),
            (Kind::Local | Kind::Remote, _) => {
                return Err(refuse("expected [bind_address:]port:host:hostport"));
            }
        };
        let bind_port = port
            .parse::<u16>()
            .ok()
            .filter(|port| *port != 0)
            .ok_or_else(|| refuse("the port must be an integer from 1 through 65535"))?;
        let bind_address = match bind {
            // ssh reads an empty bind address as every address.
            Some(bind) if bind.is_empty() => Some("*".to_owned()),
            Some(bind) => {
                check_word(TunnelField::BindAddress, bind)
                    .map_err(|_| refuse("invalid bind address"))?;
                Some(bind.clone())
            }
            None => None,
        };
        let destination = match destination {
            Some((host, target)) => {
                check_word(TunnelField::TargetHost, host)
                    .map_err(|_| refuse("invalid target host"))?;
                let port = target
                    .parse::<u16>()
                    .ok()
                    .filter(|port| *port != 0)
                    .ok_or_else(|| {
                        refuse("the target port must be an integer from 1 through 65535")
                    })?;
                Some(Destination {
                    host: host.clone(),
                    port,
                })
            }
            None => None,
        };
        Ok(Self {
            kind,
            bind_address,
            bind_port,
            destination,
        })
    }

    /// Builds a forward from form fields, each checked on its own so errors name their field.
    pub fn from_fields(
        kind: Kind,
        bind_address: &str,
        bind_port: &str,
        target_host: &str,
        target_port: &str,
    ) -> Result<Self, TunnelError> {
        let bind_address = match bind_address {
            "" => None,
            address => {
                check_word(TunnelField::BindAddress, address)?;
                Some(address.to_owned())
            }
        };
        let bind_port = check_port(TunnelField::BindPort, bind_port)?;
        let destination = match kind {
            Kind::Dynamic => None,
            Kind::Local | Kind::Remote => {
                check_word(TunnelField::TargetHost, target_host)?;
                Some(Destination {
                    host: target_host.to_owned(),
                    port: check_port(TunnelField::TargetPort, target_port)?,
                })
            }
        };
        Ok(Self {
            kind,
            bind_address,
            bind_port,
            destination,
        })
    }

    /// `[bind_address:]port`, as both the command line and the file write it.
    pub fn listen(&self) -> String {
        match &self.bind_address {
            Some(address) => format!("{}:{}", bracketed(address), self.bind_port),
            None => self.bind_port.to_string(),
        }
    }

    fn destination_text(&self) -> Option<String> {
        self.destination
            .as_ref()
            .map(|destination| format!("{}:{}", bracketed(&destination.host), destination.port))
    }

    /// The forward in ssh's command-line syntax, the argument after `-L`, `-R` or `-D`.
    pub fn spec(&self) -> String {
        match self.destination_text() {
            Some(destination) => format!("{}:{destination}", self.listen()),
            None => self.listen(),
        }
    }

    /// The value of the forward's directive in the tunnels file.
    fn directive_value(&self) -> String {
        match self.destination_text() {
            Some(destination) => format!("{} {destination}", self.listen()),
            None => self.listen(),
        }
    }

    /// The forward in a few words for lists: `5432 → localhost:5432`, `socks 1080` or
    /// `remote 8080 → localhost:3000`.
    pub fn describe(&self) -> String {
        let destination = self.destination_text().unwrap_or_default();
        match self.kind {
            Kind::Local => format!("{} \u{2192} {destination}", self.listen()),
            Kind::Remote => format!("remote {} \u{2192} {destination}", self.listen()),
            Kind::Dynamic => format!("socks {}", self.listen()),
        }
    }

    /// The local addresses this forward listens on, which must be free to start it: none for a
    /// remote forward. Without a bind address ssh listens on both loopback addresses, and `*`
    /// (or an empty address) on every address of both families.
    pub fn listen_addresses(&self) -> Vec<String> {
        if !self.kind.listens_locally() {
            return Vec::new();
        }
        listen_addresses(self.bind_address.as_deref(), false)
    }
}

/// The addresses a local listener with `bind_address` binds, when `gateway_ports` is ssh's
/// `GatewayPorts` setting (which makes the default every address).
pub fn listen_addresses(bind_address: Option<&str>, gateway_ports: bool) -> Vec<String> {
    match bind_address {
        None if gateway_ports => vec!["0.0.0.0".into(), "::".into()],
        None => vec!["127.0.0.1".into(), "::1".into()],
        Some(address) if address.eq_ignore_ascii_case("localhost") => {
            vec!["127.0.0.1".into(), "::1".into()]
        }
        Some("*" | "") => vec!["0.0.0.0".into(), "::".into()],
        Some(address) => vec![address.to_owned()],
    }
}

/// A saved tunnel.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Tunnel {
    pub name: String,
    pub host: String,
    pub forward: Forward,
}

impl Tunnel {
    pub fn new(
        name: impl Into<String>,
        host: impl Into<String>,
        forward: Forward,
    ) -> Result<Self, TunnelError> {
        let tunnel = Self {
            name: name.into(),
            host: host.into(),
            forward,
        };
        validate_name(&tunnel.name)?;
        validate_host(&tunnel.host)?;
        Ok(tunnel)
    }
}

pub fn validate_name(name: &str) -> Result<(), TunnelError> {
    HostField::Alias.validate(name).map_err(|_| {
        invalid(
            TunnelField::Name,
            "must start with a letter or digit and contain only letters, digits, '.', '_' or '-'",
        )
    })
}

pub fn validate_host(host: &str) -> Result<(), TunnelError> {
    if host.is_empty() {
        return Err(invalid(TunnelField::Host, "must not be empty"));
    }
    if host.starts_with('-') || host.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(invalid(
            TunnelField::Host,
            "must be a host alias without whitespace",
        ));
    }
    Ok(())
}

/// A line of the tunnels file that could not be read.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[error("line {line}: {reason}")]
pub struct TunnelFileError {
    pub line: usize,
    pub reason: String,
}

/// The tunnels file's content: the saved tunnels in name order.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TunnelsFile {
    tunnels: Vec<Tunnel>,
}

const FILE_HEADER: &str = "# Portico tunnels. Managed by Portico: edit with `portico tunnel` or the TUI.\n\
# This file is not read by OpenSSH.\n";

#[derive(Debug, Error, Eq, PartialEq)]
pub enum FileChangeError {
    #[error("a tunnel named {0} already exists")]
    Exists(String),
    #[error("no tunnel named {0}")]
    Unknown(String),
}

impl TunnelsFile {
    pub fn parse(input: &str) -> Result<Self, TunnelFileError> {
        struct Block {
            line: usize,
            name: String,
            host: Option<String>,
            forward: Option<Forward>,
        }
        fn finish(block: Block) -> Result<Tunnel, TunnelFileError> {
            let error = |reason: String| TunnelFileError {
                line: block.line,
                reason,
            };
            let host = block
                .host
                .clone()
                .ok_or_else(|| error(format!("tunnel {} has no Host", block.name)))?;
            let forward = block
                .forward
                .clone()
                .ok_or_else(|| error(format!("tunnel {} has no forward", block.name)))?;
            Tunnel::new(block.name.clone(), host, forward).map_err(|e| error(e.to_string()))
        }
        let mut file = Self::default();
        let mut block: Option<Block> = None;
        for (index, raw) in input.lines().enumerate() {
            let line = index + 1;
            let text = raw.trim();
            if text.is_empty() || text.starts_with('#') {
                continue;
            }
            let error = |reason: String| TunnelFileError { line, reason };
            let (directive, value) = match text.split_once(char::is_whitespace) {
                Some((directive, value)) => (directive, value.trim()),
                None => (text, ""),
            };
            if directive.eq_ignore_ascii_case("Tunnel") {
                if let Some(done) = block.take() {
                    file.push(finish(done)?, line)?;
                }
                if value.is_empty() || value.contains(char::is_whitespace) {
                    return Err(error("expected `Tunnel <name>`".into()));
                }
                block = Some(Block {
                    line,
                    name: value.into(),
                    host: None,
                    forward: None,
                });
                continue;
            }
            let Some(current) = block.as_mut() else {
                return Err(error(format!("{directive} outside a Tunnel block")));
            };
            if directive.eq_ignore_ascii_case("Host") {
                if current.host.is_some() {
                    return Err(error("a second Host".into()));
                }
                if value.is_empty() || value.contains(char::is_whitespace) {
                    return Err(error("expected `Host <alias>`".into()));
                }
                current.host = Some(value.into());
            } else if let Some(kind) = Kind::from_directive(directive) {
                if current.forward.is_some() {
                    return Err(error("a tunnel holds exactly one forward".into()));
                }
                let words: Vec<&str> = value.split_whitespace().collect();
                let spec = match (kind, words.as_slice()) {
                    (Kind::Dynamic, [listen]) => (*listen).to_owned(),
                    (Kind::Local | Kind::Remote, [listen, destination]) => {
                        format!("{listen}:{destination}")
                    }
                    _ => return Err(error(format!("malformed {directive}"))),
                };
                current.forward =
                    Some(Forward::parse(kind, &spec).map_err(|e| error(e.to_string()))?);
            } else {
                return Err(error(format!("unknown directive {directive}")));
            }
        }
        if let Some(done) = block.take() {
            let line = done.line;
            file.push(finish(done)?, line)?;
        }
        Ok(file)
    }

    fn push(&mut self, tunnel: Tunnel, line: usize) -> Result<(), TunnelFileError> {
        if self.find(&tunnel.name).is_some() {
            return Err(TunnelFileError {
                line,
                reason: format!("a second tunnel named {}", tunnel.name),
            });
        }
        self.tunnels.push(tunnel);
        self.sort();
        Ok(())
    }

    fn sort(&mut self) {
        self.tunnels
            .sort_by_key(|tunnel| tunnel.name.to_ascii_lowercase());
    }

    pub fn render(&self) -> String {
        let mut output = FILE_HEADER.to_owned();
        for tunnel in &self.tunnels {
            output.push_str(&format!(
                "\nTunnel {}\n    Host {}\n    {} {}\n",
                tunnel.name,
                tunnel.host,
                tunnel.forward.kind.directive(),
                tunnel.forward.directive_value()
            ));
        }
        output
    }

    pub fn tunnels(&self) -> &[Tunnel] {
        &self.tunnels
    }

    pub fn find(&self, name: &str) -> Option<&Tunnel> {
        self.tunnels
            .iter()
            .find(|tunnel| tunnel.name.eq_ignore_ascii_case(name))
    }

    pub fn insert(&mut self, tunnel: Tunnel) -> Result<(), FileChangeError> {
        if self.find(&tunnel.name).is_some() {
            return Err(FileChangeError::Exists(tunnel.name));
        }
        self.tunnels.push(tunnel);
        self.sort();
        Ok(())
    }

    /// Replaces the tunnel named `name` with `tunnel`, which may carry a new name.
    pub fn replace(&mut self, name: &str, tunnel: Tunnel) -> Result<(), FileChangeError> {
        let index = self
            .tunnels
            .iter()
            .position(|saved| saved.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| FileChangeError::Unknown(name.into()))?;
        if !tunnel.name.eq_ignore_ascii_case(name) && self.find(&tunnel.name).is_some() {
            return Err(FileChangeError::Exists(tunnel.name));
        }
        self.tunnels[index] = tunnel;
        self.sort();
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> Result<Tunnel, FileChangeError> {
        let index = self
            .tunnels
            .iter()
            .position(|saved| saved.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| FileChangeError::Unknown(name.into()))?;
        Ok(self.tunnels.remove(index))
    }
}

/// Whether two listen addresses overlap: equal, or either is the wildcard of the other's
/// family. ssh listens on IPv6 with `IPV6_V6ONLY`, so `::` never covers IPv4 addresses.
pub fn addresses_overlap(a: &str, b: &str) -> bool {
    let v6 = |address: &str| address.contains(':');
    a.eq_ignore_ascii_case(b)
        || (a == "::" && v6(b))
        || (b == "::" && v6(a))
        || (a == "0.0.0.0" && !v6(b))
        || (b == "0.0.0.0" && !v6(a))
}

/// `text` safe to print and to store as one line: terminal escape sequences and control
/// characters (output of a `ProxyCommand`, say) are removed, and tabs become spaces.
pub fn printable(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\u{1b}' => match characters.next() {
                // CSI: parameters up to a final byte from '@' to '~'.
                Some('[') => {
                    for next in characters.by_ref() {
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                }
                // OSC and other strings: up to BEL or ESC '\\'.
                Some(']' | 'P' | 'X' | '^' | '_') => {
                    while let Some(next) = characters.next() {
                        if next == '\u{7}' {
                            break;
                        }
                        if next == '\u{1b}' {
                            characters.next_if_eq(&'\\');
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\t' => output.push(' '),
            character if character.is_control() => {}
            character => output.push(character),
        }
    }
    output
}

/// Whether `a` and `b`, two saved tunnels, cannot run at the same time: both listen on this
/// machine on the same port and overlapping addresses, or both are remote forwards of the same
/// port on the same host.
pub fn collide(a: &Tunnel, b: &Tunnel) -> bool {
    if a.name.eq_ignore_ascii_case(&b.name) || a.forward.bind_port != b.forward.bind_port {
        return false;
    }
    match (
        a.forward.kind.listens_locally(),
        b.forward.kind.listens_locally(),
    ) {
        (true, true) => {
            let (left, right) = (a.forward.listen_addresses(), b.forward.listen_addresses());
            left.iter()
                .any(|x| right.iter().any(|y| addresses_overlap(x, y)))
        }
        (false, false) => a.host.eq_ignore_ascii_case(&b.host),
        _ => false,
    }
}

/// The names of the saved tunnels that collide with `tunnel`.
pub fn collisions(saved: &[Tunnel], tunnel: &Tunnel) -> Vec<String> {
    saved
        .iter()
        .filter(|other| collide(tunnel, other))
        .map(|other| other.name.clone())
        .collect()
}

/// What a tunnel is doing now. `--json` serializes it as it is: the variant's `name()` and the
/// fields under these names. It holds nothing unsafe to print.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TunnelStatus {
    Stopped,
    /// `started` is in Unix seconds.
    Running {
        pid: u32,
        started: u64,
    },
    /// `finished` is in Unix seconds, when known.
    Failed {
        reason: String,
        finished: Option<u64>,
    },
    /// `by` names the saved tunnel holding the port, when it is one.
    PortInUse {
        address: String,
        by: Option<String>,
    },
}

impl TunnelStatus {
    /// The stable lowercase name.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Running { .. } => "running",
            Self::Failed { .. } => "failed",
            Self::PortInUse { .. } => "port-in-use",
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }

    /// What the status column shows after the name: the PID and start time of a running
    /// tunnel, a failure's reason, or the busy address and who holds it.
    pub fn detail(&self) -> String {
        match self {
            Self::Stopped => String::new(),
            Self::Running { pid, started } => format!("pid {pid}, since {}", clock(*started)),
            Self::Failed { reason, .. } => reason.clone(),
            Self::PortInUse { address, by } => match by {
                Some(by) => format!("{address} is used by tunnel {by}"),
                None => format!("{address} is in use"),
            },
        }
    }
}

/// `HH:MM` in local time of a Unix time.
fn clock(unix: u64) -> String {
    let Ok(time) = libc::time_t::try_from(unix) else {
        return "--:--".into();
    };
    // SAFETY: `localtime_r` only writes the `tm` it is given.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&time, &mut tm).is_null() {
            return "--:--".into();
        }
        format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
    }
}

/// Something about a saved tunnel worth knowing that does not stop it from being listed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TunnelWarning {
    /// The tunnel's host is no longer in the inventory.
    UnknownHost { host: String },
    /// Another saved tunnel uses the same port; only one of them can run at a time.
    CollidesWith { tunnel: String },
}

impl fmt::Display for TunnelWarning {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownHost { host } => {
                write!(
                    formatter,
                    "host {host} is not in the inventory; edit the tunnel"
                )
            }
            Self::CollidesWith { tunnel } => write!(
                formatter,
                "uses the same port as tunnel {tunnel}; only one of them can run at a time"
            ),
        }
    }
}

/// A saved tunnel with its status, as every interface shows it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TunnelReport {
    pub tunnel: Tunnel,
    pub status: TunnelStatus,
    pub warnings: Vec<TunnelWarning>,
}

/// The `ssh` arguments that start `forward` through `host` in the background, logging to `log`.
pub fn start_args(log: &Path, host: &str, forward: &Forward) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["-f".into(), "-N".into()];
    for option in [
        "ExitOnForwardFailure=yes",
        "BatchMode=yes",
        "ControlMaster=no",
        "ControlPath=none",
        "RemoteCommand=none",
    ] {
        args.push("-o".into());
        args.push(option.into());
    }
    args.push("-E".into());
    args.push(log.into());
    args.push(forward.kind.flag().into());
    args.push(forward.spec().into());
    args.push("--".into());
    args.push(host.into());
    args
}

/// The arguments of `ssh -G` that check `forward` through `host` and list what it would bind.
pub fn check_args(host: &str, forward: &Forward) -> Vec<OsString> {
    vec![
        "-G".into(),
        "-o".into(),
        "RemoteCommand=none".into(),
        forward.kind.flag().into(),
        forward.spec().into(),
        "--".into(),
        host.into(),
    ]
}

/// The local listeners `ssh -G` output says ssh would open, as `(address, port)` pairs, with
/// `GatewayPorts` applied to forwards without a bind address.
pub fn listeners_in_effective_config(output: &str) -> Vec<(String, u16)> {
    let gateway_ports = output.lines().any(|line| {
        let mut words = line.split_whitespace();
        words
            .next()
            .is_some_and(|key| key.eq_ignore_ascii_case("gatewayports"))
            && words
                .next()
                .is_some_and(|value| value.eq_ignore_ascii_case("yes"))
    });
    let mut listeners = Vec::new();
    for line in output.lines() {
        let mut words = line.split_whitespace();
        let Some(key) = words.next() else { continue };
        if !(key.eq_ignore_ascii_case("localforward") || key.eq_ignore_ascii_case("dynamicforward"))
        {
            continue;
        }
        let Some(listen) = words.next() else { continue };
        let Some((address, port)) = parse_listen(listen) else {
            continue;
        };
        for address in listen_addresses(address.as_deref(), gateway_ports) {
            if !listeners.contains(&(address.clone(), port)) {
                listeners.push((address, port));
            }
        }
    }
    listeners
}

/// Parses `[address:]port`, with an IPv6 address in brackets; `None` for a socket path.
fn parse_listen(listen: &str) -> Option<(Option<String>, u16)> {
    let parts = split_spec(listen)?;
    match parts.as_slice() {
        [port] => Some((None, port.parse().ok()?)),
        [address, port] => Some((Some(address.clone()), port.parse().ok()?)),
        _ => None,
    }
}

impl fmt::Display for Tunnel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} ({} through {})",
            self.name,
            self.forward.describe(),
            self.host
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(spec: &str) -> Forward {
        Forward::parse(Kind::Local, spec).unwrap()
    }

    fn tunnel(name: &str, host: &str, kind: Kind, spec: &str) -> Tunnel {
        Tunnel::new(name, host, Forward::parse(kind, spec).unwrap()).unwrap()
    }

    #[test]
    fn local_specs_parse_and_print_in_ssh_syntax() {
        let forward = local("5432:localhost:5432");
        assert_eq!(forward.bind_address, None);
        assert_eq!(forward.bind_port, 5432);
        assert_eq!(
            forward.destination,
            Some(Destination {
                host: "localhost".into(),
                port: 5432
            })
        );
        assert_eq!(forward.spec(), "5432:localhost:5432");
        let bound = local("127.0.0.1:5432:db.internal:5433");
        assert_eq!(bound.bind_address.as_deref(), Some("127.0.0.1"));
        assert_eq!(bound.spec(), "127.0.0.1:5432:db.internal:5433");
        assert_eq!(bound.describe(), "127.0.0.1:5432 \u{2192} db.internal:5433");
    }

    #[test]
    fn ipv6_addresses_keep_their_brackets_in_specs_only() {
        let forward = local("[::1]:6000:[fe80::1]:22");
        assert_eq!(forward.bind_address.as_deref(), Some("::1"));
        assert_eq!(forward.destination.as_ref().unwrap().host, "fe80::1");
        assert_eq!(forward.spec(), "[::1]:6000:[fe80::1]:22");
    }

    #[test]
    fn dynamic_and_remote_specs() {
        let socks = Forward::parse(Kind::Dynamic, "1080").unwrap();
        assert_eq!(socks.describe(), "socks 1080");
        assert_eq!(socks.destination, None);
        let wildcard = Forward::parse(Kind::Dynamic, "*:1080").unwrap();
        assert_eq!(wildcard.bind_address.as_deref(), Some("*"));
        let empty = Forward::parse(Kind::Dynamic, ":1080").unwrap();
        assert_eq!(empty.bind_address.as_deref(), Some("*"));
        let remote = Forward::parse(Kind::Remote, "8080:localhost:3000").unwrap();
        assert_eq!(remote.describe(), "remote 8080 \u{2192} localhost:3000");
        assert!(remote.listen_addresses().is_empty());
    }

    #[test]
    fn rejected_specs_say_why() {
        for (kind, spec, reason) in [
            (Kind::Local, "/tmp/s:db:1", "Unix-socket"),
            (Kind::Local, "5432:/run/db.sock", "Unix-socket"),
            (Kind::Remote, "8080", "remote dynamic"),
            (Kind::Local, "0:db:1", "port"),
            (Kind::Local, "65536:db:1", "port"),
            (Kind::Local, "5432:db:0", "target port"),
            (Kind::Local, "5432:db", "expected"),
            (Kind::Dynamic, "1:2:3", "expected"),
            (Kind::Local, "54 32:db:1", "whitespace"),
            (Kind::Local, "[::1:5432:db:1", "brackets"),
            (Kind::Local, "x:abc:db:1", "port"),
        ] {
            let error = Forward::parse(kind, spec).unwrap_err().to_string();
            assert!(error.contains(reason), "{spec}: {error}");
        }
    }

    #[test]
    fn form_fields_name_the_field_at_fault() {
        let error = Forward::from_fields(Kind::Local, "", "99999", "db", "1").unwrap_err();
        assert_eq!(error.field(), TunnelField::BindPort);
        let error = Forward::from_fields(Kind::Local, "", "1", "", "1").unwrap_err();
        assert_eq!(error.field(), TunnelField::TargetHost);
        let error = Forward::from_fields(Kind::Remote, "", "1", "db", "x").unwrap_err();
        assert_eq!(error.field(), TunnelField::TargetPort);
        let error = Forward::from_fields(Kind::Local, "a b", "1", "db", "1").unwrap_err();
        assert_eq!(error.field(), TunnelField::BindAddress);
        let socks = Forward::from_fields(Kind::Dynamic, "", "1080", "ignored", "").unwrap();
        assert_eq!(socks.destination, None);
    }

    #[test]
    fn names_and_hosts_are_validated() {
        let forward = local("1:db:1");
        assert!(Tunnel::new("db", "prod", forward.clone()).is_ok());
        assert_eq!(
            Tunnel::new("-db", "prod", forward.clone())
                .unwrap_err()
                .field(),
            TunnelField::Name
        );
        assert_eq!(
            Tunnel::new("db", "-oProxyCommand=x", forward.clone())
                .unwrap_err()
                .field(),
            TunnelField::Host
        );
        assert_eq!(
            Tunnel::new("db", "", forward).unwrap_err().field(),
            TunnelField::Host
        );
    }

    #[test]
    fn listen_addresses_test_both_loopbacks_by_default() {
        assert_eq!(local("5432:db:1").listen_addresses(), ["127.0.0.1", "::1"]);
        assert_eq!(
            local("localhost:5432:db:1").listen_addresses(),
            ["127.0.0.1", "::1"]
        );
        assert_eq!(local("*:5432:db:1").listen_addresses(), ["0.0.0.0", "::"]);
        assert_eq!(local("10.0.0.2:5432:db:1").listen_addresses(), ["10.0.0.2"]);
        assert_eq!(listen_addresses(None, true), ["0.0.0.0", "::"]);
    }

    #[test]
    fn the_file_round_trips_in_name_order() {
        let mut file = TunnelsFile::default();
        file.insert(tunnel("socks", "bastion", Kind::Dynamic, "1080"))
            .unwrap();
        file.insert(tunnel(
            "DB",
            "prod-db",
            Kind::Local,
            "127.0.0.1:5432:localhost:5432",
        ))
        .unwrap();
        file.insert(tunnel(
            "preview",
            "staging",
            Kind::Remote,
            "8080:localhost:3000",
        ))
        .unwrap();
        let text = file.render();
        assert!(text.starts_with("# Portico tunnels."), "{text}");
        assert!(
            text.contains(
                "\nTunnel DB\n    Host prod-db\n    LocalForward 127.0.0.1:5432 localhost:5432\n\nTunnel preview\n    Host staging\n    RemoteForward 8080 localhost:3000\n\nTunnel socks\n    Host bastion\n    DynamicForward 1080\n"
            ),
            "{text}"
        );
        assert_eq!(TunnelsFile::parse(&text).unwrap(), file);
        assert_eq!(TunnelsFile::parse("").unwrap(), TunnelsFile::default());
    }

    #[test]
    fn the_file_is_parsed_strictly() {
        for (text, line, reason) in [
            ("Host prod\n", 1, "outside"),
            ("Tunnel db\n Host prod\n", 1, "no forward"),
            ("Tunnel db\n DynamicForward 1\n", 1, "no Host"),
            (
                "Tunnel db\n Host a\n DynamicForward 1\n DynamicForward 2\n",
                4,
                "exactly one",
            ),
            ("Tunnel db\n Host a\n Port 22\n", 3, "unknown directive"),
            ("Tunnel db\n Host a\n LocalForward 1\n", 3, "malformed"),
            ("Tunnel db\n Host a\n LocalForward 0 db:1\n", 3, "port"),
            (
                "Tunnel db\n Host a\n DynamicForward 1\nTunnel DB\n Host b\n DynamicForward 2\n",
                4,
                "second tunnel",
            ),
            ("Tunnel -x\n Host a\n DynamicForward 1\n", 1, "name"),
        ] {
            let error = TunnelsFile::parse(text).unwrap_err();
            assert_eq!(error.line, line, "{text}: {error}");
            assert!(error.reason.contains(reason), "{text}: {error}");
        }
    }

    #[test]
    fn inserts_replaces_and_removes_by_name_ignoring_case() {
        let mut file = TunnelsFile::default();
        file.insert(tunnel("db", "a", Kind::Dynamic, "1")).unwrap();
        assert_eq!(
            file.insert(tunnel("DB", "a", Kind::Dynamic, "2")),
            Err(FileChangeError::Exists("DB".into()))
        );
        file.insert(tunnel("web", "a", Kind::Dynamic, "3")).unwrap();
        assert_eq!(
            file.replace("db", tunnel("Web", "a", Kind::Dynamic, "4")),
            Err(FileChangeError::Exists("Web".into()))
        );
        file.replace("DB", tunnel("Db", "b", Kind::Dynamic, "5"))
            .unwrap();
        assert_eq!(file.find("db").unwrap().host, "b");
        assert_eq!(file.remove("WEB").unwrap().name, "web");
        assert_eq!(
            file.remove("web"),
            Err(FileChangeError::Unknown("web".into()))
        );
    }

    #[test]
    fn collisions_need_the_same_port_and_overlapping_addresses() {
        let a = tunnel("a", "h1", Kind::Local, "5432:db:1");
        let b = tunnel("b", "h2", Kind::Local, "localhost:5432:db:1");
        let c = tunnel("c", "h2", Kind::Dynamic, "*:5432");
        let d = tunnel("d", "h2", Kind::Local, "10.0.0.2:5432:db:1");
        let e = tunnel("e", "h2", Kind::Local, "5433:db:1");
        let r1 = tunnel("r1", "h1", Kind::Remote, "5432:db:1");
        let r2 = tunnel("r2", "H1", Kind::Remote, "5432:db:2");
        let r3 = tunnel("r3", "h2", Kind::Remote, "5432:db:2");
        assert!(collide(&a, &b));
        assert!(collide(&a, &c));
        assert!(collide(&c, &d));
        assert!(!collide(&a, &d));
        assert!(!collide(&a, &e));
        assert!(!collide(&a, &r1));
        assert!(collide(&r1, &r2));
        assert!(!collide(&r1, &r3));
        let saved = [a.clone(), b, c, d, e, r1, r2, r3];
        assert_eq!(collisions(&saved, &a), ["b", "c"]);
    }

    #[test]
    fn printable_strips_escapes_and_control_characters() {
        assert_eq!(
            printable("\u{1b}[31mred\u{1b}[0m\tand\u{7} \u{1b}]0;title\u{7}done\r\n"),
            "red and done"
        );
        assert_eq!(
            printable("a\u{1b}]8;;x\u{1b}\\b\u{1b}c!"),
            "ab!",
            "ESC c is a sequence too"
        );
        assert_eq!(printable("caf\u{e9} ok"), "caf\u{e9} ok");
    }

    #[test]
    fn the_ipv6_wildcard_does_not_cover_ipv4() {
        assert!(addresses_overlap("::", "::1"));
        assert!(!addresses_overlap("::", "127.0.0.1"));
        assert!(addresses_overlap("0.0.0.0", "127.0.0.1"));
        assert!(!addresses_overlap("0.0.0.0", "::1"));
        assert!(!addresses_overlap("127.0.0.1", "127.0.0.2"));
    }

    #[test]
    fn status_names_are_stable() {
        assert_eq!(TunnelStatus::Stopped.name(), "stopped");
        assert_eq!(
            TunnelStatus::Running { pid: 1, started: 2 }.name(),
            "running"
        );
        assert_eq!(
            TunnelStatus::Failed {
                reason: "x".into(),
                finished: None
            }
            .name(),
            "failed"
        );
        assert_eq!(
            TunnelStatus::PortInUse {
                address: "127.0.0.1:1".into(),
                by: None
            }
            .name(),
            "port-in-use"
        );
    }

    #[test]
    fn start_args_are_the_documented_command_line() {
        let forward = local("127.0.0.1:5432:localhost:5432");
        let args = start_args(Path::new("/run/t/db.log"), "prod-db", &forward);
        let args: Vec<_> = args.iter().map(|arg| arg.to_str().unwrap()).collect();
        assert_eq!(
            args,
            [
                "-f",
                "-N",
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "BatchMode=yes",
                "-o",
                "ControlMaster=no",
                "-o",
                "ControlPath=none",
                "-o",
                "RemoteCommand=none",
                "-E",
                "/run/t/db.log",
                "-L",
                "127.0.0.1:5432:localhost:5432",
                "--",
                "prod-db"
            ]
        );
    }

    #[test]
    fn effective_config_listeners_include_the_hosts_own_forwards() {
        let output = "user me\nlocalforward 5432 [db]:5432\nlocalforward [::1]:6000 [x]:22\n\
                      dynamicforward 127.0.0.2:1080\nremoteforward 8080 [localhost]:3000\n\
                      localforward /tmp/s [db]:1\n";
        assert_eq!(
            listeners_in_effective_config(output),
            [
                ("127.0.0.1".to_owned(), 5432),
                ("::1".to_owned(), 5432),
                ("::1".to_owned(), 6000),
                ("127.0.0.2".to_owned(), 1080),
            ]
        );
        let gateway = "gatewayports yes\nlocalforward 5432 [db]:5432\n";
        assert_eq!(
            listeners_in_effective_config(gateway),
            [("0.0.0.0".to_owned(), 5432), ("::".to_owned(), 5432)]
        );
    }
}
