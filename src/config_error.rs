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
        /// The home directory `path` is shown under, as `~`.
        home: PathBuf,
    },
    /// A line Portico itself wrote into the staged validation entry.
    Internal { raw: String },
    /// Output in no recognised shape.
    Unrecognised { raw: String },
}

pub struct ConfigFiles<'a> {
    /// The home directory a rejected file of the user's is shown under.
    pub home: &'a Path,
    pub managed: &'a Path,
    pub managed_text: &'a str,
    /// The staged entry Portico wrote to validate a change, when there is one. Without it every
    /// file but the managed one is the user's own, `~/.ssh/config` included.
    pub entry: Option<StagedEntry<'a>>,
}

/// An entry config Portico staged to validate a change: a copy of the user's entry config with
/// Portico's Include line swapped one line for one line, plus any lines Portico appended.
#[derive(Clone, Copy, Debug)]
pub struct StagedEntry<'a> {
    pub path: &'a Path,
    /// The user's entry config the staged one copies.
    pub source: &'a Path,
    /// The 1-based lines Portico wrote; every other line is the user's, at the same number in
    /// `source`.
    pub portico_lines: &'a [u32],
}

impl fmt::Display for ConfigRejection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Managed {
                alias,
                directive,
                message,
                ..
            } => write!(f, "{directive} for {alias}: {message}"),
            Self::UserFile {
                path,
                line,
                message,
                home,
            } => write!(
                f,
                "line {line} of {}: {message}",
                crate::home::tilde(path, home)
            ),
            Self::Internal { raw } | Self::Unrecognised { raw } => f.write_str(raw),
        }
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
    } else if let Some(entry) = files.entry.filter(|entry| path == entry.path) {
        if entry.portico_lines.contains(&line) {
            ConfigRejection::Internal {
                raw: stderr.trim().to_string(),
            }
        } else {
            ConfigRejection::UserFile {
                path: entry.source.to_path_buf(),
                line,
                message,
                home: files.home.to_path_buf(),
            }
        }
    } else {
        ConfigRejection::UserFile {
            path,
            line,
            message,
            home: files.home.to_path_buf(),
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

    const SOURCE: &str = "/home/me/.ssh/config";

    fn files<'a>(managed: &'a Path, entry: &'a Path) -> ConfigFiles<'a> {
        ConfigFiles {
            home: Path::new("/home/me"),
            managed,
            managed_text: MANAGED,
            entry: Some(StagedEntry {
                path: entry,
                source: Path::new(SOURCE),
                portico_lines: &[2, 9, 10],
            }),
        }
    }

    #[test]
    fn without_a_staged_entry_the_entry_file_is_the_users_own() {
        let files = ConfigFiles {
            home: Path::new("/home/me"),
            managed: Path::new("/home/me/.ssh/portico/config"),
            managed_text: MANAGED,
            entry: None,
        };
        assert_eq!(
            interpret(
                "/home/me/.ssh/config: line 3: Bad configuration option: bogus\n",
                &files
            ),
            ConfigRejection::UserFile {
                path: "/home/me/.ssh/config".into(),
                line: 3,
                message: "Bad configuration option: bogus".into(),
                home: "/home/me".into(),
            }
        );
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
                    message: "Bad configuration option: bogus".into(),
                    home: "/home/me".into(),
                },
                "{text}"
            );
        }
    }

    #[test]
    fn portico_lines_of_the_staged_entry_are_internal() {
        let (m, e) = (Path::new("/tmp/m"), Path::new("/tmp/e"));
        for line in [2, 9, 10] {
            assert!(matches!(
                interpret(&format!("/tmp/e line {line}: Bad thing\n"), &files(m, e)),
                ConfigRejection::Internal { .. }
            ));
        }
    }

    #[test]
    fn the_users_lines_of_the_staged_entry_name_their_real_file() {
        let (m, e) = (
            Path::new("/tmp/m"),
            Path::new("/home/me/.ssh/.config.portico-validation"),
        );
        let rejection = interpret(
            "/home/me/.ssh/.config.portico-validation: line 3: Bad configuration option: bogus\n",
            &files(m, e),
        );
        assert_eq!(
            rejection,
            ConfigRejection::UserFile {
                path: SOURCE.into(),
                line: 3,
                message: "Bad configuration option: bogus".into(),
                home: "/home/me".into(),
            }
        );
        assert_eq!(
            rejection.to_string(),
            "line 3 of ~/.ssh/config: Bad configuration option: bogus"
        );
    }

    #[test]
    fn unrecognised_output_stays_unrecognised() {
        let (m, e) = (Path::new("/tmp/m"), Path::new("/tmp/e"));
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
            home: "/home/u".into(),
        };
        assert_eq!(rejection.to_string(), "line 1 of /etc/ssh/ssh_config: x");
        let rejection = ConfigRejection::UserFile {
            path: "/home/u/.ssh/config".into(),
            line: 12,
            message: "x".into(),
            home: "/home/u".into(),
        };
        assert_eq!(rejection.to_string(), "line 12 of ~/.ssh/config: x");
    }

    #[test]
    fn a_quoted_alias_and_a_lowercase_directive_are_understood() {
        let (m, e) = (Path::new("/tmp/m"), Path::new("/tmp/e"));
        let text = "Host \"my host\"\n    hostname x\n";
        let files = ConfigFiles {
            home: Path::new("/home/me"),
            managed: m,
            managed_text: text,
            entry: Some(StagedEntry {
                path: e,
                source: Path::new(SOURCE),
                portico_lines: &[1],
            }),
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
