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
//!
//! The watcher is Unix-only. On Windows crossterm reads console input events, and closing the
//! console ends the process (`CTRL_CLOSE_EVENT`), so nothing is watched; an error from the
//! console is taken for its end.

use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
#[cfg(unix)]
use std::{
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd},
        unix::net::UnixStream,
    },
    thread::{self, JoinHandle},
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
#[cfg(unix)]
pub(super) fn next_event(timeout: Duration) -> io::Result<Input> {
    next_event_from(event::poll, event::read, timeout)
}

/// Waits at most `timeout` for the console's next event. A key press ConPTY reports by its
/// control character alone, which crossterm would drop, is read here (see
/// `platform::console`); everything else through crossterm.
#[cfg(windows)]
pub(super) fn next_event(timeout: Duration) -> io::Result<Input> {
    use crate::platform::console;
    diag::peek();
    let bare = console::wait_for_input(timeout).and_then(|queued| {
        if queued {
            console::take_bare_key().map(Some)
        } else {
            Ok(None)
        }
    });
    let input = match bare {
        Ok(None) => Ok(Input::Idle),
        Ok(Some(Some(key))) => Ok(Input::Event(Event::Key(bare_key(
            key.character,
            key.alt,
            key.shift,
        )))),
        // Queued input crossterm reads, one record at a time.
        Ok(Some(None)) => next_event_from(event::poll, event::read, Duration::ZERO),
        Err(error) if is_closed(&error) => Ok(Input::Closed),
        Err(error) => Err(error),
    };
    diag::log(&format!("{input:?}"));
    input
}

/// Waits for the console's next event, however long that takes; its end is `UnexpectedEof`.
#[cfg(windows)]
pub(crate) fn read_event() -> io::Result<Event> {
    loop {
        match next_event(Duration::from_secs(60))? {
            Input::Event(event) => return Ok(event),
            Input::Idle => {}
            Input::Closed => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the console is gone",
                ));
            }
        }
    }
}

/// The key a control character (or DEL) stands for, read as crossterm reads it from a Unix
/// terminal, with the modifiers held.
#[cfg(any(windows, test))]
fn bare_key(character: u8, alt: bool, shift: bool) -> event::KeyEvent {
    use event::{KeyCode, KeyModifiers};
    let (mut code, mut modifiers) = match character {
        b'\r' => (KeyCode::Enter, KeyModifiers::NONE),
        b'\t' => (KeyCode::Tab, KeyModifiers::NONE),
        0x1b => (KeyCode::Esc, KeyModifiers::NONE),
        0x7f => (KeyCode::Backspace, KeyModifiers::NONE),
        0 => (KeyCode::Char(' '), KeyModifiers::CONTROL),
        0x01..=0x1a => (
            KeyCode::Char(char::from(character - 0x01 + b'a')),
            KeyModifiers::CONTROL,
        ),
        0x1c..=0x1f => (
            KeyCode::Char(char::from(character - 0x1c + b'4')),
            KeyModifiers::CONTROL,
        ),
        _ => (KeyCode::Char(char::from(character)), KeyModifiers::NONE),
    };
    if alt {
        modifiers |= KeyModifiers::ALT;
    }
    if shift {
        if code == KeyCode::Tab {
            code = KeyCode::BackTab;
        }
        modifiers |= KeyModifiers::SHIFT;
    }
    event::KeyEvent::new(code, modifiers)
}

// TEMPORARY DIAGNOSTIC (Ruling G): logs the console's raw input records and the events read.
#[cfg(windows)]
mod diag {
    use std::{
        io::Write,
        sync::{Mutex, OnceLock},
        time::Instant,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, INPUT_RECORD, PeekConsoleInputW, STD_INPUT_HANDLE,
    };

    struct Log {
        file: std::fs::File,
        start: Instant,
        last: String,
    }

    fn log_file() -> &'static Option<Mutex<Log>> {
        static LOG: OnceLock<Option<Mutex<Log>>> = OnceLock::new();
        LOG.get_or_init(|| {
            let path = std::env::var_os("PORTICO_INPUT_LOG")?;
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()?;
            Some(Mutex::new(Log {
                file,
                start: Instant::now(),
                last: String::new(),
            }))
        })
    }

    pub(super) fn log(line: &str) {
        if let Some(log) = log_file()
            && let Ok(mut log) = log.lock()
        {
            if line.contains("Idle") {
                return;
            }
            let elapsed = log.start.elapsed().as_millis();
            let _ = writeln!(log.file, "{elapsed} {line}");
        }
    }

    pub(super) fn peek() {
        let Some(log) = log_file() else { return };
        let mut records: [INPUT_RECORD; 32] = unsafe { std::mem::zeroed() };
        let mut count = 0u32;
        let ok = unsafe {
            PeekConsoleInputW(
                GetStdHandle(STD_INPUT_HANDLE),
                records.as_mut_ptr(),
                32,
                &mut count,
            )
        };
        let mut text = String::new();
        if ok == 0 {
            text = format!("peek failed: {}", std::io::Error::last_os_error());
        }
        for record in &records[..count as usize] {
            if record.EventType == 1 {
                let key = unsafe { record.Event.KeyEvent };
                text.push_str(&format!(
                    "[key down={} rep={} vk={:#x} sc={:#x} ch={:#x} ctrl={:#x}] ",
                    key.bKeyDown,
                    key.wRepeatCount,
                    key.wVirtualKeyCode,
                    key.wVirtualScanCode,
                    unsafe { key.uChar.UnicodeChar },
                    key.dwControlKeyState
                ));
            } else {
                text.push_str(&format!("[type {:#x}] ", record.EventType));
            }
        }
        if let Ok(mut log) = log.lock()
            && !text.is_empty()
            && text != log.last
        {
            let elapsed = log.start.elapsed().as_millis();
            let _ = writeln!(log.file, "{elapsed} peek {text}");
            log.last = text;
        }
    }
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
        Err(error) if is_closed(&error) => Ok(Input::Closed),
        Err(error) => Err(error),
    }
}

/// Whether `error`, from waiting for or reading the terminal, means it is gone: end of file, or
/// on Windows any error from the console.
fn is_closed(error: &io::Error) -> bool {
    cfg!(windows) || error.kind() == io::ErrorKind::UnexpectedEof
}

/// Waits for a line on the terminal, an end of input, or `terminated` (Ctrl+C, a signal or a
/// hang-up).
///
/// Reads the descriptor directly: `main` holds the lock of `io::stdin()` for the whole session,
/// so reading through it would wait for ever. One byte at a time, so nothing after the line is
/// taken from the terminal.
#[cfg(unix)]
pub(super) fn wait_for_enter(terminated: &AtomicBool, tick: Duration) {
    use std::{
        io::Read,
        os::fd::{AsFd, AsRawFd},
    };
    let Ok(fd) = io::stdin().as_fd().try_clone_to_owned() else {
        return;
    };
    let mut input = std::fs::File::from(fd);
    let mut byte = [0];
    while !terminated.load(Ordering::Relaxed) {
        let mut poll = libc::pollfd {
            fd: input.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `poll` reads and writes only the one `pollfd` it is given.
        let ready = unsafe { libc::poll(&mut poll, 1, tick.as_millis() as libc::c_int) };
        if ready < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return;
        }
        if ready <= 0 {
            continue;
        }
        match input.read(&mut byte) {
            Ok(0) => return,
            Ok(_) if matches!(byte[0], b'\n' | b'\r') => return,
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

/// Waits for Enter on the console, the console's end, or `terminated` (Ctrl+C or a signal).
///
/// Reads the console's input events, as the interface does, so only key presses count.
#[cfg(windows)]
pub(super) fn wait_for_enter(terminated: &AtomicBool, tick: Duration) {
    use crossterm::event::{KeyCode, KeyEventKind};
    while !terminated.load(Ordering::Relaxed) {
        match next_event(tick) {
            Ok(Input::Event(Event::Key(key)))
                if key.kind == KeyEventKind::Press && key.code == KeyCode::Enter =>
            {
                return;
            }
            Ok(Input::Event(_) | Input::Idle) => {}
            Ok(Input::Closed) | Err(_) => return,
        }
    }
}

/// Nothing to watch on Windows: closing the console ends the process.
#[cfg(windows)]
pub(super) struct HangupWatch;

#[cfg(windows)]
impl HangupWatch {
    pub(super) fn start(_terminated: Arc<AtomicBool>) -> io::Result<Self> {
        Ok(Self)
    }
}

/// Watches the terminal until dropped.
#[cfg(unix)]
pub(super) struct HangupWatch {
    // Dropping it wakes the watcher, which then returns.
    stop: Option<UnixStream>,
    thread: Option<JoinHandle<()>>,
}

#[cfg(unix)]
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

#[cfg(unix)]
impl Drop for HangupWatch {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(unix)]
/// Sleeps until `tty` hangs up (true) or `stopped`'s peer is dropped (false).
fn wait_for_hangup(tty: &OwnedFd, stopped: &UnixStream) -> bool {
    // Hang-ups and errors are always reported, so the terminal asks for no events: its
    // ordinary input must not wake the watcher. macOS reports a terminal's hang-up only to a
    // poll for input, so there input wakes the watcher too, which tells the two apart.
    let mut fds = [
        libc::pollfd {
            fd: tty.as_raw_fd(),
            events: if cfg!(target_os = "macos") {
                libc::POLLIN
            } else {
                0
            },
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
        if fds[0].revents & libc::POLLIN != 0 {
            if stays_readable_and_empty(tty) {
                return true;
            }
            // Input waits for its reader: look again shortly, not at once.
            thread::sleep(Duration::from_millis(50));
        }
    }
}

#[cfg(unix)]
/// Whether a terminal that polled readable holds no input and stays that way: reading it would
/// return end of file, as a hung-up terminal does. Input the interface reads between two looks
/// also leaves the terminal empty, but then no longer readable, so it is looked at a few times.
fn stays_readable_and_empty(tty: &OwnedFd) -> bool {
    for _ in 0..3 {
        let mut poll = libc::pollfd {
            fd: tty.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid `pollfd` that outlives the call.
        if unsafe { libc::poll(&mut poll, 1, 0) } == -1 {
            return false;
        }
        if poll.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            return true;
        }
        if poll.revents & libc::POLLIN == 0 {
            return false;
        }
        let mut queued: libc::c_int = 0;
        // SAFETY: `FIONREAD` writes one `int` to the location given.
        let read = unsafe { libc::ioctl(tty.as_raw_fd(), libc::FIONREAD as _, &mut queued) };
        if read == 0 && queued > 0 {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
    true
}

#[cfg(unix)]
/// Points `target` at the read end of a pipe that stays empty and open, so reading it fails
/// with `WouldBlock` instead of returning end of file.
fn replace_with_quiet_pipe(target: RawFd) -> io::Result<()> {
    let ends = crate::platform::fs::nonblocking_pipe()?;
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
    #[cfg(unix)]
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
    fn bare_control_characters_are_the_keys_a_unix_terminal_sends() {
        let key = |code, modifiers| KeyEvent::new(code, modifiers);
        let none = KeyModifiers::NONE;
        let control = KeyModifiers::CONTROL;
        for (character, expected) in [
            (b'\r', key(KeyCode::Enter, none)),
            (0x1b, key(KeyCode::Esc, none)),
            (b'\t', key(KeyCode::Tab, none)),
            (0x7f, key(KeyCode::Backspace, none)),
            (0x03, key(KeyCode::Char('c'), control)),
            (0x14, key(KeyCode::Char('t'), control)),
            (0x00, key(KeyCode::Char(' '), control)),
            (0x1c, key(KeyCode::Char('4'), control)),
        ] {
            assert_eq!(
                bare_key(character, false, false),
                expected,
                "{character:#x}"
            );
        }
    }

    #[test]
    fn a_bare_key_keeps_alt_and_shift() {
        assert_eq!(
            bare_key(b'\r', true, false),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)
        );
        assert_eq!(
            bare_key(b'\t', false, true),
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)
        );
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

    #[cfg(unix)]
    #[test]
    fn other_errors_are_still_failures() {
        let input = next_event_from(
            |_| Err(io::Error::other("Input/output error")),
            || panic!("nothing to read after a failed wait"),
            Duration::ZERO,
        );
        assert_eq!(input.unwrap_err().to_string(), "Input/output error");
    }

    #[cfg(windows)]
    #[test]
    fn any_console_error_is_a_closed_console() {
        let input = next_event_from(
            |_| Err(io::Error::other("The handle is invalid.")),
            || panic!("nothing to read after a failed wait"),
            Duration::ZERO,
        );
        assert!(matches!(input, Ok(Input::Closed)));
    }

    #[cfg(unix)]
    /// A pseudo-terminal: the controlling side and the side a program reads.
    fn pty() -> (OwnedFd, File) {
        let (mut master, mut slave) = (-1, -1);
        // SAFETY: `openpty` writes two descriptors that are then owned here.
        unsafe {
            let opened = libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            assert_eq!(opened, 0, "openpty: {}", io::Error::last_os_error());
            (OwnedFd::from_raw_fd(master), File::from_raw_fd(slave))
        }
    }

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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
