use std::{collections::BTreeMap, mem};

use super::{HEADER, HostComments, ManagedConfig, ManagedError, render::render_host};
use crate::domain::host::{Host, HostChanges, HostError, HostField, parse_port};

impl ManagedConfig {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn parse(input: &str) -> Result<Self, ManagedError> {
        let mut config = Self::empty();
        let mut current = None;
        let mut pending: Vec<String> = Vec::new();

        for (index, raw_line) in input.lines().enumerate() {
            let line = index + 1;
            let trimmed = raw_line.trim();
            // A comment separated by blank lines still belongs to what follows it; only the
            // header Portico writes on the first line is no host's.
            if trimmed.is_empty() || (index == 0 && trimmed == HEADER) {
                continue;
            }
            if trimmed.starts_with('#') {
                pending.push(trimmed.to_owned());
                continue;
            }

            let (directive, argument) = split_directive(trimmed);
            if directive.eq_ignore_ascii_case("Host") {
                if let Some(host) = current.take() {
                    config.push_parsed(host)?;
                }
                let alias = parse_value(argument, line)?;
                current = Some(ParsedHost::new(alias, line, mem::take(&mut pending)));
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
            let value = if directive.eq_ignore_ascii_case("ProxyJump") {
                // OpenSSH parses the whole ProxyJump remainder, including literal quotes.
                argument.to_owned()
            } else {
                parse_value(argument, line)?
            };
            let field = if directive.eq_ignore_ascii_case("HostName") {
                set_once(&mut host.hostname, value, line, directive)?;
                HostField::Hostname
            } else if directive.eq_ignore_ascii_case("User") {
                set_once(&mut host.user, value, line, directive)?;
                HostField::User
            } else if directive.eq_ignore_ascii_case("Port") {
                let port = parse_port(&value)
                    .map_err(|source| ManagedError::InvalidHost { line, source })?;
                set_once(&mut host.port, port, line, directive)?;
                HostField::Port
            } else if directive.eq_ignore_ascii_case("IdentityFile") {
                set_once(&mut host.identity_file, value, line, directive)?;
                HostField::IdentityFile
            } else {
                set_once(&mut host.proxy_jump, value, line, directive)?;
                HostField::ProxyJump
            };
            let attached = mem::take(&mut pending);
            if !attached.is_empty() {
                host.field_comments.insert(field, attached);
            }
        }

        if let Some(mut host) = current {
            host.trailing_comments = pending;
            config.push_parsed(host)?;
        }
        Ok(config)
    }

    pub fn render(&self) -> String {
        let mut hosts = self.hosts.iter().collect::<Vec<_>>();
        hosts.sort_by_key(|host| host.alias().to_ascii_lowercase());
        let blocks = hosts
            .into_iter()
            .map(|host| render_host(host, self.comments(host.alias())))
            .collect::<Vec<_>>();
        if blocks.is_empty() {
            format!("{HEADER}\n")
        } else {
            format!("{HEADER}\n\n{}\n", blocks.join("\n\n"))
        }
    }

    pub fn hosts(&self) -> &[Host] {
        &self.hosts
    }

    pub fn comments(&self, alias: &str) -> Option<&HostComments> {
        self.comments.get(&alias.to_ascii_lowercase())
    }

    /// Aliases declared here that are new or differ from their declaration in `previous`.
    pub fn changed_aliases(&self, previous: &Self) -> Vec<String> {
        self.hosts
            .iter()
            .filter(|host| previous.find(host.alias()) != Some(*host))
            .map(|host| host.alias().to_owned())
            .collect()
    }

    pub fn find(&self, alias: &str) -> Option<&Host> {
        self.hosts
            .iter()
            .find(|host| host.alias().eq_ignore_ascii_case(alias))
    }

    pub fn insert(&mut self, host: Host) -> Result<(), ManagedError> {
        self.insert_with_comments(host, HostComments::default())
    }

    pub fn insert_with_comments(
        &mut self,
        host: Host,
        comments: HostComments,
    ) -> Result<(), ManagedError> {
        if self.find(host.alias()).is_some() {
            return Err(ManagedError::DuplicateAlias(host.alias().into()));
        }
        self.set_comments(&host, comments);
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
        if let Some(comments) = self.comments.remove(&old_alias.to_ascii_lowercase()) {
            self.set_comments(&host, comments);
        }
        self.hosts[index] = host;
        Ok(())
    }

    pub fn apply(&mut self, alias: &str, changes: HostChanges) -> Result<(), ManagedError> {
        let host = self
            .find(alias)
            .ok_or_else(|| ManagedError::UnknownAlias(alias.into()))?
            .apply(changes)
            .map_err(|source| ManagedError::InvalidChanges {
                alias: alias.into(),
                source,
            })?;
        self.replace(alias, host)
    }

    pub fn remove(&mut self, alias: &str) -> Result<Host, ManagedError> {
        self.remove_entry(alias).map(|(host, _)| host)
    }

    pub fn remove_entry(&mut self, alias: &str) -> Result<(Host, HostComments), ManagedError> {
        let index = self
            .hosts
            .iter()
            .position(|host| host.alias().eq_ignore_ascii_case(alias))
            .ok_or_else(|| ManagedError::UnknownAlias(alias.into()))?;
        let host = self.hosts.remove(index);
        let comments = self
            .comments
            .remove(&alias.to_ascii_lowercase())
            .unwrap_or_default();
        Ok((host, comments))
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
            leading_comments,
            field_comments,
            trailing_comments,
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
                HostError::Invalid { field, .. } => match field {
                    HostField::Alias => line,
                    HostField::Hostname => hostname_line,
                    HostField::User => user_line,
                    HostField::Port => port_line,
                    HostField::IdentityFile => identity_line,
                    HostField::ProxyJump => jump_line,
                },
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
        let comments = HostComments {
            leading: leading_comments,
            fields: field_comments,
            trailing: trailing_comments,
        };
        self.set_comments(&host, comments);
        self.hosts.push(host);
        self.sort_hosts();
        Ok(())
    }

    /// Stores the comments of `host`, dropping those of fields it does not set.
    fn set_comments(&mut self, host: &Host, mut comments: HostComments) {
        comments.fields.retain(|field, lines| {
            !lines.is_empty()
                && match field {
                    HostField::User => host.user().is_some(),
                    HostField::Port => host.port().is_some(),
                    HostField::IdentityFile => host.identity_file().is_some(),
                    HostField::ProxyJump => host.proxy_jump().is_some(),
                    HostField::Hostname => true,
                    HostField::Alias => false,
                }
        });
        let key = host.alias().to_ascii_lowercase();
        if comments.is_empty() {
            self.comments.remove(&key);
        } else {
            self.comments.insert(key, comments);
        }
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
    leading_comments: Vec<String>,
    field_comments: BTreeMap<HostField, Vec<String>>,
    trailing_comments: Vec<String>,
}

impl ParsedHost {
    fn new(alias: String, line: usize, leading_comments: Vec<String>) -> Self {
        Self {
            alias,
            line,
            hostname: None,
            user: None,
            port: None,
            identity_file: None,
            proxy_jump: None,
            leading_comments,
            field_comments: BTreeMap::new(),
            trailing_comments: Vec::new(),
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

pub(crate) fn parse_value(input: &str, line: usize) -> Result<String, ManagedError> {
    if input.is_empty() {
        return Err(ManagedError::Parse {
            line,
            message: "missing value".into(),
        });
    }
    if !input.starts_with('"') {
        if input
            .chars()
            .any(|character| character.is_whitespace() || ['"', '\'', '\\'].contains(&character))
        {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::host::{Change, Host, HostChanges, HostField};

    #[test]
    fn comments_round_trip_through_parse_and_render() {
        let text = "# Managed by Portico. Do not edit manually.\n\n# work box\nHost dev\n    HostName dev.example\n    # old account\n    User bob\n";
        let config = ManagedConfig::parse(text).unwrap();
        assert_eq!(config.comments("dev").unwrap().leading, vec!["# work box"]);
        assert_eq!(
            config.comments("DEV").unwrap().fields[&HostField::User],
            vec!["# old account"]
        );
        assert_eq!(config.render(), text);
    }

    #[test]
    fn rename_keeps_comments_and_clear_drops_field_comments() {
        let mut config = ManagedConfig::empty();
        let comments = HostComments {
            leading: vec!["# lead".into()],
            fields: BTreeMap::from([
                (HostField::User, vec!["# who".into()]),
                (HostField::Port, vec!["# p".into()]),
            ]),
            trailing: Vec::new(),
        };
        config
            .insert_with_comments(
                Host::new("dev", "h", Some("bob".into()), Some(2222), None, None).unwrap(),
                comments,
            )
            .unwrap();
        config
            .apply(
                "dev",
                HostChanges {
                    alias: Change::Set("box".into()),
                    user: Change::Clear,
                    ..HostChanges::default()
                },
            )
            .unwrap();
        let kept = config.comments("box").unwrap();
        assert_eq!(kept.leading, vec!["# lead"]);
        assert!(!kept.fields.contains_key(&HostField::User));
        assert_eq!(kept.fields[&HostField::Port], vec!["# p"]);
        assert!(config.comments("dev").is_none());
    }

    #[test]
    fn replace_drops_comments_of_fields_the_new_host_lacks() {
        let mut config = ManagedConfig::empty();
        let comments = HostComments {
            leading: vec!["# lead".into()],
            fields: BTreeMap::from([
                (HostField::User, vec!["# who".into()]),
                (HostField::Port, vec!["# p".into()]),
            ]),
            trailing: Vec::new(),
        };
        let host = Host::new("dev", "h", Some("bob".into()), Some(2222), None, None).unwrap();
        config.insert_with_comments(host.clone(), comments).unwrap();
        let edited = host
            .apply(HostChanges {
                user: Change::Clear,
                ..HostChanges::default()
            })
            .unwrap();
        config.replace("dev", edited).unwrap();
        let kept = config.comments("dev").unwrap();
        assert_eq!(kept.leading, vec!["# lead"]);
        assert!(!kept.fields.contains_key(&HostField::User));
        assert_eq!(kept.fields[&HostField::Port], vec!["# p"]);
        assert_eq!(ManagedConfig::parse(&config.render()).unwrap(), config);
    }

    #[test]
    fn keeps_detached_and_final_comments_but_never_the_header() {
        let input =
            format!("{HEADER}\n\n# prod database\n\nHost db\n    HostName d\n\n    # last\n");
        let config = ManagedConfig::parse(&input).unwrap();
        let comments = config.comments("db").unwrap();
        assert_eq!(comments.leading, ["# prod database"]);
        assert_eq!(comments.trailing, ["# last"]);
        assert_eq!(
            config.render(),
            format!("{HEADER}\n\n# prod database\nHost db\n    HostName d\n    # last\n")
        );
        assert_eq!(ManagedConfig::parse(&config.render()).unwrap(), config);
    }

    #[test]
    fn remove_entry_returns_the_comments() {
        let mut config = ManagedConfig::empty();
        let comments = HostComments {
            leading: vec!["# lead".into()],
            fields: BTreeMap::from([(HostField::Hostname, vec!["# where".into()])]),
            trailing: Vec::new(),
        };
        config
            .insert_with_comments(
                Host::new("dev", "h", None, None, None, None).unwrap(),
                comments.clone(),
            )
            .unwrap();
        let (host, returned) = config.remove_entry("dev").unwrap();
        assert_eq!(host.alias(), "dev");
        assert_eq!(returned, comments);
        assert!(config.comments("dev").is_none());
    }

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
    fn rejects_malformed_strict_values() {
        for value in ["\"unterminated", "mis\"placed", "invalid\\escape"] {
            let input = format!("Host dev\n  HostName dev.example\n  IdentityFile {value}\n");
            assert!(
                matches!(
                    ManagedConfig::parse(&input),
                    Err(ManagedError::Parse { .. })
                ),
                "accepted {value:?}"
            );
        }
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

    #[test]
    fn literal_quoted_identity_survives_managed_round_trip() {
        let mut config = ManagedConfig::empty();
        config
            .insert(
                Host::new(
                    "dev",
                    "dev.example",
                    None,
                    None,
                    Some("\"key\"".into()),
                    None,
                )
                .unwrap(),
            )
            .unwrap();

        let parsed = ManagedConfig::parse(&config.render()).unwrap();

        assert_eq!(parsed.find("dev").unwrap().identity_file(), Some("\"key\""));
    }

    #[test]
    fn proxy_jump_preserves_openssh_literal_directive_value() {
        let parsed =
            ManagedConfig::parse("Host dev\n HostName dev.example\n ProxyJump \"jump.example\"\n")
                .unwrap();

        assert_eq!(
            parsed.find("dev").unwrap().proxy_jump(),
            Some("\"jump.example\"")
        );
        assert_eq!(ManagedConfig::parse(&parsed.render()).unwrap(), parsed);
    }

    #[test]
    fn invalid_port_text_in_the_fragment_reports_its_line() {
        for value in ["0", "+22", "65536"] {
            let input = format!("Host dev\n  HostName dev.example\n  Port {value}\n");
            assert!(
                matches!(
                    ManagedConfig::parse(&input),
                    Err(ManagedError::InvalidHost { line: 3, .. })
                ),
                "accepted {value:?}"
            );
        }
    }
}
