//! The Windows stand-in for a `#!/bin/sh` fake: Windows runs only `.exe` files, so
//! `write_executable` puts a copy of this program at `<name>.exe` beside the script
//! `<name>.sh`. It runs the script with `sh` (the `SH` variable, else Git for Windows' own,
//! else `sh` on `PATH`), passing its arguments, standard input, output and error, and exits
//! with the script's exit code. Tests compile it with `rustc` on first use; it is never part of
//! Portico.
//!
//! The shim and everything the script starts share a job that ends with the shim: Portico
//! stops a program by terminating the process it started, here the shim, and Git's `sh.exe`
//! is itself a wrapper, so without the job the script and its children (a `sleep` standing in
//! for a tunnel) would outlive it, holding files and the caller's pipes open.

use std::{
    env,
    ffi::OsStr,
    path::PathBuf,
    process::{exit, Command},
};

const GIT_SH: &str = r"C:\Program Files\Git\bin\sh.exe";

fn main() {
    // The running file's own path, which `argv[0]` may hold without its directory.
    let script = env::current_exe()
        .expect("the shim knows its own path")
        .with_extension("sh");
    let sh = env::var_os("SH")
        .map(PathBuf::from)
        .or_else(|| Some(PathBuf::from(GIT_SH)).filter(|path| path.is_file()))
        .unwrap_or_else(|| PathBuf::from("sh"));
    #[cfg(windows)]
    job::join_own();
    let mut command = Command::new(&sh);
    // MSYS programs parse their own command line, globbing `*` and `?` and reading `'` and a
    // leading `@`: every word goes in double quotes, and globbing is off besides.
    quoted_arg(&mut command, script.as_os_str());
    for arg in env::args_os().skip(1) {
        quoted_arg(&mut command, &arg);
    }
    let mut msys = env::var_os("MSYS").unwrap_or_default();
    if !msys.is_empty() {
        msys.push(" ");
    }
    msys.push("noglob");
    let status = command.env("MSYS", msys).status().unwrap_or_else(|error| {
        eprintln!(
            "sh_shim: cannot run {} {}: {error}",
            sh.display(),
            script.display()
        );
        exit(127)
    });
    exit(status.code().unwrap_or(1));
}

/// Adds `arg` in double quotes, by the rules of the Microsoft C runtime: a quote is escaped
/// with a backslash, and the backslashes before a quote, or before the closing one, are
/// doubled.
#[cfg(windows)]
fn quoted_arg(command: &mut Command, arg: &OsStr) {
    use std::os::windows::process::CommandExt;
    let text = arg.to_str().expect("arguments are Unicode");
    let mut quoted = String::from('"');
    let mut backslashes = 0;
    for character in text.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' => {
                quoted.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                quoted.extend(std::iter::repeat_n('\\', backslashes));
                quoted.push(character);
                backslashes = 0;
            }
        }
    }
    quoted.extend(std::iter::repeat_n('\\', backslashes * 2));
    quoted.push('"');
    command.raw_arg(quoted);
}

#[cfg(not(windows))]
fn quoted_arg(command: &mut Command, arg: &OsStr) {
    command.arg(arg);
}

#[cfg(windows)]
#[path = "job.rs"]
mod job;
