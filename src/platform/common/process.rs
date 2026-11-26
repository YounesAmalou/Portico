//! What every system's process control shares: ending a child with its descendants, reaping a
//! child within a bound, scanning for started tunnels and the fault hook that makes opening a
//! pin fail in tests.

use std::{
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    process::Child,
    thread,
    time::{Duration, Instant},
};

use super::super::process::{
    Proc, SSH_BACKGROUNDS, Uid, arg_path, kill_direct_child, kill_process_tree,
};

/// The start of the `LocalCommand` that names a foreground tunnel's log (see `tunnel_marker`).
const LOCAL_COMMAND: &[u8] = b"LocalCommand=portico-tunnel ";

/// The arguments by which a tunnel's ssh is recognised as the one logging to `log`. Where ssh
/// backgrounds itself (`background`) that is `-E <log>`, its own log. Otherwise (Windows) ssh's
/// standard error goes to `log`, since Windows' ssh keeps its own `-E` file from being read
/// while it runs, and the log is named by a `LocalCommand` that never runs: the
/// `PermitLocalCommand=no` before it on the command line wins over any configuration, and it
/// sends nothing to the server. ssh expands `%` tokens in it as it reads its options, so a `%`
/// in the path is doubled.
pub fn tunnel_marker(log: &Path, background: bool) -> Vec<OsString> {
    if background {
        return vec!["-E".into(), log.into()];
    }
    let mut command = LOCAL_COMMAND.to_vec();
    for &byte in log.as_os_str().as_encoded_bytes() {
        command.push(byte);
        if byte == b'%' {
            command.push(b'%');
        }
    }
    // SAFETY: an encoded `OsStr` with ASCII bytes added between its characters.
    let command = unsafe { OsString::from_encoded_bytes_unchecked(command) };
    vec![
        "-o".into(),
        "PermitLocalCommand=no".into(),
        "-o".into(),
        command,
    ]
}

/// The log a tunnel's ssh names on its command line `args`, as `tunnel_marker` writes it for
/// this system.
pub fn marked_log(args: &[Vec<u8>]) -> Option<PathBuf> {
    marked_log_in(args, SSH_BACKGROUNDS)
}

fn marked_log_in(args: &[Vec<u8>], background: bool) -> Option<PathBuf> {
    if background {
        return args
            .windows(2)
            .find(|pair| pair[0] == b"-E")
            .map(|pair| arg_path(&pair[1]));
    }
    let command = args
        .windows(2)
        .find(|pair| pair[0] == b"-o" && pair[1].starts_with(LOCAL_COMMAND))
        .map(|pair| &pair[1][LOCAL_COMMAND.len()..])?;
    let mut log = Vec::with_capacity(command.len());
    let mut bytes = command.iter().copied();
    while let Some(byte) = bytes.next() {
        log.push(byte);
        if byte == b'%' && bytes.next() != Some(b'%') {
            // A single `%` is no marker Portico wrote.
            return None;
        }
    }
    Some(arg_path(&log))
}

pub fn terminate_process_tree(child: &mut Child) -> io::Result<()> {
    if let Err(error) = kill_process_tree(child.id()) {
        let _ = kill_direct_child(child);
        let _ = reap_child_bounded(child);
        return Err(error);
    }
    match reap_child_bounded(child) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = kill_direct_child(child);
            let _ = reap_child_bounded(child);
            Err(error)
        }
    }
}

pub fn reap_child_bounded(child: &mut Child) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_millis(250);
    loop {
        match child.try_wait()? {
            Some(_) => return Ok(()),
            None if Instant::now() >= deadline => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out reaping child process",
                ));
            }
            None => thread::sleep(Duration::from_millis(5)),
        }
    }
}

impl Proc {
    /// Every live process of `uid` that runs in the background (as `ssh -f` does once it has
    /// put itself there) and whose command line names a log as a tunnel's does
    /// (`tunnel_marker`): what a status listing searches for tunnels without a record, read
    /// once however many tunnels it lists.
    pub fn scan(&self, uid: &Uid) -> Vec<Logged> {
        self.pids()
            .into_iter()
            .filter_map(|pid| {
                // The command line rejects nearly every process, so it is read first.
                let args = self.cmdline(pid)?;
                let log = marked_log(&args)?;
                let stat = self.stat(pid)?;
                // Only a backgrounded ssh counts: a foreground one may still fail to connect.
                (!matches!(stat.state, 'Z' | 'X' | 'x')
                    && stat.in_background(pid)
                    && self.uid(pid).as_ref() == Some(uid))
                .then_some(Logged {
                    pid,
                    start: stat.start,
                    log,
                })
            })
            .collect()
    }
}

/// A process whose command line names a tunnel's log (`tunnel_marker`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Logged {
    pub pid: u32,
    pub start: u64,
    pub log: PathBuf,
}

#[cfg(test)]
thread_local! {
    static FAIL_PIN_OPEN: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
    static PIN_OPEN_FAILED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Makes the pin open after `skipped` successful ones fail.
#[cfg(test)]
pub fn fail_pin_after(skipped: u32) {
    PIN_OPEN_FAILED.with(|failed| failed.set(false));
    FAIL_PIN_OPEN.with(|countdown| countdown.set(Some(skipped)));
}

/// Whether the failure `fail_pin_after` armed has been injected, so a test can prove it
/// exercised the path without a pin rather than passing through the normal one.
#[cfg(test)]
pub fn pin_failure_injected() -> bool {
    PIN_OPEN_FAILED.with(std::cell::Cell::get)
}

/// Whether this pin open is the one `fail_pin_after` armed to fail, counting it down otherwise.
#[cfg(test)]
pub(crate) fn pin_open_fails() -> bool {
    let fails = FAIL_PIN_OPEN.with(|countdown| match countdown.get() {
        Some(0) => {
            countdown.set(None);
            true
        }
        Some(remaining) => {
            countdown.set(Some(remaining - 1));
            false
        }
        None => false,
    });
    if fails {
        PIN_OPEN_FAILED.with(|failed| failed.set(true));
    }
    fails
}

#[cfg(test)]
mod marker_tests {
    use super::*;

    fn bytes(args: &[OsString]) -> Vec<Vec<u8>> {
        args.iter()
            .map(|arg| arg.as_encoded_bytes().to_vec())
            .collect()
    }

    #[test]
    fn a_background_tunnel_is_named_by_its_ssh_log() {
        let log = Path::new("/run/t/db.log");
        let marker = tunnel_marker(log, true);
        assert_eq!(marker, ["-E", "/run/t/db.log"]);
        let mut args = bytes(&marker);
        args.push(b"-L".to_vec());
        assert_eq!(marked_log_in(&args, true).as_deref(), Some(log));
        assert_eq!(marked_log_in(&args, false), None);
    }

    #[test]
    fn a_foreground_tunnel_is_named_by_a_local_command_that_never_runs() {
        let log = Path::new("/home/Jane Doe/100%/db.log");
        let marker = tunnel_marker(log, false);
        assert_eq!(
            marker,
            [
                "-o",
                "PermitLocalCommand=no",
                "-o",
                "LocalCommand=portico-tunnel /home/Jane Doe/100%%/db.log"
            ]
        );
        assert_eq!(marked_log_in(&bytes(&marker), false).as_deref(), Some(log));
        assert_eq!(marked_log_in(&bytes(&marker), true), None);
        // Text no marker of Portico's holds.
        let odd = bytes(&["-o".into(), "LocalCommand=portico-tunnel 1%d".into()]);
        assert_eq!(marked_log_in(&odd, false), None);
        let other = bytes(&["-o".into(), "LocalCommand=echo hi".into()]);
        assert_eq!(marked_log_in(&other, false), None);
    }
}
