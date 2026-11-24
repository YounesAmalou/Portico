//! The configuration Portico owns: the managed host file, its parser and renderer, setup of
//! the user's entry config, and the transactions that change both.

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

use thiserror::Error;

use crate::domain::host::{Host, HostError, HostField};
use crate::platform::paths::SYSTEM_SSH_CONFIG;

mod config;
mod init;
mod render;
mod store;

pub use init::{InitError, InitOutcome, InitPlan};
pub use store::{ManagedStore, TransactionError};

pub(crate) use config::parse_value;
pub(crate) use render::render_host;

const HEADER: &str = "# Managed by Portico. Do not edit manually.";
const PORTICO_INCLUDE: &str = "Include ~/.ssh/portico/config";

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
    #[error("invalid changes for alias {alias}: {source}")]
    InvalidChanges {
        alias: String,
        #[source]
        source: HostError,
    },
}

/// Comment lines attached to a managed host, each a full `#` line without indentation.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HostComments {
    /// Rendered directly above the `Host` line.
    pub leading: Vec<String>,
    /// Rendered directly above the directive of the field.
    pub fields: BTreeMap<HostField, Vec<String>>,
    /// Rendered below the last directive: the comments that end the file after the host.
    pub trailing: Vec<String>,
}

impl HostComments {
    pub fn is_empty(&self) -> bool {
        self.leading.is_empty()
            && self.fields.values().all(Vec::is_empty)
            && self.trailing.is_empty()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ManagedConfig {
    hosts: Vec<Host>,
    /// Keyed by lowercase alias; a host without comments has no entry.
    comments: BTreeMap<String, HostComments>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigPaths {
    pub ssh_dir: PathBuf,
    pub entry: PathBuf,
    pub portico_dir: PathBuf,
    pub managed: PathBuf,
    pub backup: PathBuf,
    pub lock: PathBuf,
    pub system_config: PathBuf,
}

impl ConfigPaths {
    pub fn from_home(home: impl AsRef<Path>) -> Self {
        let ssh_dir = home.as_ref().join(".ssh");
        let entry = ssh_dir.join("config");
        let portico_dir = ssh_dir.join("portico");
        Self {
            managed: portico_dir.join("config"),
            backup: portico_dir.join("config.bak"),
            lock: portico_dir.join("config.lock"),
            system_config: PathBuf::from(SYSTEM_SSH_CONFIG),
            ssh_dir,
            entry,
            portico_dir,
        }
    }

    /// The home directory these paths were made from.
    pub fn home(&self) -> &Path {
        self.ssh_dir.parent().unwrap_or(&self.ssh_dir)
    }

    pub fn init_backups(&self) -> Result<Vec<PathBuf>, io::Error> {
        let mut backups = Vec::new();
        for entry in fs::read_dir(&self.ssh_dir)? {
            let path = entry?.path();
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("config.portico-backup-"))
            {
                backups.push(path);
            }
        }
        backups.sort();
        Ok(backups)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use tempfile::TempDir;

    pub(super) struct InitializedStore {
        pub(super) _dir: TempDir,
        pub(super) store: ManagedStore,
        pub(super) paths: ConfigPaths,
    }

    pub(super) fn initialized_store() -> InitializedStore {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::from_home(dir.path());
        let store = ManagedStore::new(paths.clone());
        store.apply_init(true, |_, _| Ok(())).unwrap();
        InitializedStore {
            _dir: dir,
            store,
            paths,
        }
    }

    #[test]
    fn review_mutation_preserves_raw_entry_in_candidate_and_keeps_managed_utf8_strict() {
        let fixture = initialized_store();
        let entry =
            b"Include ~/.ssh/portico/config\n# raw \xff\r\nHost legacy\n HostName legacy.example\n";
        fs::write(&fixture.paths.entry, entry).unwrap();
        fixture
            .store
            .transact(
                |config| {
                    config
                        .insert(Host::new("dev", "dev.example", None, None, None, None).unwrap())
                        .map_err(TransactionError::Operation)
                },
                |candidate, aliases| {
                    assert!(
                        fs::read(candidate)
                            .unwrap()
                            .ends_with(b"# raw \xff\r\nHost legacy\n HostName legacy.example\n")
                    );
                    crate::domain::openssh::OpenSsh::system()
                        .validate(candidate, aliases)
                        .map_err(|error| error.to_string())
                },
            )
            .unwrap();
        assert!(fixture.store.load().unwrap().find("dev").is_some());
        assert_eq!(fs::read(&fixture.paths.entry).unwrap(), entry);
        let invalid = b"# invalid managed \xff\n";
        fs::write(&fixture.paths.managed, invalid).unwrap();
        assert!(matches!(
            fixture.store.load(),
            Err(TransactionError::Managed { .. })
        ));
        assert!(matches!(
            fixture.store.apply_init(true, |_, _| Ok(())),
            Err(InitError::Managed { .. })
        ));
        assert_eq!(fs::read(&fixture.paths.managed).unwrap(), invalid);
    }
}
