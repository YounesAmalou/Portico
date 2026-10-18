use std::{
    fmt,
    path::{Path, PathBuf},
};

use crate::host::HostField;

/// A configuration error reported by OpenSSH, tied to whatever wrote the line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigRejection {
    /// A line Portico wrote, mapped to the host and field it belongs to.
    Managed {
        alias: String,
        field: Option<HostField>,
        directive: String,
        message: String,
    },
    /// A line in the user's own configuration.
    UserFile {
        path: PathBuf,
        line: u32,
        message: String,
    },
    /// The validation entry Portico wrote; never expected.
    Internal { raw: String },
    /// Output in no recognised shape.
    Unrecognised { raw: String },
}

pub struct ConfigFiles<'a> {
    pub managed: &'a Path,
    pub managed_text: &'a str,
    pub entry: &'a Path,
}

impl ConfigRejection {
    pub fn display_with_home(&self, home: &Path) -> String {
        match self {
            Self::Managed {
                alias,
                directive,
                message,
                ..
            } => format!("{directive} for {alias}: {message}"),
            Self::UserFile {
                path,
                line,
                message,
            } => {
                let shown = match path.strip_prefix(home) {
                    Ok(rest) if !home.as_os_str().is_empty() => {
                        Path::new("~").join(rest).display().to_string()
                    }
                    _ => path.display().to_string(),
                };
                format!("line {line} of {shown}: {message}")
            }
            Self::Internal { raw } | Self::Unrecognised { raw } => raw.clone(),
        }
    }
}

impl fmt::Display for ConfigRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        f.write_str(&self.display_with_home(&home))
    }
}

pub fn interpret(stderr: &str, files: &ConfigFiles<'_>) -> ConfigRejection {
    let located = stderr
        .lines()
        .filter(|line| !line.contains("terminating,"))
        .find_map(locate);
    let Some((path, line, message)) = located else {
        return ConfigRejection::Unrecognised {
            raw: stderr.trim().to_string(),
        };
    };
    if path == files.managed {
        let (alias, directive) = managed_line(files.managed_text, line);
        ConfigRejection::Managed {
            alias,
            field: field_of(&directive),
            directive,
            message,
        }
    } else if path == files.entry {
        ConfigRejection::Internal {
            raw: stderr.trim().to_string(),
        }
    } else {
        ConfigRejection::UserFile {
            path,
            line,
            message,
        }
    }
}

/// Reads `<path>: line <n>: <msg>` or `<path> line <n>: <msg>`.
fn locate(text: &str) -> Option<(PathBuf, u32, String)> {
    let mut from = 0;
    while let Some(offset) = text[from..].find(" line ") {
        let at = from + offset;
        let rest = &text[at + " line ".len()..];
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 && rest[digits..].starts_with(':') {
            let path = text[..at].strip_suffix(':').unwrap_or(&text[..at]);
            let number = rest[..digits].parse().ok()?;
            let message = rest[digits + 1..].trim();
            return Some((PathBuf::from(path), number, message.to_string()));
        }
        from = at + 1;
    }
    None
}

/// The alias of the `Host` block holding `line` and the directive written on it.
fn managed_line(text: &str, line: u32) -> (String, String) {
    let mut alias = String::new();
    for (index, content) in text.lines().enumerate() {
        let trimmed = content.trim();
        let (keyword, rest) = trimmed
            .split_once(char::is_whitespace)
            .unwrap_or((trimmed, ""));
        if keyword.eq_ignore_ascii_case("host") {
            alias = unquote(rest.trim());
        }
        if index + 1 == line as usize {
            return (alias, keyword.to_string());
        }
    }
    (alias, String::new())
}

fn unquote(value: &str) -> String {
    match value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
        Some(inner) => inner.replace("\\\"", "\"").replace("\\\\", "\\"),
        None => value.to_string(),
    }
}

fn field_of(directive: &str) -> Option<HostField> {
    const FIELDS: [(&str, HostField); 6] = [
        ("host", HostField::Alias),
        ("hostname", HostField::Hostname),
        ("user", HostField::User),
        ("port", HostField::Port),
        ("identityfile", HostField::IdentityFile),
        ("proxyjump", HostField::ProxyJump),
    ];
    FIELDS
        .iter()
        .find(|(name, _)| directive.eq_ignore_ascii_case(name))
        .map(|(_, field)| *field)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANAGED: &str = "Host dev\n  HostName 10.0.0.5\n  Port 99999\n";

    fn files<'a>(managed: &'a Path, entry: &'a Path) -> ConfigFiles<'a> {
        ConfigFiles {
            managed,
            managed_text: MANAGED,
            entry,
        }
    }

    #[test]
    fn a_managed_line_names_its_host_and_field() {
        let (m, e) = (Path::new("/tmp/stage.m"), Path::new("/tmp/stage.e"));
        let rejection = interpret(
            "/tmp/stage.m line 3: Bad port '99999'.\n/tmp/stage.m: terminating, 1 bad configuration options\n",
            &files(m, e),
        );
        assert_eq!(
            rejection,
            ConfigRejection::Managed {
                alias: "dev".into(),
                field: Some(HostField::Port),
                directive: "Port".into(),
                message: "Bad port '99999'.".into()
            }
        );
        assert_eq!(rejection.to_string(), "Port for dev: Bad port '99999'.");
    }

    #[test]
    fn both_line_forms_and_paths_with_spaces_are_read() {
        let (m, e) = (Path::new("/tmp/m"), Path::new("/tmp/e"));
        for (text, path) in [
            (
                "/home/u/my configs/extra.conf: line 7: Bad configuration option: bogus\n",
                "/home/u/my configs/extra.conf",
            ),
            (
                "/etc/ssh/ssh_config.d/x.conf line 7: Bad configuration option: bogus\n",
                "/etc/ssh/ssh_config.d/x.conf",
            ),
        ] {
            assert_eq!(
                interpret(text, &files(m, e)),
                ConfigRejection::UserFile {
                    path: path.into(),
                    line: 7,
                    message: "Bad configuration option: bogus".into()
                },
                "{text}"
            );
        }
    }

    #[test]
    fn the_entry_wrapper_is_internal_and_anything_else_unrecognised() {
        let (m, e) = (Path::new("/tmp/m"), Path::new("/tmp/e"));
        assert!(matches!(
            interpret("/tmp/e line 2: Bad thing\n", &files(m, e)),
            ConfigRejection::Internal { .. }
        ));
        assert!(matches!(
            interpret("something odd\n", &files(m, e)),
            ConfigRejection::Unrecognised { .. }
        ));
    }

    #[test]
    fn only_home_paths_are_shortened() {
        let rejection = ConfigRejection::UserFile {
            path: "/etc/ssh/ssh_config".into(),
            line: 1,
            message: "x".into(),
        };
        assert_eq!(
            rejection.display_with_home(Path::new("/home/u")),
            "line 1 of /etc/ssh/ssh_config: x"
        );
        let rejection = ConfigRejection::UserFile {
            path: "/home/u/.ssh/config".into(),
            line: 12,
            message: "x".into(),
        };
        assert_eq!(
            rejection.display_with_home(Path::new("/home/u")),
            "line 12 of ~/.ssh/config: x"
        );
    }

    #[test]
    fn a_quoted_alias_and_a_lowercase_directive_are_understood() {
        let (m, e) = (Path::new("/tmp/m"), Path::new("/tmp/e"));
        let text = "Host \"my host\"\n    hostname x\n";
        let files = ConfigFiles {
            managed: m,
            managed_text: text,
            entry: e,
        };
        assert_eq!(
            interpret("/tmp/m line 2: Bad x\n", &files),
            ConfigRejection::Managed {
                alias: "my host".into(),
                field: Some(HostField::Hostname),
                directive: "hostname".into(),
                message: "Bad x".into()
            }
        );
    }
}
