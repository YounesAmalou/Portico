//! Unix files (Linux and macOS): permissions, symbolic links, descriptors and file identity.

use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    os::{
        fd::{FromRawFd, OwnedFd, RawFd},
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use super::super::common::fs::{PendingFile, Stamp};
#[cfg(test)]
use super::super::common::fs::{take_directory_sync_failure, take_rollback_write_failure};

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

pub fn create_private_dir(path: &Path) -> io::Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => fs::set_permissions(path, fs::Permissions::from_mode(0o700)),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        Err(error) => Err(error),
    }
}

pub fn write_unique(dir: &Path, stem: &str, contents: &[u8], mode: u32) -> io::Result<PendingFile> {
    #[cfg(test)]
    if stem == ".config.portico-rollback" && take_rollback_write_failure() {
        return Err(io::Error::other("injected rollback write failure"));
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    for attempt in 0..1000 {
        let path = dir.join(format!("{stem}-{}-{nonce}-{attempt}", std::process::id()));
        match write_named_new(&path, contents, mode) {
            Ok(()) => return Ok(PendingFile::new(path)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique temporary file",
    ))
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

pub fn sync_dir(path: &Path) -> io::Result<()> {
    #[cfg(test)]
    if take_directory_sync_failure(path) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "injected directory sync failure",
        ));
    }
    fs::File::open(path)?.sync_all()
}

/// A new socket that is closed on `exec`. Linux sets the flag atomically; macOS has no
/// `SOCK_CLOEXEC`, so it is set straight after (Portico starts no process between the two).
pub fn cloexec_socket(domain: libc::c_int, kind: libc::c_int) -> io::Result<OwnedFd> {
    #[cfg(target_os = "linux")]
    let kind = kind | libc::SOCK_CLOEXEC;
    // SAFETY: `socket` has no preconditions; a valid descriptor is owned at once.
    let fd = unsafe { libc::socket(domain, kind, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let socket = unsafe { OwnedFd::from_raw_fd(fd) };
    #[cfg(not(target_os = "linux"))]
    set_cloexec(std::os::fd::AsRawFd::as_raw_fd(&socket))?;
    Ok(socket)
}

/// A new pipe whose ends are non-blocking and closed on `exec`, as `[read, write]`. Linux
/// sets the flags atomically with `pipe2`; macOS has none, so they are set straight after.
pub fn nonblocking_pipe() -> io::Result<[RawFd; 2]> {
    let mut ends = [-1; 2];
    #[cfg(target_os = "linux")]
    // SAFETY: `pipe2` writes two new descriptors into `ends`.
    let created = unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) };
    #[cfg(not(target_os = "linux"))]
    // SAFETY: `pipe` writes two new descriptors into `ends`.
    let created = unsafe { libc::pipe(ends.as_mut_ptr()) };
    if created == -1 {
        return Err(io::Error::last_os_error());
    }
    #[cfg(not(target_os = "linux"))]
    for end in ends {
        let flags = set_cloexec(end).and_then(|()| {
            // SAFETY: `fcntl` on a descriptor owned here.
            let flags = unsafe { libc::fcntl(end, libc::F_GETFL) };
            if flags == -1
                || unsafe { libc::fcntl(end, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
            {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
        if let Err(error) = flags {
            // SAFETY: both descriptors were created above and are not used again.
            unsafe {
                libc::close(ends[0]);
                libc::close(ends[1]);
            }
            return Err(error);
        }
    }
    Ok(ends)
}

#[cfg(not(target_os = "linux"))]
fn set_cloexec(fd: RawFd) -> io::Result<()> {
    // SAFETY: `fcntl` on a descriptor the caller owns.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
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
