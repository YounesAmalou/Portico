use std::{
    fs::{self, File},
    io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

use thiserror::Error;

use super::{
    ConfigPaths, ManagedConfig, ManagedError, PORTICO_INCLUDE,
    render::{quote, quote_path},
    store::{ManagedStore, interpret_rejection},
};
use crate::domain::config_error::ConfigRejection;
use crate::domain::discovery::{ConfigDirective, parse_directive_bytes, split_comment};
use crate::platform::fs::{
    acquire_lock, create_private_dir, normalize_path, open_lock, symlink_target, sync_dir,
    write_unique,
};

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

impl ManagedStore {
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
                InitError::Validation(interpret_rejection(
                    &stderr,
                    self.paths.home(),
                    &self.paths.managed,
                    None,
                ))
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
            let backup = write_unique(
                &self.paths.ssh_dir,
                "config.portico-backup",
                &entry_contents,
                entry_mode
                    .as_ref()
                    .expect("existing entry has permissions")
                    .mode(),
            )
            .map_err(|source| init_io(&self.paths.entry, source))?;
            let path = backup.keep();
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
        symlink_target(&self.paths.entry).map_err(|(path, source)| init_io(&path, source))
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
}

pub(super) struct EntryAnalysis {
    has_include: bool,
    pub(super) is_normalized: bool,
    normalized: Vec<u8>,
}

pub(super) fn analyze_entry(
    contents: &[u8],
    paths: &ConfigPaths,
) -> Result<EntryAnalysis, InitError> {
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
pub(crate) fn substitute_include(
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
pub(crate) fn line_count(contents: &[u8]) -> u32 {
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

fn init_io(path: &Path, source: io::Error) -> InitError {
    InitError::Io {
        path: path.into(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::domain::managed::tests::initialized_store;
    use crate::platform::fs::{
        fail_directory_sync_after, fail_next_directory_sync, fail_next_transaction_rollback_write,
    };

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
                // `/proc/self/fd` names each descriptor's file; macOS has no such list.
                let descriptors = if cfg!(target_os = "linux") {
                    fs::read_dir("/proc/self/fd").unwrap().flatten().collect()
                } else {
                    Vec::new()
                };
                for descriptor in descriptors {
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
                    home: paths.home().to_owned(),
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
}
