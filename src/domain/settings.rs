//! Portico's preferences file: `settings.toml` in the config directory.
//!
//! Every setting has a default, so a missing file, a missing table and a missing key all mean
//! "the default". A file that cannot be read as settings is reported, never overwritten: `set`
//! refuses to write over it. Writes keep comments, order and keys this version does not know.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use toml_edit::{Document, DocumentMut, Item, Table, Value};

use crate::{
    backend::BackendId,
    platform::fs::{acquire_lock, open_lock, sync_dir, write_unique},
};

/// The settings file's name inside the config directory.
pub const FILE_NAME: &str = "settings.toml";

const HEADER: &str = "# Portico's settings. Edit by hand or with portico settings.\n";

/// Which tabs the interface shows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Tabs {
    pub tunnels: bool,
    pub docker: bool,
    pub podman: bool,
    pub proxmox: bool,
}

impl Default for Tabs {
    fn default() -> Self {
        Self {
            tunnels: true,
            docker: true,
            podman: true,
            proxmox: true,
        }
    }
}

impl Tabs {
    /// Whether the tab of `backend` is shown.
    pub fn shows(self, backend: BackendId) -> bool {
        Key::for_backend(backend).get(&Settings { tabs: self })
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Settings {
    pub tabs: Tabs,
}

/// A setting that can be read and changed by name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Key {
    TabsTunnels,
    TabsDocker,
    TabsPodman,
    TabsProxmox,
}

impl Key {
    pub const ALL: [Key; 4] = [
        Key::TabsTunnels,
        Key::TabsDocker,
        Key::TabsPodman,
        Key::TabsProxmox,
    ];

    /// The dotted name, as `portico settings` spells it.
    pub fn name(self) -> &'static str {
        match self {
            Key::TabsTunnels => "tabs.tunnels",
            Key::TabsDocker => "tabs.docker",
            Key::TabsPodman => "tabs.podman",
            Key::TabsProxmox => "tabs.proxmox",
        }
    }

    pub fn parse(name: &str) -> Option<Key> {
        Key::ALL.into_iter().find(|key| key.name() == name)
    }

    pub fn get(self, settings: &Settings) -> bool {
        let tabs = &settings.tabs;
        match self {
            Key::TabsTunnels => tabs.tunnels,
            Key::TabsDocker => tabs.docker,
            Key::TabsPodman => tabs.podman,
            Key::TabsProxmox => tabs.proxmox,
        }
    }

    pub fn set(self, settings: &mut Settings, value: bool) {
        let tabs = &mut settings.tabs;
        match self {
            Key::TabsTunnels => tabs.tunnels = value,
            Key::TabsDocker => tabs.docker = value,
            Key::TabsPodman => tabs.podman = value,
            Key::TabsProxmox => tabs.proxmox = value,
        }
    }

    pub fn for_backend(backend: BackendId) -> Key {
        match backend {
            BackendId::Docker => Key::TabsDocker,
            BackendId::Podman => Key::TabsPodman,
            BackendId::Proxmox => Key::TabsProxmox,
        }
    }

    /// The key's name inside its table.
    fn leaf(self) -> &'static str {
        self.name()
            .split_once('.')
            .map_or(self.name(), |(_, leaf)| leaf)
    }
}

/// `on|off|true|false|yes|no`, in any case.
pub fn parse_bool(text: &str) -> Option<bool> {
    match text.to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" => Some(true),
        "off" | "false" | "no" => Some(false),
        _ => None,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("invalid settings file {}: {}{reason}", .path.display(), line_prefix(*.line))]
    Invalid {
        path: PathBuf,
        line: Option<usize>,
        reason: String,
    },
    #[error("cannot access settings file {}: {source}", .path.display())]
    Io { path: PathBuf, source: io::Error },
}

fn line_prefix(line: Option<usize>) -> String {
    line.map_or(String::new(), |line| format!("line {line}: "))
}

/// The settings in `path`. A missing file means the defaults.
pub fn load(path: &Path) -> Result<Settings, SettingsError> {
    Ok(read(path)?.1)
}

/// Sets `key` in the file at `path`, creating it (and its directory) when needed. Returns
/// whether the file changed: setting a value already in effect, written or by default, writes
/// nothing. A file that is not valid settings is refused and left as it is.
pub fn set(path: &Path, key: Key, value: bool) -> Result<bool, SettingsError> {
    let io_error = |source| SettingsError::Io {
        path: path.to_path_buf(),
        source,
    };
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    // Fail early without creating anything when the file is already in order.
    let (_, current) = read(path)?;
    if key.get(&current) == value {
        return Ok(false);
    }
    fs::create_dir_all(dir).map_err(io_error)?;
    let lock = open_lock(&dir.join(".settings.lock")).map_err(io_error)?;
    acquire_lock(&lock, None).map_err(io_error)?;

    // Under the lock, read again: another writer may have changed the file.
    let (text, current) = read(path)?;
    if key.get(&current) == value {
        return Ok(false);
    }
    let mut document = if text.is_empty() {
        DocumentMut::new()
    } else {
        text.parse::<DocumentMut>().map_err(|error| {
            invalid(
                path,
                &text,
                error.span().map(|span| span.start),
                error.message(),
            )
        })?
    };
    if text.is_empty() {
        document.decor_mut().set_prefix(HEADER);
    }
    let table = document
        .entry("tabs")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| invalid(path, &text, None, "`tabs` must be a table"))?;
    table.insert(key.leaf(), toml_edit::value(value));

    let pending =
        write_unique(dir, "settings", document.to_string().as_bytes(), 0o644).map_err(io_error)?;
    pending.rename_to(path).map_err(io_error)?;
    sync_dir(dir).map_err(io_error)?;
    Ok(true)
}

/// The file's text (empty when it is missing) and the settings it holds.
fn read(path: &Path) -> Result<(String, Settings), SettingsError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok((String::new(), Settings::default()));
        }
        Err(source) => {
            return Err(SettingsError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let document = Document::parse(text.as_str()).map_err(|error| {
        invalid(
            path,
            &text,
            error.span().map(|span| span.start),
            error.message(),
        )
    })?;
    let mut settings = Settings::default();
    if let Some(tabs) = document.get("tabs") {
        let table = tabs.as_table().ok_or_else(|| {
            invalid(
                path,
                &text,
                tabs.span().map(|span| span.start),
                "`tabs` must be a table",
            )
        })?;
        for key in Key::ALL {
            let Some(item) = table.get(key.leaf()) else {
                continue;
            };
            match item.as_value() {
                Some(Value::Boolean(flag)) => key.set(&mut settings, *flag.value()),
                _ => {
                    return Err(invalid(
                        path,
                        &text,
                        item.span().map(|span| span.start),
                        &format!("`{}` must be a boolean (true or false)", key.name()),
                    ));
                }
            }
        }
    }
    Ok((text, settings))
}

fn invalid(path: &Path, text: &str, offset: Option<usize>, reason: &str) -> SettingsError {
    let line = offset.map(|offset| text[..offset.min(text.len())].matches('\n').count() + 1);
    SettingsError::Invalid {
        path: path.to_path_buf(),
        line,
        reason: reason.lines().next().unwrap_or_default().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn file(dir: &tempfile::TempDir, text: &str) -> PathBuf {
        let path = dir.path().join(FILE_NAME);
        fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn missing_file_is_every_tab_on() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load(&dir.path().join(FILE_NAME)).unwrap(),
            Settings::default()
        );
    }

    #[test]
    fn partial_file_keeps_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let settings = load(&file(&dir, "[tabs]\npodman = false\n")).unwrap();
        assert_eq!(
            settings.tabs,
            Tabs {
                podman: false,
                ..Tabs::default()
            }
        );
    }

    #[test]
    fn unknown_keys_are_ignored_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir, "[tabs]\nfuture = 1\n[other]\nx = 'y'\n");
        assert_eq!(load(&path).unwrap(), Settings::default());
        assert!(set(&path, Key::TabsDocker, false).unwrap());
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("future = 1"), "{text}");
        assert!(text.contains("[other]"), "{text}");
        assert!(!load(&path).unwrap().tabs.docker);
    }

    #[test]
    fn wrong_type_is_invalid_with_its_line() {
        let dir = tempfile::tempdir().unwrap();
        let error = load(&file(&dir, "[tabs]\n\npodman = \"no\"\n")).unwrap_err();
        assert!(
            matches!(error, SettingsError::Invalid { line: Some(3), .. }),
            "{error}"
        );
        assert!(error.to_string().contains("line 3: "), "{error}");
    }

    #[test]
    fn tabs_not_a_table_is_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let error = load(&file(&dir, "tabs = true\n")).unwrap_err();
        assert!(
            matches!(error, SettingsError::Invalid { line: Some(1), .. }),
            "{error}"
        );
    }

    #[test]
    fn parse_error_is_invalid_with_its_line() {
        let dir = tempfile::tempdir().unwrap();
        let error = load(&file(&dir, "[tabs\n")).unwrap_err();
        assert!(
            matches!(error, SettingsError::Invalid { line: Some(1), .. }),
            "{error}"
        );
    }

    #[test]
    fn set_keeps_comments_and_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(
            &dir,
            "# mine\n[tabs]\ndocker = true # keep\npodman = true\n",
        );
        assert!(set(&path, Key::TabsPodman, false).unwrap());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "# mine\n[tabs]\ndocker = true # keep\npodman = false\n"
        );
    }

    #[test]
    fn set_creates_the_file_and_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("portico").join(FILE_NAME);
        assert!(set(&path, Key::TabsTunnels, false).unwrap());
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# Portico's settings"), "{text}");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert!(!load(&path).unwrap().tabs.tunnels);
    }

    #[test]
    fn set_refuses_an_invalid_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir, "[tabs\n");
        let error = set(&path, Key::TabsDocker, false).unwrap_err();
        assert!(matches!(error, SettingsError::Invalid { .. }), "{error}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "[tabs\n");
    }

    #[test]
    fn set_same_value_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        assert!(!set(&path, Key::TabsDocker, true).unwrap());
        assert!(!path.exists());
    }

    #[test]
    fn key_names_round_trip() {
        for key in Key::ALL {
            assert_eq!(Key::parse(key.name()), Some(key));
        }
        assert_eq!(Key::parse("tabs.podman"), Some(Key::TabsPodman));
        assert_eq!(parse_bool("OFF"), Some(false));
        assert_eq!(parse_bool("Yes"), Some(true));
        assert_eq!(parse_bool("maybe"), None);
    }
}
