//! What every system's process control shares: ending a child with its descendants, reaping a
//! child within a bound, scanning for started tunnels and the fault hook that makes opening a
//! pin fail in tests.

use std::{
    io,
    path::PathBuf,
    process::Child,
    thread,
    time::{Duration, Instant},
};

use super::super::process::{Proc, Uid, arg_path, kill_direct_child, kill_process_tree};

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
    /// put itself there) and whose command line has `-E <path>`: what a status listing searches
    /// for tunnels without a record, read once however many tunnels it lists.
    pub fn scan(&self, uid: &Uid) -> Vec<Logged> {
        self.pids()
            .into_iter()
            .filter_map(|pid| {
                // The command line rejects nearly every process, so it is read first.
                let args = self.cmdline(pid)?;
                let log = args
                    .windows(2)
                    .find(|pair| pair[0] == b"-E")
                    .map(|pair| arg_path(&pair[1]))?;
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

/// A process that logs to a file with `-E`, as every started tunnel does.
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
