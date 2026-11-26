//! Ending a process with all of its descendants.

use std::{io, os::windows::io::OwnedHandle};

use windows_sys::Win32::System::Threading::{
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
};

use super::{
    pin::{Pin, creation_time, is_gone, open_pin, open_process, parent_id},
    proc::process_table,
};

/// A process of the tree. Its handles keep its ID unassigned while the walk runs, so the
/// parent ID its children report cannot come to name a later process.
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
/// listed with a known parent's ID counts as its child only once a handle holds it and, through
/// that handle, it still names that parent and was created after it: between the reading of the
/// table and the opening, the listed process may have exited and its ID gone to another, and a
/// parent ID may name an exited process whose ID a later process got. Failures never abandon the
/// walk: each process that could be reached is still ended, and the first error is reported
/// afterwards.
pub fn kill_process_tree(root: u32) -> io::Result<()> {
    let mut first_error = None;
    let mut known = match member(root, None) {
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
            match member(pid, Some((parent_pid, parent_created))) {
                Ok(Some(child)) => known.push(child),
                Ok(None) => {}
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        parent += 1;
    }
}

/// Holds `pid` and, when `parent` (its ID and creation time) is given, confirms through the
/// handle that the process is that parent's child; then pins it to end it. `None` when it is
/// gone or not the child. A process that cannot even be queried is not the parent's child as
/// far as anything can show (another user's or a system process whose stale parent ID matches),
/// so it is passed over without an error.
fn member(pid: u32, parent: Option<(u32, u64)>) -> io::Result<Option<Member>> {
    let query = match open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE) {
        Ok(query) => query,
        Err(error) if is_gone(&error) => return Ok(None),
        Err(_) if parent.is_some() => return Ok(None),
        Err(error) => return Err(error),
    };
    let created = creation_time(&query)?;
    if let Some((parent_pid, parent_created)) = parent
        && !is_child(&query, created, parent_pid, parent_created)?
    {
        return Ok(None);
    }
    // `query` keeps the ID on this process while the pin is opened.
    let pin = match open_pin(pid) {
        Ok(pin) => pin,
        Err(error) if is_gone(&error) => return Ok(None),
        Err(_) => match Pin::open_to_terminate(pid) {
            Ok(pin) => pin,
            Err(error) if is_gone(&error) => return Ok(None),
            Err(error) => return Err(error),
        },
    };
    Ok(Some(Member { pid, pin, created }))
}

/// Whether the process `query` holds, created at `created`, was created by the process with
/// `parent_pid` that was created at `parent_created`.
fn is_child(
    query: &OwnedHandle,
    created: u64,
    parent_pid: u32,
    parent_created: u64,
) -> io::Result<bool> {
    Ok(created >= parent_created && parent_id(query)? == parent_pid)
}
