use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use fs2::FileExt;
use thiserror::Error;

use crate::discovery::{ConfigDirective, parse_directive};
use crate::host::{Host, HostError};

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
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ManagedConfig {
    hosts: Vec<Host>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigPaths {
    pub ssh_dir: PathBuf,
    pub entry: PathBuf,
    pub portico_dir: PathBuf,
    pub managed: PathBuf,
    pub backup: PathBuf,
    pub lock: PathBuf,
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
    #[error("{path} is a symbolic link; add `{directive}` to its target and rerun initialization")]
    SymlinkRequiresManualSetup {
        path: PathBuf,
        directive: &'static str,
    },
    #[error("could not parse entry config: {0}")]
    EntryParse(String),
    #[error("managed config is invalid: {0}")]
    Managed(#[from] ManagedError),
    #[error("configuration validation failed: {0}")]
    Validation(String),
    #[error("filesystem error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[derive(Debug, Error)]
pub enum TransactionError {
    #[error("managed config is invalid; recover from {backup}: {source}")]
    Managed {
        backup: PathBuf,
        #[source]
        source: ManagedError,
    },
    #[error("configuration validation failed: {0}")]
    Validation(String),
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
}

impl ManagedStore {
    pub fn new(paths: ConfigPaths) -> Self {
        Self { paths }
    }

    pub fn paths(&self) -> &ConfigPaths {
        &self.paths
    }

    pub fn load(&self) -> Result<ManagedConfig, TransactionError> {
        self.read_managed().map(|(_, config)| config)
    }

    pub fn plan_init(&self) -> Result<InitPlan, InitError> {
        let entry_kind = match fs::symlink_metadata(&self.paths.entry) {
            Ok(metadata) => Some(metadata.file_type()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(source) => return Err(init_io(&self.paths.entry, source)),
        };
        let contents = match entry_kind {
            Some(_) => fs::read_to_string(&self.paths.entry)
                .map_err(|source| init_io(&self.paths.entry, source))?,
            None => String::new(),
        };
        let entry = analyze_entry(&contents, &self.paths)?;
        if entry_kind.as_ref().is_some_and(|kind| kind.is_symlink()) && !entry.is_normalized {
            return Err(InitError::SymlinkRequiresManualSetup {
                path: self.paths.entry.clone(),
                directive: PORTICO_INCLUDE,
            });
        }
        if entry_kind
            .as_ref()
            .is_some_and(|kind| !kind.is_file() && !kind.is_symlink())
        {
            return Err(init_io(
                &self.paths.entry,
                io::Error::new(io::ErrorKind::InvalidInput, "entry config is not a file"),
            ));
        }
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
        let entry_writable = !entry_kind.as_ref().is_some_and(|kind| kind.is_symlink());
        Ok(InitPlan {
            create_entry,
            create_managed: managed_mode.is_none(),
            insert_include: entry_writable && !entry.has_include,
            reposition_include: entry_writable && entry.has_include && !entry.is_normalized,
            backup_entry: entry_writable && !create_entry && !entry.is_normalized,
            fix_portico_permissions: portico_mode.is_some_and(|mode| mode != 0o700),
            fix_managed_permissions: managed_mode.is_some_and(|mode| mode != 0o600),
        })
    }

    pub fn apply_init<V>(&self, approved: bool, validate: V) -> Result<InitOutcome, InitError>
    where
        V: Fn(&Path, &[String]) -> Result<(), String>,
    {
        let preview = self.plan_init()?;
        if !preview.requires_confirmation() {
            return Ok(InitOutcome::Unchanged);
        }
        if !approved {
            return Err(InitError::ConfirmationRequired);
        }

        create_private_dir(&self.paths.ssh_dir)
            .map_err(|source| init_io(&self.paths.ssh_dir, source))?;
        create_private_dir(&self.paths.portico_dir)
            .map_err(|source| init_io(&self.paths.portico_dir, source))?;
        if preview.fix_portico_permissions {
            fs::set_permissions(&self.paths.portico_dir, fs::Permissions::from_mode(0o700))
                .map_err(|source| init_io(&self.paths.portico_dir, source))?;
            sync_dir(&self.paths.portico_dir)
                .map_err(|source| init_io(&self.paths.portico_dir, source))?;
        }
        let lock =
            open_lock(&self.paths.lock).map_err(|source| init_io(&self.paths.lock, source))?;
        FileExt::lock_exclusive(&lock).map_err(|source| init_io(&self.paths.lock, source))?;

        let plan = self.plan_init()?;
        if !plan.requires_confirmation() {
            return Ok(if preview.fix_portico_permissions {
                InitOutcome::Initialized
            } else {
                InitOutcome::Unchanged
            });
        }
        if plan.fix_managed_permissions {
            fs::set_permissions(&self.paths.managed, fs::Permissions::from_mode(0o600))
                .map_err(|source| init_io(&self.paths.managed, source))?;
            File::open(&self.paths.managed)
                .and_then(|file| file.sync_all())
                .map_err(|source| init_io(&self.paths.managed, source))?;
        }

        let entry_contents = if plan.create_entry {
            String::new()
        } else {
            fs::read_to_string(&self.paths.entry)
                .map_err(|source| init_io(&self.paths.entry, source))?
        };
        let entry_metadata = if plan.create_entry {
            None
        } else {
            Some(
                fs::metadata(&self.paths.entry)
                    .map_err(|source| init_io(&self.paths.entry, source))?,
            )
        };
        let final_entry = if plan.create_entry || plan.insert_include || plan.reposition_include {
            analyze_entry(&entry_contents, &self.paths)?.normalized
        } else {
            entry_contents.clone()
        };

        let managed_config = if self.paths.managed.exists() {
            let contents = fs::read_to_string(&self.paths.managed)
                .map_err(|source| init_io(&self.paths.managed, source))?;
            ManagedConfig::parse(&contents)?
        } else {
            ManagedConfig::empty()
        };
        let managed_candidate = if plan.create_managed {
            Some(
                write_unique(
                    &self.paths.portico_dir,
                    ".config.init",
                    managed_config.render().as_bytes(),
                    0o600,
                )
                .map_err(|source| init_io(&self.paths.portico_dir, source))?,
            )
        } else {
            None
        };
        let validation_target = managed_candidate
            .as_ref()
            .map_or(self.paths.managed.as_path(), |candidate| candidate.path());
        let validation_entry = substitute_include(&final_entry, &self.paths, validation_target)?;
        let validation_file = write_unique(
            &self.paths.ssh_dir,
            ".config.portico-validation",
            validation_entry.as_bytes(),
            entry_metadata
                .as_ref()
                .map_or(0o600, |metadata| metadata.permissions().mode()),
        )
        .map_err(|source| init_io(&self.paths.ssh_dir, source))?;
        let aliases = managed_config
            .hosts()
            .iter()
            .map(|host| host.alias().to_owned())
            .collect::<Vec<_>>();
        validate(validation_file.path(), &aliases).map_err(InitError::Validation)?;
        drop(validation_file);

        if plan.backup_entry {
            let metadata = entry_metadata.as_ref().ok_or_else(|| {
                init_io(
                    &self.paths.entry,
                    io::Error::other("entry metadata disappeared during initialization"),
                )
            })?;
            self.back_up_entry(&entry_contents, metadata)?;
        }

        let entry_candidate = if plan.create_entry || plan.insert_include || plan.reposition_include
        {
            Some(
                write_unique(
                    &self.paths.ssh_dir,
                    ".config.portico-init",
                    final_entry.as_bytes(),
                    entry_metadata
                        .as_ref()
                        .map_or(0o600, |metadata| metadata.permissions().mode()),
                )
                .map_err(|source| init_io(&self.paths.ssh_dir, source))?,
            )
        } else {
            None
        };

        let created_managed = managed_candidate.is_some();
        if let Some(candidate) = managed_candidate {
            candidate
                .rename_to(&self.paths.managed)
                .map_err(|source| init_io(&self.paths.managed, source))?;
            if let Err(source) = sync_dir(&self.paths.portico_dir) {
                let _ = fs::remove_file(&self.paths.managed);
                let _ = sync_dir(&self.paths.portico_dir);
                return Err(init_io(&self.paths.portico_dir, source));
            }
        }
        let mut entry_activated = false;
        if let Some(candidate) = entry_candidate {
            if let Err(source) = candidate.rename_to(&self.paths.entry) {
                if created_managed {
                    let _ = fs::remove_file(&self.paths.managed);
                    let _ = sync_dir(&self.paths.portico_dir);
                }
                return Err(init_io(&self.paths.entry, source));
            }
            entry_activated = true;
        }
        if let Err(source) = sync_dir(&self.paths.ssh_dir) {
            self.rollback_init(
                &entry_contents,
                entry_metadata.as_ref(),
                entry_activated,
                created_managed,
            );
            return Err(init_io(&self.paths.ssh_dir, source));
        }
        Ok(InitOutcome::Initialized)
    }

    fn back_up_entry(&self, contents: &str, metadata: &fs::Metadata) -> Result<(), InitError> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| {
                init_io(
                    &self.paths.entry,
                    io::Error::other(format!("system clock before Unix epoch: {error}")),
                )
            })?
            .as_secs();
        let backup = self.paths.ssh_dir.join(format!(
            "config.portico-backup-{timestamp}-{}",
            std::process::id()
        ));
        write_named_new(&backup, contents.as_bytes(), metadata.permissions().mode())
            .map_err(|source| init_io(&backup, source))?;
        sync_dir(&self.paths.ssh_dir).map_err(|source| init_io(&self.paths.ssh_dir, source))
    }

    fn rollback_init(
        &self,
        entry_contents: &str,
        entry_metadata: Option<&fs::Metadata>,
        entry_activated: bool,
        created_managed: bool,
    ) {
        if entry_activated {
            if let Some(metadata) = entry_metadata {
                if let Ok(candidate) = write_unique(
                    &self.paths.ssh_dir,
                    ".config.portico-rollback",
                    entry_contents.as_bytes(),
                    metadata.permissions().mode(),
                ) {
                    let _ = candidate.rename_to(&self.paths.entry);
                }
            } else {
                let _ = fs::remove_file(&self.paths.entry);
            }
        }
        if created_managed {
            let _ = fs::remove_file(&self.paths.managed);
            let _ = sync_dir(&self.paths.portico_dir);
        }
        let _ = sync_dir(&self.paths.ssh_dir);
    }

    pub fn transact<R, M, V>(&self, mutate: M, validate: V) -> Result<R, TransactionError>
    where
        M: FnOnce(&mut ManagedConfig) -> Result<R, ManagedError>,
        V: Fn(&Path, &[String]) -> Result<(), String>,
    {
        let lock = open_lock(&self.paths.lock)
            .map_err(|source| transaction_io(&self.paths.lock, source))?;
        FileExt::lock_exclusive(&lock)
            .map_err(|source| transaction_io(&self.paths.lock, source))?;

        let (active, mut config) = self.read_managed()?;
        let result = mutate(&mut config).map_err(|source| TransactionError::Managed {
            backup: self.paths.backup.clone(),
            source,
        })?;
        let candidate = write_unique(
            &self.paths.portico_dir,
            ".config.portico-candidate",
            config.render().as_bytes(),
            0o600,
        )
        .map_err(|source| transaction_io(&self.paths.portico_dir, source))?;

        let entry = fs::read_to_string(&self.paths.entry)
            .map_err(|source| transaction_io(&self.paths.entry, source))?;
        let validation_entry = substitute_include(&entry, &self.paths, candidate.path())
            .map_err(transaction_entry_error)?;
        let entry_mode = fs::metadata(&self.paths.entry)
            .map_err(|source| transaction_io(&self.paths.entry, source))?
            .permissions()
            .mode();
        let validation_file = write_unique(
            &self.paths.ssh_dir,
            ".config.portico-validation",
            validation_entry.as_bytes(),
            entry_mode,
        )
        .map_err(|source| transaction_io(&self.paths.ssh_dir, source))?;
        let aliases = config
            .hosts()
            .iter()
            .map(|host| host.alias().to_owned())
            .collect::<Vec<_>>();
        validate(validation_file.path(), &aliases).map_err(TransactionError::Validation)?;
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
            .and_then(|candidate| candidate.rename_to(&self.paths.managed));
            match rollback {
                Ok(()) => {
                    let _ = sync_dir(&self.paths.portico_dir);
                    return Err(transaction_io(&self.paths.portico_dir, source));
                }
                Err(rollback) => {
                    return Err(TransactionError::Rollback {
                        path: self.paths.managed.clone(),
                        recovery: self.paths.backup.clone(),
                        sync: source,
                        rollback,
                    });
                }
            }
        }
        Ok(result)
    }

    fn read_managed(&self) -> Result<(Vec<u8>, ManagedConfig), TransactionError> {
        reject_symlink(&self.paths.managed)
            .map_err(|source| transaction_io(&self.paths.managed, source))?;
        let contents = fs::read(&self.paths.managed)
            .map_err(|source| transaction_io(&self.paths.managed, source))?;
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

        for (index, raw_line) in input.lines().enumerate() {
            let line = index + 1;
            let trimmed = raw_line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            let (directive, argument) = split_directive(trimmed);
            if directive.eq_ignore_ascii_case("Host") {
                if let Some(host) = current.take() {
                    config.push_parsed(host)?;
                }
                let alias = parse_value(argument, line)?;
                current = Some(ParsedHost::new(alias, line));
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
            let value = parse_value(argument, line)?;
            if directive.eq_ignore_ascii_case("HostName") {
                set_once(&mut host.hostname, value, line, directive)?;
            } else if directive.eq_ignore_ascii_case("User") {
                set_once(&mut host.user, value, line, directive)?;
            } else if directive.eq_ignore_ascii_case("Port") {
                let port = value.parse().map_err(|_| ManagedError::Parse {
                    line,
                    message: format!("invalid port: {value}"),
                })?;
                set_once(&mut host.port, port, line, directive)?;
            } else if directive.eq_ignore_ascii_case("IdentityFile") {
                set_once(&mut host.identity_file, value, line, directive)?;
            } else if directive.eq_ignore_ascii_case("ProxyJump") {
                set_once(&mut host.proxy_jump, value, line, directive)?;
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
        let blocks = hosts.into_iter().map(render_host).collect::<Vec<_>>();
        if blocks.is_empty() {
            format!("{HEADER}\n")
        } else {
            format!("{HEADER}\n\n{}\n", blocks.join("\n\n"))
        }
    }

    pub fn hosts(&self) -> &[Host] {
        &self.hosts
    }

    pub fn find(&self, alias: &str) -> Option<&Host> {
        self.hosts
            .iter()
            .find(|host| host.alias().eq_ignore_ascii_case(alias))
    }

    pub fn insert(&mut self, host: Host) -> Result<(), ManagedError> {
        if self.find(host.alias()).is_some() {
            return Err(ManagedError::DuplicateAlias(host.alias().into()));
        }
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
        self.hosts[index] = host;
        Ok(())
    }

    pub fn remove(&mut self, alias: &str) -> Result<Host, ManagedError> {
        let index = self
            .hosts
            .iter()
            .position(|host| host.alias().eq_ignore_ascii_case(alias))
            .ok_or_else(|| ManagedError::UnknownAlias(alias.into()))?;
        Ok(self.hosts.remove(index))
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
                HostError::InvalidAlias(_) => line,
                HostError::InvalidHostname(_) => hostname_line,
                HostError::InvalidUser(_) => user_line,
                HostError::InvalidPort(_) => port_line,
                HostError::InvalidIdentity(_) => identity_line,
                HostError::InvalidJump(_) => jump_line,
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
        self.hosts.push(host);
        self.sort_hosts();
        Ok(())
    }

    fn sort_hosts(&mut self) {
        self.hosts
            .sort_by_key(|host| host.alias().to_ascii_lowercase());
    }
}

struct EntryAnalysis {
    has_include: bool,
    is_normalized: bool,
    normalized: String,
}

fn analyze_entry(contents: &str, paths: &ConfigPaths) -> Result<EntryAnalysis, InitError> {
    let mut lines = contents.lines().map(str::to_owned).collect::<Vec<_>>();
    let trailing_newline = contents.ends_with('\n') || contents.is_empty();
    let mut matches = Vec::new();
    let mut first_active = None;

    for (index, line) in lines.iter().enumerate() {
        let directive =
            parse_directive(line).map_err(|error| InitError::EntryParse(error.to_string()))?;
        if directive.is_some() && first_active.is_none() {
            first_active = Some(index);
        }
        if let Some(ConfigDirective::Include(arguments)) = directive {
            let count = arguments
                .iter()
                .filter(|argument| is_managed_include(argument, paths))
                .count();
            matches.extend(std::iter::repeat_n(index, count));
        }
    }

    let already_normalized = matches.len() == 1
        && first_active == matches.first().copied()
        && matches.first().is_some_and(|index| {
            matches!(
                parse_directive(&lines[*index]),
                Ok(Some(ConfigDirective::Include(arguments)))
                    if arguments.len() == 1 && is_managed_include(&arguments[0], paths)
            )
        });
    if already_normalized {
        return Ok(EntryAnalysis {
            has_include: true,
            is_normalized: true,
            normalized: contents.to_owned(),
        });
    }

    let mut retained = Vec::with_capacity(lines.len() + 1);
    for line in lines.drain(..) {
        match parse_directive(&line).map_err(|error| InitError::EntryParse(error.to_string()))? {
            Some(ConfigDirective::Include(arguments)) => {
                if !arguments
                    .iter()
                    .any(|argument| is_managed_include(argument, paths))
                {
                    retained.push(line);
                    continue;
                }
                let remaining = arguments
                    .into_iter()
                    .filter(|argument| !is_managed_include(argument, paths))
                    .collect::<Vec<_>>();
                if !remaining.is_empty() {
                    retained.push(format!(
                        "Include {}",
                        remaining
                            .iter()
                            .map(|argument| quote(argument))
                            .collect::<Vec<_>>()
                            .join(" ")
                    ));
                }
            }
            _ => retained.push(line),
        }
    }
    let insertion = retained
        .iter()
        .position(|line| parse_directive(line).is_ok_and(|directive| directive.is_some()))
        .unwrap_or(retained.len());
    retained.insert(insertion, PORTICO_INCLUDE.to_owned());
    let mut normalized = retained.join("\n");
    if trailing_newline {
        normalized.push('\n');
    }

    Ok(EntryAnalysis {
        has_include: !matches.is_empty(),
        is_normalized: false,
        normalized,
    })
}

fn substitute_include(
    contents: &str,
    paths: &ConfigPaths,
    target: &Path,
) -> Result<String, InitError> {
    let mut replaced = false;
    let mut lines = Vec::new();
    for line in contents.lines() {
        match parse_directive(line).map_err(|error| InitError::EntryParse(error.to_string()))? {
            Some(ConfigDirective::Include(arguments))
                if arguments
                    .iter()
                    .any(|argument| is_managed_include(argument, paths)) =>
            {
                replaced = true;
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
                lines.push(format!("Include {}", arguments.join(" ")));
            }
            _ => lines.push(line.to_owned()),
        }
    }
    if !replaced {
        return Err(InitError::EntryParse(
            "Portico Include is missing from candidate entry config".into(),
        ));
    }
    let mut result = lines.join("\n");
    if contents.ends_with('\n') {
        result.push('\n');
    }
    Ok(result)
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

struct PendingFile {
    path: Option<PathBuf>,
}

impl PendingFile {
    fn path(&self) -> &Path {
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

fn write_unique(dir: &Path, stem: &str, contents: &[u8], mode: u32) -> io::Result<PendingFile> {
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
    File::open(path)?.sync_all()
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
}

impl ParsedHost {
    fn new(alias: String, line: usize) -> Self {
        Self {
            alias,
            line,
            hostname: None,
            user: None,
            port: None,
            identity_file: None,
            proxy_jump: None,
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
    if input.len() < 2 || !input.starts_with('"') || !input.ends_with('"') {
        if input.chars().any(char::is_whitespace) {
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

fn render_host(host: &Host) -> String {
    let mut lines = vec![
        format!("Host {}", quote(host.alias())),
        format!("    HostName {}", quote(host.hostname())),
    ];
    if let Some(user) = host.user() {
        lines.push(format!("    User {}", quote(user)));
    }
    if let Some(port) = host.port() {
        lines.push(format!("    Port {port}"));
    }
    if let Some(identity_file) = host.identity_file() {
        lines.push(format!("    IdentityFile {}", quote(identity_file)));
    }
    if let Some(proxy_jump) = host.proxy_jump() {
        lines.push(format!("    ProxyJump {}", quote(proxy_jump)));
    }
    lines.join("\n")
}

fn quote(value: &str) -> String {
    if value.chars().any(char::is_whitespace) {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
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

        let substituted = substitute_include(entry, &paths, &candidate).unwrap();

        assert_eq!(
            substituted,
            format!(
                "Include \"{}\" first.conf\nHost unrelated\nInclude second.conf \"{}\" third.conf\n",
                candidate.display(),
                candidate.display()
            )
        );
    }

    #[test]
    fn symlinked_entry_requires_manual_setup() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::from_home(dir.path());
        std::fs::create_dir_all(&paths.ssh_dir).unwrap();
        let target = dir.path().join("dotfiles-ssh-config");
        std::fs::write(&target, "Host external\n  HostName external.example\n").unwrap();
        symlink(&target, &paths.entry).unwrap();

        let error = ManagedStore::new(paths).plan_init().unwrap_err();
        assert!(matches!(
            error,
            InitError::SymlinkRequiresManualSetup { .. }
        ));
    }

    #[test]
    fn conditional_include_in_symlink_requires_manual_setup() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::from_home(dir.path());
        std::fs::create_dir_all(&paths.ssh_dir).unwrap();
        let target = dir.path().join("dotfiles-ssh-config");
        std::fs::write(&target, "Host unrelated\n  Include ~/.ssh/portico/config\n").unwrap();
        symlink(&target, &paths.entry).unwrap();

        let error = ManagedStore::new(paths).plan_init().unwrap_err();

        assert!(matches!(
            error,
            InitError::SymlinkRequiresManualSetup { .. }
        ));
    }

    #[test]
    fn managed_only_init_rollback_preserves_entry_symlink() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::from_home(dir.path());
        std::fs::create_dir_all(&paths.ssh_dir).unwrap();
        let target = dir.path().join("dotfiles-ssh-config");
        std::fs::write(&target, "Include ~/.ssh/portico/config\n").unwrap();
        symlink(&target, &paths.entry).unwrap();
        std::fs::set_permissions(&paths.ssh_dir, std::fs::Permissions::from_mode(0o300)).unwrap();

        let result = ManagedStore::new(paths.clone()).apply_init(true, |_, _| Ok(()));

        std::fs::set_permissions(&paths.ssh_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(result, Err(InitError::Io { .. })));
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
            |config| config.insert(host),
            |_, _| Err("invalid candidate".into()),
        );

        assert!(matches!(result, Err(TransactionError::Validation(_))));
        assert_eq!(std::fs::read(&fixture.paths.managed).unwrap(), before);
    }

    #[test]
    fn successful_mutation_keeps_exact_previous_version() {
        let fixture = initialized_store();
        let first = Host::new("one", "one.example", None, None, None, None).unwrap();
        fixture
            .store
            .transact(|config| config.insert(first), |_, _| Ok(()))
            .unwrap();
        let before_second = std::fs::read(&fixture.paths.managed).unwrap();
        let second = Host::new("two", "two.example", None, None, None, None).unwrap();
        fixture
            .store
            .transact(|config| config.insert(second), |_, _| Ok(()))
            .unwrap();

        assert_eq!(std::fs::read(&fixture.paths.backup).unwrap(), before_second);
    }

    #[test]
    fn post_rename_sync_failure_restores_active_bytes() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = initialized_store();
        let before = std::fs::read(&fixture.paths.managed).unwrap();
        let host = Host::new("dev", "dev.example", None, None, None, None).unwrap();
        std::fs::set_permissions(
            &fixture.paths.portico_dir,
            std::fs::Permissions::from_mode(0o300),
        )
        .unwrap();

        let result = fixture
            .store
            .transact(|config| config.insert(host), |_, _| Ok(()));

        std::fs::set_permissions(
            &fixture.paths.portico_dir,
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
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
            .transact(|config| config.insert(host), |_, _| Ok(()))
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
}
