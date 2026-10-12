//! Writes executable scripts that tests run immediately afterwards.
//!
//! Executing a file fails with `ETXTBSY` while any process holds it open for
//! writing. Closing our own handle is not enough: a concurrent `fork` in
//! another test thread duplicates it, and the copy lives until that child
//! reaches `exec`. The only reliable signal is the kernel's own: a read lease
//! cannot be taken while the file has a writer, so the script is handed out
//! once the lease succeeds.

use std::{
    fs,
    io::{self, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{OpenOptionsExt, PermissionsExt},
    },
    path::Path,
    thread,
    time::{Duration, Instant},
};

/// Creates or replaces the executable script at `path`.
pub fn write_executable(path: impl AsRef<Path>, contents: &str) {
    let path = path.as_ref();
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o700)
        .open(path)
        .unwrap();
    file.write_all(contents.as_bytes()).unwrap();
    file.set_permissions(fs::Permissions::from_mode(0o700))
        .unwrap();
    drop(file);
    wait_until_no_writers(path);
}

fn wait_until_no_writers(path: &Path) {
    let file = fs::File::open(path).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match set_lease(&file, libc::F_RDLCK) {
            Ok(()) => {
                set_lease(&file, libc::F_UNLCK).unwrap();
                return;
            }
            Err(error) if error.raw_os_error() == Some(libc::EAGAIN) => {
                assert!(
                    Instant::now() < deadline,
                    "{} is still open for writing",
                    path.display()
                );
                thread::yield_now();
            }
            Err(error) => panic!("could not check {} for writers: {error}", path.display()),
        }
    }
}

fn set_lease(file: &fs::File, lease: libc::c_int) -> io::Result<()> {
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLEASE, lease) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
