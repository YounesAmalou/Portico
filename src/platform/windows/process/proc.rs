//! Reading the process table: the Toolhelp snapshot lists processes, and a handle opened for
//! limited queries reads each one's start, owner and command line.

use std::{
    ffi::OsString,
    io,
    os::windows::{
        ffi::OsStringExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::PathBuf,
    slice,
};

use windows_sys::{
    Wdk::System::Threading::{NtQueryInformationProcess, ProcessCommandLineInformation},
    Win32::{
        Foundation::{
            ERROR_SUCCESS, HANDLE, INVALID_HANDLE_VALUE, LocalFree, STATUS_BUFFER_OVERFLOW,
            STATUS_BUFFER_TOO_SMALL, STATUS_INFO_LENGTH_MISMATCH, UNICODE_STRING,
        },
        Security::{
            Authorization::ConvertSidToStringSidW, GetTokenInformation, TOKEN_QUERY, TOKEN_USER,
            TokenUser,
        },
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
                TH32CS_SNAPPROCESS,
            },
            Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RegGetValueW},
            Threading::{OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE},
        },
        UI::Shell::CommandLineToArgvW,
    },
};

use super::{
    Uid,
    pin::{creation_time, has_exited, open_process},
};

/// A process as Portico describes one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Stat {
    /// When the process was created: 100 ns intervals since 1601 (a `FILETIME`).
    pub start: u64,
    /// `R` while it runs and `Z` once it has exited but a handle still holds it, as Unix spells
    /// a process that has not been reaped.
    pub state: char,
}

impl Stat {
    /// Whether the process `pid` this describes runs in the background. Windows' ssh has no
    /// background mode: Portico starts it detached itself, and a start holds `start.lock` until
    /// it has written the record, so every live process counts.
    pub(crate) fn in_background(&self, _pid: u32) -> bool {
        true
    }
}

/// Reads the process table.
#[derive(Clone, Debug)]
pub struct Proc {
    _private: (),
}

impl Proc {
    pub fn system() -> Self {
        Self { _private: () }
    }

    /// The number Windows gives this boot, as text: the `BootId` the kernel's prefetcher keeps
    /// in the registry, which counts boots. `None` when it cannot be read; a record's exact
    /// creation time still tells its process from any other.
    pub fn boot_id(&self) -> Option<String> {
        let key: Vec<u16> =
            r"SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management\PrefetchParameters"
                .encode_utf16()
                .chain([0])
                .collect();
        let value: Vec<u16> = "BootId".encode_utf16().chain([0]).collect();
        let mut id = 0u32;
        let mut size = size_of::<u32>() as u32;
        // SAFETY: NUL-terminated names, and a `u32` of the size passed for the value.
        let status = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                key.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_DWORD,
                std::ptr::null_mut(),
                (&raw mut id).cast(),
                &mut size,
            )
        };
        (status == ERROR_SUCCESS).then(|| id.to_string())
    }

    /// Unix seconds at which a process with `start` (a `FILETIME`) started.
    pub fn started_at(&self, start: u64) -> Option<u64> {
        const SECONDS_FROM_1601_TO_1970: u64 = 11_644_473_600;
        (start / 10_000_000).checked_sub(SECONDS_FROM_1601_TO_1970)
    }

    pub fn stat(&self, pid: u32) -> Option<Stat> {
        let process =
            open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE).ok()?;
        let start = creation_time(&process).ok()?;
        let state = if has_exited(process.as_raw_handle()) {
            'Z'
        } else {
            'R'
        };
        Some(Stat { start, state })
    }

    /// The user `pid` runs as: its token's user.
    pub fn uid(&self, pid: u32) -> Option<Uid> {
        let process = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION).ok()?;
        user_sid(process.as_raw_handle())
    }

    /// The arguments of `pid`, split as Windows' `CommandLineToArgvW` splits them, each as the
    /// bytes `OsStr::as_encoded_bytes` gives (UTF-8 for any argument that is valid text).
    pub fn cmdline(&self, pid: u32) -> Option<Vec<Vec<u8>>> {
        let process = open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION).ok()?;
        split_command_line(&command_line(process.as_raw_handle())?)
    }

    pub fn pids(&self) -> Vec<u32> {
        process_table()
            .map(|table| table.into_iter().map(|(pid, _)| pid).collect())
            .unwrap_or_default()
    }
}

/// The path an argument of `Proc::cmdline` names.
pub(crate) fn arg_path(arg: &[u8]) -> PathBuf {
    // SAFETY: `cmdline` makes each argument with `OsString::into_encoded_bytes`, in this
    // program, and the argument is passed whole.
    PathBuf::from(unsafe { OsString::from_encoded_bytes_unchecked(arg.to_vec()) })
}

/// Every process's ID with its parent's. A parent ID may name a process that has exited, or a
/// later one that got its ID.
pub(super) fn process_table() -> io::Result<Vec<(u32, u32)>> {
    // SAFETY: no pointers are passed.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a handle the call just returned, owned by nothing else.
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut table = Vec::new();
    // SAFETY: an entry whose size field is set, as both calls require.
    let mut more = unsafe { Process32FirstW(snapshot.as_raw_handle(), &mut entry) } != 0;
    while more {
        table.push((entry.th32ProcessID, entry.th32ParentProcessID));
        // SAFETY: as above.
        more = unsafe { Process32NextW(snapshot.as_raw_handle(), &mut entry) } != 0;
    }
    Ok(table)
}

/// The string form (`S-1-5-21-…`) of the user of the process `process`.
pub(super) fn user_sid(process: HANDLE) -> Option<String> {
    let mut token = std::ptr::null_mut();
    // SAFETY: a valid pointer for the token handle.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return None;
    }
    // SAFETY: a handle the call just returned, owned by nothing else.
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let mut length = 0;
    // The first call fails, reporting the size the information needs.
    // SAFETY: a null buffer of length 0 and a valid pointer for the length.
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            std::ptr::null_mut(),
            0,
            &mut length,
        )
    };
    // `u64`s align the buffer for the structure it holds.
    let mut buffer = vec![0u64; (length as usize).div_ceil(8)];
    // SAFETY: a buffer of at least `length` bytes.
    if length == 0
        || unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                buffer.as_mut_ptr().cast(),
                length,
                &mut length,
            )
        } == 0
    {
        return None;
    }
    // SAFETY: the call filled the aligned buffer with a `TOKEN_USER`.
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut text = std::ptr::null_mut();
    // SAFETY: the SID the token information holds, and a valid pointer for the string.
    if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) } == 0 {
        return None;
    }
    // SAFETY: the call returned a NUL-terminated string, which `LocalFree` releases.
    let sid = String::from_utf16(unsafe { wide_until_nul(text) }).ok();
    unsafe { LocalFree(text.cast()) };
    sid
}

/// The NUL-terminated wide string at `text`.
///
/// SAFETY: `text` points to a NUL-terminated wide string that outlives the slice.
unsafe fn wide_until_nul<'a>(text: *const u16) -> &'a [u16] {
    let mut length = 0;
    // SAFETY: the caller promises a terminating NUL.
    while unsafe { *text.add(length) } != 0 {
        length += 1;
    }
    // SAFETY: `length` units before the NUL.
    unsafe { slice::from_raw_parts(text, length) }
}

/// The command line of the process `process`, which the process may have changed since it
/// started, as Unix's `/proc/<pid>/cmdline` may be.
fn command_line(process: HANDLE) -> Option<Vec<u16>> {
    // The first call reports the size the command line needs. The process may change its
    // command line meanwhile, so a size that no longer fits is asked again, a few times.
    let mut length = 0;
    let mut buffer: Vec<u64> = Vec::new();
    let mut tries = 0;
    loop {
        // `u64`s align the buffer for the `UNICODE_STRING` it starts with.
        buffer.resize((length as usize).div_ceil(8), 0);
        // SAFETY: a buffer of `length` bytes or more (null and 0 on the first call), and a valid
        // pointer for the length.
        let status = unsafe {
            NtQueryInformationProcess(
                process,
                ProcessCommandLineInformation,
                if length == 0 {
                    std::ptr::null_mut()
                } else {
                    buffer.as_mut_ptr().cast()
                },
                length,
                &mut length,
            )
        };
        if status >= 0 && !buffer.is_empty() {
            break;
        }
        let too_small = matches!(
            status,
            STATUS_INFO_LENGTH_MISMATCH | STATUS_BUFFER_TOO_SMALL | STATUS_BUFFER_OVERFLOW
        );
        tries += 1;
        if !too_small || length == 0 || tries > 3 {
            return None;
        }
    }
    // SAFETY: the call filled the aligned buffer with a `UNICODE_STRING` whose text follows it
    // in the same buffer.
    let text = unsafe { &*buffer.as_ptr().cast::<UNICODE_STRING>() };
    if text.Buffer.is_null() {
        return Some(Vec::new());
    }
    // SAFETY: `Length` bytes of text at `Buffer`, inside `buffer`.
    Some(unsafe { slice::from_raw_parts(text.Buffer, usize::from(text.Length) / 2) }.to_vec())
}

/// Splits a command line into its arguments with Windows' rules.
fn split_command_line(line: &[u16]) -> Option<Vec<Vec<u8>>> {
    // An empty line would be read as the path of this program.
    if line
        .iter()
        .all(|&unit| unit == u16::from(b' ') || unit == u16::from(b'\t'))
    {
        return Some(Vec::new());
    }
    let mut text = line.to_vec();
    text.push(0);
    let mut count = 0;
    // SAFETY: a NUL-terminated wide string and a valid pointer for the count.
    let argv = unsafe { CommandLineToArgvW(text.as_ptr(), &mut count) };
    if argv.is_null() {
        return None;
    }
    let args = (0..usize::try_from(count).unwrap_or(0))
        .map(|index| {
            // SAFETY: `count` NUL-terminated strings, which live until `LocalFree` below.
            let arg = unsafe { wide_until_nul(*argv.add(index)) };
            OsString::from_wide(arg).into_encoded_bytes()
        })
        .collect();
    // SAFETY: the array `CommandLineToArgvW` allocated, freed once.
    unsafe { LocalFree(argv.cast()) };
    Some(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    #[test]
    fn a_command_line_splits_with_windows_quoting() {
        assert_eq!(
            split_command_line(&wide(r#"ssh.exe -E "C:\Users\A B\db.log" -- prod"#)),
            Some(vec![
                b"ssh.exe".to_vec(),
                b"-E".to_vec(),
                br"C:\Users\A B\db.log".to_vec(),
                b"--".to_vec(),
                b"prod".to_vec(),
            ])
        );
        assert_eq!(split_command_line(&wide("")), Some(Vec::new()));
    }
}
