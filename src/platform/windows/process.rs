//! Windows process control. There are no signals to send: a handle pins a process's identity
//! (`Pin`) and ends it with `TerminateProcess`, the Toolhelp snapshot lists processes and their
//! parents, and a process group is a new console process group, out of reach of Ctrl+C.

mod pin;
mod proc;
mod tree;

use std::{
    io,
    os::windows::{io::AsRawHandle, process::CommandExt},
    process::{Child, Command, ExitStatus},
};

use windows_sys::Win32::{
    Foundation::{HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation},
    System::{
        Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE},
        Threading::{
            CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, DETACHED_PROCESS, GetCurrentProcess,
        },
    },
};

use pin::{is_gone, open_pin};
pub(crate) use proc::arg_path;
pub use proc::{Proc, Stat};
pub use tree::kill_process_tree;

/// A user, as the string form of their security identifier (`S-1-5-21-…`).
pub type Uid = String;

/// Whether `ssh -f` backgrounds itself once its forwards are up: Windows' OpenSSH has no
/// background mode and stays in the foreground, so Portico detaches it and waits for the
/// forward itself.
pub const SSH_BACKGROUNDS: bool = false;

/// The signals that end a session, which Portico treats alike: the console's Ctrl+C and
/// Ctrl+Break reach the C runtime as these. Windows has no hang-up signal.
pub const HANGUP_SIGNALS: &[i32] = &[signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM];

/// A process's exit status as Portico's: its code, which no signal ever replaces on Windows.
/// `None` when that is not a valid exit code (above 255, or an `NTSTATUS` such as a crash).
pub fn exit_code(status: ExitStatus) -> Option<u8> {
    status.code().and_then(|code| u8::try_from(code).ok())
}

/// The status of a process that exited with `code`.
#[cfg(test)]
pub fn exit_status(code: i32) -> ExitStatus {
    std::os::windows::process::ExitStatusExt::from_raw(code as u32)
}

/// Puts the process `command` starts in a console process group of its own, out of reach of
/// the console's Ctrl+C.
pub fn own_group(command: &mut Command) -> &mut Command {
    command.creation_flags(CREATE_NEW_PROCESS_GROUP)
}

/// Makes the process `command` starts independent of this one's console, so it outlives
/// Portico and the console window, and holds none of this process's standard handles (the
/// caller sets the child's own).
pub fn detach(command: &mut Command) -> &mut Command {
    keep_standard_handles();
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
}

/// Stops this process's standard handles from being inherited. The standard library creates
/// every process with handle inheritance on, so a long-lived child would otherwise inherit
/// them when they are inheritable (a pipe Portico's output goes to, under `| jq` or a test
/// harness), and the reader would never see the end of the output. Children given
/// `Stdio::inherit` are unaffected: the standard library hands them a fresh inheritable
/// duplicate of the handle at each spawn (`Stdio::to_handle` in its
/// `sys/process/windows.rs`: `io.duplicate(0, true, DUPLICATE_SAME_ACCESS)`).
fn keep_standard_handles() {
    for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: `GetStdHandle` has no preconditions, and `SetHandleInformation` only
        // changes the flags of a handle this process holds. A failure leaves the handle as it
        // was, which is no worse.
        unsafe {
            let handle = GetStdHandle(id);
            if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

/// The current user.
pub fn current_uid() -> Uid {
    // Reading this process's own token cannot fail in practice; an empty SID would match no
    // process.
    // SAFETY: `GetCurrentProcess` has no preconditions; its pseudo handle needs no closing.
    proc::user_sid(unsafe { GetCurrentProcess() }).unwrap_or_default()
}

/// The user the process acts as: on Windows the same as `current_uid`.
pub fn effective_uid() -> Uid {
    current_uid()
}

/// Whether `child` has exited. Windows keeps an exited process for as long as a handle to it
/// is open, which `child` holds, so its ID cannot be reused meanwhile.
pub fn has_exited(child: &Child) -> io::Result<bool> {
    use windows_sys::Win32::{
        Foundation::{WAIT_FAILED, WAIT_OBJECT_0},
        System::Threading::WaitForSingleObject,
    };
    // SAFETY: waiting with no timeout only reads the state of the handle `child` owns.
    match unsafe { WaitForSingleObject(child.as_raw_handle(), 0) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_FAILED => Err(io::Error::last_os_error()),
        _ => Ok(false),
    }
}

/// Ends the process group `own_group` gave `child` (its process tree on Windows) and waits for
/// `child`.
pub fn terminate_process_group(child: &mut Child) {
    if kill_process_tree(child.id()).is_err() {
        let _ = child.kill();
    }
    let _ = child.wait();
}

pub fn kill_direct_child(child: &mut Child) -> io::Result<()> {
    match child.kill() {
        Ok(()) => Ok(()),
        Err(_) if has_exited(child).unwrap_or(false) => Ok(()),
        Err(error) => Err(error),
    }
}

/// How long a stopped tunnel has to exit once it was terminated.
pub const TERM_GRACE_MS: u32 = 3000;

/// Ends `pid` if `verify` still recognises it once a pin holds it. Returns whether it was
/// ended. Because the pin is taken before `verify` runs, and a pinned process is never ended
/// once it has exited, a process that passes `verify` is the one the pin refers to. Windows'
/// ssh has no graceful stop Portico can ask of a detached process, and a tunnel holds nothing
/// to flush, so the process is terminated at once.
pub fn stop_process(pid: u32, verify: impl FnOnce() -> bool) -> io::Result<bool> {
    let pin = match open_pin(pid) {
        Ok(pin) => pin,
        Err(error) if is_gone(&error) => return Ok(false),
        Err(error) => return Err(error),
    };
    if !verify() {
        return Ok(false);
    }
    match pin.terminate() {
        Ok(()) => {}
        Err(error) if is_gone(&error) => return Ok(true),
        Err(error) => return Err(error),
    }
    pin.wait_exit(TERM_GRACE_MS);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::{
        process::Stdio,
        thread,
        time::{Duration, Instant},
    };

    use super::super::super::common::process::terminate_process_tree;
    use super::pin::Pin;
    use super::proc::process_table;
    use super::*;

    /// A process that runs for half a minute unless it is ended, with `marker` on its command
    /// line.
    fn long_lived(marker: &str) -> Child {
        quiet(Command::new("cmd").raw_arg(format!("/c ping -n 30 127.0.0.1 >nul & rem {marker}")))
    }

    /// Spawns `command` holding none of the test's standard handles, so a `ping` left running
    /// by a test that ends only `cmd` keeps no output of the test open.
    fn quiet(command: &mut Command) -> Child {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    fn end(mut child: Child) {
        let _ = terminate_process_tree(&mut child);
    }

    fn now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    /// Run by `a_detached_child_does_not_hold_the_output_of_its_parent` as a separate process
    /// whose standard output is a pipe: starts a detached `ping` and prints its PID.
    #[test]
    #[ignore = "a helper process of another test"]
    fn detached_child_helper() {
        if std::env::var_os("PORTICO_DETACH_HELPER").is_none() {
            return;
        }
        let child = detach(Command::new("ping").args(["-n", "30", "127.0.0.1"]))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        println!("detached pid {}", child.id());
    }

    #[test]
    fn a_detached_child_does_not_hold_the_output_of_its_parent() {
        let mut helper = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "platform::windows::process::tests::detached_child_helper",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("PORTICO_DETACH_HELPER", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut stdout = helper.stdout.take().unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let mut text = String::new();
            let _ = std::io::Read::read_to_string(&mut stdout, &mut text);
            let _ = sender.send(text);
        });
        // The output ends when the helper exits, unless the detached ping holds the pipe for
        // its 30 seconds.
        let text = receiver.recv_timeout(Duration::from_secs(15));
        let _ = helper.wait();
        let pid = text.as_ref().ok().and_then(|text| {
            text.lines()
                .find_map(|line| line.strip_prefix("detached pid "))
                .and_then(|pid| pid.trim().parse::<u32>().ok())
        });
        if let Some(pid) = pid {
            let _ = kill_process_tree(pid);
        }
        let text = text.expect("the detached child kept its parent's output open");
        assert!(pid.is_some(), "the helper did not start the child: {text}");
    }

    #[test]
    fn exit_codes_above_255_are_failures() {
        assert_eq!(exit_code(exit_status(0)), Some(0));
        assert_eq!(exit_code(exit_status(255)), Some(255));
        assert_eq!(exit_code(exit_status(256)), None);
        // STATUS_CONTROL_C_EXIT, as a process ended by Ctrl+C reports it.
        assert_eq!(exit_code(exit_status(0xC000013A_u32 as i32)), None);
    }

    #[test]
    fn proc_reads_own_command_line() {
        let child = long_lived("portico-marker-7f3a");
        let proc = Proc::system();
        let args = proc.cmdline(child.id()).unwrap();
        assert!(
            args.iter().any(|arg| arg == b"portico-marker-7f3a"),
            "{args:?}"
        );
        assert_eq!(proc.uid(child.id()), Some(current_uid()));
        assert!(current_uid().starts_with("S-1-"), "{}", current_uid());
        assert!(proc.pids().contains(&child.id()));
        end(child);
    }

    #[test]
    fn stat_start_is_stable_and_differs_between_processes() {
        let (first, second) = (long_lived("first"), long_lived("second"));
        let proc = Proc::system();
        let stat = proc.stat(first.id()).unwrap();
        assert_eq!(stat.state, 'R');
        assert_eq!(proc.stat(first.id()), Some(stat));
        assert_ne!(proc.stat(second.id()).unwrap().start, stat.start);
        end(first);
        end(second);
    }

    #[test]
    fn started_at_is_close_to_now() {
        let child = long_lived("started");
        let proc = Proc::system();
        let start = proc.stat(child.id()).unwrap().start;
        let started = proc.started_at(start).unwrap();
        assert!(started.abs_diff(now()) < 60, "{started}");
        end(child);
    }

    #[test]
    fn boot_id_is_read_and_stable() {
        let proc = Proc::system();
        let boot = proc.boot_id().expect("the registry's BootId");
        assert!(boot.parse::<u32>().is_ok(), "{boot}");
        assert_eq!(proc.boot_id(), Some(boot));
    }

    #[test]
    fn kill_process_tree_ends_children() {
        let mut child = quiet(Command::new("cmd").raw_arg("/c ping -n 30 127.0.0.1 >nul"));
        let proc = Proc::system();
        let deadline = Instant::now() + Duration::from_secs(10);
        // cmd starts ping a moment after it starts; the console host may be a child too.
        let ping = loop {
            let ping = process_table().unwrap().into_iter().find(|&(pid, parent)| {
                parent == child.id()
                    && proc
                        .cmdline(pid)
                        .is_some_and(|args| args.iter().any(|arg| arg == b"127.0.0.1"))
            });
            if let Some((pid, _)) = ping {
                break Pin::open(pid).unwrap();
            }
            assert!(Instant::now() < deadline, "ping never started");
            thread::sleep(Duration::from_millis(10));
        };
        terminate_process_tree(&mut child).unwrap();
        assert!(ping.wait_exit(5000), "the grandchild survived");
    }

    #[test]
    fn stop_process_refuses_when_verify_fails() {
        let mut child = long_lived("stop");
        assert!(!stop_process(child.id(), || false).unwrap());
        assert!(
            child.try_wait().unwrap().is_none(),
            "an unverified process was ended"
        );
        assert!(stop_process(child.id(), || true).unwrap());
        assert!(has_exited(&child).unwrap());
        assert!(!child.wait().unwrap().success());
    }

    #[test]
    fn pin_reports_an_exited_process_as_gone() {
        let mut child = long_lived("pin");
        let pin = Pin::open(child.id()).unwrap();
        assert!(!pin.wait_exit(0), "a running process counted as exited");
        child.kill().unwrap();
        child.wait().unwrap();
        drop(child);
        // With `child` gone, the pin alone holds the process and its ID.
        assert!(pin.wait_exit(2000), "the exit was not seen");
        assert!(is_gone(&pin.terminate().unwrap_err()));
    }
}
