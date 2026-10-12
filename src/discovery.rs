use std::{
    collections::{BTreeMap, HashSet},
    env, fs, io,
    path::{Path, PathBuf},
};

use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExternalHost {
    pub alias: String,
    pub source: PathBuf,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Discovery {
    pub hosts: Vec<ExternalHost>,
    pub warnings: Vec<DiscoveryWarning>,
    pub aliases_complete: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiscoveryWarning {
    Include {
        path: PathBuf,
        message: String,
    },
    Parse {
        path: PathBuf,
        line: usize,
        message: String,
    },
    DuplicateAlias {
        alias: String,
        sources: Vec<PathBuf>,
    },
}

#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error("could not read entry config {path}: {source}")]
    EntryRead {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("malformed config directive: {0}")]
    Parse(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ConfigDirective {
    Include(Vec<String>),
    Host(Vec<String>),
    Other,
}

pub(crate) fn parse_directive(line: &str) -> Result<Option<ConfigDirective>, DiscoveryError> {
    let mut tokens = tokenize(line)?.into_iter();
    let Some(first) = tokens.next() else {
        return Ok(None);
    };
    let (directive, value) = first
        .split_once('=')
        .map_or((&*first, None), |(key, value)| (key, Some(value)));
    let mut arguments = tokens.collect::<Vec<_>>();
    if let Some(value) = value
        && !value.is_empty()
    {
        arguments.insert(0, value.into());
    }
    if let Some(argument) = arguments.first_mut()
        && let Some(value) = argument.strip_prefix('=')
    {
        *argument = value.into();
        if argument.is_empty() {
            arguments.remove(0);
        }
    }

    if directive.eq_ignore_ascii_case("Include") {
        if arguments.is_empty() {
            return Err(DiscoveryError::Parse("Include requires a path".into()));
        }
        Ok(Some(ConfigDirective::Include(arguments)))
    } else if directive.eq_ignore_ascii_case("Host") {
        if arguments.is_empty() {
            return Err(DiscoveryError::Parse("Host requires an alias".into()));
        }
        Ok(Some(ConfigDirective::Host(arguments)))
    } else {
        Ok(Some(ConfigDirective::Other))
    }
}

pub(crate) fn split_comment(line: &[u8]) -> (&[u8], &[u8]) {
    let mut quote = None;
    let mut escaped = false;
    let mut token_started = false;
    for (index, &byte) in line.iter().enumerate() {
        if escaped {
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
            token_started = true;
        } else if let Some(delimiter) = quote {
            if byte == delimiter {
                quote = None;
            }
        } else if matches!(byte, b'\'' | b'"') {
            quote = Some(byte);
            token_started = true;
        } else if byte == b'#' && !token_started {
            return line.split_at(index);
        } else {
            token_started = !byte.is_ascii_whitespace();
        }
    }
    (line, &[])
}

pub(crate) fn parse_directive_bytes(
    line: &[u8],
) -> Result<Option<ConfigDirective>, DiscoveryError> {
    let active = std::str::from_utf8(split_comment(line).0)
        .map_err(|error| DiscoveryError::Parse(error.to_string()))?;
    parse_directive(active)
}

pub fn discover(entry: &Path, managed: &Path) -> Result<Discovery, DiscoveryError> {
    let root = entry.parent().unwrap_or(Path::new(".")).to_path_buf();
    let canonical = entry
        .canonicalize()
        .map_err(|source| DiscoveryError::EntryRead {
            path: entry.into(),
            source,
        })?;
    let mut scanner = Scanner {
        root,
        managed: managed.canonicalize().ok(),
        visited: HashSet::new(),
        discovery: Discovery {
            aliases_complete: true,
            ..Discovery::default()
        },
    };

    scanner.visited.insert(canonical);
    scanner.scan_entry(entry.to_path_buf())?;
    scanner.report_duplicates();
    Ok(scanner.discovery)
}

struct Scanner {
    root: PathBuf,
    managed: Option<PathBuf>,
    visited: HashSet<PathBuf>,
    discovery: Discovery,
}

impl Scanner {
    fn scan_entry(&mut self, path: PathBuf) -> Result<(), DiscoveryError> {
        let contents = fs::read(&path).map_err(|source| DiscoveryError::EntryRead {
            path: path.clone(),
            source,
        })?;
        self.scan_contents(&contents, &path);
        Ok(())
    }

    fn scan_include(&mut self, path: PathBuf) {
        let canonical = match path.canonicalize() {
            Ok(path) => path,
            Err(error) => return self.warn_include(path, error.to_string()),
        };
        if self.managed.as_ref() == Some(&canonical) || !self.visited.insert(canonical.clone()) {
            return;
        }
        let contents = match fs::read(&path) {
            Ok(contents) => contents,
            Err(error) => return self.warn_include(path, error.to_string()),
        };
        self.scan_contents(&contents, &path);
    }

    fn scan_contents(&mut self, contents: &[u8], source: &Path) {
        for (index, line) in contents.split(|byte| *byte == b'\n').enumerate() {
            match parse_directive_bytes(line) {
                Ok(Some(ConfigDirective::Include(patterns))) => {
                    for pattern in patterns {
                        self.expand_include(&pattern);
                    }
                }
                Ok(Some(ConfigDirective::Host(aliases))) => {
                    for alias in aliases {
                        if !alias.contains(['*', '?', '!']) {
                            self.discovery.hosts.push(ExternalHost {
                                alias,
                                source: source.into(),
                            });
                        }
                    }
                }
                Ok(Some(ConfigDirective::Other)) | Ok(None) => {}
                Err(error) => {
                    self.discovery.aliases_complete = false;
                    self.discovery.warnings.push(DiscoveryWarning::Parse {
                        path: source.into(),
                        line: index + 1,
                        message: error.to_string(),
                    });
                }
            }
        }
    }

    fn expand_include(&mut self, pattern: &str) {
        let target = match self.include_target(pattern) {
            Ok(target) => target,
            Err(message) => return self.warn_include(pattern.into(), message),
        };
        let path = target.path();
        let matches = match glob::glob(&target.glob()) {
            Ok(matches) => matches,
            Err(error) => return self.warn_include(path, error.to_string()),
        };
        let mut paths = Vec::new();
        let mut had_error = false;
        for result in matches {
            match result {
                Ok(path) => paths.push(path),
                Err(error) => {
                    had_error = true;
                    self.warn_include(error.path().into(), error.error().to_string());
                }
            }
        }
        paths.sort();
        if paths.is_empty() && !had_error {
            match fs::metadata(&path) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => {
                    self.warn_include(path, error.to_string());
                }
                _ => self.discovery.warnings.push(DiscoveryWarning::Include {
                    path,
                    message: "no matching files".into(),
                }),
            }
        } else {
            for path in paths {
                self.scan_include(path);
            }
        }
    }

    fn include_target(&self, pattern: &str) -> Result<IncludeTarget, String> {
        let pattern = expand_environment(pattern)?;
        if pattern.contains('%') {
            return Err("unsupported Include token expansion".into());
        }
        let home = self.root.parent().unwrap_or(&self.root);
        if let Some(pattern) = pattern.strip_prefix("~/") {
            Ok(IncludeTarget::under(home, pattern))
        } else if pattern == "~" {
            Ok(IncludeTarget::under(home, ""))
        } else if pattern.starts_with('~') {
            Err("unsupported Include user expansion".into())
        } else if Path::new(&pattern).is_absolute() {
            Ok(IncludeTarget::under(Path::new(""), &pattern))
        } else {
            Ok(IncludeTarget::under(&self.root, &pattern))
        }
    }

    fn warn_include(&mut self, path: PathBuf, message: String) {
        self.discovery.aliases_complete = false;
        self.discovery
            .warnings
            .push(DiscoveryWarning::Include { path, message });
    }

    fn report_duplicates(&mut self) {
        let mut aliases: BTreeMap<String, (String, Vec<PathBuf>)> = BTreeMap::new();
        for host in &self.discovery.hosts {
            let entry = aliases
                .entry(host.alias.to_ascii_lowercase())
                .or_insert_with(|| (host.alias.clone(), Vec::new()));
            entry.1.push(host.source.clone());
        }
        for (_, (alias, sources)) in aliases
            .into_iter()
            .filter(|(_, (_, sources))| sources.len() > 1)
        {
            self.discovery
                .warnings
                .push(DiscoveryWarning::DuplicateAlias { alias, sources });
        }
    }
}

/// An Include argument resolved against a literal base directory.
///
/// Only the user's own pattern may act as a glob; the base is a real path, so wildcard
/// characters in the home or SSH directory name must not widen or hide matches.
struct IncludeTarget {
    base: PathBuf,
    pattern: String,
}

impl IncludeTarget {
    fn under(base: &Path, pattern: &str) -> Self {
        Self {
            base: base.into(),
            pattern: pattern.into(),
        }
    }

    fn path(&self) -> PathBuf {
        self.base.join(&self.pattern)
    }

    fn glob(&self) -> String {
        PathBuf::from(glob::Pattern::escape(&self.base.to_string_lossy()))
            .join(&self.pattern)
            .to_string_lossy()
            .into_owned()
    }
}

fn expand_environment(pattern: &str) -> Result<String, String> {
    let mut expanded = String::new();
    let mut remaining = pattern;
    while let Some(start) = remaining.find('$') {
        expanded.push_str(&remaining[..start]);
        let variable = &remaining[start..];
        let Some(variable) = variable.strip_prefix("${") else {
            return Err("unsupported Include environment expansion".into());
        };
        let Some(end) = variable.find('}') else {
            return Err("malformed Include environment expansion".into());
        };
        let name = &variable[..end];
        if name.is_empty()
            || !name.bytes().enumerate().all(|(index, byte)| {
                byte == b'_' || byte.is_ascii_alphabetic() || (index > 0 && byte.is_ascii_digit())
            })
        {
            return Err("malformed Include environment expansion".into());
        }
        let value = env::var(name)
            .map_err(|_| format!("unresolved Include environment variable: {name}"))?;
        if value.contains('\\') {
            return Err(format!(
                "unsupported syntax in Include environment variable: {name}"
            ));
        }
        expanded.push_str(&value);
        remaining = &variable[end + 1..];
    }
    expanded.push_str(remaining);
    Ok(expanded)
}

fn tokenize(line: &str) -> Result<Vec<String>, DiscoveryError> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut characters = line.chars().peekable();
    let mut token_started = false;

    while let Some(character) = characters.next() {
        match character {
            '#' if !token_started => break,
            character if character.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
                token_started = false;
            }
            quote @ ('"' | '\'') => {
                token_started = true;
                loop {
                    match characters.next() {
                        Some(character) if character == quote => break,
                        Some('\\') => {
                            current.push(characters.next().ok_or_else(|| {
                                DiscoveryError::Parse("unterminated escape".into())
                            })?)
                        }
                        Some(character) => current.push(character),
                        None => return Err(DiscoveryError::Parse("unterminated quote".into())),
                    }
                }
            }
            '\\' => {
                token_started = true;
                current.push(
                    characters
                        .next()
                        .ok_or_else(|| DiscoveryError::Parse("unterminated escape".into()))?,
                );
            }
            character => {
                token_started = true;
                current.push(character);
            }
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_includes_skips_managed_and_ignores_patterns() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        std::fs::create_dir_all(ssh.join("conf.d")).unwrap();
        let managed = ssh.join("portico/config");
        std::fs::create_dir_all(managed.parent().unwrap()).unwrap();
        std::fs::write(&managed, "Host owned\n  HostName owned.example\n").unwrap();
        std::fs::write(
            ssh.join("config"),
            "Include conf.d/*.conf\nInclude portico/config\nHost direct *.corp !blocked\n  User deploy\n",
        )
        .unwrap();
        std::fs::write(
            ssh.join("conf.d/hosts.conf"),
            "Host bastion\n  HostName bastion.example\nInclude config\n",
        )
        .unwrap();

        let result = discover(&ssh.join("config"), &managed).unwrap();
        let aliases: Vec<_> = result
            .hosts
            .iter()
            .map(|host| host.alias.as_str())
            .collect();

        assert_eq!(aliases, ["bastion", "direct"]);
        assert!(result.warnings.is_empty());
    }

    #[test]
    fn missing_include_warns_without_blocking_known_aliases() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        std::fs::write(
            ssh.join("config"),
            "Include missing/*.conf\nHost visible\n  HostName visible.example\n",
        )
        .unwrap();

        let result = discover(&ssh.join("config"), &ssh.join("portico/config")).unwrap();
        assert!(result.aliases_complete);
        assert_eq!(result.hosts[0].alias, "visible");
        assert_eq!(result.warnings.len(), 1);
    }

    #[test]
    fn unknown_or_unsupported_include_expansions_make_discovery_incomplete() {
        for pattern in [
            "${PORTICO_TEST_VARIABLE_THAT_DOES_NOT_EXIST}",
            "${HOME",
            "~otheruser/.ssh/config",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let entry = dir.path().join("config");
            fs::write(&entry, format!("Include {pattern}\n")).unwrap();

            let result = discover(&entry, &dir.path().join("managed")).unwrap();

            assert!(!result.aliases_complete, "{pattern}: {:?}", result.warnings);
        }
    }

    #[test]
    fn case_only_duplicate_warns_without_hiding_other_known_aliases() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        std::fs::write(
            ssh.join("config"),
            "Host Prod\n  HostName one.example\nHost prod\n  HostName two.example\n",
        )
        .unwrap();

        let result = discover(&ssh.join("config"), &ssh.join("portico/config")).unwrap();
        assert!(result.aliases_complete);
        assert!(result.warnings.iter().any(|warning| matches!(warning, DiscoveryWarning::DuplicateAlias { alias, .. } if alias.eq_ignore_ascii_case("prod"))));
    }

    #[test]
    fn nested_includes_stay_relative_to_the_entry_config() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        std::fs::create_dir_all(ssh.join("conf.d")).unwrap();
        std::fs::write(ssh.join("config"), "Include conf.d/first.conf\n").unwrap();
        std::fs::write(ssh.join("conf.d/first.conf"), "Include sibling.conf\n").unwrap();
        std::fs::write(ssh.join("sibling.conf"), "Host entry-root\n").unwrap();
        std::fs::write(ssh.join("conf.d/sibling.conf"), "Host included-parent\n").unwrap();

        let result = discover(&ssh.join("config"), &ssh.join("portico/config")).unwrap();
        let aliases: Vec<_> = result
            .hosts
            .iter()
            .map(|host| host.alias.as_str())
            .collect();

        assert_eq!(aliases, ["entry-root"]);
        assert!(result.aliases_complete);
    }

    #[test]
    fn glob_characters_in_the_ssh_directory_are_literal() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("me[work]*?");
        let ssh = home.join(".ssh");
        fs::create_dir_all(ssh.join("conf.d")).unwrap();
        fs::write(
            ssh.join("config"),
            "Include conf.d/*.conf ~/.ssh/tilde.conf\n",
        )
        .unwrap();
        fs::write(ssh.join("conf.d/a.conf"), "Host relative\n").unwrap();
        fs::write(ssh.join("tilde.conf"), "Host tilde\n").unwrap();
        // A directory the unescaped `me[work]*?` class would match instead.
        let decoy = dir.path().join("mew/.ssh/conf.d");
        fs::create_dir_all(&decoy).unwrap();
        fs::write(decoy.join("a.conf"), "Host decoy\n").unwrap();

        let result = discover(&ssh.join("config"), &ssh.join("portico/config")).unwrap();

        assert_eq!(
            result
                .hosts
                .iter()
                .map(|host| host.alias.as_str())
                .collect::<Vec<_>>(),
            ["relative", "tilde"]
        );
        assert!(result.aliases_complete, "{:?}", result.warnings);
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    }

    #[test]
    fn recognizes_host_equals_syntax() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        std::fs::write(ssh.join("config"), "Host=prod\n").unwrap();

        let result = discover(&ssh.join("config"), &ssh.join("portico/config")).unwrap();

        assert_eq!(result.hosts[0].alias, "prod");
        assert!(result.aliases_complete);
    }

    #[test]
    fn recognizes_include_equals_syntax() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        std::fs::create_dir_all(ssh.join("conf.d")).unwrap();
        std::fs::write(ssh.join("config"), "Include=conf.d/*.conf\n").unwrap();
        std::fs::write(ssh.join("conf.d/host.conf"), "Host included\n").unwrap();

        let result = discover(&ssh.join("config"), &ssh.join("portico/config")).unwrap();

        assert_eq!(result.hosts[0].alias, "included");
        assert!(result.aliases_complete);
    }

    #[test]
    fn recognizes_host_space_equals_syntax() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        std::fs::write(ssh.join("config"), "Host = prod\n").unwrap();

        let result = discover(&ssh.join("config"), &ssh.join("portico/config")).unwrap();
        let aliases: Vec<_> = result
            .hosts
            .iter()
            .map(|host| host.alias.as_str())
            .collect();

        assert_eq!(aliases, ["prod"]);
        assert!(result.aliases_complete);
    }

    #[test]
    fn recognizes_include_space_equals_syntax() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        std::fs::create_dir_all(ssh.join("conf.d")).unwrap();
        std::fs::write(ssh.join("config"), "Include = conf.d/*.conf\n").unwrap();
        std::fs::write(ssh.join("conf.d/host.conf"), "Host included\n").unwrap();

        let result = discover(&ssh.join("config"), &ssh.join("portico/config")).unwrap();

        assert_eq!(result.hosts[0].alias, "included");
        assert!(result.aliases_complete);
    }

    #[test]
    fn single_quoted_includes_discover_literal_aliases() {
        let dir = tempfile::tempdir().unwrap();
        let entry = dir.path().join("config");
        fs::write(&entry, "Include 'hosts with spaces'\n").unwrap();
        fs::write(dir.path().join("hosts with spaces"), "Host 'prod'\n").unwrap();

        let result = discover(&entry, &dir.path().join("managed")).unwrap();

        assert!(result.aliases_complete, "{:?}", result.warnings);
        assert_eq!(result.hosts[0].alias, "prod");
    }

    #[test]
    fn literal_hashes_in_include_paths_do_not_truncate_inventory() {
        let dir = tempfile::tempdir().unwrap();
        let entry = dir.path().join("config");
        fs::write(&entry, "Include hosts#extra # a real comment\n").unwrap();
        fs::write(dir.path().join("hosts"), "Host wrong-file\n").unwrap();
        fs::write(dir.path().join("hosts#extra"), "Host prod\n").unwrap();

        let result = discover(&entry, &dir.path().join("managed")).unwrap();

        assert_eq!(result.hosts[0].alias, "prod");
        assert!(result.aliases_complete, "{:?}", result.warnings);
    }

    #[test]
    fn symlinked_entry_keeps_lexical_include_roots() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join("home/.ssh");
        let dotfiles = dir.path().join("dotfiles/ssh");
        fs::create_dir_all(&ssh).unwrap();
        fs::create_dir_all(&dotfiles).unwrap();
        fs::write(
            dotfiles.join("config"),
            "Include relative.conf ~/.ssh/tilde.conf\n",
        )
        .unwrap();
        fs::write(ssh.join("relative.conf"), "Host relative\nInclude config\n").unwrap();
        fs::write(ssh.join("tilde.conf"), "Host tilde\n").unwrap();
        fs::write(dotfiles.join("relative.conf"), "Host wrong-root\n").unwrap();
        std::os::unix::fs::symlink(dotfiles.join("config"), ssh.join("config")).unwrap();

        let result = discover(&ssh.join("config"), &ssh.join("portico/config")).unwrap();

        assert_eq!(
            result
                .hosts
                .iter()
                .map(|host| host.alias.as_str())
                .collect::<Vec<_>>(),
            ["relative", "tilde"]
        );
        assert!(result.aliases_complete, "{:?}", result.warnings);
    }
}
