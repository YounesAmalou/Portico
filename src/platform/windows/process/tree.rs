//! Ending a process with all of its descendants.

use std::io;

use super::{
    pin::{Pin, is_gone, open_pin},
    proc::process_table,
};

/// A process of the tree, pinned so its ID, and so the parent ID its children list, cannot
/// name a later process while the walk runs.
struct Member {
    pid: u32,
    pin: Pin,
    created: u64,
}

/// Ends `root` and all of its descendants, children before their parent.
///
/// Windows cannot stop a process the way Unix's `SIGSTOP` does, so a process may start a child
/// after the table was read; the table is read again after each round of ends until it shows
/// no child that was not known, and a process that has ended cannot start another. A process
/// counts as a child only if it was created after its parent: a parent ID may name an exited
/// process whose ID a later process got. Failures never abandon the walk: each process that
/// could be reached is still ended, and the first error is reported afterwards.
pub fn kill_process_tree(root: u32) -> io::Result<()> {
    let mut first_error = None;
    let mut known = match member(root) {
        Ok(Some(root)) => vec![root],
        Ok(None) => return Ok(()),
        Err(error) => return Err(error),
    };
    let mut ended = 0;
    loop {
        let read = process_table();
        if let Ok(table) = &read {
            add_descendants(table, &mut known, &mut first_error);
        }
        if ended == known.len() {
            break;
        }
        // Discovery order lists every parent before its children, so the reverse ends children
        // first.
        for member in known[ended..].iter().rev() {
            match member.pin.terminate() {
                Err(error) if !is_gone(&error) => {
                    first_error.get_or_insert(error);
                }
                _ => {}
            }
        }
        ended = known.len();
        if let Err(error) = read {
            first_error.get_or_insert(error);
            break;
        }
    }
    first_error.map_or(Ok(()), Err)
}

/// Adds the children of every known process, ended or not, that are not known yet, and their
/// children in turn, since `known` grows while it is walked.
fn add_descendants(
    table: &[(u32, u32)],
    known: &mut Vec<Member>,
    first_error: &mut Option<io::Error>,
) {
    let mut parent = 0;
    while parent < known.len() {
        let (parent_pid, parent_created) = (known[parent].pid, known[parent].created);
        for &(pid, _) in table
            .iter()
            .filter(|&&(pid, ppid)| ppid == parent_pid && pid != parent_pid)
        {
            if known.iter().any(|member| member.pid == pid) {
                continue;
            }
            match member(pid) {
                Ok(Some(child)) if child.created >= parent_created => known.push(child),
                Ok(_) => {}
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        parent += 1;
    }
}

/// Pins `pid` with its creation time; `None` when it is gone. When the full pin cannot be
/// opened, a handle that can only end the process does.
fn member(pid: u32) -> io::Result<Option<Member>> {
    let pin = match open_pin(pid) {
        Ok(pin) => pin,
        Err(error) if is_gone(&error) => return Ok(None),
        Err(_) => match Pin::open_to_terminate(pid) {
            Ok(pin) => pin,
            Err(error) if is_gone(&error) => return Ok(None),
            Err(error) => return Err(error),
        },
    };
    let created = pin.creation_time()?;
    Ok(Some(Member { pid, pin, created }))
}
