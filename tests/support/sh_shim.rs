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
mod job {
    use std::{ffi::c_void, ptr};

    type Handle = *mut c_void;

    #[repr(C)]
    struct BasicLimits {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    /// `JOBOBJECT_EXTENDED_LIMIT_INFORMATION`.
    #[repr(C)]
    struct ExtendedLimits {
        basic: BasicLimits,
        io_counters: [u64; 6],
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateJobObjectW(attributes: *mut c_void, name: *const u16) -> Handle;
        fn SetInformationJobObject(job: Handle, class: i32, info: *mut c_void, length: u32) -> i32;
        fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
        fn GetCurrentProcess() -> Handle;
    }

    /// Puts this process in a new job whose processes all end when its last handle closes,
    /// which is when this process ends; the children it starts join the job. The handle is
    /// deliberately never closed. Without a job the script still runs, only unguarded.
    pub fn join_own() {
        // SAFETY: plain calls with valid arguments; `limits` is zeroed plain data of the
        // size passed, and a null job handle is checked.
        unsafe {
            let job = CreateJobObjectW(ptr::null_mut(), ptr::null());
            if job.is_null() {
                return;
            }
            let mut limits: ExtendedLimits = std::mem::zeroed();
            limits.basic.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let set = SetInformationJobObject(
                job,
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                (&mut limits as *mut ExtendedLimits).cast(),
                size_of::<ExtendedLimits>() as u32,
            );
            if set != 0 {
                AssignProcessToJobObject(job, GetCurrentProcess());
            }
        }
    }
}
