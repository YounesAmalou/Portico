//! One tray app per user session: an exclusive lock held for the process's life.

use std::io;
use std::path::Path;

/// Holds the lock; dropping it (or the process ending) releases it.
#[derive(Debug)]
pub struct Instance {
    #[cfg(unix)]
    _file: std::fs::File,
    #[cfg(windows)]
    _handle: windows_sys::Win32::Foundation::HANDLE,
}

/// Takes the lock on `dir/tray.lock` (Unix: `flock`) or the named mutex
/// `Local\PorticoTray` (Windows, where `dir` is unused). `None` when another
/// instance holds it.
#[cfg(unix)]
pub fn acquire(dir: &Path) -> io::Result<Option<Instance>> {
    use std::os::fd::AsRawFd;

    std::fs::create_dir_all(dir)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("tray.lock"))?;
    // SAFETY: the descriptor is open for as long as `file` lives.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(Instance { _file: file }));
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::WouldBlock {
        Ok(None)
    } else {
        Err(error)
    }
}

#[cfg(windows)]
pub fn acquire(_dir: &Path) -> io::Result<Option<Instance>> {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError};
    use windows_sys::Win32::System::Threading::CreateMutexW;

    let name: Vec<u16> = "Local\\PorticoTray\0".encode_utf16().collect();
    // SAFETY: `name` is a NUL-terminated UTF-16 string that outlives the call.
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: read straight after the call, before anything can change it.
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        // SAFETY: `handle` is valid and owned here.
        unsafe { CloseHandle(handle) };
        return Ok(None);
    }
    Ok(Some(Instance { _handle: handle }))
}
