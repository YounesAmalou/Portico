use std::{
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

#[cfg(test)]
use std::sync::Arc;

use thiserror::Error;

use super::{
    ConfigPaths, ManagedConfig, ManagedError,
    init::{InitError, analyze_entry, line_count, substitute_include},
    render::quote_path,
};
use crate::domain::config_error::{ConfigFiles, ConfigRejection, StagedEntry, interpret};
use crate::platform::fs::{
    PendingFile, acquire_lock, mode_of, open_lock, read_lossy, reject_symlink, symlink_target,
    sync_dir, write_unique,
};

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
    pub(super) paths: ConfigPaths,
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

    /// The file a write to the entry config replaces: the entry, or the end of its symlink chain.
    pub(crate) fn entry_file(&self) -> Result<PathBuf, TransactionError> {
        symlink_target(&self.paths.entry).map_err(|(path, source)| transaction_io(&path, source))
    }

    pub(crate) fn ensure_ready(&self) -> Result<(), TransactionError> {
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
                self.paths.home(),
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
        let entry_mode = mode_of(
            &fs::metadata(&self.paths.entry)
                .map_err(|source| transaction_io(&self.paths.entry, source))?,
        );
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
                entry,
                validation_file,
                portico_lines,
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
                entry,
                validation_file,
                ..
            },
        ) = self.stage(mutate, cancelled)?;
        drop(validation_file);
        let (preview_file, portico_lines) =
            self.stage_entry_as(&entry, candidate.path(), ".config.portico-preview")?;
        drop(lock);
        let managed_text = read_lossy(candidate.path());
        let result = use_entry(ConfigFiles {
            home: self.paths.home(),
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

    /// Writes a copy of the entry config `entry` whose Portico Include points at `managed`,
    /// ending with `Match all` and an Include of the system configuration when it exists, as
    /// `preview` hands to OpenSSH. Nothing is activated; the file is removed when dropped.
    pub(crate) fn stage_entry(
        &self,
        entry: &[u8],
        managed: &Path,
    ) -> Result<PendingFile, TransactionError> {
        self.stage_entry_as(entry, managed, ".config.portico-adopt-entry")
            .map(|(file, _)| file)
    }

    /// `stage_entry` under the temporary name `stem`, also returning the lines Portico wrote.
    fn stage_entry_as(
        &self,
        entry: &[u8],
        managed: &Path,
        stem: &str,
    ) -> Result<(PendingFile, Vec<u32>), TransactionError> {
        let (mut contents, mut portico_lines) =
            substitute_include(entry, &self.paths, managed).map_err(transaction_entry_error)?;
        if self.paths.system_config.exists() {
            if !contents.is_empty() && !contents.ends_with(b"\n") {
                contents.push(b'\n');
            }
            let lines = line_count(&contents);
            portico_lines.extend([lines + 1, lines + 2]);
            contents.extend_from_slice(
                format!(
                    "Match all\nInclude {}\n",
                    quote_path(&self.paths.system_config.to_string_lossy())
                )
                .as_bytes(),
            );
        }
        let mode = mode_of(
            &fs::metadata(&self.paths.entry)
                .map_err(|source| transaction_io(&self.paths.entry, source))?,
        );
        let file = write_unique(&self.paths.ssh_dir, stem, &contents, mode)
            .map_err(|source| transaction_io(&self.paths.ssh_dir, source))?;
        Ok((file, portico_lines))
    }

    pub(crate) fn read_managed(&self) -> Result<(Vec<u8>, ManagedConfig), TransactionError> {
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

/// Interprets ssh's stderr against the managed config at `managed` and the entry Portico staged
/// at `entry`, if any, showing files under `home` with `~`.
pub(super) fn interpret_rejection(
    stderr: &str,
    home: &Path,
    managed: &Path,
    entry: Option<StagedEntry<'_>>,
) -> ConfigRejection {
    interpret(
        stderr,
        &ConfigFiles {
            home,
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
    /// The entry config as read while staging.
    entry: Vec<u8>,
    validation_file: PendingFile,
    /// The lines of the validation entry Portico wrote rather than copied from the user's.
    portico_lines: Vec<u32>,
    changed_aliases: Vec<String>,
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

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::Path,
        sync::atomic::{AtomicBool, Ordering},
        thread,
        time::Duration,
    };

    use super::*;
    use crate::domain::host::{Host, HostChanges, HostField};
    use crate::domain::managed::{
        ConfigPaths, ManagedConfig, ManagedStore,
        tests::{InitializedStore, initialized_store},
    };
    use crate::platform::fs::fail_next_directory_sync;
    #[cfg(unix)]
    use crate::platform::fs::fail_next_transaction_rollback_write;

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
                    hostname: crate::domain::host::Change::Set("b.example".into()),
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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
