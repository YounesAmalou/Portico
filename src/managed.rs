use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    mem,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use fs2::FileExt;
use thiserror::Error;

#[cfg(test)]
use std::sync::Arc;

use crate::config_error::{ConfigFiles, ConfigRejection, StagedEntry, interpret};
use crate::discovery::{ConfigDirective, parse_directive_bytes, split_comment};
use crate::host::{Host, HostChanges, HostError, HostField, parse_port};

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
}

impl HostComments {
    pub fn is_empty(&self) -> bool {
        self.leading.is_empty() && self.fields.values().all(Vec::is_empty)
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
            system_config: PathBuf::from("/etc/ssh/ssh_config"),
            ssh_dir,
            entry,
            portico_dir,
        }
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitPlan {
    pub create_entry: bool,
    pub create_managed: bool,
    pub insert_include: bool,
    pub reposition_include: bool,
    pub backup_entry: bool,
    pub fix_portico_permissions: bool,
    pub fix_managed_permissions: bool,
}

impl InitPlan {
    pub fn requires_confirmation(&self) -> bool {
        self.create_entry
            || self.create_managed
            || self.insert_include
            || self.reposition_include
            || self.fix_portico_permissions
            || self.fix_managed_permissions
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitOutcome {
    Initialized,
    Unchanged,
}

#[derive(Debug, Error)]
pub enum InitError {
    #[error("initialization requires confirmation")]
    ConfirmationRequired,
    #[error(
        "could not parse ~/.ssh/config: {0}; correct the reported configuration and rerun `portico init`"
    )]
    EntryParse(String),
    #[error(
        "managed config is invalid at ~/.ssh/portico/config: {source}; recover from {backup} where available, correct the configuration, and rerun `portico init`"
    )]
    Managed {
        backup: PathBuf,
        #[source]
        source: ManagedError,
    },
    #[error(
        "configuration validation failed: {0}; correct the reported option in ~/.ssh/config or its included files (check with `ssh -G -F ~/.ssh/config portico-validation`), then rerun `portico init`"
    )]
    Validation(ConfigRejection),
    #[error(
        "filesystem error at {path}: {source}; check permissions or whether another writer is blocking this path, correct the problem, and rerun `portico init`"
    )]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("initialization failed: {original}; rollback also failed; active state is uncertain; recovery: {recovery}; rollback: {failures}", failures = .rollback.iter().map(ToString::to_string).collect::<Vec<_>>().join("; "))]
    Rollback {
        #[source]
        original: Box<InitError>,
        rollback: Vec<InitError>,
        recovery: String,
    },
}

#[derive(Debug, Error)]
pub enum TransactionError {
    #[error(
        "Portico setup requires ~/.ssh/config, ~/.ssh/portico/config, and one exact top-level `Include ~/.ssh/portico/config` on the first line; run `portico init`"
    )]
    NotInitialized,
    #[error(transparent)]
    Operation(#[from] ManagedError),
    #[error("managed config is invalid; recover from {backup}: {source}")]
    Managed {
        backup: PathBuf,
        #[source]
        source: ManagedError,
    },
    #[error(
        "configuration validation failed: {0}; nothing was saved{suffix}",
        suffix = if matches!(.0, ConfigRejection::UserFile { .. }) { "; fix that line, then save again" } else { "" }
    )]
    Validation(ConfigRejection),
    #[error("entry config cannot be used for validation: {0}")]
    Entry(String),
    #[error("filesystem error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "activation sync failed at {path} and rollback also failed; active state is uncertain; recover from {recovery}: sync: {sync}; rollback: {rollback}"
    )]
    Rollback {
        path: PathBuf,
        recovery: PathBuf,
        sync: io::Error,
        #[source]
        rollback: io::Error,
    },
}

pub struct ManagedStore {
    paths: ConfigPaths,
    #[cfg(test)]
    before_transaction_lock: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl ManagedStore {
    pub fn new(paths: ConfigPaths) -> Self {
        Self {
            paths,
            #[cfg(test)]
            before_transaction_lock: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_before_transaction_lock(
        mut self,
        hook: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        self.before_transaction_lock = Some(hook);
        self
    }

    pub fn paths(&self) -> &ConfigPaths {
        &self.paths
    }

    pub fn load(&self) -> Result<ManagedConfig, TransactionError> {
        self.read_managed().map(|(_, config)| config)
    }

    pub fn plan_init(&self) -> Result<InitPlan, InitError> {
        let entry_path = self.entry_target()?;
        let entry_kind = match fs::metadata(&entry_path) {
            Ok(metadata) => Some(metadata.file_type()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(source) => return Err(init_io(&entry_path, source)),
        };
        if entry_kind.as_ref().is_some_and(|kind| !kind.is_file()) {
            return Err(init_io(
                &entry_path,
                io::Error::new(io::ErrorKind::InvalidInput, "entry config is not a file"),
            ));
        }
        let contents = match entry_kind {
            Some(_) => fs::read(&entry_path).map_err(|source| init_io(&entry_path, source))?,
            None => Vec::new(),
        };
        let entry = analyze_entry(&contents, &self.paths)?;
        let portico_mode = match fs::symlink_metadata(&self.paths.portico_dir) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(init_io(
                    &self.paths.portico_dir,
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "Portico directory is a symbolic link",
                    ),
                ));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(init_io(
                    &self.paths.portico_dir,
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "Portico path is not a directory",
                    ),
                ));
            }
            Ok(metadata) => Some(metadata.permissions().mode() & 0o777),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(source) => return Err(init_io(&self.paths.portico_dir, source)),
        };
        let managed_mode = match fs::symlink_metadata(&self.paths.managed) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(init_io(
                    &self.paths.managed,
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "managed config is a symbolic link",
                    ),
                ));
            }
            Ok(metadata) if !metadata.is_file() => {
                return Err(init_io(
                    &self.paths.managed,
                    io::Error::new(io::ErrorKind::InvalidInput, "managed config is not a file"),
                ));
            }
            Ok(metadata) => Some(metadata.permissions().mode() & 0o777),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(source) => return Err(init_io(&self.paths.managed, source)),
        };

        let create_entry = entry_kind.is_none();
        self.init_managed()?;
        Ok(InitPlan {
            create_entry,
            create_managed: managed_mode.is_none(),
            insert_include: !entry.has_include,
            reposition_include: entry.has_include && !entry.is_normalized,
            backup_entry: !create_entry && !entry.is_normalized,
            fix_portico_permissions: portico_mode.is_some_and(|mode| mode != 0o700),
            fix_managed_permissions: managed_mode.is_some_and(|mode| mode != 0o600),
        })
    }

    pub fn apply_init<V>(&self, approved: bool, validate: V) -> Result<InitOutcome, InitError>
    where
        V: Fn(&Path, &[String]) -> Result<(), String>,
    {
        self.apply_init_inner(approved, None, validate)
    }

    pub(crate) fn apply_init_cancellable<V>(
        &self,
        approved: bool,
        cancelled: &AtomicBool,
        validate: V,
    ) -> Result<InitOutcome, InitError>
    where
        V: Fn(&Path, &[String]) -> Result<(), String>,
    {
        self.apply_init_inner(approved, Some(cancelled), validate)
    }

    fn apply_init_inner<V>(
        &self,
        approved: bool,
        cancelled: Option<&AtomicBool>,
        validate: V,
    ) -> Result<InitOutcome, InitError>
    where
        V: Fn(&Path, &[String]) -> Result<(), String>,
    {
        let validate_installed = || {
            self.init_managed()?;
            // Existing hosts were validated when written; parsing the installed config is enough.
            validate(&self.paths.entry, &[]).map_err(|stderr| {
                // The installed entry is the user's own file, not one Portico staged.
                InitError::Validation(interpret_rejection(&stderr, &self.paths.managed, None))
            })
        };
        let preview = self.plan_init()?;
        if !preview.requires_confirmation() {
            validate_installed()?;
            return Ok(InitOutcome::Unchanged);
        }
        if !approved {
            return Err(InitError::ConfirmationRequired);
        }

        create_private_dir(&self.paths.ssh_dir)
            .map_err(|source| init_io(&self.paths.ssh_dir, source))?;
        create_private_dir(&self.paths.portico_dir)
            .map_err(|source| init_io(&self.paths.portico_dir, source))?;
        let lock =
            open_lock(&self.paths.lock).map_err(|source| init_io(&self.paths.lock, source))?;
        acquire_lock(&lock, cancelled).map_err(|source| init_io(&self.paths.lock, source))?;
        let plan = self.plan_init()?;
        if !plan.requires_confirmation() {
            validate_installed()?;
            return Ok(InitOutcome::Unchanged);
        }

        // Resolve the target once; atomic replacement must never replace the user's symlink.
        let entry_path = self.entry_target()?;
        let entry_dir = entry_path.parent().expect("entry config has a parent");
        let entry_contents = if plan.create_entry {
            Vec::new()
        } else {
            fs::read(&entry_path).map_err(|source| init_io(&entry_path, source))?
        };
        let entry_mode = if plan.create_entry {
            None
        } else {
            Some(
                fs::metadata(&entry_path)
                    .map_err(|source| init_io(&entry_path, source))?
                    .permissions(),
            )
        };
        let managed_mode = if plan.create_managed {
            None
        } else {
            Some(
                fs::metadata(&self.paths.managed)
                    .map_err(|source| init_io(&self.paths.managed, source))?
                    .permissions(),
            )
        };
        let directory_mode = fs::metadata(&self.paths.portico_dir)
            .map_err(|source| init_io(&self.paths.portico_dir, source))?
            .permissions();
        let managed = self.init_managed()?;
        let change_entry = plan.create_entry || plan.insert_include || plan.reposition_include;
        let final_entry = analyze_entry(&entry_contents, &self.paths)?.normalized;
        let backup = if plan.backup_entry {
            let mut backup = write_unique(
                &self.paths.ssh_dir,
                "config.portico-backup",
                &entry_contents,
                entry_mode
                    .as_ref()
                    .expect("existing entry has permissions")
                    .mode(),
            )
            .map_err(|source| init_io(&self.paths.entry, source))?;
            let path = backup.path.take().expect("backup has a path");
            sync_dir(&self.paths.ssh_dir).map_err(|source| init_io(&self.paths.ssh_dir, source))?;
            Some(path)
        } else {
            None
        };

        let mut entry_written = false;
        let mut managed_written = false;
        let installation = (|| {
            // ponytail: editors do not share our lock; an editor with an older view must refresh.
            if change_entry {
                write_unique(
                    entry_dir,
                    ".config.portico-init",
                    &final_entry,
                    entry_mode.as_ref().map_or(0o600, PermissionsExt::mode),
                )
                .and_then(|candidate| candidate.rename_to(&entry_path))
                .map_err(|source| init_io(&entry_path, source))?;
                entry_written = true;
                sync_dir(entry_dir).map_err(|source| init_io(entry_dir, source))?;
            }
            if plan.fix_portico_permissions {
                fs::set_permissions(&self.paths.portico_dir, fs::Permissions::from_mode(0o700))
                    .map_err(|source| init_io(&self.paths.portico_dir, source))?;
            }
            if plan.create_managed {
                write_unique(
                    &self.paths.portico_dir,
                    ".config.init",
                    managed.render().as_bytes(),
                    0o600,
                )
                .and_then(|candidate| candidate.rename_to(&self.paths.managed))
                .map_err(|source| init_io(&self.paths.managed, source))?;
                managed_written = true;
            } else if plan.fix_managed_permissions {
                fs::set_permissions(&self.paths.managed, fs::Permissions::from_mode(0o600))
                    .and_then(|()| File::open(&self.paths.managed)?.sync_all())
                    .map_err(|source| init_io(&self.paths.managed, source))?;
            }
            sync_dir(&self.paths.portico_dir)
                .map_err(|source| init_io(&self.paths.portico_dir, source))?;
            // write_unique closes every writable handle before returning.
            validate_installed()
        })();
        if let Err(original) = installation {
            let mut rollback = Vec::new();
            let mut record = |path: &Path, result: io::Result<()>| {
                if let Err(source) = result {
                    rollback.push(init_io(path, source));
                }
            };
            if entry_written {
                let restored = if let Some(mode) = &entry_mode {
                    write_unique(
                        entry_dir,
                        ".config.portico-rollback",
                        &entry_contents,
                        mode.mode(),
                    )
                    .and_then(|candidate| candidate.rename_to(&entry_path))
                } else {
                    fs::remove_file(&entry_path)
                };
                record(&entry_path, restored);
                record(entry_dir, sync_dir(entry_dir));
            }
            if managed_written {
                record(&self.paths.managed, fs::remove_file(&self.paths.managed));
            } else if plan.fix_managed_permissions
                && let Some(mode) = managed_mode
            {
                record(
                    &self.paths.managed,
                    fs::set_permissions(&self.paths.managed, mode)
                        .and_then(|()| File::open(&self.paths.managed)?.sync_all()),
                );
            }
            record(&self.paths.portico_dir, sync_dir(&self.paths.portico_dir));
            if plan.fix_portico_permissions {
                record(
                    &self.paths.portico_dir,
                    fs::set_permissions(&self.paths.portico_dir, directory_mode),
                );
            }
            if rollback.is_empty() {
                return Err(original);
            }
            let entry_recovery = if entry_written {
                backup.map_or_else(
                    || format!("remove newly created {} if present", entry_path.display()),
                    |path| format!("restore {} from {}", entry_path.display(), path.display()),
                )
            } else {
                format!("entry config was unchanged at {}", entry_path.display())
            };
            let managed_recovery = if managed_written {
                format!(
                    "remove newly created {} if present",
                    self.paths.managed.display()
                )
            } else {
                format!(
                    "managed content was unchanged at {}",
                    self.paths.managed.display()
                )
            };
            return Err(InitError::Rollback {
                original: Box::new(original),
                rollback,
                recovery: format!(
                    "{entry_recovery}; {managed_recovery}; correct permissions or configuration, then rerun `portico init`"
                ),
            });
        }
        Ok(InitOutcome::Initialized)
    }

    fn entry_target(&self) -> Result<PathBuf, InitError> {
        let mut path = self.paths.entry.clone();
        for _ in 0..40 {
            match fs::read_link(&path) {
                Ok(target) => path = path.parent().expect("entry has a parent").join(target),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::InvalidInput | io::ErrorKind::NotFound
                    ) =>
                {
                    return Ok(path);
                }
                Err(source) => return Err(init_io(&path, source)),
            }
        }
        Err(init_io(
            &path,
            io::Error::other("too many symbolic links; correct the entry target"),
        ))
    }

    fn init_managed(&self) -> Result<ManagedConfig, InitError> {
        let bytes = match fs::read(&self.paths.managed) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(ManagedConfig::empty());
            }
            Err(source) => return Err(init_io(&self.paths.managed, source)),
        };
        let invalid = |source| InitError::Managed {
            backup: self.paths.backup.clone(),
            source,
        };
        let text = std::str::from_utf8(&bytes).map_err(|error| {
            invalid(ManagedError::Parse {
                line: 1,
                message: error.to_string(),
            })
        })?;
        ManagedConfig::parse(text).map_err(invalid)
    }

    fn ensure_ready(&self) -> Result<(), TransactionError> {
        let entry = fs::read(&self.paths.entry)
            .map_err(|source| transaction_storage_io(&self.paths.entry, source))?;
        if !self.paths.managed.is_file()
            || !analyze_entry(&entry, &self.paths)
                .map_err(transaction_entry_error)?
                .is_normalized
        {
            return Err(TransactionError::NotInitialized);
        }
        Ok(())
    }

    /// Applies `mutate` to the managed config under the exclusive lock and activates the result.
    ///
    /// `validate` receives the candidate entry config and the aliases whose declarations the
    /// transaction added or changed. Resolving one alias makes OpenSSH parse the whole candidate,
    /// so only those aliases are probed; an empty slice (nothing changed, as for a removal)
    /// asks for a syntax-only probe. Validation cost therefore does not grow with host count.
    pub fn transact<R, E, M, V>(&self, mutate: M, validate: V) -> Result<R, E>
    where
        E: From<TransactionError>,
        M: FnOnce(&mut ManagedConfig) -> Result<R, E>,
        V: Fn(&Path, &[String]) -> Result<(), String>,
    {
        self.transact_inner(mutate, validate, None)
    }

    pub(crate) fn transact_cancellable<R, E, M, V>(
        &self,
        mutate: M,
        validate: V,
        cancelled: &AtomicBool,
    ) -> Result<R, E>
    where
        E: From<TransactionError>,
        M: FnOnce(&mut ManagedConfig) -> Result<R, E>,
        V: Fn(&Path, &[String]) -> Result<(), String>,
    {
        self.transact_inner(mutate, validate, Some(cancelled))
    }

    fn transact_inner<R, E, M, V>(
        &self,
        mutate: M,
        validate: V,
        cancelled: Option<&AtomicBool>,
    ) -> Result<R, E>
    where
        E: From<TransactionError>,
        M: FnOnce(&mut ManagedConfig) -> Result<R, E>,
        V: Fn(&Path, &[String]) -> Result<(), String>,
    {
        let (
            result,
            Staged {
                lock: _lock,
                active,
                candidate,
                validation_file,
                portico_lines,
                changed_aliases,
                ..
            },
        ) = self.stage(mutate, cancelled)?;
        validate(validation_file.path(), &changed_aliases).map_err(|stderr| {
            TransactionError::Validation(interpret_rejection(
                &stderr,
                candidate.path(),
                Some(StagedEntry {
                    path: validation_file.path(),
                    source: &self.paths.entry,
                    portico_lines: &portico_lines,
                }),
            ))
        })?;
        drop(validation_file);

        let backup_candidate = write_unique(
            &self.paths.portico_dir,
            ".config.portico-backup",
            &active,
            0o600,
        )
        .map_err(|source| transaction_io(&self.paths.portico_dir, source))?;
        backup_candidate
            .rename_to(&self.paths.backup)
            .map_err(|source| transaction_io(&self.paths.backup, source))?;
        candidate
            .rename_to(&self.paths.managed)
            .map_err(|source| transaction_io(&self.paths.managed, source))?;
        if let Err(source) = sync_dir(&self.paths.portico_dir) {
            let rollback = write_unique(
                &self.paths.portico_dir,
                ".config.portico-rollback",
                &active,
                0o600,
            )
            .and_then(|candidate| candidate.rename_to(&self.paths.managed))
            .and_then(|()| sync_dir(&self.paths.portico_dir));
            match rollback {
                Ok(()) => {
                    return Err(transaction_io(&self.paths.portico_dir, source).into());
                }
                Err(rollback) => {
                    return Err(TransactionError::Rollback {
                        path: self.paths.managed.clone(),
                        recovery: self.paths.backup.clone(),
                        sync: source,
                        rollback,
                    }
                    .into());
                }
            }
        }
        Ok(result)
    }

    /// Applies `mutate` under the exclusive lock and writes the candidate managed config and an
    /// entry config that includes it, without activating anything.
    fn stage<R, E, M>(&self, mutate: M, cancelled: Option<&AtomicBool>) -> Result<(R, Staged), E>
    where
        E: From<TransactionError>,
        M: FnOnce(&mut ManagedConfig) -> Result<R, E>,
    {
        self.ensure_ready()?;
        let lock = open_lock(&self.paths.lock)
            .map_err(|source| transaction_storage_io(&self.paths.lock, source))?;
        #[cfg(test)]
        if let Some(hook) = &self.before_transaction_lock {
            hook();
        }
        acquire_lock(&lock, cancelled)
            .map_err(|source| transaction_io(&self.paths.lock, source))?;

        let (active, mut config) = self.read_managed()?;
        let previous = config.clone();
        let result = mutate(&mut config)?;
        let candidate = write_unique(
            &self.paths.portico_dir,
            ".config.portico-candidate",
            config.render().as_bytes(),
            0o600,
        )
        .map_err(|source| transaction_io(&self.paths.portico_dir, source))?;

        let entry = fs::read(&self.paths.entry)
            .map_err(|source| transaction_io(&self.paths.entry, source))?;
        let (validation_entry, portico_lines) =
            substitute_include(&entry, &self.paths, candidate.path())
                .map_err(transaction_entry_error)?;
        let entry_mode = fs::metadata(&self.paths.entry)
            .map_err(|source| transaction_io(&self.paths.entry, source))?
            .permissions()
            .mode();
        let validation_file = write_unique(
            &self.paths.ssh_dir,
            ".config.portico-validation",
            &validation_entry,
            entry_mode,
        )
        .map_err(|source| transaction_io(&self.paths.ssh_dir, source))?;
        let changed_aliases = config.changed_aliases(&previous);
        Ok((
            result,
            Staged {
                lock,
                active,
                candidate,
                validation_entry,
                validation_file,
                portico_lines,
                entry_mode,
                changed_aliases,
            },
        ))
    }

    /// Stages `mutate` exactly as a save would and hands the staged entry config to `use_entry`,
    /// then discards everything; nothing is activated.
    ///
    /// When `paths.system_config` exists, the handed-over entry ends with `Match all` and an
    /// Include of it, because `ssh -F` would otherwise skip the system configuration. The lock is
    /// released before `use_entry` runs, so a slow test does not block a concurrent save.
    pub(crate) fn preview<R, E, M, U>(
        &self,
        mutate: M,
        use_entry: U,
        cancelled: Option<&AtomicBool>,
    ) -> Result<R, E>
    where
        E: From<TransactionError>,
        M: FnOnce(&mut ManagedConfig) -> Result<(), E>,
        U: FnOnce(ConfigFiles<'_>) -> Result<R, E>,
    {
        let (
            (),
            Staged {
                lock,
                candidate,
                mut validation_entry,
                validation_file,
                mut portico_lines,
                entry_mode,
                ..
            },
        ) = self.stage(mutate, cancelled)?;
        let preview_file = if self.paths.system_config.exists() {
            if !validation_entry.is_empty() && !validation_entry.ends_with(b"\n") {
                validation_entry.push(b'\n');
            }
            let lines = line_count(&validation_entry);
            portico_lines.extend([lines + 1, lines + 2]);
            validation_entry.extend_from_slice(
                format!(
                    "Match all\nInclude {}\n",
                    quote_path(&self.paths.system_config.to_string_lossy())
                )
                .as_bytes(),
            );
            drop(validation_file);
            write_unique(
                &self.paths.ssh_dir,
                ".config.portico-preview",
                &validation_entry,
                entry_mode,
            )
            .map_err(|source| transaction_io(&self.paths.ssh_dir, source))?
        } else {
            validation_file
        };
        drop(lock);
        let managed_text = read_lossy(candidate.path());
        let result = use_entry(ConfigFiles {
            managed: candidate.path(),
            managed_text: &managed_text,
            entry: Some(StagedEntry {
                path: preview_file.path(),
                source: &self.paths.entry,
                portico_lines: &portico_lines,
            }),
        });
        drop(preview_file);
        drop(candidate);
        result
    }

    fn read_managed(&self) -> Result<(Vec<u8>, ManagedConfig), TransactionError> {
        self.ensure_ready()?;
        reject_symlink(&self.paths.managed)
            .map_err(|source| transaction_io(&self.paths.managed, source))?;
        let contents = fs::read(&self.paths.managed)
            .map_err(|source| transaction_storage_io(&self.paths.managed, source))?;
        let text = std::str::from_utf8(&contents).map_err(|source| TransactionError::Managed {
            backup: self.paths.backup.clone(),
            source: ManagedError::Parse {
                line: 1,
                message: source.to_string(),
            },
        })?;
        let config = ManagedConfig::parse(text).map_err(|source| TransactionError::Managed {
            backup: self.paths.backup.clone(),
            source,
        })?;
        Ok((contents, config))
    }
}

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
            if trimmed.is_empty() {
                pending.clear();
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

        if let Some(host) = current {
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

struct EntryAnalysis {
    has_include: bool,
    is_normalized: bool,
    normalized: Vec<u8>,
}

fn analyze_entry(contents: &[u8], paths: &ConfigPaths) -> Result<EntryAnalysis, InitError> {
    let mut matches = 0;
    let mut retained = Vec::new();
    for line in contents.split_inclusive(|byte| *byte == b'\n') {
        // Discovery owns unrelated syntax warnings; installed OpenSSH validation reports errors.
        match parse_directive_bytes(line) {
            Ok(Some(ConfigDirective::Include(arguments))) => {
                let count = arguments
                    .iter()
                    .filter(|argument| is_managed_include(argument, paths))
                    .count();
                matches += count;
                if count == 0 {
                    retained.extend_from_slice(line);
                } else {
                    let remaining = arguments
                        .iter()
                        .filter(|argument| !is_managed_include(argument, paths))
                        .map(|argument| quote(argument))
                        .collect::<Vec<_>>();
                    append_include(&mut retained, &remaining, line);
                }
            }
            _ => retained.extend_from_slice(line),
        }
    }
    // OpenSSH ignores trailing whitespace, so a CRLF-terminated first line is still exact.
    let is_normalized = matches == 1
        && contents
            .strip_prefix(PORTICO_INCLUDE.as_bytes())
            .is_some_and(|rest| {
                rest.is_empty() || rest.starts_with(b"\n") || rest.starts_with(b"\r\n")
            });
    Ok(EntryAnalysis {
        has_include: matches > 0,
        is_normalized,
        normalized: if is_normalized {
            contents.to_vec()
        } else {
            let mut normalized = format!("{PORTICO_INCLUDE}\n").into_bytes();
            normalized.extend(retained);
            normalized
        },
    })
}

fn append_include(output: &mut Vec<u8>, arguments: &[String], original: &[u8]) {
    let comment = split_comment(original).1;
    if !arguments.is_empty() {
        output.extend_from_slice(format!("Include {}", arguments.join(" ")).as_bytes());
        if !comment.is_empty() {
            output.push(b' ');
        }
    }
    if !comment.is_empty() {
        output.extend_from_slice(comment);
    } else if !arguments.is_empty() {
        if original.ends_with(b"\r\n") {
            output.extend_from_slice(b"\r\n");
        } else if original.ends_with(b"\n") {
            output.push(b'\n');
        }
    }
}

/// Copies the entry config `contents`, pointing Portico's Include at `target` instead, and
/// returns the copy with the 1-based numbers of the lines it rewrote. Each rewritten line stays a
/// single line, so every other line keeps its number.
fn substitute_include(
    contents: &[u8],
    paths: &ConfigPaths,
    target: &Path,
) -> Result<(Vec<u8>, Vec<u32>), InitError> {
    let mut replaced = Vec::new();
    let mut result = Vec::new();
    for (index, line) in contents.split_inclusive(|byte| *byte == b'\n').enumerate() {
        match parse_directive_bytes(line)
            .map_err(|error| InitError::EntryParse(error.to_string()))?
        {
            Some(ConfigDirective::Include(arguments))
                if arguments
                    .iter()
                    .any(|argument| is_managed_include(argument, paths)) =>
            {
                replaced.push(u32::try_from(index + 1).unwrap_or(u32::MAX));
                let arguments = arguments
                    .iter()
                    .map(|argument| {
                        if is_managed_include(argument, paths) {
                            quote_path(&target.to_string_lossy())
                        } else {
                            quote(argument)
                        }
                    })
                    .collect::<Vec<_>>();
                append_include(&mut result, &arguments, line);
            }
            _ => result.extend_from_slice(line),
        }
    }
    if replaced.is_empty() {
        return Err(InitError::EntryParse(
            "Portico Include is missing from candidate entry config".into(),
        ));
    }
    Ok((result, replaced))
}

/// The number of lines in `contents` as OpenSSH counts them.
fn line_count(contents: &[u8]) -> u32 {
    let count = contents.split_inclusive(|byte| *byte == b'\n').count();
    u32::try_from(count).unwrap_or(u32::MAX)
}

fn is_managed_include(argument: &str, paths: &ConfigPaths) -> bool {
    let path = if let Some(relative) = argument.strip_prefix("~/") {
        paths
            .ssh_dir
            .parent()
            .unwrap_or(&paths.ssh_dir)
            .join(relative)
    } else if argument == "~" {
        paths
            .ssh_dir
            .parent()
            .unwrap_or(&paths.ssh_dir)
            .to_path_buf()
    } else if Path::new(argument).is_absolute() {
        argument.into()
    } else {
        paths.ssh_dir.join(argument)
    };
    normalize_path(&path) == normalize_path(&paths.managed)
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => fs::set_permissions(path, fs::Permissions::from_mode(0o700)),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        Err(error) => Err(error),
    }
}

fn reject_symlink(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "managed path is a symbolic link",
        )),
        Ok(metadata) if !metadata.is_file() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "managed path is not a file",
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn open_lock(path: &Path) -> io::Result<File> {
    reject_symlink(path)?;
    match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(file) => {
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            file.sync_all()?;
            Ok(file)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            OpenOptions::new().read(true).write(true).open(path)
        }
        Err(error) => Err(error),
    }
}

fn acquire_lock(file: &File, cancelled: Option<&AtomicBool>) -> io::Result<()> {
    let Some(cancelled) = cancelled else {
        return FileExt::lock_exclusive(file);
    };
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "operation cancelled",
            ));
        }
        match FileExt::try_lock_exclusive(file) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error),
        }
    }
}

/// Reads a managed config for mapping OpenSSH's line numbers back to hosts.
///
/// A read failure yields empty text: the line then maps to no host, so the rejection degrades
/// to an unmapped one, which is still reported verbatim. Nothing depends on the text otherwise.
pub(crate) fn read_lossy(path: &Path) -> String {
    fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

/// Interprets ssh's stderr against the managed config at `managed` and the entry Portico staged
/// at `entry`, if any.
fn interpret_rejection(
    stderr: &str,
    managed: &Path,
    entry: Option<StagedEntry<'_>>,
) -> ConfigRejection {
    interpret(
        stderr,
        &ConfigFiles {
            managed,
            managed_text: &read_lossy(managed),
            entry,
        },
    )
}

/// A staged change: the candidate managed config and an entry config including it, written but
/// not activated, with the transaction lock still held.
struct Staged {
    lock: File,
    active: Vec<u8>,
    candidate: PendingFile,
    validation_entry: Vec<u8>,
    validation_file: PendingFile,
    /// The lines of the validation entry Portico wrote rather than copied from the user's.
    portico_lines: Vec<u32>,
    entry_mode: u32,
    changed_aliases: Vec<String>,
}

pub(crate) struct PendingFile {
    path: Option<PathBuf>,
}

impl PendingFile {
    pub(crate) fn path(&self) -> &Path {
        self.path.as_deref().expect("pending file has a path")
    }

    fn rename_to(mut self, destination: &Path) -> io::Result<()> {
        fs::rename(self.path(), destination)?;
        self.path = None;
        Ok(())
    }
}

impl Drop for PendingFile {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_file(path);
        }
    }
}

pub(crate) fn write_unique(
    dir: &Path,
    stem: &str,
    contents: &[u8],
    mode: u32,
) -> io::Result<PendingFile> {
    #[cfg(test)]
    if stem == ".config.portico-rollback"
        && FAIL_TRANSACTION_ROLLBACK_WRITE.with(|fail| fail.replace(false))
    {
        return Err(io::Error::other("injected rollback write failure"));
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    for attempt in 0..1000 {
        let path = dir.join(format!("{stem}-{}-{nonce}-{attempt}", std::process::id()));
        match write_named_new(&path, contents, mode) {
            Ok(()) => return Ok(PendingFile { path: Some(path) }),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique temporary file",
    ))
}

#[cfg(test)]
thread_local! {
    static FAIL_TRANSACTION_ROLLBACK_WRITE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn fail_next_transaction_rollback_write() {
    FAIL_TRANSACTION_ROLLBACK_WRITE.with(|fail| fail.set(true));
}

fn write_named_new(path: &Path, contents: &[u8], mode: u32) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)?;
    let result = (|| {
        file.set_permissions(fs::Permissions::from_mode(mode))?;
        file.write_all(contents)?;
        file.sync_all()
    })();
    if result.is_err() {
        drop(file);
        let _ = fs::remove_file(path);
    }
    result
}

fn sync_dir(path: &Path) -> io::Result<()> {
    #[cfg(test)]
    if DIRECTORY_SYNC_FAILURES.with(|failures| {
        let mut failures = failures.borrow_mut();
        let Some((target, remaining)) = failures.front_mut() else {
            return false;
        };
        if target != path {
            return false;
        }
        if *remaining > 0 {
            *remaining -= 1;
            return false;
        }
        failures.pop_front();
        true
    }) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "injected directory sync failure",
        ));
    }
    File::open(path)?.sync_all()
}

#[cfg(test)]
thread_local! {
    static DIRECTORY_SYNC_FAILURES: std::cell::RefCell<std::collections::VecDeque<(PathBuf, usize)>> = const { std::cell::RefCell::new(std::collections::VecDeque::new()) };
}

#[cfg(test)]
fn fail_next_directory_sync(path: &Path) {
    fail_directory_sync_after(path, 0);
}

#[cfg(test)]
fn fail_directory_sync_after(path: &Path, successful_syncs: usize) {
    DIRECTORY_SYNC_FAILURES.with(|failures| {
        failures
            .borrow_mut()
            .push_back((path.to_owned(), successful_syncs))
    });
}

fn init_io(path: &Path, source: io::Error) -> InitError {
    InitError::Io {
        path: path.into(),
        source,
    }
}

fn transaction_io(path: &Path, source: io::Error) -> TransactionError {
    TransactionError::Io {
        path: path.into(),
        source,
    }
}

fn transaction_storage_io(path: &Path, source: io::Error) -> TransactionError {
    if source.kind() == io::ErrorKind::NotFound {
        TransactionError::NotInitialized
    } else {
        transaction_io(path, source)
    }
}

fn transaction_entry_error(error: InitError) -> TransactionError {
    TransactionError::Entry(error.to_string())
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

fn render_host(host: &Host, comments: Option<&HostComments>) -> String {
    let mut lines = Vec::new();
    if let Some(comments) = comments {
        lines.extend(comments.leading.iter().cloned());
    }
    lines.push(format!("Host {}", quote(host.alias())));
    let mut directive = |field: HostField, line: String| {
        if let Some(comments) = comments.and_then(|comments| comments.fields.get(&field)) {
            lines.extend(comments.iter().map(|comment| format!("    {comment}")));
        }
        lines.push(format!("    {line}"));
    };
    directive(
        HostField::Hostname,
        format!("HostName {}", quote(host.hostname())),
    );
    if let Some(user) = host.user() {
        directive(HostField::User, format!("User {}", quote(user)));
    }
    if let Some(port) = host.port() {
        directive(HostField::Port, format!("Port {port}"));
    }
    if let Some(identity_file) = host.identity_file() {
        directive(
            HostField::IdentityFile,
            format!("IdentityFile {}", quote(identity_file)),
        );
    }
    if let Some(proxy_jump) = host.proxy_jump() {
        directive(HostField::ProxyJump, format!("ProxyJump {proxy_jump}"));
    }
    lines.join("\n")
}

fn quote(value: &str) -> String {
    if value
        .chars()
        .any(|c| c.is_whitespace() || "\\\"'#".contains(c))
    {
        quote_path(value)
    } else {
        value.into()
    }
}

fn quote_path(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::Change;
    use tempfile::TempDir;

    struct InitializedStore {
        _dir: TempDir,
        store: ManagedStore,
        paths: ConfigPaths,
    }

    fn initialized_store() -> InitializedStore {
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
    fn header_and_detached_comments_never_attach() {
        let text = "# Managed by Portico. Do not edit manually.\n\n# detached\n\nHost dev\n    HostName dev.example\n";
        let config = ManagedConfig::parse(text).unwrap();
        assert!(config.comments("dev").is_none_or(HostComments::is_empty));
        assert!(!config.render().contains("# detached"));
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
    fn remove_entry_returns_the_comments() {
        let mut config = ManagedConfig::empty();
        let comments = HostComments {
            leading: vec!["# lead".into()],
            fields: BTreeMap::from([(HostField::Hostname, vec!["# where".into()])]),
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
    fn review_init_preserves_raw_comments_and_validates_with_real_openssh() {
        for (original, expected) in [
            (&b"# caf\xe9\r\nHost legacy\r\n HostName legacy.example"[..], &b"Include ~/.ssh/portico/config\n# caf\xe9\r\nHost legacy\r\n HostName legacy.example"[..]),
            (&b"# caf\xe9\r\nInclude portico/config other.conf # ann\xff\r\nHost legacy\n HostName legacy.example\n"[..], &b"Include ~/.ssh/portico/config\n# caf\xe9\r\nInclude other.conf # ann\xff\r\nHost legacy\n HostName legacy.example\n"[..]),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let paths = ConfigPaths::from_home(dir.path());
            fs::create_dir_all(&paths.ssh_dir).unwrap();
            fs::write(&paths.entry, original).unwrap();
            let store = ManagedStore::new(paths.clone());
            store.apply_init(true, |entry, _| {
                let output = std::process::Command::new("ssh").args(["-G", "-F"]).arg(entry).args(["--", "legacy"]).output().unwrap();
                assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
                assert!(String::from_utf8_lossy(&output.stdout).contains("hostname legacy.example"));
                Ok(())
            }).unwrap();
            assert_eq!(fs::read(&paths.entry).unwrap(), expected);
            assert_eq!(fs::read(&paths.init_backups().unwrap()[0]).unwrap(), original);
            assert_eq!(store.apply_init(true, |_, _| Ok(())).unwrap(), InitOutcome::Unchanged);
            assert!(store.load().is_ok());
        }
    }

    #[test]
    fn review_init_retains_mixed_include_annotation() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::from_home(dir.path());
        fs::create_dir_all(&paths.ssh_dir).unwrap();
        fs::write(
            &paths.entry,
            "Include 'other#literal.conf' portico/config # keep this annotation\r\n",
        )
        .unwrap();
        ManagedStore::new(paths.clone())
            .apply_init(true, |_, _| Ok(()))
            .unwrap();
        assert_eq!(fs::read(&paths.entry).unwrap(), b"Include ~/.ssh/portico/config\nInclude \"other#literal.conf\" # keep this annotation\r\n");
    }

    #[test]
    fn review_validation_rollback_restores_raw_entry_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::from_home(dir.path());
        fs::create_dir_all(&paths.ssh_dir).unwrap();
        let original = b"# raw \xff\r\nHost legacy\n";
        fs::write(&paths.entry, original).unwrap();
        let error = ManagedStore::new(paths.clone())
            .apply_init(true, |_, _| Err("rejected installed setup".into()))
            .unwrap_err();
        assert!(matches!(error, InitError::Validation(_)), "{error}");
        assert_eq!(fs::read(&paths.entry).unwrap(), original);
        assert_eq!(
            fs::read(&paths.init_backups().unwrap()[0]).unwrap(),
            original
        );
        assert!(!paths.managed.exists());
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
                    crate::openssh::OpenSsh::system()
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

    #[test]
    fn automatic_init_checks_complete_setup_without_changing_files() {
        let fixture = initialized_store();
        let entry = b"Include ~/.ssh/portico/config\nBrokenOption yes\n";
        fs::write(&fixture.paths.entry, entry).unwrap();
        let managed = fs::read(&fixture.paths.managed).unwrap();
        let error = fixture
            .store
            .apply_init(true, |path, _| {
                assert_eq!(path, fixture.paths.entry);
                Err("Bad configuration option: BrokenOption".into())
            })
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Bad configuration option: BrokenOption")
        );
        assert!(error.to_string().contains("portico init"));
        assert_eq!(fs::read(&fixture.paths.entry).unwrap(), entry);
        assert_eq!(fs::read(&fixture.paths.managed).unwrap(), managed);
        assert!(fixture.paths.init_backups().unwrap().is_empty());
    }

    #[test]
    fn automatic_init_rollback_never_suggests_removing_an_unchanged_entry() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::from_home(dir.path());
        fs::create_dir_all(&paths.ssh_dir).unwrap();
        fs::write(&paths.entry, "Include ~/.ssh/portico/config\n").unwrap();
        fail_next_directory_sync(&paths.portico_dir);
        fail_next_directory_sync(&paths.portico_dir);
        let error = ManagedStore::new(paths.clone())
            .apply_init(true, |_, _| Ok(()))
            .unwrap_err();
        let InitError::Rollback { recovery, .. } = error else {
            panic!("expected rollback failure")
        };
        assert!(
            recovery.contains("entry config was unchanged"),
            "{recovery}"
        );
        assert_eq!(
            fs::read(&paths.entry).unwrap(),
            b"Include ~/.ssh/portico/config\n"
        );
    }

    #[test]
    fn automatic_init_repairs_a_missing_symlink_target() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::from_home(dir.path());
        fs::create_dir_all(&paths.ssh_dir).unwrap();
        let target = dir.path().join("missing-target");
        symlink("../missing-target", &paths.entry).unwrap();
        ManagedStore::new(paths.clone())
            .apply_init(true, |entry, _| {
                assert_eq!(entry, paths.entry);
                assert_eq!(fs::read(entry).unwrap(), b"Include ~/.ssh/portico/config\n");
                Ok(())
            })
            .unwrap();
        assert_eq!(
            fs::read_link(&paths.entry).unwrap(),
            Path::new("../missing-target")
        );
        assert_eq!(
            fs::read(target).unwrap(),
            b"Include ~/.ssh/portico/config\n"
        );
        assert!(paths.init_backups().unwrap().is_empty());
    }

    #[test]
    fn automatic_init_validates_installed_files_after_writes_close() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::from_home(dir.path());
        ManagedStore::new(paths.clone())
            .apply_init(true, |entry, aliases| {
                assert_eq!(entry, paths.entry);
                assert!(aliases.is_empty());
                assert_eq!(fs::read(entry).unwrap(), b"Include ~/.ssh/portico/config\n");
                assert_eq!(
                    fs::read(&paths.managed).unwrap(),
                    b"# Managed by Portico. Do not edit manually.\n"
                );
                for descriptor in fs::read_dir("/proc/self/fd").unwrap().flatten() {
                    if let Ok(target) = fs::read_link(descriptor.path())
                        && (target == paths.entry || target == paths.managed)
                    {
                        let fd: i32 = descriptor.file_name().to_str().unwrap().parse().unwrap();
                        assert_eq!(
                            unsafe { libc::fcntl(fd, libc::F_GETFL) } & libc::O_ACCMODE,
                            libc::O_RDONLY
                        );
                    }
                }
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn automatic_init_prepends_without_changing_user_bytes_or_mode() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::from_home(dir.path());
        fs::create_dir_all(&paths.ssh_dir).unwrap();
        let original = b"# my config\r\n\r\nHost legacy\r\n\tHostName legacy.example";
        fs::write(&paths.entry, original).unwrap();
        fs::set_permissions(&paths.entry, fs::Permissions::from_mode(0o640)).unwrap();
        ManagedStore::new(paths.clone())
            .apply_init(true, |_, _| Ok(()))
            .unwrap();
        let installed = fs::read(&paths.entry).unwrap();
        assert_eq!(
            installed
                .strip_prefix(b"Include ~/.ssh/portico/config\n")
                .unwrap(),
            original
        );
        assert_eq!(
            fs::metadata(&paths.entry).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(
            fs::read(&paths.init_backups().unwrap()[0]).unwrap(),
            original
        );
    }

    #[test]
    fn automatic_init_repairs_partial_states_and_normalizes_includes() {
        for (before, after) in [
            (None, "Include ~/.ssh/portico/config\n"),
            (
                Some("Include ~/.ssh/portico/config\n"),
                "Include ~/.ssh/portico/config\n",
            ),
            (
                Some(
                    "# note\r\nInclude portico/config\nHost *\n Include ~/.ssh/portico/config other.conf\n",
                ),
                "Include ~/.ssh/portico/config\n# note\r\nHost *\nInclude other.conf\n",
            ),
        ] {
            for managed_exists in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let paths = ConfigPaths::from_home(dir.path());
                fs::create_dir_all(&paths.portico_dir).unwrap();
                if let Some(before) = before {
                    fs::write(&paths.entry, before).unwrap();
                }
                if managed_exists {
                    fs::write(&paths.managed, "Host dev\n HostName dev.example\n").unwrap();
                }
                let store = ManagedStore::new(paths.clone());
                store.apply_init(true, |_, _| Ok(())).unwrap();
                assert_eq!(fs::read_to_string(&paths.entry).unwrap(), after);
                let managed = fs::read(&paths.managed).unwrap();
                if managed_exists {
                    assert_eq!(managed, b"Host dev\n HostName dev.example\n");
                }
                assert_eq!(
                    store
                        .apply_init(true, |entry, _| {
                            assert_eq!(entry, paths.entry);
                            Ok(())
                        })
                        .unwrap(),
                    InitOutcome::Unchanged
                );
                assert_eq!(fs::read_to_string(&paths.entry).unwrap(), after);
                assert_eq!(fs::read(&paths.managed).unwrap(), managed);
            }
        }
    }

    #[test]
    fn automatic_init_repairs_symlink_target_and_keeps_exact_backups() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::from_home(dir.path());
        fs::create_dir_all(&paths.ssh_dir).unwrap();
        let target = dir.path().join("dotfiles-config");
        fs::write(&target, "# note\nHost legacy\n").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
        symlink("../dotfiles-config", &paths.entry).unwrap();
        let store = ManagedStore::new(paths.clone());
        for _ in 0..2 {
            fs::write(&target, "# note\nHost legacy\n").unwrap();
            store
                .apply_init(true, |entry, _| {
                    assert_eq!(entry, paths.entry);
                    Ok(())
                })
                .unwrap();
            assert_eq!(
                fs::read_link(&paths.entry).unwrap(),
                Path::new("../dotfiles-config")
            );
            assert_eq!(
                fs::read(&target).unwrap(),
                b"Include ~/.ssh/portico/config\n# note\nHost legacy\n"
            );
            assert_eq!(
                fs::metadata(&target).unwrap().permissions().mode() & 0o777,
                0o640
            );
        }
        assert_eq!(paths.init_backups().unwrap().len(), 2);
        for backup in paths.init_backups().unwrap() {
            assert_eq!(fs::read(backup).unwrap(), b"# note\nHost legacy\n");
        }
    }

    #[test]
    fn automatic_init_validation_failure_restores_entry_and_managed_state() {
        use std::os::unix::fs::symlink;
        for existing in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let paths = ConfigPaths::from_home(dir.path());
            fs::create_dir_all(&paths.portico_dir).unwrap();
            let target = dir.path().join("target");
            if existing {
                fs::write(&target, "Host legacy\n").unwrap();
                symlink(&target, &paths.entry).unwrap();
                fs::write(&paths.managed, "Host dev\n HostName dev.example\n").unwrap();
                fs::set_permissions(&paths.managed, fs::Permissions::from_mode(0o640)).unwrap();
            }
            let error = ManagedStore::new(paths.clone())
                .apply_init(true, |entry, _| {
                    assert_eq!(entry, paths.entry);
                    assert!(paths.managed.exists());
                    Err("Bad configuration option: BrokenOption".into())
                })
                .unwrap_err();
            let message = error.to_string();
            for expected in [
                "Bad configuration option",
                "~/.ssh/config",
                "correct",
                "portico init",
            ] {
                assert!(message.contains(expected), "{message}");
            }
            if existing {
                assert!(
                    fs::symlink_metadata(&paths.entry)
                        .unwrap()
                        .file_type()
                        .is_symlink()
                );
                assert_eq!(fs::read(&target).unwrap(), b"Host legacy\n");
                assert_eq!(
                    fs::read(&paths.managed).unwrap(),
                    b"Host dev\n HostName dev.example\n"
                );
                assert_eq!(
                    fs::metadata(&paths.managed).unwrap().permissions().mode() & 0o777,
                    0o640
                );
            } else {
                assert!(!paths.entry.exists());
                assert!(!paths.managed.exists());
            }
        }
    }

    #[test]
    fn automatic_init_write_failure_names_path_and_remedy() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::from_home(dir.path());
        fs::create_dir_all(&paths.ssh_dir).unwrap();
        fs::write(&paths.entry, "Host legacy\n").unwrap();
        fs::set_permissions(&paths.ssh_dir, fs::Permissions::from_mode(0o500)).unwrap();
        let error = ManagedStore::new(paths.clone())
            .apply_init(true, |_, _| Ok(()))
            .unwrap_err();
        fs::set_permissions(&paths.ssh_dir, fs::Permissions::from_mode(0o700)).unwrap();
        let message = error.to_string();
        for expected in [
            paths.portico_dir.to_str().unwrap(),
            "permissions",
            "another writer",
            "portico init",
        ] {
            assert!(message.contains(expected), "{message}");
        }
        assert_eq!(fs::read(&paths.entry).unwrap(), b"Host legacy\n");
    }

    #[test]
    fn automatic_init_invalid_managed_config_names_recovery_and_preserves_bytes() {
        let fixture = initialized_store();
        let invalid = b"Host dev\n BrokenOption value\n";
        fs::write(&fixture.paths.managed, invalid).unwrap();
        fs::write(&fixture.paths.backup, "known good backup").unwrap();
        let error = fixture.store.apply_init(true, |_, _| Ok(())).unwrap_err();
        let message = error.to_string();
        for expected in ["config.bak", "portico init", "invalid"] {
            assert!(message.contains(expected), "{message}");
        }
        assert_eq!(fs::read(&fixture.paths.managed).unwrap(), invalid);
        assert_eq!(
            fs::read(&fixture.paths.backup).unwrap(),
            b"known good backup"
        );
    }

    #[test]
    fn initialization_backs_up_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::from_home(dir.path());
        std::fs::create_dir_all(&paths.ssh_dir).unwrap();
        std::fs::write(&paths.entry, "Host existing\n  HostName old.example\n").unwrap();
        let store = ManagedStore::new(paths.clone());

        let plan = store.plan_init().unwrap();
        assert!(plan.requires_confirmation());
        store.apply_init(true, |_, _| Ok(())).unwrap();
        let once = std::fs::read(&paths.entry).unwrap();
        assert!(String::from_utf8_lossy(&once).starts_with("Include ~/.ssh/portico/config\n"));
        assert_eq!(
            store.apply_init(true, |_, _| Ok(())).unwrap(),
            InitOutcome::Unchanged
        );
        assert_eq!(std::fs::read(&paths.entry).unwrap(), once);
        assert_eq!(
            std::fs::read(paths.init_backups().unwrap().first().unwrap()).unwrap(),
            b"Host existing\n  HostName old.example\n"
        );
    }

    #[test]
    fn initialization_preserves_unrelated_include_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::from_home(dir.path());
        std::fs::create_dir_all(&paths.ssh_dir).unwrap();
        std::fs::write(
            &paths.entry,
            "Include   conf.d/*.conf # keep this comment\nHost existing\n",
        )
        .unwrap();

        ManagedStore::new(paths.clone())
            .apply_init(true, |_, _| Ok(()))
            .unwrap();

        assert!(
            std::fs::read_to_string(&paths.entry)
                .unwrap()
                .contains("Include   conf.d/*.conf # keep this comment\n")
        );
    }

    #[test]
    fn candidate_substitution_preserves_all_include_arguments_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::from_home(dir.path());
        let candidate = paths.portico_dir.join("candidate");
        let entry = "Include ~/.ssh/portico/config first.conf\nHost unrelated\nInclude second.conf ~/.ssh/portico/config third.conf\n";

        let (substituted, portico_lines) =
            substitute_include(entry.as_bytes(), &paths, &candidate).unwrap();

        assert_eq!(portico_lines, [1, 3]);
        assert_eq!(
            String::from_utf8(substituted).unwrap(),
            format!(
                "Include \"{}\" first.conf\nHost unrelated\nInclude second.conf \"{}\" third.conf\n",
                candidate.display(),
                candidate.display()
            )
        );
    }

    #[test]
    fn init_reports_an_error_in_the_users_entry_as_theirs() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::from_home(dir.path());
        std::fs::create_dir_all(&paths.ssh_dir).unwrap();
        std::fs::write(
            &paths.entry,
            "Include ~/.ssh/portico/config\n# mine\nBogus yes\n",
        )
        .unwrap();
        let entry = paths.entry.clone();
        let result = ManagedStore::new(paths.clone()).apply_init(true, |_, _| {
            Err(format!(
                "{}: line 3: Bad configuration option: bogus",
                entry.display()
            ))
        });

        match result {
            Err(InitError::Validation(rejection)) => assert_eq!(
                rejection,
                ConfigRejection::UserFile {
                    path: paths.entry.clone(),
                    line: 3,
                    message: "Bad configuration option: bogus".into(),
                }
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn managed_only_init_rollback_preserves_entry_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::from_home(dir.path());
        std::fs::create_dir_all(&paths.ssh_dir).unwrap();
        let target = dir.path().join("dotfiles-ssh-config");
        std::fs::write(&target, "Include ~/.ssh/portico/config\n").unwrap();
        symlink(&target, &paths.entry).unwrap();
        let result = ManagedStore::new(paths.clone())
            .apply_init(true, |_, _| Err("invalid installed configuration".into()));

        assert!(matches!(result, Err(InitError::Validation(_))));
        assert!(!paths.managed.exists());
        assert!(
            std::fs::symlink_metadata(&paths.entry)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "Include ~/.ssh/portico/config\n"
        );
    }

    #[test]
    fn rejected_initialization_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::from_home(dir.path());
        let store = ManagedStore::new(paths.clone());
        let error = store.apply_init(false, |_, _| Ok(())).unwrap_err();

        assert!(matches!(error, InitError::ConfirmationRequired));
        assert!(!paths.ssh_dir.exists());
    }

    #[test]
    fn initialization_failed_restoration_reports_uncertain_state_and_backup() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::from_home(dir.path());
        fs::create_dir_all(&paths.ssh_dir).unwrap();
        fs::write(&paths.entry, "Host existing\n HostName original.example\n").unwrap();
        // Fail activation sync and the rollback write.
        fail_directory_sync_after(&paths.ssh_dir, 1);
        fail_next_transaction_rollback_write();

        let error = ManagedStore::new(paths.clone())
            .apply_init(true, |_, _| Ok(()))
            .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("active state is uncertain"), "{message}");
        assert!(
            message.contains("injected directory sync failure"),
            "{message}"
        );
        assert!(
            message.contains("injected rollback write failure"),
            "{message}"
        );
        let backup = paths.init_backups().unwrap().pop().unwrap();
        assert!(message.contains(&backup.display().to_string()), "{message}");
        assert!(
            message.contains(&paths.entry.display().to_string()),
            "{message}"
        );
        assert!(
            message.contains(&paths.managed.display().to_string()),
            "{message}"
        );
        assert_eq!(
            fs::read(&backup).unwrap(),
            b"Host existing\n HostName original.example\n"
        );
        assert!(!paths.managed.exists());
    }

    #[test]
    fn initialization_failed_rollback_sync_reports_uncertain_state() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::from_home(dir.path());
        fail_next_directory_sync(&paths.ssh_dir);
        fail_next_directory_sync(&paths.ssh_dir);

        let error = ManagedStore::new(paths.clone())
            .apply_init(true, |_, _| Ok(()))
            .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("active state is uncertain"), "{message}");
        assert_eq!(
            message.matches("injected directory sync failure").count(),
            2,
            "{message}"
        );
        assert!(
            message.contains(&paths.entry.display().to_string()),
            "{message}"
        );
        assert!(
            message.contains(&paths.managed.display().to_string()),
            "{message}"
        );
        assert!(!paths.entry.exists());
        assert!(!paths.managed.exists());
    }

    #[test]
    fn initialization_successful_rollback_restores_entry_bytes_and_mode() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::from_home(dir.path());
        fs::create_dir_all(&paths.ssh_dir).unwrap();
        fs::write(&paths.entry, "Host existing\n HostName original.example\n").unwrap();
        fs::set_permissions(&paths.entry, fs::Permissions::from_mode(0o640)).unwrap();
        fail_directory_sync_after(&paths.ssh_dir, 1);

        let error = ManagedStore::new(paths.clone())
            .apply_init(true, |_, _| Ok(()))
            .unwrap_err();

        assert!(matches!(error, InitError::Io { .. }));
        assert_eq!(
            fs::read(&paths.entry).unwrap(),
            b"Host existing\n HostName original.example\n"
        );
        assert_eq!(
            fs::metadata(&paths.entry).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert!(!paths.managed.exists());
    }

    #[test]
    fn created_paths_have_restrictive_modes() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::from_home(dir.path());
        ManagedStore::new(paths.clone())
            .apply_init(true, |_, _| Ok(()))
            .unwrap();

        assert_eq!(
            std::fs::metadata(&paths.portico_dir)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&paths.managed)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn existing_managed_paths_are_repaired_to_private_modes() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = initialized_store();
        std::fs::set_permissions(
            &fixture.paths.portico_dir,
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::fs::set_permissions(
            &fixture.paths.managed,
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();

        assert!(fixture.store.plan_init().unwrap().requires_confirmation());
        fixture.store.apply_init(true, |_, _| Ok(())).unwrap();

        assert_eq!(
            std::fs::metadata(&fixture.paths.portico_dir)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&fixture.paths.managed)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn symlinked_portico_directory_is_rejected() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::from_home(dir.path());
        std::fs::create_dir_all(&paths.ssh_dir).unwrap();
        std::fs::write(&paths.entry, "Include ~/.ssh/portico/config\n").unwrap();
        let target = dir.path().join("external-portico");
        std::fs::create_dir(&target).unwrap();
        symlink(&target, &paths.portico_dir).unwrap();

        let error = ManagedStore::new(paths).plan_init().unwrap_err();

        assert!(matches!(error, InitError::Io { .. }));
        assert!(error.to_string().contains("symbolic link"));
    }

    #[test]
    fn failed_validation_preserves_active_bytes() {
        let fixture = initialized_store();
        let before = std::fs::read(&fixture.paths.managed).unwrap();
        let host = Host::new("dev", "dev.example", None, None, None, None).unwrap();

        let result = fixture.store.transact(
            |config| config.insert(host).map_err(TransactionError::Operation),
            |_, _| Err("invalid candidate".into()),
        );

        assert!(matches!(result, Err(TransactionError::Validation(_))));
        assert_eq!(std::fs::read(&fixture.paths.managed).unwrap(), before);
    }

    #[test]
    fn a_rejected_managed_line_reports_its_host_and_field() {
        let fixture = initialized_store();
        let host = Host::new("dev", "dev.example", None, Some(22), None, None).unwrap();

        let result = fixture.store.transact(
            |config| config.insert(host).map_err(TransactionError::Operation),
            |entry, _| {
                let candidate = included_candidate(&fs::read(entry).unwrap());
                let text = fs::read_to_string(&candidate).unwrap();
                let line = text
                    .lines()
                    .position(|line| line.trim() == "Port 22")
                    .expect("the candidate has a Port line")
                    + 1;
                Err(format!(
                    "{} line {line}: Bad port '22'.",
                    candidate.display()
                ))
            },
        );

        assert!(
            matches!(
                result,
                Err(TransactionError::Validation(ConfigRejection::Managed {
                    field: Some(HostField::Port),
                    ..
                }))
            ),
            "{result:?}"
        );
    }

    /// Runs the real `ssh -G` on a staged entry and returns its stderr on failure.
    fn real_ssh_g(entry: &Path) -> Result<(), String> {
        let output = std::process::Command::new("ssh")
            .args(["-G", "-F"])
            .arg(entry)
            .args(["--", "dev"])
            .output()
            .unwrap();
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).into_owned())
        }
    }

    #[test]
    fn a_rejected_line_of_the_users_entry_names_their_real_file() {
        let fixture = initialized_store();
        fs::write(
            &fixture.paths.entry,
            "Include ~/.ssh/portico/config\n# mine\nBogus yes\n",
        )
        .unwrap();
        let host = Host::new("dev", "dev.example", None, None, None, None).unwrap();

        let result = fixture.store.transact(
            |config| config.insert(host).map_err(TransactionError::Operation),
            |entry, _| real_ssh_g(entry),
        );

        match result {
            Err(TransactionError::Validation(ConfigRejection::UserFile { path, line, .. })) => {
                assert_eq!(path, fixture.paths.entry);
                assert_eq!(line, 3);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_preview_maps_the_users_entry_lines_back_to_their_file() {
        let fixture = preview_fixture("ssh_config");
        fs::write(&fixture.paths.system_config, "# system\n").unwrap();
        fs::write(
            &fixture.paths.entry,
            "Include ~/.ssh/portico/config\n# mine\nBogus yes\n",
        )
        .unwrap();

        let rejection = fixture
            .store
            .preview(
                |_| Ok::<_, TransactionError>(()),
                |files| {
                    let stderr = real_ssh_g(files.entry.unwrap().path).unwrap_err();
                    Ok::<_, TransactionError>(interpret(&stderr, &files))
                },
                None,
            )
            .unwrap();

        assert!(
            matches!(
                &rejection,
                ConfigRejection::UserFile { path, line: 3, .. } if path == &fixture.paths.entry
            ),
            "{rejection:?}"
        );
    }

    #[test]
    fn validation_probes_only_added_or_changed_aliases() {
        let fixture = initialized_store();
        let host = |alias: &str, hostname: &str| {
            Host::new(alias, hostname, None, None, None, None).unwrap()
        };
        let probed = std::sync::Mutex::new(Vec::new());
        let transact = |mutate: &dyn Fn(&mut ManagedConfig) -> Result<(), ManagedError>| {
            probed.lock().unwrap().clear();
            fixture
                .store
                .transact(
                    |config| mutate(config).map_err(TransactionError::Operation),
                    |_, aliases| {
                        probed.lock().unwrap().extend_from_slice(aliases);
                        Ok(())
                    },
                )
                .unwrap();
            probed.lock().unwrap().clone()
        };

        transact(&|config| {
            for index in 0..5 {
                config.insert(host(&format!("host{index}"), "a.example"))?;
            }
            Ok(())
        });
        assert_eq!(
            transact(&|config| config.insert(host("extra", "x"))),
            ["extra"]
        );
        assert_eq!(
            transact(&|config| config.apply(
                "host2",
                HostChanges {
                    hostname: crate::host::Change::Set("b.example".into()),
                    ..HostChanges::default()
                }
            )),
            ["host2"]
        );
        assert_eq!(
            transact(&|config| config.replace("host3", host("Renamed", "a.example"))),
            ["Renamed"]
        );
        assert!(transact(&|config| config.remove("host0").map(drop)).is_empty());
    }

    #[test]
    fn crlf_terminated_include_line_is_normalized() {
        let fixture = initialized_store();
        let entry = b"Include ~/.ssh/portico/config\r\nHost legacy\r\n HostName legacy.example\r\n";
        fs::write(&fixture.paths.entry, entry).unwrap();

        assert!(!fixture.store.plan_init().unwrap().requires_confirmation());
        assert!(fixture.store.load().is_ok());
        assert_eq!(
            fixture.store.apply_init(false, |_, _| Ok(())).unwrap(),
            InitOutcome::Unchanged
        );
        assert_eq!(fs::read(&fixture.paths.entry).unwrap(), entry);
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

    #[test]
    fn rejected_operation_does_not_suggest_recovering_valid_config() {
        let fixture = initialized_store();
        let before = fs::read(&fixture.paths.managed).unwrap();
        let error = fixture
            .store
            .transact(
                |config| {
                    config
                        .remove("missing")
                        .map_err(TransactionError::Operation)
                },
                |_, _| Ok(()),
            )
            .unwrap_err();

        assert!(error.to_string().contains("unknown alias"));
        assert!(!error.to_string().contains("recover"), "{error}");
        assert!(!error.to_string().contains("config is invalid"), "{error}");
        assert_eq!(fs::read(&fixture.paths.managed).unwrap(), before);
        assert!(!fixture.paths.backup.exists());
    }

    #[test]
    fn corrupt_active_config_still_names_the_recovery_backup() {
        let fixture = initialized_store();
        fs::write(&fixture.paths.managed, "Host bad\n Unexpected value\n").unwrap();

        let error = fixture
            .store
            .transact(|_| Ok::<_, TransactionError>(()), |_, _| Ok(()))
            .unwrap_err();

        assert!(
            matches!(&error, TransactionError::Managed { backup, .. } if backup == &fixture.paths.backup)
        );
        assert!(error.to_string().contains("recover"));
    }

    #[test]
    fn malformed_quoted_active_config_preserves_active_and_backup_bytes() {
        let fixture = initialized_store();
        fixture
            .store
            .transact(
                |config| {
                    config
                        .insert(Host::new("one", "one.example", None, None, None, None).unwrap())
                        .map_err(TransactionError::Operation)
                },
                |_, _| Ok(()),
            )
            .unwrap();
        let backup = fs::read(&fixture.paths.backup).unwrap();
        let malformed = b"Host one\n  HostName one.example\n  IdentityFile \"unterminated\n";
        fs::write(&fixture.paths.managed, malformed).unwrap();

        let result = fixture
            .store
            .transact(|_| Ok::<_, TransactionError>(()), |_, _| Ok(()));

        assert!(matches!(result, Err(TransactionError::Managed { .. })));
        assert_eq!(fs::read(&fixture.paths.managed).unwrap(), malformed);
        assert_eq!(fs::read(&fixture.paths.backup).unwrap(), backup);
    }

    #[test]
    fn successful_mutation_keeps_exact_previous_version() {
        let fixture = initialized_store();
        let first = Host::new("one", "one.example", None, None, None, None).unwrap();
        fixture
            .store
            .transact(
                |config| config.insert(first).map_err(TransactionError::Operation),
                |_, _| Ok(()),
            )
            .unwrap();
        let before_second = std::fs::read(&fixture.paths.managed).unwrap();
        let second = Host::new("two", "two.example", None, None, None, None).unwrap();
        fixture
            .store
            .transact(
                |config| config.insert(second).map_err(TransactionError::Operation),
                |_, _| Ok(()),
            )
            .unwrap();

        assert_eq!(std::fs::read(&fixture.paths.backup).unwrap(), before_second);
    }

    #[test]
    fn post_rename_sync_failure_restores_active_bytes() {
        let fixture = initialized_store();
        let before = std::fs::read(&fixture.paths.managed).unwrap();
        let host = Host::new("dev", "dev.example", None, None, None, None).unwrap();
        fail_next_directory_sync(&fixture.paths.portico_dir);

        let result = fixture.store.transact(
            |config| config.insert(host).map_err(TransactionError::Operation),
            |_, _| Ok(()),
        );

        assert!(matches!(result, Err(TransactionError::Io { .. })));
        assert_eq!(std::fs::read(&fixture.paths.managed).unwrap(), before);
    }

    #[test]
    fn rollback_failure_reports_uncertain_state_and_recovery_path() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = initialized_store();
        let before = std::fs::read(&fixture.paths.managed).unwrap();
        let host = Host::new("dev", "dev.example", None, None, None, None).unwrap();
        std::fs::set_permissions(
            &fixture.paths.portico_dir,
            std::fs::Permissions::from_mode(0o300),
        )
        .unwrap();
        fail_next_transaction_rollback_write();

        let error = fixture
            .store
            .transact(
                |config| config.insert(host).map_err(TransactionError::Operation),
                |_, _| Ok(()),
            )
            .unwrap_err();

        std::fs::set_permissions(
            &fixture.paths.portico_dir,
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        assert!(matches!(
            &error,
            TransactionError::Rollback { path, recovery, sync, rollback }
                if path == &fixture.paths.managed
                    && recovery == &fixture.paths.backup
                    && sync.kind() == io::ErrorKind::PermissionDenied
                    && rollback.to_string() == "injected rollback write failure"
        ));
        let message = error.to_string();
        assert!(message.contains("active state is uncertain"));
        assert!(message.contains(&fixture.paths.backup.display().to_string()));
        assert_eq!(std::fs::read(&fixture.paths.backup).unwrap(), before);
    }

    #[test]
    fn rollback_directory_sync_failure_reports_uncertain_state() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = initialized_store();
        let before = std::fs::read(&fixture.paths.managed).unwrap();
        let host = Host::new("dev", "dev.example", None, None, None, None).unwrap();
        std::fs::set_permissions(
            &fixture.paths.portico_dir,
            std::fs::Permissions::from_mode(0o300),
        )
        .unwrap();

        let error = fixture
            .store
            .transact(
                |config| config.insert(host).map_err(TransactionError::Operation),
                |_, _| Ok(()),
            )
            .unwrap_err();

        std::fs::set_permissions(
            &fixture.paths.portico_dir,
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        assert!(matches!(
            &error,
            TransactionError::Rollback { path, recovery, sync, rollback }
                if path == &fixture.paths.managed
                    && recovery == &fixture.paths.backup
                    && sync.kind() == io::ErrorKind::PermissionDenied
                    && rollback.kind() == io::ErrorKind::PermissionDenied
        ));
        assert_eq!(std::fs::read(&fixture.paths.managed).unwrap(), before);
    }

    fn preview_fixture(system_config: &str) -> InitializedStore {
        let mut fixture = initialized_store();
        fixture
            .store
            .transact(
                |config| {
                    config
                        .insert(Host::new("dev", "dev.example", None, None, None, None).unwrap())
                        .map_err(TransactionError::Operation)
                },
                |_, _| Ok(()),
            )
            .unwrap();
        fixture.paths.system_config = fixture._dir.path().join(system_config);
        fixture.store = ManagedStore::new(fixture.paths.clone());
        fixture
    }

    fn preview_snapshot(paths: &ConfigPaths) -> [Option<Vec<u8>>; 3] {
        [&paths.managed, &paths.backup, &paths.entry].map(|path| fs::read(path).ok())
    }

    fn included_candidate(entry: &[u8]) -> PathBuf {
        let text = std::str::from_utf8(entry).unwrap();
        let line = text
            .lines()
            .find(|line| line.contains(".config.portico-candidate"))
            .expect("staged entry includes the candidate");
        let start = line.find('"').unwrap() + 1;
        let end = start + line[start..].find('"').unwrap();
        PathBuf::from(&line[start..end])
    }

    fn assert_no_staged_files(paths: &ConfigPaths) {
        for dir in [&paths.ssh_dir, &paths.portico_dir] {
            for entry in fs::read_dir(dir).unwrap() {
                let name = entry.unwrap().file_name();
                assert!(
                    !name.to_string_lossy().starts_with(".config.portico-"),
                    "left behind {name:?} in {}",
                    dir.display()
                );
            }
        }
    }

    #[test]
    fn preview_hands_over_the_candidate_and_changes_nothing() {
        let fixture = preview_fixture("missing_ssh_config");
        let before = preview_snapshot(&fixture.paths);

        let (entry, candidate) = fixture
            .store
            .preview(
                |config| {
                    config
                        .insert(Host::new("new", "new.example", None, None, None, None).unwrap())
                        .map_err(TransactionError::Operation)
                },
                |files| {
                    let entry = files.entry.expect("a preview stages an entry").path;
                    let candidate = included_candidate(&fs::read(entry).unwrap());
                    let contents = fs::read_to_string(&candidate).unwrap();
                    assert!(contents.contains("Host new"), "{contents}");
                    assert!(contents.contains("Host dev"), "{contents}");
                    Ok::<_, TransactionError>((entry.to_owned(), candidate))
                },
                None,
            )
            .unwrap();

        assert_eq!(preview_snapshot(&fixture.paths), before);
        assert!(!entry.exists());
        assert!(!candidate.exists());
        assert_no_staged_files(&fixture.paths);
    }

    #[test]
    fn preview_cleans_up_when_the_closure_fails() {
        let fixture = preview_fixture("missing_ssh_config");
        let before = preview_snapshot(&fixture.paths);
        let mut seen = None;

        let result = fixture.store.preview(
            |config| {
                config
                    .insert(Host::new("new", "new.example", None, None, None, None).unwrap())
                    .map_err(TransactionError::Operation)
            },
            |files| -> Result<(), TransactionError> {
                let entry = files.entry.expect("a preview stages an entry").path;
                seen = Some((
                    entry.to_owned(),
                    included_candidate(&fs::read(entry).unwrap()),
                ));
                Err(TransactionError::Validation(
                    ConfigRejection::Unrecognised {
                        raw: "ssh failed".into(),
                    },
                ))
            },
            None,
        );

        assert!(
            matches!(result, Err(TransactionError::Validation(ConfigRejection::Unrecognised { raw })) if raw == "ssh failed")
        );
        let (entry, candidate) = seen.expect("closure ran");
        assert_eq!(preview_snapshot(&fixture.paths), before);
        assert!(!entry.exists());
        assert!(!candidate.exists());
        assert_no_staged_files(&fixture.paths);
    }

    #[test]
    fn preview_reports_mutation_errors_before_staging() {
        let fixture = preview_fixture("missing_ssh_config");
        let before = preview_snapshot(&fixture.paths);
        let mut called = false;

        let result = fixture.store.preview(
            |config| {
                config
                    .remove("missing")
                    .map(drop)
                    .map_err(TransactionError::Operation)
            },
            |_| {
                called = true;
                Ok(())
            },
            None,
        );

        assert!(matches!(
            result,
            Err(TransactionError::Operation(ManagedError::UnknownAlias(_)))
        ));
        assert!(!called);
        assert_eq!(preview_snapshot(&fixture.paths), before);
        assert_no_staged_files(&fixture.paths);
    }

    #[test]
    fn preview_ends_with_the_system_config_only_when_it_exists() {
        let fixture = preview_fixture("ssh_config");
        fs::write(
            &fixture.paths.system_config,
            "Host *\n  ServerAliveInterval 30\n",
        )
        .unwrap();
        let entry = fixture
            .store
            .preview(
                |_| Ok::<_, TransactionError>(()),
                |files| Ok(fs::read_to_string(files.entry.unwrap().path).unwrap()),
                None,
            )
            .unwrap();
        let expected = format!(
            "\nMatch all\nInclude {}\n",
            quote_path(&fixture.paths.system_config.to_string_lossy())
        );
        assert!(entry.ends_with(&expected), "{entry:?}");
        assert_eq!(entry.matches("Match all").count(), 1, "{entry:?}");
        assert_no_staged_files(&fixture.paths);

        let fixture = preview_fixture("missing_ssh_config");
        let entry = fixture
            .store
            .preview(
                |_| Ok::<_, TransactionError>(()),
                |files| {
                    Ok::<_, TransactionError>(
                        fs::read_to_string(files.entry.unwrap().path).unwrap(),
                    )
                },
                None,
            )
            .unwrap();
        assert!(!entry.contains("Match all"), "{entry:?}");
        assert_no_staged_files(&fixture.paths);
    }

    #[test]
    fn preview_does_not_hold_the_lock_while_testing() {
        let fixture = preview_fixture("missing_ssh_config");
        let other = ManagedStore::new(fixture.paths.clone());
        let cancelled = AtomicBool::new(false);
        let finished = AtomicBool::new(false);

        let saved = thread::scope(|scope| {
            let finished = &finished;
            let cancelled = &cancelled;
            scope.spawn(move || {
                for _ in 0..250 {
                    if finished.load(Ordering::Relaxed) {
                        return;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                cancelled.store(true, Ordering::Relaxed);
            });
            let result = fixture.store.preview(
                |_| Ok::<_, TransactionError>(()),
                |_| {
                    other.transact_cancellable(
                        |config| {
                            config
                                .insert(
                                    Host::new("saved", "saved.example", None, None, None, None)
                                        .unwrap(),
                                )
                                .map_err(TransactionError::Operation)
                        },
                        |_, _| Ok(()),
                        cancelled,
                    )
                },
                None,
            );
            finished.store(true, Ordering::Relaxed);
            result
        });

        saved.expect("a save during the test is not blocked by the preview");
        assert!(fixture.store.load().unwrap().find("saved").is_some());
    }
}
