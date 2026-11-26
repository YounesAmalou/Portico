//! The Windows stand-in for a `#!/bin/sh` fake: Windows runs only `.exe` files, so
//! `write_executable` puts a copy of this program at `<name>.exe` beside the script
//! `<name>.sh`. It runs the script with `sh` (the `SH` variable, else Git for Windows' own,
//! else `sh` on `PATH`), passing its arguments, standard input, output and error, and exits
//! with the script's exit code. Tests compile it with `rustc` on first use; it is never part of
//! Portico.
//!
//! The arguments travel in the environment, not on `sh`'s command line: MSYS programs parse
//! their own command line, and no quoting survives that parse for every argument (a `"` inside
//! an argument loses its escape). The shim sets `PORTICO_SHIM_ARGC` and
//! `PORTICO_SHIM_ARG_<n>`, and the prelude `write_executable` puts at the top of the script
//! turns them back into `"$@"`, and restores `HOME` as given. MSYS's conversion of path-like arguments for native programs
//! is turned off, so a script passes its arguments on to other programs as it got them.
//!
//! The shim and everything the script starts share a job that ends with the shim when it is
//! terminated: Portico stops a program by terminating the process it started, here the shim,
//! and Git's `sh.exe` is itself a wrapper, so without the job the script and its children (a
//! `sleep` standing in for a tunnel) would outlive it, holding files and the caller's pipes
//! open. When the script exits on its own, the job lets its background children go on, as
//! they would on Unix.

use std::{
    env,
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
    let job = job::join_own();
    let mut command = Command::new(&sh);
    command
        .arg(&script)
        .env("MSYS_NO_PATHCONV", "1")
        .env("MSYS2_ARG_CONV_EXCL", "*");
    let mut count = 0;
    for (index, arg) in env::args_os().skip(1).enumerate() {
        command.env(format!("PORTICO_SHIM_ARG_{}", index + 1), arg);
        count = index + 1;
    }
    // MSYS rewrites `HOME` into its own spelling (`/c/Users/…`); the prelude puts back the
    // one this program was given, which is what Portico and Windows' programs read.
    if let Some(home) = env::var_os("HOME") {
        command.env("PORTICO_SHIM_HOME", home);
    }
    let status = command
        .env("PORTICO_SHIM_ARGC", count.to_string())
        .status()
        .unwrap_or_else(|error| {
            eprintln!(
                "sh_shim: cannot run {} {}: {error}",
                sh.display(),
                script.display()
            );
            exit(127)
        });
    #[cfg(windows)]
    if let Some(job) = job {
        job.let_go();
    }
    exit(status.code().unwrap_or(1));
}

#[cfg(windows)]
#[path = "job.rs"]
mod job;
