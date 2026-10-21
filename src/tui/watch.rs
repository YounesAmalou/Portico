//! Notices when the files the host inventory was read from change.
//!
//! Nothing runs in the background: on the TUI's ticks the watch compares each path's metadata
//! with what it was when the inventory was last read. A difference is reported once it has
//! settled, so an editor's save (write, rename, chmod) or a script touching several files gives
//! one reload, and a file is not read half-written.

use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// How often the files are looked at while nothing has changed.
const CHECK_EVERY: Duration = Duration::from_secs(1);
/// How long a change must stay as it is before it is reported.
const SETTLE: Duration = Duration::from_millis(300);

/// What tells one version of a path from another. An absent or unreadable path has none.
///
/// The inode catches a file renamed into place within one timestamp tick, and the status-change
/// time a copy that kept the old modification time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Stamp {
    device: u64,
    inode: u64,
    size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

/// Where stamps come from: the filesystem in the TUI, a map in tests.
pub(super) trait Metadata {
    fn stamp(&self, path: &Path) -> Option<Stamp>;
}

/// The filesystem, following symbolic links as reading the file does.
pub(super) struct Filesystem;

impl Metadata for Filesystem {
    fn stamp(&self, path: &Path) -> Option<Stamp> {
        let metadata = fs::metadata(path).ok()?;
        Some(Stamp {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.size(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        })
    }
}

#[derive(Debug, Eq, PartialEq)]
enum State {
    /// The files are as they were read.
    Unchanged,
    /// The files differ from what was read; these are the stamps the last check saw.
    Settling(Vec<Option<Stamp>>),
    /// A change has settled and waits for the inventory to be read again.
    Changed,
}

/// Follows the paths an inventory was read from, from one read to the next.
pub(super) struct ConfigWatch<M = Filesystem> {
    metadata: M,
    paths: Vec<PathBuf>,
    /// Each path's stamp right after the read, in the order of `paths`.
    read: Vec<Option<Stamp>>,
    state: State,
    next_check: Instant,
}

impl ConfigWatch {
    pub(super) fn new(paths: &[PathBuf], now: Instant) -> Self {
        Self::with_metadata(Filesystem, paths, now)
    }
}

impl<M: Metadata> ConfigWatch<M> {
    fn with_metadata(metadata: M, paths: &[PathBuf], now: Instant) -> Self {
        let mut watch = Self {
            metadata,
            paths: Vec::new(),
            read: Vec::new(),
            state: State::Unchanged,
            next_check: now,
        };
        watch.rebase(paths, now);
        watch
    }

    /// Starts over from `paths` as they are now, just after the inventory was read from them.
    /// Whatever was pending is taken as read.
    pub(super) fn rebase(&mut self, paths: &[PathBuf], now: Instant) {
        self.paths = paths.to_vec();
        self.read = self.stamps();
        self.state = State::Unchanged;
        self.next_check = now + CHECK_EVERY;
    }

    /// Looks at the files when a check is due. Returns whether a settled change waits for the
    /// inventory to be read again, which stays true until `rebase`.
    pub(super) fn tick(&mut self, now: Instant) -> bool {
        if self.state != State::Changed && now >= self.next_check {
            let stamps = self.stamps();
            (self.state, self.next_check) = if stamps == self.read {
                (State::Unchanged, now + CHECK_EVERY)
            } else if self.state == State::Settling(stamps.clone()) {
                (State::Changed, now)
            } else {
                (State::Settling(stamps), now + SETTLE)
            };
        }
        self.state == State::Changed
    }

    fn stamps(&self) -> Vec<Option<Stamp>> {
        self.paths
            .iter()
            .map(|path| self.metadata.stamp(path))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::HashMap, os::unix::fs::PermissionsExt};

    use super::*;

    /// Files whose stamps the test sets by hand.
    #[derive(Default)]
    struct Files(RefCell<HashMap<PathBuf, Stamp>>);

    impl Files {
        fn with(paths: &[&str]) -> Self {
            let files = Self::default();
            for (inode, path) in paths.iter().enumerate() {
                files.write(path, inode as u64 + 1, 0);
            }
            files
        }

        /// Gives `path` the stamp of a file with this inode, written at `second`.
        fn write(&self, path: &str, inode: u64, second: i64) {
            let stamp = Stamp {
                device: 1,
                inode,
                size: 10,
                modified: (second, 0),
                changed: (second, 0),
            };
            self.0.borrow_mut().insert(PathBuf::from(path), stamp);
        }

        fn touch(&self, path: &str, second: i64) {
            let inode = self.0.borrow()[Path::new(path)].inode;
            self.write(path, inode, second);
        }

        fn remove(&self, path: &str) {
            self.0.borrow_mut().remove(Path::new(path));
        }
    }

    impl Metadata for &Files {
        fn stamp(&self, path: &Path) -> Option<Stamp> {
            self.0.borrow().get(path).copied()
        }
    }

    fn paths(paths: &[&str]) -> Vec<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    fn ms(milliseconds: u64) -> Duration {
        Duration::from_millis(milliseconds)
    }

    #[test]
    fn unchanged_files_are_never_reported() {
        let files = Files::with(&["config", "included"]);
        let start = Instant::now();
        let mut watch = ConfigWatch::with_metadata(&files, &paths(&["config", "included"]), start);
        for second in 0..5 {
            assert!(!watch.tick(start + Duration::from_secs(second)));
        }
    }

    #[test]
    fn a_change_is_reported_once_it_has_stayed_put_for_the_settling_time() {
        let files = Files::with(&["config"]);
        let start = Instant::now();
        let mut watch = ConfigWatch::with_metadata(&files, &paths(&["config"]), start);

        files.touch("config", 5);
        // Not looked at before a second has passed.
        assert!(!watch.tick(start + ms(999)));
        // Seen, but not yet settled.
        assert!(!watch.tick(start + ms(1000)));
        assert!(!watch.tick(start + ms(1299)));
        assert!(watch.tick(start + ms(1300)));
    }

    #[test]
    fn a_file_that_keeps_changing_is_reported_once_it_stops() {
        let files = Files::with(&["config"]);
        let start = Instant::now();
        let mut watch = ConfigWatch::with_metadata(&files, &paths(&["config"]), start);

        files.touch("config", 5);
        assert!(!watch.tick(start + ms(1000)));
        files.touch("config", 6);
        assert!(!watch.tick(start + ms(1300)));
        files.touch("config", 7);
        assert!(!watch.tick(start + ms(1600)));
        assert!(watch.tick(start + ms(1900)));
    }

    #[test]
    fn a_change_undone_before_it_settles_is_forgotten() {
        let files = Files::with(&["config"]);
        let start = Instant::now();
        let mut watch = ConfigWatch::with_metadata(&files, &paths(&["config"]), start);

        files.touch("config", 5);
        assert!(!watch.tick(start + ms(1000)));
        files.touch("config", 0);
        assert!(!watch.tick(start + ms(1300)));
        // Back to the slow pace: the next look is a second later, and finds nothing.
        assert!(!watch.tick(start + ms(1600)));
        assert!(!watch.tick(start + ms(2300)));
    }

    #[test]
    fn deleting_creating_or_renaming_over_a_file_is_a_change() {
        type Edit = fn(&Files);
        let edits: [(&str, Edit); 3] = [
            ("deleted", |files| files.remove("config")),
            ("created", |files| files.write("later", 9, 0)),
            // An editor's save by rename: same size and times, another inode.
            ("renamed over", |files| files.write("config", 9, 0)),
        ];
        for (edit, apply) in edits {
            let files = Files::with(&["config"]);
            let start = Instant::now();
            let watched = paths(&["config", "later"]);
            let mut watch = ConfigWatch::with_metadata(&files, &watched, start);

            apply(&files);

            assert!(!watch.tick(start + ms(1000)), "{edit}");
            assert!(watch.tick(start + ms(1300)), "{edit}");
        }
    }

    #[test]
    fn a_settled_change_stays_reported_until_the_inventory_is_read_again() {
        let files = Files::with(&["config"]);
        let start = Instant::now();
        let mut watch = ConfigWatch::with_metadata(&files, &paths(&["config"]), start);

        files.touch("config", 5);
        watch.tick(start + ms(1000));
        assert!(watch.tick(start + ms(1300)));
        assert!(watch.tick(start + ms(9000)));

        watch.rebase(&paths(&["config"]), start + ms(9000));
        assert!(!watch.tick(start + ms(10_000)));
    }

    #[test]
    fn rebasing_takes_what_was_pending_as_read_and_follows_the_new_paths() {
        let files = Files::with(&["config", "old.conf", "new.conf"]);
        let start = Instant::now();
        let mut watch = ConfigWatch::with_metadata(&files, &paths(&["config", "old.conf"]), start);

        // Portico's own write, read back before the change settles: never reported.
        files.touch("config", 5);
        assert!(!watch.tick(start + ms(1000)));
        watch.rebase(&paths(&["config", "new.conf"]), start + ms(1100));
        assert!(!watch.tick(start + ms(1400)));
        assert!(!watch.tick(start + ms(2100)));

        // A file no longer included is no longer watched; a newly included one is.
        files.touch("old.conf", 6);
        assert!(!watch.tick(start + ms(3100)));
        assert!(!watch.tick(start + ms(3400)));
        files.touch("new.conf", 7);
        assert!(!watch.tick(start + ms(4100)));
        assert!(watch.tick(start + ms(4400)));
    }

    #[test]
    fn the_filesystem_stamp_follows_existence_renames_and_access() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        assert_eq!(Filesystem.stamp(&config), None);

        fs::write(&config, "Host a\n").unwrap();
        let written = Filesystem.stamp(&config).unwrap();
        let replacement = dir.path().join("config.tmp");
        fs::write(&replacement, "Host b\n").unwrap();
        fs::rename(&replacement, &config).unwrap();
        let renamed = Filesystem.stamp(&config).unwrap();
        assert_ne!(written.inode, renamed.inode);

        // A path that cannot be looked at counts as absent, so the next read reports why.
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o000)).unwrap();
        let hidden = Filesystem.stamp(&config);
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(hidden, None);
    }
}
