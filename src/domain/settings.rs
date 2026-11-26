//! Portico's preferences file: `settings.toml` in the config directory.
//!
//! Every setting has a default, so a missing file, a missing table and a missing key all mean
//! "the default". A file that cannot be read as settings is reported, never overwritten: `set`
//! refuses to write over it. Writes keep comments, order and keys this version does not know.
//!
//! Besides the tabs, the file remembers the user a container's shell runs as, per machine,
//! backend and container key: `[users.local.docker]` or `[users.host.<alias>.podman]`.

use std::{
    collections::BTreeMap,
    fs, io,
    ops::Range,
    path::{Path, PathBuf},
};

use toml_edit::{
    Document, DocumentMut, InlineTable, Item, Key as TomlKey, Table, TableLike, Value,
};

use crate::{
    backend::{AUTOMATIC, BackendId, Target},
    platform::fs::{acquire_lock, open_lock, symlink_target, sync_dir, write_unique},
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
        Key::for_backend(backend).get(&Settings {
            tabs: self,
            ..Settings::default()
        })
    }
}

/// Which container a remembered user belongs to: the machine, the backend, and the container's
/// key (`backend::remember_key`).
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct UserKey {
    pub target: Target,
    pub backend: BackendId,
    pub container: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Settings {
    pub tabs: Tabs,
    /// The remembered users, in a stable order.
    pub users: BTreeMap<UserKey, String>,
}

impl Settings {
    /// The user remembered for a container, if any.
    pub fn user(&self, key: &UserKey) -> Option<&str> {
        self.users.get(key).map(String::as_str)
    }
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
    pub fn leaf(self) -> &'static str {
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
    edit(
        path,
        |current| key.get(current) == value,
        |document, text| {
            let table = document
                .entry("tabs")
                .or_insert_with(|| Item::Table(Table::new()))
                .as_table_like_mut()
                .ok_or_else(|| invalid(path, text, None, "`tabs` must be a table"))?;
            table.insert(key.leaf(), toml_edit::value(value));
            Ok(())
        },
    )
}

/// Remembers `user` for a container (`None` forgets it), creating the file when needed. The
/// user is trimmed, and a blank one forgets. Returns whether the file changed: an entry that
/// already says that writes nothing. Proxmox shells always run as root, so a Proxmox key
/// writes nothing either. A file that is not valid settings is refused and left as it is.
pub fn remember_user(
    path: &Path,
    key: &UserKey,
    user: Option<&str>,
) -> Result<bool, SettingsError> {
    if !key.backend.chooses_user() {
        return Ok(false);
    }
    let user = user.map(str::trim).filter(|user| !user.is_empty());
    edit(
        path,
        |current| current.user(key) == user,
        |document, text| {
            let not_a_table =
                |name: &str| invalid(path, text, None, &format!("`{name}` must be a table"));
            let root = document.as_item_mut();
            let Some(user) = user else {
                if let Some(entries) = existing_entries(root, key) {
                    entries.remove(&key.container);
                }
                return Ok(());
            };
            let users = child(root, "users").ok_or_else(|| not_a_table("users"))?;
            let machine = match &key.target {
                Target::Local => child(users, "local").ok_or_else(|| not_a_table("users.local"))?,
                Target::Host(alias) => {
                    let hosts = child(users, "host").ok_or_else(|| not_a_table("users.host"))?;
                    child(hosts, alias).ok_or_else(|| not_a_table(&machine_name(&key.target)))?
                }
            };
            let entries = child(machine, key.backend.as_str())
                .and_then(Item::as_table_like_mut)
                .ok_or_else(|| not_a_table(&backend_name(key)))?;
            // A replaced user keeps the comments around it.
            let mut value = Value::from(user);
            if let Some(old) = entries.get(&key.container).and_then(Item::as_value) {
                *value.decor_mut() = old.decor().clone();
            }
            entries.insert(&key.container, Item::Value(value));
            Ok(())
        },
    )
}

/// The table `name` inside `parent`, created when missing: implicit, so the file shows only the
/// innermost header (`[users.local.docker]`), or inline inside an inline table. `None` when
/// `parent` or the child is not a table.
fn child<'a>(parent: &'a mut Item, name: &str) -> Option<&'a mut Item> {
    let inline = parent.is_inline_table();
    let table = parent.as_table_like_mut()?;
    let item = table.entry(name).or_insert_with(|| {
        if inline {
            Item::Value(Value::InlineTable(InlineTable::new()))
        } else {
            let mut table = Table::new();
            table.set_implicit(true);
            Item::Table(table)
        }
    });
    item.is_table_like().then_some(item)
}

/// The existing entries table of a container's backend, without creating anything.
fn existing_entries<'a>(root: &'a mut Item, key: &UserKey) -> Option<&'a mut dyn TableLike> {
    let users = root.get_mut("users")?;
    let machine = match &key.target {
        Target::Local => users.get_mut("local")?,
        Target::Host(alias) => users.get_mut("host")?.get_mut(alias.as_str())?,
    };
    machine.get_mut(key.backend.as_str())?.as_table_like_mut()
}

/// The dotted path of a machine's table, quoted as TOML needs.
fn machine_name(target: &Target) -> String {
    match target {
        Target::Local => "users.local".into(),
        Target::Host(alias) => {
            format!("users.host.{}", TomlKey::new(alias.as_str()).display_repr())
        }
    }
}

fn backend_name(key: &UserKey) -> String {
    format!("{}.{}", machine_name(&key.target), key.backend.as_str())
}

/// The locked read-modify-write every change goes through. `already` tells whether the file
/// already holds the change: then nothing is written, nor created. `change` edits the parsed
/// document (it gets the file's text for its errors). Returns whether the file changed.
fn edit(
    path: &Path,
    already: impl Fn(&Settings) -> bool,
    change: impl FnOnce(&mut DocumentMut, &str) -> Result<(), SettingsError>,
) -> Result<bool, SettingsError> {
    let io_error = |source| SettingsError::Io {
        path: path.to_path_buf(),
        source,
    };
    // A symlinked file (a dotfile manager's) is written through: the link stays a link.
    let target =
        symlink_target(path).map_err(|(path, source)| SettingsError::Io { path, source })?;
    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    // Fail early without creating anything when the file is already in order.
    let (_, current) = read(path)?;
    if already(&current) {
        return Ok(false);
    }
    fs::create_dir_all(dir).map_err(io_error)?;
    let lock = open_lock(&dir.join(".settings.lock")).map_err(io_error)?;
    acquire_lock(&lock, None).map_err(io_error)?;

    // Under the lock, read again: another writer may have changed the file.
    let (text, current) = read(path)?;
    if already(&current) {
        return Ok(false);
    }
    let mut document = if text.is_empty() {
        DocumentMut::new()
    } else {
        text.parse::<DocumentMut>()
            .map_err(|error| invalid(path, &text, error.span(), error.message()))?
    };
    if text.is_empty() {
        document.decor_mut().set_prefix(HEADER);
    }
    change(&mut document, &text)?;

    let pending =
        write_unique(dir, "settings", document.to_string().as_bytes(), 0o644).map_err(io_error)?;
    pending.rename_to(&target).map_err(io_error)?;
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
    let document = Document::parse(text.as_str())
        .map_err(|error| invalid(path, &text, error.span(), error.message()))?;
    let mut settings = Settings::default();
    if let Some(tabs) = document.get("tabs") {
        let table = tabs
            .as_table_like()
            .ok_or_else(|| invalid(path, &text, tabs.span(), "`tabs` must be a table"))?;
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
                        item.span(),
                        &format!("`{}` must be a boolean (true or false)", key.name()),
                    ));
                }
            }
        }
    }
    if let Some(users) = document.get("users") {
        let not_a_table = |item: &Item, name: &str| {
            invalid(
                path,
                &text,
                item.span(),
                &format!("`{name}` must be a table"),
            )
        };
        let users = users
            .as_table_like()
            .ok_or_else(|| not_a_table(users, "users"))?;
        let mut machines = Vec::new();
        if let Some(local) = users.get("local") {
            machines.push((Target::Local, local));
        }
        if let Some(hosts) = users.get("host") {
            let hosts = hosts
                .as_table_like()
                .ok_or_else(|| not_a_table(hosts, "users.host"))?;
            for (alias, machine) in hosts.iter() {
                machines.push((Target::Host(alias.to_string()), machine));
            }
        }
        for (target, machine) in machines {
            let table = machine
                .as_table_like()
                .ok_or_else(|| not_a_table(machine, &machine_name(&target)))?;
            // Other backends (Proxmox shells run as root) and unknown names are left alone.
            for backend in AUTOMATIC
                .into_iter()
                .filter(|backend| backend.chooses_user())
            {
                let Some(entries) = table.get(backend.as_str()) else {
                    continue;
                };
                let mut key = UserKey {
                    target: target.clone(),
                    backend,
                    container: String::new(),
                };
                let entries = entries
                    .as_table_like()
                    .ok_or_else(|| not_a_table(entries, &backend_name(&key)))?;
                for (container, item) in entries.iter() {
                    key.container = container.to_string();
                    let user = match item.as_value() {
                        Some(Value::String(user)) => user.value().trim(),
                        _ => "",
                    };
                    if user.is_empty() {
                        let name = format!(
                            "{}.{}",
                            backend_name(&key),
                            TomlKey::new(container).display_repr()
                        );
                        return Err(invalid(
                            path,
                            &text,
                            item.span(),
                            &format!("`{name}` must be a user name (a non-empty string)"),
                        ));
                    }
                    settings.users.insert(key.clone(), user.to_string());
                }
            }
        }
    }
    Ok((text, settings))
}

/// An invalid-file error whose line is the one `span` starts on.
fn invalid(path: &Path, text: &str, span: Option<Range<usize>>, reason: &str) -> SettingsError {
    let line = span.map(|span| text[..span.start.min(text.len())].matches('\n').count() + 1);
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
    fn inline_tabs_table_loads_and_is_edited_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir, "tabs = { podman = false }\n");
        assert!(!load(&path).unwrap().tabs.podman);
        assert!(set(&path, Key::TabsDocker, false).unwrap());
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("tabs = {"), "{text}");
        assert!(!text.contains("[tabs]"), "{text}");
        let tabs = load(&path).unwrap().tabs;
        assert!(!tabs.podman && !tabs.docker && tabs.tunnels);
    }

    #[test]
    fn set_writes_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles.toml");
        fs::write(&real, "[tabs]\ndocker = true\n").unwrap();
        let link = dir.path().join(FILE_NAME);
        std::os::unix::fs::symlink("dotfiles.toml", &link).unwrap();
        assert!(set(&link, Key::TabsDocker, false).unwrap());
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_to_string(&real).unwrap(),
            "[tabs]\ndocker = false\n"
        );
        assert!(!load(&link).unwrap().tabs.docker);
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

    fn key(target: Target, backend: BackendId, container: &str) -> UserKey {
        UserKey {
            target,
            backend,
            container: container.into(),
        }
    }

    fn local_api() -> UserKey {
        key(Target::Local, BackendId::Docker, "shop/api")
    }

    #[test]
    fn users_load_from_nested_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(
            &dir,
            "[users.local.docker]\n\"shop/api\" = \"deploy\"\nweb = \"www-data\"\n[users.host.prod.podman]\n\"shop/api\" = \"app\"\n",
        );
        let settings = load(&path).unwrap();
        let expected = BTreeMap::from([
            (local_api(), "deploy".to_string()),
            (
                key(Target::Local, BackendId::Docker, "web"),
                "www-data".into(),
            ),
            (
                key(Target::Host("prod".into()), BackendId::Podman, "shop/api"),
                "app".into(),
            ),
        ]);
        assert_eq!(settings.users, expected);
        assert_eq!(settings.user(&local_api()), Some("deploy"));
        assert_eq!(settings.tabs, Tabs::default());
    }

    #[test]
    fn alias_with_a_dot_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let db = key(
            Target::Host("db.example".into()),
            BackendId::Docker,
            "shop/db",
        );
        assert!(remember_user(&path, &db, Some("postgres")).unwrap());
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("[users.host.\"db.example\".docker]"),
            "{text}"
        );
        assert!(!text.contains("[users]"), "{text}");
        assert!(!text.contains("[users.host]"), "{text}");
        let users = load(&path).unwrap().users;
        assert_eq!(users, BTreeMap::from([(db, "postgres".to_string())]));
    }

    #[test]
    fn wrong_user_type_is_invalid_with_its_line() {
        let dir = tempfile::tempdir().unwrap();
        let error = load(&file(&dir, "[users.local.docker]\n\napi = 3\n")).unwrap_err();
        assert!(
            matches!(error, SettingsError::Invalid { line: Some(3), .. }),
            "{error}"
        );
        let error = load(&file(&dir, "[users.local.docker]\napi = \"  \"\n")).unwrap_err();
        assert!(matches!(error, SettingsError::Invalid { .. }), "{error}");
        let error = load(&file(&dir, "users = 1\n")).unwrap_err();
        assert!(
            matches!(error, SettingsError::Invalid { line: Some(1), .. }),
            "{error}"
        );
        for text in [
            "[users]\nlocal = 1\n",
            "[users]\nhost = 1\n",
            "[users.host]\nprod = 1\n",
            "[users.local]\ndocker = 1\n",
            "[users.host.prod]\npodman = []\n",
        ] {
            let error = load(&file(&dir, text)).unwrap_err();
            assert!(
                matches!(error, SettingsError::Invalid { .. }),
                "{text}: {error}"
            );
        }
    }

    #[test]
    fn unknown_backend_and_proxmox_are_ignored_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(
            &dir,
            "[users.local.proxmox]\n\"110\" = 5\n[users.local.lxd]\nx = 1\n[users.remote]\ny = 2\n",
        );
        assert!(load(&path).unwrap().users.is_empty());
        assert!(remember_user(&path, &local_api(), Some("deploy")).unwrap());
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"110\" = 5"), "{text}");
        assert!(text.contains("x = 1"), "{text}");
        assert!(text.contains("y = 2"), "{text}");
        assert_eq!(load(&path).unwrap().user(&local_api()), Some("deploy"));
    }

    #[test]
    fn remember_user_replaces_and_forgets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        assert!(remember_user(&path, &local_api(), Some("deploy")).unwrap());
        assert!(remember_user(&path, &local_api(), Some("root")).unwrap());
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"shop/api\" = \"root\""), "{text}");
        assert!(!text.contains("deploy"), "{text}");
        assert!(!remember_user(&path, &local_api(), Some("root")).unwrap());
        assert!(remember_user(&path, &local_api(), None).unwrap());
        assert!(!remember_user(&path, &local_api(), None).unwrap());
    }

    #[test]
    fn forget_then_load_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        assert!(remember_user(&path, &local_api(), Some("deploy")).unwrap());
        assert!(remember_user(&path, &local_api(), None).unwrap());
        assert!(load(&path).unwrap().users.is_empty());
    }

    #[test]
    fn forget_on_missing_file_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("portico").join(FILE_NAME);
        assert!(!remember_user(&path, &local_api(), None).unwrap());
        assert!(!path.exists());
        assert!(!path.parent().unwrap().exists());
    }

    #[test]
    fn remember_user_keeps_comments_and_tabs() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir, "# mine\n[tabs]\npodman = false\n");
        assert!(remember_user(&path, &local_api(), Some("deploy")).unwrap());
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("# mine\n[tabs]\npodman = false\n"),
            "{text}"
        );
        assert!(text.contains("[users.local.docker]"), "{text}");
        let settings = load(&path).unwrap();
        assert!(!settings.tabs.podman);
        assert_eq!(settings.user(&local_api()), Some("deploy"));
    }

    #[test]
    fn remember_user_refuses_an_invalid_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = file(&dir, "[tabs\n");
        let error = remember_user(&path, &local_api(), Some("deploy")).unwrap_err();
        assert!(matches!(error, SettingsError::Invalid { .. }), "{error}");
        assert_eq!(fs::read_to_string(&path).unwrap(), "[tabs\n");
    }

    #[test]
    fn remember_user_writes_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles.toml");
        fs::write(&real, "[tabs]\ndocker = true\n").unwrap();
        let link = dir.path().join(FILE_NAME);
        std::os::unix::fs::symlink("dotfiles.toml", &link).unwrap();
        assert!(remember_user(&link, &local_api(), Some("deploy")).unwrap());
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let text = fs::read_to_string(&real).unwrap();
        assert!(text.contains("\"shop/api\" = \"deploy\""), "{text}");
        assert_eq!(load(&link).unwrap().user(&local_api()), Some("deploy"));
    }

    #[test]
    fn remember_user_refuses_proxmox() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let ct = key(Target::Local, BackendId::Proxmox, "110");
        assert!(!remember_user(&path, &ct, Some("root")).unwrap());
        assert!(!path.exists());
    }

    #[test]
    fn remembered_user_is_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        assert!(remember_user(&path, &local_api(), Some("  deploy ")).unwrap());
        assert_eq!(load(&path).unwrap().user(&local_api()), Some("deploy"));
        assert!(!remember_user(&path, &local_api(), Some("deploy")).unwrap());
    }
}
