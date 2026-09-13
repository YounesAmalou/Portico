use std::{path::PathBuf, process::ExitStatus};

use thiserror::Error;

use crate::{
    discovery::{Discovery, DiscoveryError, DiscoveryWarning, discover},
    host::{Host, HostChanges, HostError},
    managed::{InitError, InitOutcome, InitPlan, ManagedStore, TransactionError},
    openssh::{OpenSsh, OpenSshError, ResolvedConfig},
};

pub struct App {
    store: ManagedStore,
    openssh: OpenSsh,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostDetails {
    pub summary: HostSummary,
    pub declared: Option<Host>,
    pub resolved: ResolvedConfig,
    pub warnings: Vec<DiscoveryWarning>,
}

#[derive(Debug, Error)]
pub enum AppError {
    #[error(transparent)]
    Init(#[from] InitError),
    #[error(transparent)]
    Transaction(#[from] TransactionError),
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
}

impl App {
    pub fn new(store: ManagedStore, openssh: OpenSsh) -> Self {
        Self { store, openssh }
    }

    pub fn init_plan(&self) -> Result<InitPlan, AppError> {
        Ok(self.store.plan_init()?)
    }

    pub fn initialize(&self, approved: bool) -> Result<InitOutcome, AppError> {
        Ok(self.store.apply_init(approved, |entry, aliases| {
            self.openssh
                .validate(entry, aliases)
                .map_err(|error| error.to_string())
        })?)
    }

    pub fn hosts(&self) -> Result<Vec<HostSummary>, AppError> {
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
        Ok(hosts)
    }

    pub fn show(&self, alias: &str) -> Result<HostDetails, AppError> {
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
        let resolved = self.openssh.resolve(&summary.alias)?;
        Ok(HostDetails {
            summary,
            declared,
            resolved,
            warnings: discovery.warnings,
        })
    }

    pub fn add(&self, host: Host) -> Result<(), AppError> {
        self.ensure_alias_available(host.alias(), None)?;
        self.transact(|config| config.insert(host))
    }

    pub fn edit(&self, alias: &str, changes: HostChanges) -> Result<(), AppError> {
        let config = self.store.load()?;
        let current = match config.find(alias) {
            Some(host) => host.clone(),
            None => return Err(self.missing_host_error(alias)?),
        };
        let changed = current.apply(changes)?;
        if changed.alias() != current.alias() {
            self.ensure_alias_available(changed.alias(), Some(current.alias()))?;
        }
        self.transact(|config| config.replace(alias, changed))
    }

    pub fn remove(&self, alias: &str) -> Result<(), AppError> {
        if self.store.load()?.find(alias).is_none() {
            return Err(self.missing_host_error(alias)?);
        }
        self.transact(|config| config.remove(alias).map(drop))
    }

    pub fn connect(&self, alias: &str, new_terminal: bool) -> Result<ExitStatus, AppError> {
        let host = self
            .hosts()?
            .into_iter()
            .find(|host| host.alias.eq_ignore_ascii_case(alias))
            .ok_or_else(|| AppError::UnknownHost(alias.into()))?;
        Ok(self.openssh.connect(&host.alias, new_terminal)?)
    }

    fn discover(&self) -> Result<Discovery, AppError> {
        Ok(discover(
            &self.store.paths().entry,
            &self.store.paths().managed,
        )?)
    }

    fn ensure_alias_available(
        &self,
        alias: &str,
        current_alias: Option<&str>,
    ) -> Result<(), AppError> {
        let discovery = self.discover()?;
        if !discovery.aliases_complete {
            return Err(AppError::IncompleteDiscovery(discovery.warnings));
        }
        let managed = self.store.load()?;
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

    fn transact<M>(&self, mutate: M) -> Result<(), AppError>
    where
        M: FnOnce(&mut crate::managed::ManagedConfig) -> Result<(), crate::managed::ManagedError>,
    {
        Ok(self.store.transact(mutate, |entry, aliases| {
            self.openssh
                .validate(entry, aliases)
                .map_err(|error| error.to_string())
        })?)
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, ops::Deref, os::unix::fs::PermissionsExt};

    use tempfile::TempDir;

    use crate::{
        host::{Change, Host, HostChanges},
        managed::{ConfigPaths, ManagedStore},
        openssh::OpenSsh,
    };

    use super::*;

    struct TestApp {
        _dir: TempDir,
        app: App,
    }

    impl Deref for TestApp {
        type Target = App;

        fn deref(&self) -> &Self::Target {
            &self.app
        }
    }

    fn initialized_test_app() -> TestApp {
        test_app(None)
    }

    fn test_app_with_external(contents: &str) -> TestApp {
        test_app(Some(contents))
    }

    fn test_app(external: Option<&str>) -> TestApp {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::from_home(dir.path());
        let fake = dir.path().join("ssh");
        fs::write(
            &fake,
            "#!/bin/sh\nif [ \"$1\" = '-G' ] && [ \"$2\" != '-F' ]; then\n  printf 'hostname resolved.example\\nuser resolved\\nport 22\\n'\nfi\n",
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
        let app = App::new(
            ManagedStore::new(paths.clone()),
            OpenSsh::new(fake.clone(), fake),
        );
        app.initialize(true).unwrap();
        if let Some(contents) = external {
            let mut entry = fs::read_to_string(&paths.entry).unwrap();
            entry.push_str(contents);
            fs::write(&paths.entry, entry).unwrap();
        }
        TestApp { _dir: dir, app }
    }

    #[test]
    fn external_hosts_are_visible_but_immutable() {
        let app = test_app_with_external("Host legacy\n  HostName legacy.example\n");
        let listed = app.hosts().unwrap();
        assert!(
            listed
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
    fn incomplete_discovery_blocks_add_but_not_existing_connect() {
        let app = test_app_with_external(
            "Include missing/*.conf\nHost legacy\n  HostName legacy.example\n",
        );
        let error = app
            .add(Host::new("dev", "dev.example", None, None, None, None).unwrap())
            .unwrap_err();
        assert!(matches!(error, AppError::IncompleteDiscovery(_)));
        assert!(app.show("legacy").is_ok());
        assert!(app.connect("legacy", false).unwrap().success());
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
}
