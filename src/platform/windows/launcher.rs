//! Opening a command in a new terminal window on Windows: a new Windows Terminal window
//! through `wt.exe`, else a new console window.

use std::{
    env,
    ffi::{OsStr, OsString},
    io,
    os::windows::{
        ffi::OsStrExt,
        io::{FromRawHandle, OwnedHandle},
        process::ExitStatusExt,
    },
    path::{Path, PathBuf},
    process::ExitStatus,
    ptr,
};

use windows_sys::Win32::{
    Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::Threading::{
        CREATE_NEW_CONSOLE, CreateProcessW, GetExitCodeProcess, PROCESS_INFORMATION, STARTUPINFOW,
        WaitForSingleObject,
    },
};

use super::{
    super::common::launcher::{LAUNCH_GRACE, Launch, run, spawn_detached},
    process::keep_standard_handles,
};

/// The launcher run when none is configured: Windows Terminal, which Portico drives itself.
pub const DEFAULT_LAUNCHER: &str = "wt";

/// Where a new terminal window is opened.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Choice {
    /// A launcher program, run with the command as its arguments like `xdg-terminal-exec`.
    Program(PathBuf),
    /// A new console window of its own.
    Console,
    /// A new Windows Terminal window, through `wt.exe`.
    WindowsTerminal,
}

/// Opens `url` in the default browser with `start`, through `cmd /c` (it is a shell built-in),
/// without waiting for it. The empty argument is the window title `start` would otherwise take
/// from a quoted URL.
pub fn open_url(url: &str) -> io::Result<()> {
    spawn_detached("cmd", &["/c", "start", "", url])
}

/// `portico_terminal` when it names an executable or `cmd` (a new console window); otherwise
/// Windows Terminal when `wt_found`, else a new console window.
pub fn choose(portico_terminal: Option<&OsStr>, wt_found: bool) -> Choice {
    if let Some(value) = portico_terminal.filter(|value| !value.is_empty()) {
        if value.eq_ignore_ascii_case("cmd") {
            return Choice::Console;
        }
        let path = Path::new(value);
        if path.is_absolute() && is_executable(path) {
            return Choice::Program(path.to_owned());
        }
    }
    if wt_found {
        Choice::WindowsTerminal
    } else {
        Choice::Console
    }
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && super::fs::is_executable(path, &metadata))
}

/// The arguments of `wt.exe` that run `command` in a new window.
pub fn wt_args(command: &[OsString]) -> Vec<OsString> {
    let mut arguments: Vec<OsString> = ["-w", "new", "--"].map(OsString::from).into();
    arguments.extend(command.iter().cloned());
    arguments
}

/// `wt.exe` in a directory of `PATH`. It is an App Execution Alias (under
/// `%LOCALAPPDATA%\Microsoft\WindowsApps`), a reparse point that is looked at, not followed.
fn find_wt() -> Option<PathBuf> {
    env::split_paths(&env::var_os("PATH")?)
        .map(|dir| dir.join("wt.exe"))
        .find(|path| path.is_absolute() && std::fs::symlink_metadata(path).is_ok())
}

/// Opens `command` in a new terminal window. `DEFAULT_LAUNCHER` chooses the terminal (see
/// `choose`); any other launcher is run with the command as its arguments. The launcher and
/// what it starts inherit none of Portico's standard handles, so a launcher that stays (or the
/// terminal it opens) never keeps a pipe Portico writes to open after Portico exits.
pub fn launch(launcher: &Path, command: &[OsString]) -> io::Result<Launch> {
    keep_standard_handles();
    if launcher != Path::new(DEFAULT_LAUNCHER) {
        return run(launcher, command);
    }
    let wt = find_wt();
    match choose(env::var_os("PORTICO_TERMINAL").as_deref(), wt.is_some()) {
        Choice::Program(program) => run(&program, command),
        Choice::WindowsTerminal => run(
            wt.as_deref().unwrap_or(Path::new("wt.exe")),
            &wt_args(command),
        ),
        Choice::Console => new_console(command),
    }
}

/// Starts `command` in a new console window, with the same outcomes as `run`.
///
/// `CreateProcessW` is called directly: the standard library always hands the child this
/// process's standard handles, which would keep it writing to Portico's console instead of its
/// own. Here the child gets none, so it uses the new console's.
fn new_console(command: &[OsString]) -> io::Result<Launch> {
    let mut line = command_line(command);
    let startup = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        ..STARTUPINFOW::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    // SAFETY: `line` is a writable, NUL-terminated wide string, `startup` and `process` are
    // valid for the call, and the other pointers are null, which the call accepts.
    let created = unsafe {
        CreateProcessW(
            ptr::null(),
            line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            0,
            CREATE_NEW_CONSOLE,
            ptr::null(),
            ptr::null(),
            &startup,
            &mut process,
        )
    };
    if created == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both handles were just returned by `CreateProcessW` and are owned here.
    let (child, _thread) = unsafe {
        (
            OwnedHandle::from_raw_handle(process.hProcess),
            OwnedHandle::from_raw_handle(process.hThread),
        )
    };
    let handle = std::os::windows::io::AsRawHandle::as_raw_handle(&child);
    // SAFETY: `handle` is a valid process handle for the duration of the calls.
    match unsafe { WaitForSingleObject(handle, LAUNCH_GRACE.as_millis() as u32) } {
        WAIT_TIMEOUT => Ok(Launch::Requested),
        WAIT_OBJECT_0 => {
            let mut code = 0;
            // SAFETY: `code` is a valid location for the exit code.
            if unsafe { GetExitCodeProcess(handle, &mut code) } == 0 {
                return Err(io::Error::last_os_error());
            }
            let status = ExitStatus::from_raw(code);
            Ok(if status.success() {
                Launch::Requested
            } else {
                Launch::Failed(status)
            })
        }
        _ => Err(io::Error::last_os_error()),
    }
}

/// `command` as one NUL-terminated command line, each word quoted as `CommandLineToArgvW`
/// and the C runtime read it back.
fn command_line(command: &[OsString]) -> Vec<u16> {
    let mut line = Vec::new();
    for (index, word) in command.iter().enumerate() {
        if index > 0 {
            line.push(u16::from(b' '));
        }
        quote(word, &mut line);
    }
    line.push(0);
    line
}

/// Appends `word` to `line`, in double quotes when it is empty or holds a blank or a quote: a
/// quote is escaped with a backslash, and the backslashes before a quote, or before the
/// closing one, are doubled.
fn quote(word: &OsStr, line: &mut Vec<u16>) {
    const QUOTE: u16 = b'"' as u16;
    const BACKSLASH: u16 = b'\\' as u16;
    let wide: Vec<u16> = word.encode_wide().collect();
    let plain = !wide.is_empty()
        && !wide
            .iter()
            .any(|&unit| matches!(unit, 0x20 | 0x09 | 0x0a | 0x0b | QUOTE));
    if plain {
        line.extend(wide);
        return;
    }
    line.push(QUOTE);
    let mut backslashes = 0;
    for unit in wide {
        match unit {
            BACKSLASH => backslashes += 1,
            QUOTE => {
                line.extend(std::iter::repeat_n(BACKSLASH, backslashes * 2 + 1));
                line.push(QUOTE);
                backslashes = 0;
            }
            _ => {
                line.extend(std::iter::repeat_n(BACKSLASH, backslashes));
                line.push(unit);
                backslashes = 0;
            }
        }
    }
    line.extend(std::iter::repeat_n(BACKSLASH, backslashes * 2));
    line.push(QUOTE);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    #[test]
    fn wt_command_line_quotes_spaces() {
        assert_eq!(
            wt_args(&words(&[r"C:\Program Files\x.exe", "a b"])),
            words(&["-w", "new", "--", r"C:\Program Files\x.exe", "a b"])
        );
    }

    #[test]
    fn falls_back_to_console_without_wt() {
        assert_eq!(choose(None, false), Choice::Console);
        assert_eq!(choose(None, true), Choice::WindowsTerminal);
        assert_eq!(choose(Some(OsStr::new("")), true), Choice::WindowsTerminal);
        assert_eq!(choose(Some(OsStr::new("CMD")), true), Choice::Console);
        assert_eq!(
            choose(Some(OsStr::new("no-such-terminal")), false),
            Choice::Console
        );
    }

    #[test]
    fn an_executable_named_by_path_is_run_as_the_launcher() {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("launcher.cmd");
        std::fs::write(&program, "@echo off\r\n").unwrap();
        assert_eq!(
            choose(Some(program.as_os_str()), true),
            Choice::Program(program.clone())
        );
        let text = dir.path().join("launcher.txt");
        std::fs::write(&text, "").unwrap();
        assert_eq!(
            choose(Some(text.as_os_str()), true),
            Choice::WindowsTerminal,
            "a file that is not executable is no launcher"
        );
    }

    #[test]
    fn the_console_command_line_is_quoted_for_the_c_runtime() {
        let line = command_line(&words(&[
            r"C:\Program Files\portico.exe",
            "connect",
            "",
            r#"say "hi""#,
            r"C:\dir with space\",
            r"a\\b",
        ]));
        assert_eq!(
            String::from_utf16(&line[..line.len() - 1]).unwrap(),
            r#""C:\Program Files\portico.exe" connect "" "say \"hi\"" "C:\dir with space\\" a\\b"#
        );
        assert_eq!(line.last(), Some(&0));
    }
}
