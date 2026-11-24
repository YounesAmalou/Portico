//! Linux files: locking, atomic writes and file identity.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use fs2::FileExt;

/// Follows the symbolic link chain at `path` to the file a write must replace, which may not
/// exist yet. An error names the path it occurred at.
pub fn symlink_target(start: &Path) -> Result<PathBuf, (PathBuf, io::Error)> {
    let mut path = start.to_path_buf();
    for _ in 0..40 {
        match fs::read_link(&path) {
            Ok(target) => path = path.parent().unwrap_or(Path::new("")).join(target),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::InvalidInput | io::ErrorKind::NotFound
                ) =>
            {
                return Ok(path);
            }
            Err(source) => return Err((path, source)),
        }
    }
    Err((
        path,
        io::Error::other(format!(
            "too many levels of symbolic links from {}",
            start.display()
        )),
    ))
}

pub fn normalize_path(path: &Path) -> PathBuf {
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

pub fn create_private_dir(path: &Path) -> io::Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => fs::set_permissions(path, fs::Permissions::from_mode(0o700)),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        Err(error) => Err(error),
    }
}

pub fn reject_symlink(path: &Path) -> io::Result<()> {
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

pub fn open_lock(path: &Path) -> io::Result<File> {
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

pub fn acquire_lock(file: &File, cancelled: Option<&AtomicBool>) -> io::Result<()> {
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
pub fn read_lossy(path: &Path) -> String {
    fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

pub struct PendingFile {
    path: Option<PathBuf>,
}

impl PendingFile {
    pub fn path(&self) -> &Path {
        self.path.as_deref().expect("pending file has a path")
    }

    pub fn rename_to(mut self, destination: &Path) -> io::Result<()> {
        fs::rename(self.path(), destination)?;
        self.path = None;
        Ok(())
    }

    /// Keeps the file instead of removing it when dropped, and returns its path.
    pub fn keep(mut self) -> PathBuf {
        self.path.take().expect("pending file has a path")
    }
}

impl Drop for PendingFile {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_file(path);
        }
    }
}

pub fn write_unique(dir: &Path, stem: &str, contents: &[u8], mode: u32) -> io::Result<PendingFile> {
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
pub fn fail_next_transaction_rollback_write() {
    FAIL_TRANSACTION_ROLLBACK_WRITE.with(|fail| fail.set(true));
}

pub fn write_named_new(path: &Path, contents: &[u8], mode: u32) -> io::Result<()> {
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

pub fn sync_dir(path: &Path) -> io::Result<()> {
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
pub fn fail_next_directory_sync(path: &Path) {
    fail_directory_sync_after(path, 0);
}

#[cfg(test)]
pub fn fail_directory_sync_after(path: &Path, successful_syncs: usize) {
    DIRECTORY_SYNC_FAILURES.with(|failures| {
        failures
            .borrow_mut()
            .push_back((path.to_owned(), successful_syncs))
    });
}

/// What tells one version of a path from another. An absent or unreadable path has none.
///
/// The inode catches a file renamed into place within one timestamp tick, and the status-change
/// time a copy that kept the old modification time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Stamp {
    pub device: u64,
    pub inode: u64,
    pub size: u64,
    pub modified: (i64, i64),
    pub changed: (i64, i64),
}

/// The stamp of the file at `path`, following symbolic links as reading the file does.
pub fn stamp(path: &Path) -> Option<Stamp> {
    let metadata = fs::metadata(path).ok()?;
    Some(Stamp {
        device: metadata.dev(),
        inode: metadata.ino(),
        size: metadata.size(),
        modified: (metadata.mtime(), metadata.mtime_nsec()),
        changed: (metadata.ctime(), metadata.ctime_nsec()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_symbolic_link_loop_names_the_path_it_started_from() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        std::os::unix::fs::symlink(&second, &first).unwrap();
        std::os::unix::fs::symlink(&first, &second).unwrap();
        let (_, error) = symlink_target(&first).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("too many levels of symbolic links from {}", first.display())
        );
    }
}
