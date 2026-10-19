use std::{
    io,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};

use thiserror::Error;

mod containers;
mod identities;

pub(crate) use identities::key_files_in;
mod tunnels;

pub use containers::{
    ContainerError, ContainerList, ContainerPrograms, DockerIssue, Offer, Target,
};
pub use tunnels::{StartOutcome, StopOutcome, TunnelChanges, TunnelRefusal};

use crate::{
    adopt::{AdoptError, Direction, MigrationPlan},
    config_error::{ConfigFiles, ConfigRejection, interpret},
    discovery::{Discovery, DiscoveryError, DiscoveryWarning, discover},
    explain::{Context, Explanation, Operation, explain},
    host::{Change, Host, HostChanges, HostError},
    identity::HostIdentities,
    managed::{
        InitError, InitOutcome, InitPlan, ManagedConfig, ManagedStore, TransactionError, read_lossy,
    },
    openssh::{
        ConnectionOutcome, KeyFetch, OpenSsh, OpenSshError, PresentedKey, ResolvedConfig,
        append_known_hosts,
    },
    probe::{Diagnosis, Failure, ProbeResult, diagnose, known_hosts_path},
    tunnel::{
        TunnelError,
        runtime::{Proc, RuntimeDir},
        store::{StoreError, TunnelStore},
    },
};

pub struct App {
    store: ManagedStore,
    openssh: OpenSsh,
    container_programs: ContainerPrograms,
    identities: identities::IdentityState,
    tunnels: TunnelStore,
    runtime: RuntimeDir,
    proc: Proc,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Ownership {
    Managed,
    External,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostSummary {
    pub alias: String,
    pub ownership: Ownership,
    pub source: PathBuf,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HostInventory {
    pub hosts: Vec<HostSummary>,
    pub warnings: Vec<DiscoveryWarning>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostDetails {
    pub summary: HostSummary,
    pub declared: Option<Host>,
    pub resolution: Result<ResolvedConfig, String>,
    /// The host's identities and whether each is available; absent when resolution failed.
    pub identities: Option<HostIdentities>,
    pub warnings: Vec<DiscoveryWarning>,
}

/// What a connection test runs against: a saved host as OpenSSH resolves it,
/// or the values of an add or edit form, staged but not saved.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TestTarget {
    Saved(String),
    Add(Host),
    Edit { alias: String, changes: HostChanges },
}

impl TestTarget {
    /// The alias the test connects to.
    pub fn alias(&self) -> &str {
        match self {
            TestTarget::Saved(alias) => alias,
            TestTarget::Add(host) => host.alias(),
            TestTarget::Edit { alias, changes } => match &changes.alias {
                Change::Set(new) => new,
                Change::Keep | Change::Clear => alias,
            },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TestReport {
    pub alias: String,
    pub user: Option<String>,
    pub result: ProbeResult,
    /// Why the server's key could not be fetched after an untrusted-key failure.
    pub key_fetch_failure: Option<Failure>,
    /// What went wrong in plain words; `Some` exactly when the result is `Failed`.
    pub explanation: Option<Explanation>,
}

impl TestReport {
    /// Records why the server's key could not be fetched, as a further next step.
    fn fail_key_fetch(&mut self, failure: Failure) {
        if let Some(explanation) = &mut self.explanation {
            explanation
                .next
                .push(format!("could not fetch the host key: {failure}"));
        }
        self.key_fetch_failure = Some(failure);
    }

    pub fn passed(&self) -> bool {
        matches!(self.result, ProbeResult::Authenticated(_))
    }

    /// The outcome in one line, without the alias: `authenticated as user@address:port (method)`,
    /// with `via <jump>` before the method through a jump host or proxy, or `failed — reason`.
    pub fn summary(&self) -> String {
        match &self.result {
            ProbeResult::Authenticated(success) => {
                let (address, port, method) = (&success.address, success.port, &success.method);
                let via = success
                    .via
                    .as_ref()
                    .map_or(String::new(), |via| format!(" via {via}"));
                match &self.user {
                    Some(user) => {
                        format!("authenticated as {user}@{address}:{port}{via} ({method})")
                    }
                    None => format!("authenticated to {address}:{port}{via} ({method})"),
                }
            }
            ProbeResult::Failed(failure) => format!("failed \u{2014} {failure}"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TestOutcome {
    Done(TestReport),
    UnknownHostKey {
        report: TestReport,
        key: PresentedKey,
    },
}

#[derive(Debug, Error)]
pub enum AppError {
    #[error(transparent)]
    Init(#[from] InitError),
    #[error(transparent)]
    Transaction(#[from] TransactionError),
    #[error(transparent)]
    Adopt(#[from] AdoptError),
    #[error(transparent)]
    Discovery(#[from] DiscoveryError),
    #[error(transparent)]
    OpenSsh(#[from] OpenSshError),
    #[error(transparent)]
    InvalidHost(#[from] HostError),
    #[error("external host is read-only: {0}")]
    ReadOnlyExternal(String),
    #[error("unknown host: {0}")]
    UnknownHost(String),
    #[error("alias already exists: {0}")]
    AliasCollision(String),
    #[error("host inventory is incomplete: {0:?}")]
    IncompleteDiscovery(Vec<DiscoveryWarning>),
    #[error("cannot update known hosts file {path}: {reason}")]
    KnownHosts { path: PathBuf, reason: String },
    #[error(transparent)]
    TunnelStore(#[from] StoreError),
    #[error(transparent)]
    InvalidTunnel(#[from] TunnelError),
    #[error(transparent)]
    Tunnel(#[from] TunnelRefusal),
}

impl App {
    pub fn new(store: ManagedStore, openssh: OpenSsh) -> Self {
        let portico_dir = store.paths().portico_dir.clone();
        let home = portico_dir
            .parent()
            .and_then(Path::parent)
            .unwrap_or(&portico_dir)
            .to_path_buf();
        Self {
            store,
            openssh,
            container_programs: ContainerPrograms::default(),
            identities: identities::IdentityState::default(),
            tunnels: TunnelStore::new(&portico_dir),
            // Without `with_runtime_dir`, records stay under the home: tests never reach the
            // user's real runtime directory.
            runtime: RuntimeDir::locate(&home, None),
            proc: Proc::system(),
        }
    }

    /// Replaces the directory of started tunnels' records and logs.
    pub fn with_runtime_dir(mut self, runtime: RuntimeDir) -> Self {
        self.runtime = runtime;
        self
    }

    /// Replaces the programs container commands run on this machine.
    pub fn with_container_programs(mut self, programs: ContainerPrograms) -> Self {
        self.container_programs = programs;
        self
    }

    pub fn init_plan(&self) -> Result<InitPlan, AppError> {
        Ok(self.store.plan_init()?)
    }

    pub fn initialize(&self, approved: bool) -> Result<InitOutcome, AppError> {
        self.initialize_inner(approved, None)
    }

    pub(crate) fn initialize_cancellable(
        &self,
        approved: bool,
        cancelled: &AtomicBool,
    ) -> Result<InitOutcome, AppError> {
        self.initialize_inner(approved, Some(cancelled))
    }

    fn initialize_inner(
        &self,
        approved: bool,
        cancelled: Option<&AtomicBool>,
    ) -> Result<InitOutcome, AppError> {
        Ok(match cancelled {
            Some(cancelled) => {
                self.store
                    .apply_init_cancellable(approved, cancelled, |entry, aliases| {
                        self.openssh
                            .validate_cancellable(entry, aliases, cancelled)
                            .map_err(stderr_of)
                    })
            }
            None => self.store.apply_init(approved, |entry, aliases| {
                self.openssh.validate(entry, aliases).map_err(stderr_of)
            }),
        }?)
    }

    pub fn hosts(&self) -> Result<HostInventory, AppError> {
        let managed = self.store.load()?;
        let external = self.discover()?;
        let mut hosts = managed
            .hosts()
            .iter()
            .map(|host| HostSummary {
                alias: host.alias().into(),
                ownership: Ownership::Managed,
                source: self.store.paths().managed.clone(),
            })
            .chain(external.hosts.into_iter().map(|host| HostSummary {
                alias: host.alias,
                ownership: Ownership::External,
                source: host.source,
            }))
            .collect::<Vec<_>>();
        hosts.sort_by_key(|host| host.alias.to_ascii_lowercase());
        Ok(HostInventory {
            hosts,
            warnings: external.warnings,
        })
    }

    pub fn show(&self, alias: &str) -> Result<HostDetails, AppError> {
        self.show_inner(alias, None)
    }

    pub(crate) fn show_cancellable(
        &self,
        alias: &str,
        cancelled: &AtomicBool,
    ) -> Result<HostDetails, AppError> {
        self.show_inner(alias, Some(cancelled))
    }

    fn show_inner(
        &self,
        alias: &str,
        cancelled: Option<&AtomicBool>,
    ) -> Result<HostDetails, AppError> {
        let managed = self.store.load()?;
        let discovery = self.discover()?;
        let (summary, declared) = if let Some(host) = managed.find(alias) {
            (
                HostSummary {
                    alias: host.alias().into(),
                    ownership: Ownership::Managed,
                    source: self.store.paths().managed.clone(),
                },
                Some(host.clone()),
            )
        } else if let Some(host) = discovery
            .hosts
            .iter()
            .find(|host| host.alias.eq_ignore_ascii_case(alias))
        {
            (
                HostSummary {
                    alias: host.alias.clone(),
                    ownership: Ownership::External,
                    source: host.source.clone(),
                },
                None,
            )
        } else {
            return Err(AppError::UnknownHost(alias.into()));
        };
        let resolution = match cancelled {
            Some(cancelled) => self.openssh.resolve_cancellable(&summary.alias, cancelled),
            None => self.openssh.resolve(&summary.alias),
        }
        .map_err(|error| error.to_string());
        let identities = resolution
            .as_ref()
            .ok()
            .map(|resolved| self.host_identities(&summary.alias, resolved));
        Ok(HostDetails {
            summary,
            declared,
            resolution,
            identities,
            warnings: discovery.warnings,
        })
    }

    pub fn add(&self, host: Host) -> Result<(), AppError> {
        self.add_inner(host, None)
    }

    pub(crate) fn add_cancellable(
        &self,
        host: Host,
        cancelled: &AtomicBool,
    ) -> Result<(), AppError> {
        self.add_inner(host, Some(cancelled))
    }

    fn add_inner(&self, host: Host, cancelled: Option<&AtomicBool>) -> Result<(), AppError> {
        self.transact(|config| self.add_mutation(config, host), cancelled)
    }

    pub fn edit(&self, alias: &str, changes: HostChanges) -> Result<(), AppError> {
        self.edit_inner(alias, changes, None)
    }

    pub(crate) fn edit_cancellable(
        &self,
        alias: &str,
        changes: HostChanges,
        cancelled: &AtomicBool,
    ) -> Result<(), AppError> {
        self.edit_inner(alias, changes, Some(cancelled))
    }

    fn edit_inner(
        &self,
        alias: &str,
        changes: HostChanges,
        cancelled: Option<&AtomicBool>,
    ) -> Result<(), AppError> {
        self.transact(
            |config| self.edit_mutation(config, alias, changes),
            cancelled,
        )
    }

    pub fn remove(&self, alias: &str) -> Result<(), AppError> {
        self.remove_inner(alias, None)
    }

    pub(crate) fn remove_cancellable(
        &self,
        alias: &str,
        cancelled: &AtomicBool,
    ) -> Result<(), AppError> {
        self.remove_inner(alias, Some(cancelled))
    }

    fn remove_inner(&self, alias: &str, cancelled: Option<&AtomicBool>) -> Result<(), AppError> {
        self.transact(
            |config| {
                if config.find(alias).is_none() {
                    return Err(self.missing_host_error(alias)?);
                }
                config.remove(alias).map_err(TransactionError::Operation)?;
                Ok(())
            },
            cancelled,
        )
    }

    /// Connects to `alias`, in this terminal or a new one. In this terminal ssh's stderr is
    /// mirrored to Portico's, and a failure Portico recognises is explained.
    pub fn connect(&self, alias: &str, new_terminal: bool) -> Result<ConnectionOutcome, AppError> {
        let alias = self.connectable_alias(alias)?;
        if new_terminal {
            return Ok(self.openssh.launch_in_new_terminal(&alias)?);
        }
        self.connect_here(&alias, None, false)
    }

    /// Connects from the TUI, whose explanations name its keys rather than commands.
    pub(crate) fn connect_cancellable(
        &self,
        alias: &str,
        cancelled: &AtomicBool,
    ) -> Result<ConnectionOutcome, AppError> {
        let alias = self.connectable_alias(alias)?;
        self.connect_here(&alias, Some(cancelled), true)
    }

    /// The alias as the inventory spells it.
    fn connectable_alias(&self, alias: &str) -> Result<String, AppError> {
        Ok(self
            .hosts()?
            .hosts
            .into_iter()
            .find(|host| host.alias.eq_ignore_ascii_case(alias))
            .ok_or_else(|| AppError::UnknownHost(alias.into()))?
            .alias)
    }

    fn connect_here(
        &self,
        alias: &str,
        cancelled: Option<&AtomicBool>,
        in_tui: bool,
    ) -> Result<ConnectionOutcome, AppError> {
        let mirrored = self
            .openssh
            .connect_mirrored(alias, Box::new(io::stderr()), cancelled)?;
        // Only 255 is ssh's own failure; any other code is the remote command's or a logout.
        let explanation = if mirrored.status.code() == Some(255) {
            self.explain_connection(alias, &mirrored.tail, in_tui)
        } else {
            None
        };
        Ok(ConnectionOutcome::Exited {
            alias: alias.into(),
            status: mirrored.status,
            explanation,
        })
    }

    /// Explains a failed connection from the end of what ssh wrote to stderr, or `None` when
    /// the cause is not recognised: ssh's own output is then already on screen.
    fn explain_connection(&self, alias: &str, tail: &[u8], in_tui: bool) -> Option<Explanation> {
        match self.openssh.resolve_with(None, alias, None) {
            Ok(resolved) => {
                let diagnosis = diagnose(&String::from_utf8_lossy(tail), false, resolved.route());
                if matches!(diagnosis.failure, Failure::Other(_)) {
                    return None;
                }
                Some(self.explain_failure(diagnosis, alias, &resolved, Operation::Connect, in_tui))
            }
            Err(OpenSshError::Failed { stderr, .. }) => {
                let managed = &self.store.paths().managed;
                // ssh read the user's own configuration; nothing was staged.
                let files = ConfigFiles {
                    home: self.home(),
                    managed,
                    managed_text: &read_lossy(managed),
                    entry: None,
                };
                match interpret(&stderr, &files) {
                    ConfigRejection::Unrecognised { .. } => None,
                    rejection => Some(self.explain_rejection(
                        alias,
                        Operation::Connect,
                        false,
                        &rejection_diagnosis(&stderr, rejection),
                    )),
                }
            }
            Err(_) => None,
        }
    }

    fn add_mutation(&self, config: &mut ManagedConfig, host: Host) -> Result<(), AppError> {
        self.ensure_alias_available(config, host.alias(), None)?;
        Ok(config.insert(host).map_err(TransactionError::Operation)?)
    }

    fn edit_mutation(
        &self,
        config: &mut ManagedConfig,
        alias: &str,
        changes: HostChanges,
    ) -> Result<(), AppError> {
        let current = match config.find(alias) {
            Some(host) => host,
            None => return Err(self.missing_host_error(alias)?),
        };
        let changed = current.apply(changes)?;
        if changed.alias() != current.alias() {
            self.ensure_alias_available(config, changed.alias(), Some(current.alias()))?;
        }
        Ok(config
            .replace(alias, changed)
            .map_err(TransactionError::Operation)?)
    }

    /// Which external hosts could move into the managed fragment, and what moving each would change.
    pub fn adoption_plan(&self) -> Result<MigrationPlan, AppError> {
        self.migration_plan(Direction::Adopt, None)
    }

    /// Which managed hosts could move back into the entry config, and what that would change.
    pub fn export_plan(&self) -> Result<MigrationPlan, AppError> {
        self.migration_plan(Direction::Export, None)
    }

    /// The plan of `direction`; once `cancelled` is set, its remaining `ssh -G` probes fail at
    /// once, so the caller should drop the result.
    pub(crate) fn migration_plan(
        &self,
        direction: Direction,
        cancelled: Option<&AtomicBool>,
    ) -> Result<MigrationPlan, AppError> {
        let effective = self.effective(cancelled);
        Ok(match direction {
            Direction::Adopt => self.store.adoption_plan(&effective)?,
            Direction::Export => self.store.export_plan(&effective)?,
        })
    }

    /// Moves the selected hosts of `plan` in one rollback-safe transaction and returns their
    /// aliases.
    pub fn migrate(
        &self,
        plan: &MigrationPlan,
        selection: &[String],
    ) -> Result<Vec<String>, AppError> {
        self.migrate_inner(plan, selection, None)
    }

    pub(crate) fn migrate_cancellable(
        &self,
        plan: &MigrationPlan,
        selection: &[String],
        cancelled: &AtomicBool,
    ) -> Result<Vec<String>, AppError> {
        self.migrate_inner(plan, selection, Some(cancelled))
    }

    fn migrate_inner(
        &self,
        plan: &MigrationPlan,
        selection: &[String],
        cancelled: Option<&AtomicBool>,
    ) -> Result<Vec<String>, AppError> {
        Ok(self
            .store
            .migrate(plan, selection, &self.effective(cancelled), cancelled)?)
    }

    /// `ssh -G` for an alias against a staged entry config, as the adoption checks need it.
    fn effective<'a>(
        &'a self,
        cancelled: Option<&'a AtomicBool>,
    ) -> impl Fn(&Path, &str) -> Result<String, String> + 'a {
        move |entry, alias| {
            self.openssh
                .effective(entry, alias, cancelled)
                .map_err(stderr_of)
        }
    }

    pub fn test(&self, target: &TestTarget) -> Result<TestOutcome, AppError> {
        self.test_cancellable(target, &AtomicBool::new(false))
    }

    pub(crate) fn test_cancellable(
        &self,
        target: &TestTarget,
        cancelled: &AtomicBool,
    ) -> Result<TestOutcome, AppError> {
        self.run_test(target, None, cancelled)
    }

    /// Adds `key` to the known hosts file the target's configuration names,
    /// then tests again.
    pub fn trust_and_retest(
        &self,
        target: &TestTarget,
        key: &PresentedKey,
    ) -> Result<TestOutcome, AppError> {
        self.trust_and_retest_cancellable(target, key, &AtomicBool::new(false))
    }

    pub(crate) fn trust_and_retest_cancellable(
        &self,
        target: &TestTarget,
        key: &PresentedKey,
        cancelled: &AtomicBool,
    ) -> Result<TestOutcome, AppError> {
        self.run_test(target, Some(key), cancelled)
    }

    fn run_test(
        &self,
        target: &TestTarget,
        trust: Option<&PresentedKey>,
        cancelled: &AtomicBool,
    ) -> Result<TestOutcome, AppError> {
        let alias = target.alias();
        // A host being added does not exist yet, so `portico edit` cannot fix it.
        let adding = matches!(target, TestTarget::Add(_));
        let run = |entry: Option<&Path>, files: &ConfigFiles<'_>, alias: &str| {
            self.test_entry(entry, files, alias, adding, trust, cancelled)
        };
        match target {
            TestTarget::Saved(alias) => {
                let alias = self
                    .hosts()?
                    .hosts
                    .into_iter()
                    .find(|host| host.alias.eq_ignore_ascii_case(alias))
                    .ok_or_else(|| AppError::UnknownHost(alias.clone()))?
                    .alias;
                let managed = &self.store.paths().managed;
                let files = ConfigFiles {
                    home: self.home(),
                    managed,
                    managed_text: &read_lossy(managed),
                    entry: None,
                };
                // ssh reads the user's own configuration, not a staged one.
                run(None, &files, &alias)
            }
            TestTarget::Add(host) => self.store.preview(
                |config| self.add_mutation(config, host.clone()),
                |files| run(files.entry.map(|entry| entry.path), &files, alias),
                Some(cancelled),
            ),
            TestTarget::Edit {
                alias: saved,
                changes,
            } => self.store.preview(
                |config| self.edit_mutation(config, saved, changes.clone()),
                |files| run(files.entry.map(|entry| entry.path), &files, alias),
                Some(cancelled),
            ),
        }
    }

    /// Tests `alias`, reading `entry` instead of the user's configuration when given; `files`
    /// name the files a configuration error is traced to. `adding` says the host is not saved yet.
    fn test_entry(
        &self,
        entry: Option<&Path>,
        files: &ConfigFiles<'_>,
        alias: &str,
        adding: bool,
        trust: Option<&PresentedKey>,
        cancelled: &AtomicBool,
    ) -> Result<TestOutcome, AppError> {
        let resolved = match self.openssh.resolve_with(entry, alias, Some(cancelled)) {
            Ok(resolved) => resolved,
            Err(error) => {
                if let OpenSshError::Failed { stderr, .. } = &error {
                    let rejection = interpret(stderr, files);
                    if !matches!(rejection, ConfigRejection::Unrecognised { .. }) {
                        return Ok(TestOutcome::Done(
                            self.rejected_report(alias, adding, stderr, rejection),
                        ));
                    }
                }
                return Err(error.into());
            }
        };
        if let Some(key) = trust {
            let path =
                known_hosts_path(&resolved.known_hosts_files, self.home()).map_err(|reason| {
                    AppError::KnownHosts {
                        path: resolved
                            .known_hosts_files
                            .first()
                            .map(PathBuf::from)
                            .unwrap_or_default(),
                        reason,
                    }
                })?;
            append_known_hosts(&path, &key.lines).map_err(|error| AppError::KnownHosts {
                path: path.clone(),
                reason: error.to_string(),
            })?;
        }
        let route = resolved.route();
        let (result, raw) = self.openssh.probe(entry, alias, route, cancelled)?;
        let explanation = match &result {
            ProbeResult::Failed(failure) => Some(self.explain_failure(
                Diagnosis {
                    failure: failure.clone(),
                    raw,
                },
                alias,
                &resolved,
                Operation::Test,
                false,
            )),
            ProbeResult::Authenticated(_) => None,
        };
        let mut report = TestReport {
            alias: alias.into(),
            user: resolved.user.clone(),
            result,
            key_fetch_failure: None,
            explanation,
        };
        if report.result != ProbeResult::Failed(Failure::HostKeyUnknown) {
            return Ok(TestOutcome::Done(report));
        }
        Ok(
            match self
                .openssh
                .fetch_host_key(entry, alias, route, cancelled)?
            {
                // A key nobody can see the fingerprint of must not be offered for trust.
                KeyFetch::Presented(key) if key.fingerprints.is_empty() => {
                    report.fail_key_fetch(Failure::Other(
                        "ssh-keygen reported no fingerprint for the host key".into(),
                    ));
                    TestOutcome::Done(report)
                }
                KeyFetch::Presented(key) => TestOutcome::UnknownHostKey { report, key },
                KeyFetch::Failed(failure) => {
                    report.fail_key_fetch(failure);
                    TestOutcome::Done(report)
                }
            },
        )
    }

    /// The user's home directory as the configuration paths know it.
    pub fn home(&self) -> &Path {
        self.store.paths().home()
    }

    /// The test result for a configuration OpenSSH refused before it could connect.
    fn rejected_report(
        &self,
        alias: &str,
        adding: bool,
        stderr: &str,
        rejection: ConfigRejection,
    ) -> TestReport {
        let diagnosis = rejection_diagnosis(stderr, rejection);
        TestReport {
            alias: alias.into(),
            user: None,
            explanation: Some(self.explain_rejection(alias, Operation::Test, adding, &diagnosis)),
            result: ProbeResult::Failed(diagnosis.failure),
            key_fetch_failure: None,
        }
    }

    /// Explains a configuration OpenSSH refused; nothing about the host was resolved.
    fn explain_rejection(
        &self,
        alias: &str,
        operation: Operation,
        adding: bool,
        diagnosis: &Diagnosis,
    ) -> Explanation {
        let context = Context {
            alias,
            operation,
            user: None,
            hostname: None,
            port: None,
            identity_files: &[],
            jump_is_known: false,
            home: self.home(),
            managed_file: Some(&self.store.paths().managed),
            adding,
            in_tui: false,
        };
        explain(diagnosis, &context)
    }

    fn explain_failure(
        &self,
        diagnosis: Diagnosis,
        alias: &str,
        resolved: &ResolvedConfig,
        operation: Operation,
        in_tui: bool,
    ) -> Explanation {
        let home = self.home();
        let identity_files: Vec<(String, bool)> = resolved
            .identity_files
            .iter()
            .map(|file| {
                let path = match file.strip_prefix("~/") {
                    Some(rest) => home.join(rest),
                    None => PathBuf::from(file),
                };
                (file.clone(), path.exists())
            })
            .collect();
        let jump_is_known = match &diagnosis.failure {
            Failure::JumpHostFailed { jump, .. } => self.hosts().is_ok_and(|inventory| {
                inventory
                    .hosts
                    .iter()
                    .any(|host| host.alias.eq_ignore_ascii_case(jump))
            }),
            _ => false,
        };
        explain(
            &diagnosis,
            &Context {
                alias,
                operation,
                user: resolved.user.as_deref(),
                hostname: Some(&resolved.hostname),
                port: resolved.port,
                identity_files: &identity_files,
                jump_is_known,
                home,
                managed_file: Some(&self.store.paths().managed),
                adding: false,
                in_tui,
            },
        )
    }

    fn discover(&self) -> Result<Discovery, AppError> {
        Ok(discover(
            &self.store.paths().entry,
            &self.store.paths().managed,
        )?)
    }

    fn ensure_alias_available(
        &self,
        managed: &ManagedConfig,
        alias: &str,
        current_alias: Option<&str>,
    ) -> Result<(), AppError> {
        let discovery = self.discover()?;
        if !discovery.aliases_complete {
            return Err(AppError::IncompleteDiscovery(discovery.warnings));
        }
        let managed_collision = managed.hosts().iter().any(|host| {
            host.alias().eq_ignore_ascii_case(alias)
                && !current_alias.is_some_and(|current| host.alias().eq_ignore_ascii_case(current))
        });
        let external_collision = discovery
            .hosts
            .iter()
            .any(|host| host.alias.eq_ignore_ascii_case(alias));
        if managed_collision || external_collision {
            Err(AppError::AliasCollision(alias.into()))
        } else {
            Ok(())
        }
    }

    fn missing_host_error(&self, alias: &str) -> Result<AppError, AppError> {
        if self
            .discover()?
            .hosts
            .iter()
            .any(|host| host.alias.eq_ignore_ascii_case(alias))
        {
            Ok(AppError::ReadOnlyExternal(alias.into()))
        } else {
            Ok(AppError::UnknownHost(alias.into()))
        }
    }

    fn transact<M>(&self, mutate: M, cancelled: Option<&AtomicBool>) -> Result<(), AppError>
    where
        M: FnOnce(&mut ManagedConfig) -> Result<(), AppError>,
    {
        match cancelled {
            Some(cancelled) => self.store.transact_cancellable(
                mutate,
                |entry, aliases| {
                    self.openssh
                        .validate_cancellable(entry, aliases, cancelled)
                        .map_err(stderr_of)
                },
                cancelled,
            ),
            None => self.store.transact(mutate, |entry, aliases| {
                self.openssh.validate(entry, aliases).map_err(stderr_of)
            }),
        }
    }
}

/// A configuration OpenSSH refused, as a diagnosis that keeps every line it printed.
fn rejection_diagnosis(stderr: &str, rejection: ConfigRejection) -> Diagnosis {
    Diagnosis {
        failure: Failure::ConfigRejected(rejection),
        raw: stderr
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| line.trim_end().to_string())
            .collect(),
    }
}

/// The text OpenSSH printed when it refused a configuration, else the error's own message.
fn stderr_of(error: OpenSshError) -> String {
    match error {
        OpenSshError::Failed { stderr, .. } => stderr,
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        ops::Deref,
        os::unix::fs::PermissionsExt,
        panic,
        sync::{
            Arc, Condvar, Mutex,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        thread,
        time::Duration,
    };

    use tempfile::TempDir;

    use crate::{
        host::{Change, Host, HostChanges},
        managed::{ConfigPaths, ManagedStore},
        openssh::OpenSsh,
        test_executable::write_executable,
    };

    use super::*;

    struct TestApp {
        _dir: TempDir,
        app: App,
        fake_log: std::path::PathBuf,
    }

    impl Deref for TestApp {
        type Target = App;

        fn deref(&self) -> &Self::Target {
            &self.app
        }
    }

    fn initialized_test_app() -> TestApp {
        test_app(None, None)
    }

    fn test_app_with_external(contents: &str) -> TestApp {
        test_app(Some(contents), None)
    }

    fn test_app(
        external: Option<&str>,
        before_transaction_lock: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> TestApp {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::from_home(dir.path());
        let fake = dir.path().join("ssh");
        let fake_log = dir.path().join("ssh.args");
        write_executable(
            &fake,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\ncase \"$1\" in\n  -G) if [ \"$2\" != '-F' ]; then printf 'hostname resolved.example\\nuser resolved\\nport 22\\n'; fi ;;\n  --) ;;\n  -*) exit 23 ;;\nesac\n",
                fake_log.display()
            ),
        );
        let mut store = ManagedStore::new(paths.clone());
        if let Some(hook) = before_transaction_lock {
            store = store.with_before_transaction_lock(hook);
        }
        let keygen = dir.path().join("ssh-keygen");
        write_executable(
            &keygen,
            "#!/bin/sh\nprintf '256 SHA256:abc h (ED25519)\\n'\n",
        );
        let app = App::new(store, OpenSsh::new(fake.clone(), fake).with_keygen(keygen));
        app.initialize(true).unwrap();
        if let Some(contents) = external {
            let mut entry = fs::read_to_string(&paths.entry).unwrap();
            entry.push_str(contents);
            fs::write(&paths.entry, entry).unwrap();
        }
        TestApp {
            _dir: dir,
            app,
            fake_log,
        }
    }

    fn fixture_path(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/ssh")
            .join(format!("{name}.txt"))
    }

    fn print_fixture(name: &str) -> String {
        format!("cat '{}' >&2", fixture_path(name).display())
    }

    impl TestApp {
        /// Replaces the fake ssh. `-G` prints `resolved`; a run that sets
        /// `UserKnownHostsFile` is a key fetch and runs `fetch` with `$kh`
        /// naming that file; any other run is a probe and runs `probe`.
        fn install_ssh(&self, resolved: &str, probe: &str, fetch: &str) {
            write_executable(
                self._dir.path().join("ssh"),
                &format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{log}'\ncase \"$1\" in\n  -G) printf '{resolved}' ;;\n  *)\n    kh=; prev=\n    for a in \"$@\"; do\n      case \"$prev$a\" in -oUserKnownHostsFile=*) kh=${{a#UserKnownHostsFile=}} ;; esac\n      prev=; [ \"$a\" = -o ] && prev=-o\n    done\n    if [ -n \"$kh\" ]; then\n{fetch}\n    else\n      echo probe >> '{probes}'\n{probe}\n    fi ;;\nesac\n",
                    log = self.fake_log.display(),
                    probes = self.probes().display(),
                ),
            );
        }

        fn probes(&self) -> std::path::PathBuf {
            self._dir.path().join("probes")
        }

        fn known_hosts(&self) -> std::path::PathBuf {
            self.store.paths().ssh_dir.join("known_hosts")
        }
    }

    const USER_DEPLOY: &str = "hostname h\\nuser deploy\\nport 22\\n";

    fn host(alias: &str) -> Host {
        Host::new(alias, "h.example", None, None, None, None).unwrap()
    }

    #[test]
    fn a_saved_host_is_tested_with_the_users_configuration() {
        let app = initialized_test_app();
        app.add(host("dev")).unwrap();
        app.install_ssh(USER_DEPLOY, &print_fixture("authenticated"), ":");

        let outcome = app.test(&TestTarget::Saved("DEV".into())).unwrap();

        let TestOutcome::Done(report) = outcome else {
            panic!("{outcome:?}")
        };
        assert_eq!(report.alias, "dev");
        assert_eq!(report.user.as_deref(), Some("deploy"));
        assert!(matches!(report.result, ProbeResult::Authenticated(_)));
        let args = fs::read_to_string(&app.fake_log).unwrap();
        assert!(!args.lines().any(|arg| arg == "-F"), "{args}");
        assert!(args.ends_with("--\ndev\n"), "{args}");
    }

    #[test]
    fn a_login_through_a_jump_host_or_proxy_names_the_resolved_target() {
        let app = initialized_test_app();
        app.add(host("dev")).unwrap();
        for (resolved, summary) in [
            (
                "hostname 127.0.0.1\\nuser deploy\\nport 2222\\nproxyjump bastion\\n",
                "authenticated as deploy@127.0.0.1:2222 via bastion (publickey)",
            ),
            (
                "hostname 127.0.0.1\\nport 22\\nproxycommand nc %%h %%p\\n",
                "authenticated to 127.0.0.1:22 via proxy (publickey)",
            ),
        ] {
            app.install_ssh(resolved, &print_fixture("authenticated-via-proxy"), ":");

            let outcome = app.test(&TestTarget::Saved("dev".into())).unwrap();

            let TestOutcome::Done(report) = outcome else {
                panic!("{outcome:?}")
            };
            assert_eq!(report.summary(), summary);
        }
    }

    #[test]
    fn an_unknown_alias_is_an_unknown_host_error() {
        let app = initialized_test_app();
        let error = app.test(&TestTarget::Saved("nope".into())).unwrap_err();
        assert!(matches!(error, AppError::UnknownHost(alias) if alias == "nope"));
    }

    #[test]
    fn an_add_form_is_tested_against_the_staged_config_and_nothing_is_saved() {
        let app = initialized_test_app();
        let before = fs::read(&app.store.paths().managed).unwrap();
        app.install_ssh(USER_DEPLOY, &print_fixture("authenticated"), ":");

        let outcome = app.test(&TestTarget::Add(host("new"))).unwrap();

        assert!(matches!(outcome, TestOutcome::Done(_)));
        let args = fs::read_to_string(&app.fake_log).unwrap();
        assert!(args.lines().any(|arg| arg == "-F"), "{args}");
        assert!(args.ends_with("--\nnew\n"), "{args}");
        assert_eq!(fs::read(&app.store.paths().managed).unwrap(), before);
        assert!(app.hosts().unwrap().hosts.iter().all(|h| h.alias != "new"));
    }

    #[test]
    fn an_edit_form_tests_the_renamed_alias() {
        let app = initialized_test_app();
        app.add(host("dev")).unwrap();
        let before = fs::read(&app.store.paths().managed).unwrap();
        app.install_ssh(USER_DEPLOY, &print_fixture("authenticated"), ":");
        let target = TestTarget::Edit {
            alias: "dev".into(),
            changes: HostChanges {
                alias: Change::Set("prod".into()),
                ..Default::default()
            },
        };
        assert_eq!(target.alias(), "prod");

        app.test(&target).unwrap();

        let args = fs::read_to_string(&app.fake_log).unwrap();
        assert!(args.ends_with("--\nprod\n"), "{args}");
        assert_eq!(fs::read(&app.store.paths().managed).unwrap(), before);
    }

    #[test]
    fn form_errors_surface_as_on_save() {
        let app = test_app_with_external("Host legacy\n  HostName legacy.example\n");
        let error = app.test(&TestTarget::Add(host("legacy"))).unwrap_err();
        assert!(matches!(error, AppError::AliasCollision(_)), "{error:?}");
    }

    const FETCH_KEY: &str = "printf 'h ssh-ed25519 AAAA\\n' >> \"$kh\"";

    #[test]
    fn an_unknown_key_comes_back_with_its_fingerprint() {
        let app = initialized_test_app();
        app.add(host("dev")).unwrap();
        app.install_ssh(USER_DEPLOY, &print_fixture("host-key-unknown"), FETCH_KEY);

        let outcome = app.test(&TestTarget::Saved("dev".into())).unwrap();

        let TestOutcome::UnknownHostKey { report, key } = outcome else {
            panic!("{outcome:?}")
        };
        assert_eq!(report.result, ProbeResult::Failed(Failure::HostKeyUnknown));
        assert_eq!(key.fingerprints.len(), 1);
        assert_eq!(key.lines, b"h ssh-ed25519 AAAA\n");
    }

    #[test]
    fn a_failed_test_carries_its_explanation_and_a_pass_has_none() {
        let app = initialized_test_app();
        app.add(host("dev")).unwrap();
        app.install_ssh(USER_DEPLOY, &print_fixture("auth-refused"), ":");

        let TestOutcome::Done(report) = app.test(&TestTarget::Saved("dev".into())).unwrap() else {
            panic!("expected a report")
        };
        let lines = report.explanation.unwrap().render("dev", false);

        assert_eq!(
            lines[0],
            "dev: failed \u{2014} the server refused the login"
        );
        assert_eq!(lines[1], "  user:    deploy@h:22");

        app.install_ssh(USER_DEPLOY, &print_fixture("authenticated"), ":");
        let TestOutcome::Done(report) = app.test(&TestTarget::Saved("dev".into())).unwrap() else {
            panic!("expected a report")
        };
        assert_eq!(report.explanation, None);
    }

    #[test]
    fn a_configuration_ssh_refuses_is_a_failed_report_not_an_error() {
        let app = initialized_test_app();
        app.add(host("dev")).unwrap();
        write_executable(
            app._dir.path().join("ssh"),
            "#!/bin/sh\necho '/home/x/.ssh/extra: line 4: Bad configuration option: bogus' >&2\nexit 255\n",
        );

        let TestOutcome::Done(report) = app.test(&TestTarget::Saved("dev".into())).unwrap() else {
            panic!("expected a report")
        };

        assert_eq!(
            report.result,
            ProbeResult::Failed(Failure::ConfigRejected(ConfigRejection::UserFile {
                path: "/home/x/.ssh/extra".into(),
                line: 4,
                message: "Bad configuration option: bogus".into(),
                home: app.home().to_owned(),
            }))
        );
        assert!(report.explanation.is_some());
    }

    /// Next steps of a form test whose staged HostName line ssh rejects.
    fn rejected_hostname_next(app: &TestApp, target: &TestTarget) -> Vec<String> {
        write_executable(
            app._dir.path().join("ssh"),
            r#"#!/bin/sh
while [ "$#" -gt 0 ]; do
  if [ "$1" = -F ]; then entry="$2"; fi
  shift
done
managed=$(sed -n 's/^Include "\(.*portico-candidate[^"]*\)".*/\1/p' "$entry")
line=$(grep -n HostName "$managed" | head -n 1 | cut -d: -f1)
printf '%s line %s: Bad hostname.\n' "$managed" "$line" >&2
exit 255
"#,
        );
        let TestOutcome::Done(report) = app.test(target).unwrap() else {
            panic!("expected a report")
        };
        assert!(
            matches!(
                report.result,
                ProbeResult::Failed(Failure::ConfigRejected(ConfigRejection::Managed { .. }))
            ),
            "{:?}",
            report.result
        );
        report.explanation.unwrap().next
    }

    #[test]
    fn a_rejected_field_is_corrected_in_an_add_form_and_edited_otherwise() {
        let app = initialized_test_app();
        assert_eq!(
            rejected_hostname_next(&app, &TestTarget::Add(host("new"))),
            ["correct the hostname field and try again"]
        );

        let app = initialized_test_app();
        app.add(host("dev")).unwrap();
        let edit = TestTarget::Edit {
            alias: "dev".into(),
            changes: HostChanges::default(),
        };
        assert_eq!(
            rejected_hostname_next(&app, &edit),
            ["change it with `portico edit dev`"]
        );
    }

    #[test]
    fn a_failed_fetch_keeps_host_key_not_trusted_and_says_why() {
        let app = initialized_test_app();
        app.add(host("dev")).unwrap();
        app.install_ssh(
            USER_DEPLOY,
            &print_fixture("host-key-unknown"),
            &print_fixture("connection-refused"),
        );

        let outcome = app.test(&TestTarget::Saved("dev".into())).unwrap();

        let TestOutcome::Done(report) = outcome else {
            panic!("{outcome:?}")
        };
        assert_eq!(report.result, ProbeResult::Failed(Failure::HostKeyUnknown));
        assert_eq!(report.key_fetch_failure, Some(Failure::ConnectionRefused));
    }

    #[test]
    fn a_key_without_fingerprints_is_never_offered() {
        let app = initialized_test_app();
        app.add(host("dev")).unwrap();
        app.install_ssh(USER_DEPLOY, &print_fixture("host-key-unknown"), FETCH_KEY);
        write_executable(app._dir.path().join("ssh-keygen"), "#!/bin/sh\n");

        let outcome = app.test(&TestTarget::Saved("dev".into())).unwrap();

        let TestOutcome::Done(report) = outcome else {
            panic!("{outcome:?}")
        };
        assert_eq!(report.result, ProbeResult::Failed(Failure::HostKeyUnknown));
        assert_eq!(
            report.key_fetch_failure,
            Some(Failure::Other(
                "ssh-keygen reported no fingerprint for the host key".into()
            ))
        );
    }

    #[test]
    fn trusting_appends_to_the_resolved_known_hosts_file_and_retests() {
        let app = initialized_test_app();
        app.add(host("dev")).unwrap();
        let probe = format!(
            "if grep -qs AAAA '{}'; then {}; else {}; fi",
            app.known_hosts().display(),
            print_fixture("authenticated"),
            print_fixture("host-key-unknown"),
        );
        app.install_ssh(
            "hostname h\\nuserknownhostsfile ~/.ssh/known_hosts\\n",
            &probe,
            FETCH_KEY,
        );
        let target = TestTarget::Saved("dev".into());
        let TestOutcome::UnknownHostKey { key, .. } = app.test(&target).unwrap() else {
            panic!("expected an unknown key")
        };

        let outcome = app.trust_and_retest(&target, &key).unwrap();

        assert!(matches!(
            outcome,
            TestOutcome::Done(TestReport {
                result: ProbeResult::Authenticated(_),
                ..
            })
        ));
        assert_eq!(fs::read(app.known_hosts()).unwrap(), key.lines);
        assert_eq!(fs::read_to_string(app.probes()).unwrap().lines().count(), 2);
    }

    #[test]
    fn tokens_in_the_known_hosts_path_are_refused() {
        let app = initialized_test_app();
        app.add(host("dev")).unwrap();
        app.install_ssh(
            "hostname h\\nuserknownhostsfile ~/.ssh/%%u\\n",
            &print_fixture("authenticated"),
            ":",
        );
        let key = PresentedKey {
            lines: b"h ssh-ed25519 AAAA\n".to_vec(),
            fingerprints: Vec::new(),
        };

        let error = app
            .trust_and_retest(&TestTarget::Saved("dev".into()), &key)
            .unwrap_err();

        assert!(matches!(error, AppError::KnownHosts { .. }), "{error:?}");
        assert!(!app.known_hosts().exists());
        assert!(!app.store.paths().ssh_dir.join("%u").exists());
    }

    #[test]
    fn external_hosts_are_visible_but_immutable() {
        let app = test_app_with_external("Host legacy\n  HostName legacy.example\n");
        let listed = app.hosts().unwrap();
        assert!(
            listed
                .hosts
                .iter()
                .any(|host| host.alias == "legacy" && host.ownership == Ownership::External)
        );

        let error = app.remove("legacy").unwrap_err();
        assert!(matches!(error, AppError::ReadOnlyExternal(alias) if alias == "legacy"));
    }

    #[test]
    fn add_edit_rename_and_remove_share_one_transaction_path() {
        let app = initialized_test_app();
        app.add(Host::new("dev", "dev.example", None, None, None, None).unwrap())
            .unwrap();
        app.edit(
            "dev",
            HostChanges {
                alias: Change::Set("prod".into()),
                user: Change::Set("deploy".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            app.show("PROD").unwrap().declared.unwrap().user(),
            Some("deploy")
        );
        app.remove("prod").unwrap();
        assert!(matches!(app.show("prod"), Err(AppError::UnknownHost(_))));
    }

    #[test]
    fn add_rejects_case_only_external_collision() {
        let app = test_app_with_external("Host Prod\n  HostName prod.example\n");
        let error = app
            .add(Host::new("prod", "other.example", None, None, None, None).unwrap())
            .unwrap_err();
        assert!(matches!(error, AppError::AliasCollision(alias) if alias == "prod"));
    }

    #[test]
    fn add_rejects_single_quoted_external_alias() {
        let app = test_app_with_external("Host 'Prod'\n  HostName prod.example\n");
        let before = fs::read(&app.store.paths().managed).unwrap();

        let error = app
            .add(Host::new("prod", "other.example", None, None, None, None).unwrap())
            .unwrap_err();

        assert!(matches!(error, AppError::AliasCollision(alias) if alias == "prod"));
        assert_eq!(fs::read(&app.store.paths().managed).unwrap(), before);
    }

    #[test]
    fn add_rejects_aliases_after_literal_hashes() {
        for line in [
            "Host ignored# Prod\n",
            "Host ''# Prod\n",
            "Host \"\"# Prod\n",
        ] {
            let app = test_app_with_external(line);
            let before = fs::read(&app.store.paths().managed).unwrap();

            let error = app
                .add(Host::new("prod", "other.example", None, None, None, None).unwrap())
                .unwrap_err();

            assert!(
                matches!(error, AppError::AliasCollision(alias) if alias == "prod"),
                "{line}"
            );
            assert_eq!(fs::read(&app.store.paths().managed).unwrap(), before);
        }
    }

    #[test]
    fn harmless_discovery_warnings_do_not_block_unrelated_add() {
        let app = test_app_with_external(
            "Include missing/*.conf\nHost legacy\n  HostName legacy.example\nHost LEGACY\n  HostName duplicate.example\n",
        );
        app.add(Host::new("dev", "dev.example", None, None, None, None).unwrap())
            .unwrap();
        let error = app
            .add(Host::new("legacy", "other.example", None, None, None, None).unwrap())
            .unwrap_err();
        assert!(matches!(error, AppError::AliasCollision(_)));
        assert!(app.show("legacy").is_ok());
        assert!(matches!(
            app.connect("legacy", false).unwrap(),
            ConnectionOutcome::Exited { status, explanation: None, .. } if status.success()
        ));
    }

    #[test]
    fn inaccessible_include_parent_blocks_mutation() {
        let app = test_app_with_external("Include locked/hidden.conf\n");
        let locked = app.store.paths().ssh_dir.join("locked");
        fs::create_dir(&locked).unwrap();
        fs::write(locked.join("hidden.conf"), "Host hidden\n").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        let result = app.add(Host::new("hidden", "other.example", None, None, None, None).unwrap());

        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(result, Err(AppError::IncompleteDiscovery(_))));
    }

    #[test]
    fn edit_rejects_external_host() {
        let app = test_app_with_external("Host legacy\n  HostName legacy.example\n");

        let error = app.edit("LEGACY", HostChanges::default()).unwrap_err();

        assert!(matches!(error, AppError::ReadOnlyExternal(alias) if alias == "LEGACY"));
    }

    #[test]
    fn rename_rejects_external_collision() {
        let app = test_app_with_external("Host Prod\n  HostName prod.example\n");
        app.add(Host::new("dev", "dev.example", None, None, None, None).unwrap())
            .unwrap();

        let error = app
            .edit(
                "dev",
                HostChanges {
                    alias: Change::Set("prod".into()),
                    ..Default::default()
                },
            )
            .unwrap_err();

        assert!(matches!(error, AppError::AliasCollision(alias) if alias == "prod"));
    }

    #[test]
    fn concurrent_disjoint_edits_both_survive() {
        let armed = Arc::new(AtomicBool::new(false));
        let (entered_tx, entered_rx) = mpsc::channel();
        let gate = Arc::new(TransactionGate::new(entered_tx));
        let hook_armed = Arc::clone(&armed);
        let hook_gate = Arc::clone(&gate);
        let app = test_app(
            None,
            Some(Arc::new(move || {
                if hook_armed.load(Ordering::SeqCst) {
                    hook_gate.wait();
                }
            })),
        );
        app.add(Host::new("dev", "dev.example", None, None, None, None).unwrap())
            .unwrap();
        armed.store(true, Ordering::SeqCst);

        thread::scope(|scope| {
            let _release_on_unwind = OpenGateOnDrop(Arc::clone(&gate));
            let user_app = &app.app;
            let user = scope.spawn(move || {
                user_app.edit(
                    "dev",
                    HostChanges {
                        user: Change::Set("deploy".into()),
                        ..Default::default()
                    },
                )
            });
            let port_app = &app.app;
            let port = scope.spawn(move || {
                port_app.edit(
                    "dev",
                    HostChanges {
                        port: Change::Set(2222),
                        ..Default::default()
                    },
                )
            });
            entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            gate.open();
            user.join().unwrap().unwrap();
            port.join().unwrap().unwrap();
        });

        let host = app.show("dev").unwrap().declared.unwrap();
        assert_eq!(host.user(), Some("deploy"));
        assert_eq!(host.port(), Some(2222));
    }

    #[test]
    fn waiting_mutations_recheck_external_aliases_under_the_lock() {
        use fs2::FileExt;

        for rename in [false, true] {
            let armed = Arc::new(AtomicBool::new(false));
            let (entered_tx, entered_rx) = mpsc::channel();
            let hook_armed = Arc::clone(&armed);
            let app = test_app(
                None,
                Some(Arc::new(move || {
                    if hook_armed.load(Ordering::SeqCst) {
                        entered_tx.send(()).unwrap();
                    }
                })),
            );
            app.add(Host::new("dev", "dev.example", None, None, None, None).unwrap())
                .unwrap();
            let paths = app.store.paths();
            let before = fs::read(&paths.managed).unwrap();
            let backup = fs::read(&paths.backup).unwrap();
            let lock = fs::File::open(&paths.lock).unwrap();
            lock.lock_exclusive().unwrap();
            armed.store(true, Ordering::SeqCst);

            let error = thread::scope(|scope| {
                let operation = scope.spawn(|| {
                    if rename {
                        app.edit(
                            "dev",
                            HostChanges {
                                alias: Change::Set("prod".into()),
                                ..Default::default()
                            },
                        )
                    } else {
                        app.add(Host::new("prod", "prod.example", None, None, None, None).unwrap())
                    }
                });
                // Dropping the lock also releases a waiter if an assertion panics.
                let held_lock = lock;
                entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                fs::write(&paths.entry, "Include ~/.ssh/portico/config\nHost 'Prod'\n").unwrap();
                drop(held_lock);
                operation.join().unwrap().unwrap_err()
            });

            assert!(matches!(error, AppError::AliasCollision(alias) if alias == "prod"));
            assert_eq!(fs::read(&paths.managed).unwrap(), before);
            assert_eq!(fs::read(&paths.backup).unwrap(), backup);
        }
    }

    #[test]
    fn concurrent_duplicate_add_reports_collision_without_recovery_advice() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let gate = Arc::new(TransactionGate::new(entered_tx));
        let hook_gate = Arc::clone(&gate);
        let app = test_app(None, Some(Arc::new(move || hook_gate.wait())));

        let results = thread::scope(|scope| {
            let _release_on_unwind = OpenGateOnDrop(Arc::clone(&gate));
            let first = scope.spawn(|| {
                app.add(Host::new("same", "one.example", None, None, None, None).unwrap())
            });
            let second = scope.spawn(|| {
                app.add(Host::new("SAME", "two.example", None, None, None, None).unwrap())
            });
            entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            gate.open();
            [first.join().unwrap(), second.join().unwrap()]
        });

        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let error = results.into_iter().find_map(Result::err).unwrap();
        assert!(
            matches!(&error, AppError::AliasCollision(alias) if alias.eq_ignore_ascii_case("same"))
        );
        assert!(!error.to_string().contains("recover"), "{error}");
        assert!(!error.to_string().contains("config is invalid"), "{error}");
        assert_eq!(app.store.load().unwrap().hosts().len(), 1);
    }

    #[test]
    fn parent_panic_releases_transaction_gate() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let gate = Arc::new(TransactionGate::new(entered_tx));
        let result = panic::catch_unwind({
            let gate = Arc::clone(&gate);
            move || {
                thread::scope(|scope| {
                    let _release_on_unwind = OpenGateOnDrop(Arc::clone(&gate));
                    scope.spawn(|| gate.wait());
                    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                    panic!("simulated parent failure");
                });
            }
        });

        assert!(result.is_err());
    }

    #[test]
    fn dash_prefixed_external_alias_is_a_destination_not_an_option() {
        let app = test_app_with_external("Host -external\n  HostName external.example\n");

        assert!(matches!(
            app.connect("-external", false).unwrap(),
            ConnectionOutcome::Exited { status, explanation: None, .. } if status.success()
        ));
        assert_eq!(
            fs::read_to_string(&app.fake_log).unwrap(),
            "--\n-external\n"
        );
    }

    struct TransactionGate {
        open: Mutex<bool>,
        opened: Condvar,
        entered: mpsc::Sender<()>,
    }

    impl TransactionGate {
        fn new(entered: mpsc::Sender<()>) -> Self {
            Self {
                open: Mutex::new(false),
                opened: Condvar::new(),
                entered,
            }
        }

        fn wait(&self) {
            self.entered.send(()).unwrap();
            let mut open = self.open.lock().unwrap();
            while !*open {
                open = self.opened.wait(open).unwrap();
            }
        }

        fn open(&self) {
            *self.open.lock().unwrap() = true;
            self.opened.notify_all();
        }
    }

    struct OpenGateOnDrop(Arc<TransactionGate>);

    impl Drop for OpenGateOnDrop {
        fn drop(&mut self) {
            self.0.open();
        }
    }
}
