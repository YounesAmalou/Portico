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
        let changed = current.apply(changes.clone())?;
        if changed.alias() != current.alias() {
            self.ensure_alias_available(changed.alias(), Some(current.alias()))?;
        }
        self.transact(|config| config.apply(alias, changes))
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
        fs::write(
            &fake,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\ncase \"$1\" in\n  -G) if [ \"$2\" != '-F' ]; then printf 'hostname resolved.example\\nuser resolved\\nport 22\\n'; fi ;;\n  --) ;;\n  -*) exit 23 ;;\nesac\n",
                fake_log.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
        wait_until_executable(&fake);
        let mut store = ManagedStore::new(paths.clone());
        if let Some(hook) = before_transaction_lock {
            store = store.with_before_transaction_lock(hook);
        }
        let app = App::new(store, OpenSsh::new(fake.clone(), fake));
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

        assert!(app.connect("-external", false).unwrap().success());
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

    fn wait_until_executable(path: &std::path::Path) {
        loop {
            match std::process::Command::new(path)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
            {
                Ok(_) => return,
                Err(error) if error.raw_os_error() == Some(26) => thread::yield_now(),
                Err(error) => panic!("could not execute fake program: {error}"),
            }
        }
    }
}
