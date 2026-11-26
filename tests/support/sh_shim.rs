//! The Windows stand-in for a `#!/bin/sh` fake: Windows runs only `.exe` files, so
//! `write_executable` puts a copy of this program at `<name>.exe` beside the script
//! `<name>.sh`. It runs the script with `sh` (the `SH` variable, else Git for Windows' own,
//! else `sh` on `PATH`), passing its arguments, standard input, output and error, and exits
//! with the script's exit code. Tests compile it with `rustc` on first use; it is never part of
//! Portico.

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
    let status = Command::new(&sh)
        .arg(&script)
        .args(env::args_os().skip(1))
        .status()
        .unwrap_or_else(|error| {
            eprintln!(
                "sh_shim: cannot run {} {}: {error}",
                sh.display(),
                script.display()
            );
            exit(127)
        });
    exit(status.code().unwrap_or(1));
}
