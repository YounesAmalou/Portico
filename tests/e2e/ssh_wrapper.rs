//! The Windows stand-in for the end-to-end suite's `ssh` wrapper script (`client.rs`): Windows
//! runs only `.exe` files, so the client's `ssh` is this program, compiled with `rustc` on first
//! use. It is never part of Portico.
//!
//! It hands OpenSSH (`PORTICO_E2E_SSH`) a flattened copy of the temporary `~/.ssh/config` with
//! `~/` spelled out and a closing `Host *` block that keeps known hosts in the temporary home and
//! leaves the agent alone, as the script does; a command line that already has `-F` passes
//! through unchanged.
//!
//! Windows cannot replace a process the way `exec` does, so this program stays and waits for
//! ssh, in a job that ends ssh with it: Portico stops a tunnel by terminating the process it
//! started, which is this one. Portico also finds a tunnel by the `-E <log>` pair on its command
//! line, which must name only that process: ssh gets the pair joined into one argument
//! (`-E<log>`), which it reads the same way.

use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::{exit, Command},
};

#[path = "../support/job.rs"]
mod job;

fn main() {
    let ssh = env::var_os("PORTICO_E2E_SSH").unwrap_or_else(|| {
        eprintln!("ssh_wrapper: PORTICO_E2E_SSH is not set");
        exit(127)
    });
    let args: Vec<OsString> = env::args_os().skip(1).collect();
    job::join_own();
    let mut command = Command::new(&ssh);
    let flat = if args.iter().any(|arg| arg == "-F") {
        command.args(&args);
        None
    } else {
        let home = env::var_os("USERPROFILE").unwrap_or_else(|| {
            eprintln!("ssh_wrapper: USERPROFILE is not set");
            exit(127)
        });
        let flat = flat_config(Path::new(&home));
        command.arg("-F").arg(&flat).args(join_log_option(args));
        Some(flat)
    };
    let status = command.status().unwrap_or_else(|error| {
        eprintln!("ssh_wrapper: cannot run {}: {error}", Path::new(&ssh).display());
        exit(127)
    });
    if let Some(flat) = flat {
        let _ = fs::remove_file(flat);
    }
    exit(status.code().unwrap_or(1));
}

/// Writes this process's own flattened copy of `home`'s `.ssh/config` and returns its path.
/// Portico runs several ssh processes at once, and Windows cannot replace a file another
/// process has open, so each one reads a copy of its own.
fn flat_config(home: &Path) -> PathBuf {
    // OpenSSH's configuration reads `/` in Windows paths, and a `\` may start an escape.
    let home_text = home.to_string_lossy().replace('\\', "/");
    let ssh_dir = home.join(".ssh");
    fs::create_dir_all(&ssh_dir).unwrap();
    let mut text = fs::read_to_string(ssh_dir.join("config"))
        .unwrap_or_default()
        .replace("~/", &format!("{home_text}/"));
    text.push_str(&format!(
        "\nHost *\n  UserKnownHostsFile \"{home_text}/.ssh/known_hosts\"\n  \
         GlobalKnownHostsFile \"{home_text}/.ssh/no_global_known_hosts\"\n  \
         IdentityAgent none\n  IdentitiesOnly yes\n"
    ));
    let flat = ssh_dir.join(format!("e2e-flat-config.{}", std::process::id()));
    fs::write(&flat, text).unwrap();
    flat
}

/// `args` with each `-E <log>` before `--` joined into `-E<log>`.
fn join_log_option(args: Vec<OsString>) -> Vec<OsString> {
    let mut joined = Vec::with_capacity(args.len());
    let mut rest = args.into_iter();
    while let Some(arg) = rest.next() {
        if arg == "--" {
            joined.push(arg);
            joined.extend(rest);
            break;
        }
        if arg == "-E" {
            if let Some(log) = rest.next() {
                let mut option = arg;
                option.push(log);
                joined.push(option);
                continue;
            }
        }
        joined.push(arg);
    }
    joined
}
