//! Ends the interface when its terminal closes.
//!
//! crossterm 0.29 reads the terminal in a loop that only ends on an event or on `WouldBlock`.
//! Once the terminal hangs up, every read returns end of file, so the loop spins on a core and
//! never returns to Portico, whatever signal arrives. A watcher thread sleeps until the terminal
//! hangs up, then puts an empty, non-blocking pipe in place of the terminal: crossterm's next read
//! would block, which ends its loop, and the flag the watcher raises ends the TUI.
//!
//! crossterm-rs/crossterm#1067 fixes the loop by returning `UnexpectedEof` instead, so the
//! terminal's events are read through `next_event`, which takes that error for a hang-up too.

use std::{
    io,
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd},
        unix::net::UnixStream,
    },
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use crossterm::event::{self, Event};

/// What the terminal had to say within a wait.
#[derive(Debug)]
pub(super) enum Input {
    Event(Event),
    Idle,
    /// The terminal is gone: the TUI ends as on a hang-up.
    Closed,
}

/// Waits at most `timeout` for the terminal's next event.
pub(super) fn next_event(timeout: Duration) -> io::Result<Input> {
    next_event_from(event::poll, event::read, timeout)
}

fn next_event_from(
    poll: impl FnOnce(Duration) -> io::Result<bool>,
    read: impl FnOnce() -> io::Result<Event>,
    timeout: Duration,
) -> io::Result<Input> {
    let event = match poll(timeout) {
        Ok(false) => return Ok(Input::Idle),
        Ok(true) => read(),
        Err(error) => Err(error),
    };
    match event {
        Ok(event) => Ok(Input::Event(event)),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Ok(Input::Closed),
        Err(error) => Err(error),
    }
}

/// Watches the terminal until dropped.
pub(super) struct HangupWatch {
    // Dropping it wakes the watcher, which then returns.
    stop: Option<UnixStream>,
    thread: Option<JoinHandle<()>>,
}

impl HangupWatch {
    /// Watches standard input, the terminal crossterm reads when standard input is one.
    ///
    /// Otherwise crossterm reads a `/dev/tty` of its own, which cannot be replaced from here,
    /// so nothing is watched.
    pub(super) fn start(terminated: Arc<AtomicBool>) -> io::Result<Self> {
        let stdin = io::stdin();
        // SAFETY: `isatty` only inspects the descriptor.
        if unsafe { libc::isatty(stdin.as_raw_fd()) } != 1 {
            return Ok(Self {
                stop: None,
                thread: None,
            });
        }
        Self::watch(stdin.as_fd(), terminated)
    }

    /// Replaces `tty` and raises `terminated` once `tty` hangs up.
    fn watch(tty: BorrowedFd<'_>, terminated: Arc<AtomicBool>) -> io::Result<Self> {
        let target = tty.as_raw_fd();
        let watched = tty.try_clone_to_owned()?;
        let (stop, stopped) = UnixStream::pair()?;
        let thread = thread::Builder::new()
            .name("portico-hangup".into())
            .spawn(move || {
                if wait_for_hangup(&watched, &stopped) {
                    // Replaced before the flag is raised, so a read that sees the flag never
                    // meets the closed terminal again.
                    let _ = replace_with_quiet_pipe(target);
                    terminated.store(true, Ordering::Relaxed);
                }
            })?;
        Ok(Self {
            stop: Some(stop),
            thread: Some(thread),
        })
    }
}

impl Drop for HangupWatch {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Sleeps until `tty` hangs up (true) or `stopped`'s peer is dropped (false).
fn wait_for_hangup(tty: &OwnedFd, stopped: &UnixStream) -> bool {
    // Hang-ups and errors are always reported, so the terminal asks for no events: its
    // ordinary input must not wake the watcher.
    let mut fds = [
        libc::pollfd {
            fd: tty.as_raw_fd(),
            events: 0,
            revents: 0,
        },
        libc::pollfd {
            fd: stopped.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    loop {
        // SAFETY: `fds` is a valid array of two `pollfd`s for the duration of the call.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if ready == -1 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return false;
        }
        if fds[1].revents != 0 {
            return false;
        }
        if fds[0].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            return true;
        }
    }
}

/// Points `target` at the read end of a pipe that stays empty and open, so reading it fails
/// with `WouldBlock` instead of returning end of file.
fn replace_with_quiet_pipe(target: RawFd) -> io::Result<()> {
    let mut ends = [-1; 2];
    // SAFETY: `pipe2` writes two new descriptors into `ends`.
    if unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `dup2` atomically swaps `target` for the pipe; the read end's own descriptor is
    // then closed. The write end is left open for the life of the process, or reads would see
    // end of file.
    unsafe {
        let replaced = libc::dup2(ends[0], target);
        libc::close(ends[0]);
        if replaced == -1 {
            libc::close(ends[1]);
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        fs::File,
        io::Read,
        os::fd::FromRawFd,
        thread,
        time::{Duration, Instant},
    };

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;

    fn end_of_file() -> io::Error {
        io::Error::new(io::ErrorKind::UnexpectedEof, "the terminal is gone")
    }

    #[test]
    fn an_event_is_passed_on() {
        let key = Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        let input = next_event_from(|_| Ok(true), || Ok(key.clone()), Duration::ZERO).unwrap();
        assert!(matches!(input, Input::Event(event) if event == key));
    }

    #[test]
    fn a_quiet_wait_reads_nothing() {
        let input = next_event_from(
            |_| Ok(false),
            || panic!("nothing to read after a quiet wait"),
            Duration::ZERO,
        )
        .unwrap();
        assert!(matches!(input, Input::Idle));
    }

    #[test]
    fn end_of_file_from_the_wait_or_the_read_is_a_closed_terminal() {
        let polled = next_event_from(
            |_| Err(end_of_file()),
            || panic!("nothing to read after a failed wait"),
            Duration::ZERO,
        );
        assert!(matches!(polled, Ok(Input::Closed)));
        let read = next_event_from(|_| Ok(true), || Err(end_of_file()), Duration::ZERO);
        assert!(matches!(read, Ok(Input::Closed)));
    }

    #[test]
    fn other_errors_are_still_failures() {
        let input = next_event_from(
            |_| Err(io::Error::other("Input/output error")),
            || panic!("nothing to read after a failed wait"),
            Duration::ZERO,
        );
        assert_eq!(input.unwrap_err().to_string(), "Input/output error");
    }

    /// A pseudo-terminal: the controlling side and the side a program reads.
    fn pty() -> (OwnedFd, File) {
        let (mut master, mut slave) = (-1, -1);
        // SAFETY: `openpty` writes two descriptors that are then owned here.
        unsafe {
            let opened = libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
            );
            assert_eq!(opened, 0, "openpty: {}", io::Error::last_os_error());
            (OwnedFd::from_raw_fd(master), File::from_raw_fd(slave))
        }
    }

    fn wait_until(flag: &AtomicBool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !flag.load(Ordering::Relaxed) {
            if Instant::now() > deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(5));
        }
        true
    }

    #[test]
    fn a_hang_up_raises_the_flag_and_leaves_reads_that_would_block() {
        let (master, mut slave) = pty();
        let terminated = Arc::new(AtomicBool::new(false));
        let _watch = HangupWatch::watch(slave.as_fd(), Arc::clone(&terminated)).unwrap();

        drop(master);

        assert!(wait_until(&terminated), "the hang-up went unnoticed");
        // What used to be the terminal now reports that nothing is there yet, which ends
        // crossterm's read loop, instead of end of file, which spins it.
        let error = slave.read(&mut [0; 16]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn input_on_a_live_terminal_leaves_it_alone() {
        let (master, mut slave) = pty();
        let terminated = Arc::new(AtomicBool::new(false));
        let watch = HangupWatch::watch(slave.as_fd(), Arc::clone(&terminated)).unwrap();

        let mut controller = File::from(master);
        std::io::Write::write_all(&mut controller, b"q\n").unwrap();
        thread::sleep(Duration::from_millis(100));

        assert!(!terminated.load(Ordering::Relaxed));
        // The watcher read nothing: the input is all still there for crossterm.
        let mut input = [0; 16];
        let read = slave.read(&mut input).unwrap();
        assert_eq!(&input[..read], b"q\n");
        drop(watch);
        assert!(!terminated.load(Ordering::Relaxed));
    }

    #[test]
    fn dropping_the_watch_stops_it_promptly() {
        let (_master, slave) = pty();
        let terminated = Arc::new(AtomicBool::new(false));
        let watch = HangupWatch::watch(slave.as_fd(), Arc::clone(&terminated)).unwrap();

        let started = Instant::now();
        drop(watch);

        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(!terminated.load(Ordering::Relaxed));
    }
}
