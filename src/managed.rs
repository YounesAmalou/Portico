use thiserror::Error;

use crate::host::{Host, HostError};

const HEADER: &str = "# Managed by Portico. Do not edit manually.";

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ManagedError {
    #[error("parse error on line {line}: {message}")]
    Parse { line: usize, message: String },
    #[error("unsupported directive on line {line}: {directive}")]
    UnsupportedDirective { line: usize, directive: String },
    #[error("duplicate directive on line {line}: {directive}")]
    DuplicateDirective { line: usize, directive: String },
    #[error("missing HostName for host declared on line {line}")]
    MissingHostName { line: usize },
    #[error("duplicate alias on line {line}: {alias}")]
    DuplicateAliasAtLine { line: usize, alias: String },
    #[error("invalid host on line {line}: {source}")]
    InvalidHost {
        line: usize,
        #[source]
        source: HostError,
    },
    #[error("duplicate alias: {0}")]
    DuplicateAlias(String),
    #[error("unknown alias: {0}")]
    UnknownAlias(String),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ManagedConfig {
    hosts: Vec<Host>,
}

impl ManagedConfig {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn parse(input: &str) -> Result<Self, ManagedError> {
        let mut config = Self::empty();
        let mut current = None;

        for (index, raw_line) in input.lines().enumerate() {
            let line = index + 1;
            let trimmed = raw_line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            let (directive, argument) = split_directive(trimmed);
            if directive.eq_ignore_ascii_case("Host") {
                if let Some(host) = current.take() {
                    config.push_parsed(host)?;
                }
                let alias = parse_value(argument, line)?;
                current = Some(ParsedHost::new(alias, line));
                continue;
            }

            let host = current.as_mut().ok_or_else(|| ManagedError::Parse {
                line,
                message: "directive appears before Host".into(),
            })?;
            if !is_supported_directive(directive) {
                return Err(ManagedError::UnsupportedDirective {
                    line,
                    directive: directive.into(),
                });
            }
            let value = parse_value(argument, line)?;
            if directive.eq_ignore_ascii_case("HostName") {
                set_once(&mut host.hostname, value, line, directive)?;
            } else if directive.eq_ignore_ascii_case("User") {
                set_once(&mut host.user, value, line, directive)?;
            } else if directive.eq_ignore_ascii_case("Port") {
                let port = value.parse().map_err(|_| ManagedError::Parse {
                    line,
                    message: format!("invalid port: {value}"),
                })?;
                set_once(&mut host.port, port, line, directive)?;
            } else if directive.eq_ignore_ascii_case("IdentityFile") {
                set_once(&mut host.identity_file, value, line, directive)?;
            } else if directive.eq_ignore_ascii_case("ProxyJump") {
                set_once(&mut host.proxy_jump, value, line, directive)?;
            }
        }

        if let Some(host) = current {
            config.push_parsed(host)?;
        }
        Ok(config)
    }

    pub fn render(&self) -> String {
        let mut hosts = self.hosts.iter().collect::<Vec<_>>();
        hosts.sort_by_key(|host| host.alias().to_ascii_lowercase());
        let blocks = hosts.into_iter().map(render_host).collect::<Vec<_>>();
        if blocks.is_empty() {
            format!("{HEADER}\n")
        } else {
            format!("{HEADER}\n\n{}\n", blocks.join("\n\n"))
        }
    }

    pub fn hosts(&self) -> &[Host] {
        &self.hosts
    }

    pub fn find(&self, alias: &str) -> Option<&Host> {
        self.hosts
            .iter()
            .find(|host| host.alias().eq_ignore_ascii_case(alias))
    }

    pub fn insert(&mut self, host: Host) -> Result<(), ManagedError> {
        if self.find(host.alias()).is_some() {
            return Err(ManagedError::DuplicateAlias(host.alias().into()));
        }
        self.hosts.push(host);
        self.sort_hosts();
        Ok(())
    }

    pub fn replace(&mut self, old_alias: &str, host: Host) -> Result<(), ManagedError> {
        let index = self
            .hosts
            .iter()
            .position(|existing| existing.alias().eq_ignore_ascii_case(old_alias))
            .ok_or_else(|| ManagedError::UnknownAlias(old_alias.into()))?;
        if self.hosts.iter().enumerate().any(|(other, existing)| {
            other != index && existing.alias().eq_ignore_ascii_case(host.alias())
        }) {
            return Err(ManagedError::DuplicateAlias(host.alias().into()));
        }
        self.hosts[index] = host;
        Ok(())
    }

    pub fn remove(&mut self, alias: &str) -> Result<Host, ManagedError> {
        let index = self
            .hosts
            .iter()
            .position(|host| host.alias().eq_ignore_ascii_case(alias))
            .ok_or_else(|| ManagedError::UnknownAlias(alias.into()))?;
        Ok(self.hosts.remove(index))
    }

    fn push_parsed(&mut self, parsed: ParsedHost) -> Result<(), ManagedError> {
        let ParsedHost {
            alias,
            line,
            hostname,
            user,
            port,
            identity_file,
            proxy_jump,
        } = parsed;
        let hostname = hostname.ok_or(ManagedError::MissingHostName { line })?;
        let hostname_line = hostname.1;
        let user_line = user.as_ref().map_or(line, |value| value.1);
        let port_line = port.as_ref().map_or(line, |value| value.1);
        let identity_line = identity_file.as_ref().map_or(line, |value| value.1);
        let jump_line = proxy_jump.as_ref().map_or(line, |value| value.1);
        let host = Host::new(
            alias,
            hostname.0,
            user.map(|value| value.0),
            port.map(|value| value.0),
            identity_file.map(|value| value.0),
            proxy_jump.map(|value| value.0),
        )
        .map_err(|source| {
            let line = match source {
                HostError::InvalidAlias(_) => line,
                HostError::InvalidHostname(_) => hostname_line,
                HostError::InvalidUser(_) => user_line,
                HostError::InvalidPort(_) => port_line,
                HostError::InvalidIdentity(_) => identity_line,
                HostError::InvalidJump(_) => jump_line,
                HostError::RequiredField(_) => line,
            };
            ManagedError::InvalidHost { line, source }
        })?;
        if self.find(host.alias()).is_some() {
            return Err(ManagedError::DuplicateAliasAtLine {
                line,
                alias: host.alias().into(),
            });
        }
        self.hosts.push(host);
        self.sort_hosts();
        Ok(())
    }

    fn sort_hosts(&mut self) {
        self.hosts
            .sort_by_key(|host| host.alias().to_ascii_lowercase());
    }
}

struct ParsedHost {
    alias: String,
    line: usize,
    hostname: Option<(String, usize)>,
    user: Option<(String, usize)>,
    port: Option<(u16, usize)>,
    identity_file: Option<(String, usize)>,
    proxy_jump: Option<(String, usize)>,
}

impl ParsedHost {
    fn new(alias: String, line: usize) -> Self {
        Self {
            alias,
            line,
            hostname: None,
            user: None,
            port: None,
            identity_file: None,
            proxy_jump: None,
        }
    }
}

fn split_directive(line: &str) -> (&str, &str) {
    line.split_once(char::is_whitespace)
        .map_or((line, ""), |(directive, argument)| {
            (directive, argument.trim())
        })
}

fn is_supported_directive(directive: &str) -> bool {
    ["HostName", "User", "Port", "IdentityFile", "ProxyJump"]
        .iter()
        .any(|supported| directive.eq_ignore_ascii_case(supported))
}

fn set_once<T>(
    field: &mut Option<(T, usize)>,
    value: T,
    line: usize,
    directive: &str,
) -> Result<(), ManagedError> {
    if field.is_some() {
        return Err(ManagedError::DuplicateDirective {
            line,
            directive: directive.into(),
        });
    }
    *field = Some((value, line));
    Ok(())
}

fn parse_value(input: &str, line: usize) -> Result<String, ManagedError> {
    if input.is_empty() {
        return Err(ManagedError::Parse {
            line,
            message: "missing value".into(),
        });
    }
    if input.len() < 2 || !input.starts_with('"') || !input.ends_with('"') {
        if input.chars().any(char::is_whitespace) {
            return Err(ManagedError::Parse {
                line,
                message: "expected one value".into(),
            });
        }
        return Ok(input.into());
    }

    let mut value = String::new();
    let mut escaped = false;
    let mut end = None;
    for (index, character) in input.char_indices().skip(1) {
        if escaped {
            if character != '\\' && character != '"' {
                return Err(ManagedError::Parse {
                    line,
                    message: "invalid escape".into(),
                });
            }
            value.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == '"' {
            end = Some(index + character.len_utf8());
            break;
        } else {
            value.push(character);
        }
    }
    let end = end.ok_or_else(|| ManagedError::Parse {
        line,
        message: "unterminated quoted value".into(),
    })?;
    if !input[end..].trim().is_empty() {
        return Err(ManagedError::Parse {
            line,
            message: "expected one value".into(),
        });
    }
    Ok(value)
}

fn render_host(host: &Host) -> String {
    let mut lines = vec![
        format!("Host {}", quote(host.alias())),
        format!("    HostName {}", quote(host.hostname())),
    ];
    if let Some(user) = host.user() {
        lines.push(format!("    User {}", quote(user)));
    }
    if let Some(port) = host.port() {
        lines.push(format!("    Port {port}"));
    }
    if let Some(identity_file) = host.identity_file() {
        lines.push(format!("    IdentityFile {}", quote(identity_file)));
    }
    if let Some(proxy_jump) = host.proxy_jump() {
        lines.push(format!("    ProxyJump {}", quote(proxy_jump)));
    }
    lines.join("\n")
}

fn quote(value: &str) -> String {
    if value.chars().any(char::is_whitespace) {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        value.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_supported_hosts_in_stable_order() {
        let input = "# Managed by Portico. Do not edit manually.\n\nHost zed\n    HostName z.example\n\nHost alpha\n    HostName a.example\n    User deploy\n    Port 2222\n    IdentityFile ~/.ssh/id_alpha\n    ProxyJump bastion\n";
        let parsed = ManagedConfig::parse(input).unwrap();
        let rendered = parsed.render();

        assert!(rendered.find("Host alpha").unwrap() < rendered.find("Host zed").unwrap());
        assert_eq!(ManagedConfig::parse(&rendered).unwrap(), parsed);
    }

    #[test]
    fn rejects_unknown_or_duplicate_directives() {
        assert!(matches!(
            ManagedConfig::parse("Host dev\n  HostName dev\n  LocalForward 3000 dev:3000\n"),
            Err(ManagedError::UnsupportedDirective { .. })
        ));
        assert!(matches!(
            ManagedConfig::parse("Host dev\n  HostName one\n  HostName two\n"),
            Err(ManagedError::DuplicateDirective { .. })
        ));
    }

    #[test]
    fn quotes_identity_paths_as_one_argument() {
        let input = "Host dev\n  HostName dev.example\n  IdentityFile \"~/.ssh/key with space\"\n";
        let parsed = ManagedConfig::parse(input).unwrap();
        let rendered = parsed.render();
        assert!(rendered.contains("IdentityFile \"~/.ssh/key with space\""));
        assert_eq!(ManagedConfig::parse(&rendered).unwrap(), parsed);
    }

    #[test]
    fn collection_operations_are_case_insensitive() {
        let mut config = ManagedConfig::empty();
        config
            .insert(Host::new("Prod", "prod.example", None, None, None, None).unwrap())
            .unwrap();
        assert!(matches!(
            config.insert(Host::new("prod", "other.example", None, None, None, None).unwrap()),
            Err(ManagedError::DuplicateAlias(_))
        ));
        assert_eq!(config.remove("PROD").unwrap().hostname(), "prod.example");
    }

    #[test]
    fn reports_the_invalid_field_line() {
        assert!(matches!(
            ManagedConfig::parse("Host -invalid\n  HostName dev.example\n"),
            Err(ManagedError::InvalidHost { line: 1, .. })
        ));
    }

    #[test]
    fn round_trips_unquoted_values_containing_quotes() {
        let mut config = ManagedConfig::empty();
        config
            .insert(
                Host::new("dev", "dev.example", None, None, Some("\"key".into()), None).unwrap(),
            )
            .unwrap();
        assert_eq!(ManagedConfig::parse(&config.render()).unwrap(), config);
    }
}
