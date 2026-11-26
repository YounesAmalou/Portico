//! Process handles. An open handle keeps its process object alive and its ID unassigned, so it
//! pins the process's identity as a pidfd does on Linux: once the process exits, the handle
//! reports it, and no later process gets its ID while the handle is open.

use std::{
    io,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
};

use windows_sys::Win32::{
    Foundation::{ERROR_INVALID_PARAMETER, FILETIME, HANDLE, STILL_ACTIVE, WAIT_OBJECT_0},
    System::Threading::{
        GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_ACCESS_RIGHTS,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
        TerminateProcess, WaitForSingleObject,
    },
};

/// Opens `pid` with `access`. A process that no longer exists is the error `is_gone` names.
pub(super) fn open_process(pid: u32, access: PROCESS_ACCESS_RIGHTS) -> io::Result<OwnedHandle> {
    // SAFETY: no pointers are passed.
    let handle = unsafe { OpenProcess(access, 0, pid) };
    if handle.is_null() {
        let error = io::Error::last_os_error();
        // Windows answers an ID that names no process as an invalid parameter.
        return Err(
            if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
                gone()
            } else {
                error
            },
        );
    }
    // SAFETY: a handle `OpenProcess` just returned, owned by nothing else.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

fn gone() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "the process has exited")
}

/// Whether `error` says the process is gone, as `ESRCH` does on Unix.
pub(super) fn is_gone(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound
}

fn raw(handle: &OwnedHandle) -> HANDLE {
    handle.as_raw_handle()
}

/// When the process `handle` refers to was created: 100 ns intervals since 1601.
pub(super) fn creation_time(handle: &OwnedHandle) -> io::Result<u64> {
    let mut times = [FILETIME::default(); 4];
    let [created, exited, kernel, user] = &mut times;
    // SAFETY: four valid `FILETIME`s for the call to fill.
    if unsafe { GetProcessTimes(raw(handle), created, exited, kernel, user) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(u64::from(created.dwHighDateTime) << 32 | u64::from(created.dwLowDateTime))
}

/// Whether the process `handle` refers to has exited.
pub(super) fn has_exited(handle: HANDLE) -> bool {
    // SAFETY: waiting with no timeout only reads the handle's state.
    match unsafe { WaitForSingleObject(handle, 0) } {
        WAIT_OBJECT_0 => true,
        // A handle opened without the right to wait on it: its exit code says it instead.
        _ => {
            let mut code = 0;
            // SAFETY: a valid `u32` for the call to fill.
            unsafe { GetExitCodeProcess(handle, &mut code) != 0 && code != STILL_ACTIVE as u32 }
        }
    }
}

/// A process handle that can end the process and wait for its exit; it never reaches a later
/// process with the same ID.
pub(crate) struct Pin {
    handle: OwnedHandle,
}

impl Pin {
    /// Pins `pid`; `is_gone` when it no longer exists.
    pub(crate) fn open(pid: u32) -> io::Result<Self> {
        Self::open_with(
            pid,
            PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
        )
    }

    /// Pins `pid` with only what ending it takes, for when the full pin cannot be opened: it
    /// still ends the process, but cannot wait for its exit.
    pub(super) fn open_to_terminate(pid: u32) -> io::Result<Self> {
        Self::open_with(pid, PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION)
    }

    fn open_with(pid: u32, access: PROCESS_ACCESS_RIGHTS) -> io::Result<Self> {
        Ok(Self {
            handle: open_process(pid, access)?,
        })
    }

    /// Ends the process; `is_gone` when it had already exited, so it is never ended twice.
    pub(crate) fn terminate(&self) -> io::Result<()> {
        if has_exited(raw(&self.handle)) {
            return Err(gone());
        }
        // SAFETY: a process handle with the right to terminate it.
        if unsafe { TerminateProcess(raw(&self.handle), 1) } != 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        // A process that exits meanwhile refuses with access denied.
        Err(if has_exited(raw(&self.handle)) {
            gone()
        } else {
            error
        })
    }

    /// Whether the process exits within `wait` milliseconds (or already has).
    pub(crate) fn wait_exit(&self, wait: u32) -> bool {
        // SAFETY: waiting only reads the handle's state.
        unsafe { WaitForSingleObject(raw(&self.handle), wait) == WAIT_OBJECT_0 }
    }

    pub(super) fn creation_time(&self) -> io::Result<u64> {
        creation_time(&self.handle)
    }
}

/// Pins `pid`, unless `fail_pin_after` armed this open to fail as access denied.
pub(super) fn open_pin(pid: u32) -> io::Result<Pin> {
    #[cfg(test)]
    if super::super::super::common::process::pin_open_fails() {
        return Err(io::Error::from_raw_os_error(
            windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED as i32,
        ));
    }
    Pin::open(pid)
}
