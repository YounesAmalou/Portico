//! The tunnels file transaction: `~/.ssh/portico/tunnels`, changed under an exclusive lock and
//! replaced atomically, with the previous version kept as `tunnels.bak`.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use thiserror::Error;

use super::{TunnelFileError, TunnelsFile};
use crate::managed::{acquire_lock, create_private_dir, open_lock, sync_dir, write_unique};

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("cannot read {path}: {reason}; the previous version is {backup}", path = .path.display(), backup = .backup.display())]
    Invalid {
        path: PathBuf,
        backup: PathBuf,
        reason: TunnelFileError,
    },
    #[error("cannot access {path}: {source}", path = .path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[derive(Clone, Debug)]
pub struct TunnelStore {
    dir: PathBuf,
}

impl TunnelStore {
    /// The store in Portico's directory, `~/.ssh/portico`.
    pub fn new(portico_dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: portico_dir.into(),
        }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join("tunnels")
    }

    fn backup(&self) -> PathBuf {
        self.dir.join("tunnels.bak")
    }

    fn lock(&self) -> PathBuf {
        self.dir.join("tunnels.lock")
    }

    /// The saved tunnels; a missing file holds none.
    pub fn load(&self) -> Result<TunnelsFile, StoreError> {
        let path = self.path();
        match fs::read_to_string(&path) {
            Ok(text) => TunnelsFile::parse(&text).map_err(|reason| StoreError::Invalid {
                path,
                backup: self.backup(),
                reason,
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(TunnelsFile::default()),
            Err(source) => Err(StoreError::Io { path, source }),
        }
    }

    /// Applies `mutate` to the current file under the lock and writes the result. Nothing is
    /// written when `mutate` fails, so its checks (validation included) run before the write.
    pub fn transact<R, E>(
        &self,
        mutate: impl FnOnce(&mut TunnelsFile) -> Result<R, E>,
    ) -> Result<R, E>
    where
        E: From<StoreError>,
    {
        let io_error = |path: &Path| {
            let path = path.to_path_buf();
            move |source| StoreError::Io { path, source }
        };
        create_private_dir(&self.dir).map_err(io_error(&self.dir))?;
        let lock = open_lock(&self.lock()).map_err(io_error(&self.lock()))?;
        acquire_lock(&lock, None).map_err(io_error(&self.lock()))?;
        let previous = match fs::read(self.path()) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(source) => {
                return Err(StoreError::Io {
                    path: self.path(),
                    source,
                }
                .into());
            }
        };
        let mut file = self.load()?;
        let result = mutate(&mut file)?;
        let candidate = write_unique(&self.dir, ".tunnels", file.render().as_bytes(), 0o600)
            .map_err(io_error(&self.dir))?;
        if let Some(previous) = previous {
            write_unique(&self.dir, ".tunnels.bak", &previous, 0o600)
                .and_then(|backup| backup.rename_to(&self.backup()))
                .map_err(io_error(&self.backup()))?;
        }
        candidate
            .rename_to(&self.path())
            .map_err(io_error(&self.path()))?;
        sync_dir(&self.dir).map_err(io_error(&self.dir))?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use std::{os::unix::fs::PermissionsExt, sync::Arc, thread};

    use super::*;
    use crate::tunnel::{Forward, Kind, Tunnel};

    fn tunnel(name: &str) -> Tunnel {
        Tunnel::new(name, "prod", Forward::parse(Kind::Dynamic, "1080").unwrap()).unwrap()
    }

    #[test]
    fn a_missing_file_holds_no_tunnels() {
        let dir = tempfile::tempdir().unwrap();
        let store = TunnelStore::new(dir.path().join("portico"));
        assert_eq!(store.load().unwrap(), TunnelsFile::default());
    }

    #[test]
    fn a_change_is_written_privately_with_a_backup() {
        let dir = tempfile::tempdir().unwrap();
        let store = TunnelStore::new(dir.path().join("portico"));
        store
            .transact(|file| file.insert(tunnel("a")).map_err(|_| unreachable!()))
            .map_err(|error: StoreError| error)
            .unwrap();
        store
            .transact(|file| file.insert(tunnel("b")).map_err(|_| unreachable!()))
            .map_err(|error: StoreError| error)
            .unwrap();
        assert_eq!(store.load().unwrap().tunnels().len(), 2);
        let backup = TunnelsFile::parse(&fs::read_to_string(store.backup()).unwrap()).unwrap();
        assert_eq!(backup.tunnels().len(), 1);
        assert_eq!(
            fs::metadata(store.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[derive(Debug)]
    enum Failure {
        Store,
        Rejected,
    }

    impl From<StoreError> for Failure {
        fn from(_: StoreError) -> Self {
            Failure::Store
        }
    }

    #[test]
    fn a_failed_mutation_leaves_the_file_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let store = TunnelStore::new(dir.path().join("portico"));
        store
            .transact(|file| {
                file.insert(tunnel("a")).unwrap();
                Ok::<_, Failure>(())
            })
            .unwrap();
        let before = fs::read(store.path()).unwrap();
        let result = store.transact(|file| {
            file.insert(tunnel("b")).unwrap();
            Err::<(), _>(Failure::Rejected)
        });
        assert!(matches!(result, Err(Failure::Rejected)));
        assert_eq!(fs::read(store.path()).unwrap(), before);
    }

    #[test]
    fn an_unreadable_file_names_the_line_and_the_backup() {
        let dir = tempfile::tempdir().unwrap();
        let store = TunnelStore::new(dir.path());
        fs::write(store.path(), "Tunnel a\n Host b\n Port 1\n").unwrap();
        let error = store.load().unwrap_err().to_string();
        assert!(error.contains("line 3"), "{error}");
        assert!(error.contains("tunnels.bak"), "{error}");
        let result = store.transact(|_| Ok::<_, Failure>(()));
        assert!(matches!(result, Err(Failure::Store)));
    }

    #[test]
    fn concurrent_changes_are_not_lost() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(TunnelStore::new(dir.path().join("portico")));
        let workers: Vec<_> = (0..8)
            .map(|index| {
                let store = Arc::clone(&store);
                thread::spawn(move || {
                    store
                        .transact(|file| {
                            file.insert(tunnel(&format!("t{index}"))).unwrap();
                            Ok::<_, Failure>(())
                        })
                        .unwrap();
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(store.load().unwrap().tunnels().len(), 8);
    }
}
