//! Running a terminal launcher: a program that opens a command in a new terminal window.

use std::{
    ffi::OsString,
    io,
    path::Path,
    process::{Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use super::process::terminate_process_tree;

/// How long a new-terminal launcher may run before its launch counts as requested.
const LAUNCH_GRACE: Duration = Duration::from_millis(500);

/// How a launch request ended.
#[derive(Debug)]
pub enum Launch {
    /// The launcher accepted the request: it exited successfully, or was still running after
    /// `LAUNCH_GRACE`.
    Requested,
    /// The launcher exited unsuccessfully within `LAUNCH_GRACE`.
    Failed(ExitStatus),
}

/// Runs `launcher` with `command` as its arguments, with null stdio.
///
/// The command follows the launcher's own arguments directly: older launchers have no option
/// parsing and would take `--` as the command. The launcher's own exit status is the only signal
/// available: it exits non-zero when it cannot find or start a terminal, but a terminal that
/// stays open (or hands the window to a running instance later) is indistinguishable from one
/// that is still starting. A launcher that fails within `LAUNCH_GRACE` is `Failed`; anything
/// else is `Requested`, which promises only that the request was accepted.
pub fn run(launcher: &Path, command: &[OsString]) -> io::Result<Launch> {
    let mut child = Command::new(launcher)
        .args(command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + LAUNCH_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(Launch::Requested),
            Ok(Some(status)) => return Ok(Launch::Failed(status)),
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                thread::spawn(move || {
                    let _ = child.wait();
                });
                return Ok(Launch::Requested);
            }
            Err(source) => {
                terminate_process_tree(&mut child)?;
                return Err(source);
            }
        }
    }
}
