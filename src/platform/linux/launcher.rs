//! Asking the desktop's terminal launcher, `xdg-terminal-exec`, to open a command in a new
//! terminal window.

use std::{ffi::OsString, io, path::Path};

use super::super::unix::launcher::{Launch, run};

/// The terminal launcher run when none is configured.
pub const DEFAULT_LAUNCHER: &str = "xdg-terminal-exec";

/// Runs `launcher` with `command` as its arguments; see `unix::launcher::run`.
pub fn launch(launcher: &Path, command: &[OsString]) -> io::Result<Launch> {
    run(launcher, command)
}
