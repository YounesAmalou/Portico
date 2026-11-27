//! Running a terminal launcher: a program that opens a command in a new terminal window; and
//! running the system's opener on a URL.

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
pub(in crate::platform) const LAUNCH_GRACE: Duration = Duration::from_millis(500);

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

/// Runs `program` with `args`, then `url`, as [`spawn_detached`] does: the system's opener on
/// a URL. A URL holding whitespace, a control character or one of `& | ^ < > " %` is refused
/// (`InvalidInput`) on every system: `cmd /c start` would read them as its own syntax.
pub fn open_url_with(program: &str, args: &[&str], url: &str) -> io::Result<()> {
    if let Some(bad) = url
        .chars()
        .find(|c| c.is_whitespace() || c.is_control() || "&|^<>\"%".contains(*c))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to open a URL holding {bad:?}"),
        ));
    }
    let mut words = args.to_vec();
    words.push(url);
    spawn_detached(program, &words)
}

/// Runs `program` with `args` and null stdio without waiting for it, in a session (on Windows
/// a process group) of its own so it outlives the terminal: an opener hands its work to the
/// desktop and its exit tells nothing more. A thread reaps it once it exits.
fn spawn_detached(program: &str, args: &[&str]) -> io::Result<()> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = super::super::process::detach(&mut command).spawn()?;
    thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_url_with_shell_syntax_is_refused_before_anything_runs() {
        let opener = "portico-no-such-opener";
        for url in [
            "http://127.0.0.1:80/&calc",
            "http://a|b",
            "http://a^b",
            "http://a<b",
            "http://a>b",
            "http://a\"b",
            "http://a%PATH%",
            "http://a b",
            "http://a\tb",
            "http://a\nb",
            "http://a\u{7}b",
        ] {
            let error = open_url_with(opener, &[], url).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{url:?}");
        }
        // A plain URL gets as far as running the opener, which does not exist here.
        let error = open_url_with(opener, &[], "http://127.0.0.1:8080/").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}
