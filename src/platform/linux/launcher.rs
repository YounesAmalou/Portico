//! Asking the desktop's terminal launcher, `xdg-terminal-exec`, to open a command in a new
//! terminal window, and `xdg-open` to open a URL.

use std::{ffi::OsString, io, path::Path};

use super::super::common::launcher::{Launch, run, spawn_detached};

/// The terminal launcher run when none is configured.
pub const DEFAULT_LAUNCHER: &str = "xdg-terminal-exec";

/// Runs `launcher` with `command` as its arguments; see `common::launcher::run`.
pub fn launch(launcher: &Path, command: &[OsString]) -> io::Result<Launch> {
    run(launcher, command)
}

/// Opens `url` in the desktop's browser with `xdg-open`, without waiting for it.
pub fn open_url(url: &str) -> io::Result<()> {
    spawn_detached("xdg-open", &[url])
}
