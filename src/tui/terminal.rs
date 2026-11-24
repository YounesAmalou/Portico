//! The terminal session: raw mode, the alternate screen, signals and handing the terminal to a child.

use std::{
    io, panic,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

use crossterm::{
    cursor::{Hide, Show},
    event::{
        DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
        supports_keyboard_enhancement,
    },
};
use ratatui::{Terminal, backend::Backend, layout::Rect};

use super::TuiError;

/// Clears the alternate screen and forgets what was drawn, so the next frame repaints everything.
///
/// Unlike `Terminal::clear`, this never asks the terminal for the cursor position: the
/// cursor is hidden here, and a terminal that is slow to answer must not abort the interface.
fn redraw_from_scratch<B: Backend>(terminal: &mut Terminal<B>) -> Result<(), B::Error> {
    let size = terminal.size()?;
    terminal.resize(Rect::new(0, 0, size.width, size.height))
}

/// Runs `child` in the terminal with the interface suspended and the terminal's Ctrl+C left to
/// the child (Portico waits for it), then redraws. Failing to leave or re-enter the screen is
/// the outer error; the child's own result is passed through.
pub(super) fn suspended<B: Backend<Error = io::Error>, T>(
    terminal: &mut Terminal<B>,
    session: &TerminalSession,
    signals: &Signals,
    child: impl FnOnce() -> Result<T, TuiError>,
) -> Result<Result<T, TuiError>, TuiError> {
    let result = suspend_for_child(
        || session.leave(),
        || {
            let _foreground = signals.foreground_child();
            child()
        },
        || session.enter(),
    );
    if let Err(TuiError::Io(error)) = result {
        return Err(TuiError::Io(error));
    }
    redraw_from_scratch(terminal)?;
    Ok(result)
}

/// The signals that end the TUI.
///
/// SIGTERM and SIGHUP always do. SIGINT does too, except while an in-place `ssh` owns the
/// terminal: with raw mode off, the terminal's Ctrl+C reaches the whole foreground process
/// group, so `ssh` and Portico both receive it, and it is `ssh`'s to act on.
pub(super) struct Signals {
    terminated: Arc<AtomicBool>,
    child_in_foreground: Arc<AtomicBool>,
}

impl Signals {
    pub(super) fn install() -> io::Result<Self> {
        let signals = Self {
            terminated: Arc::new(AtomicBool::new(false)),
            child_in_foreground: Arc::new(AtomicBool::new(false)),
        };
        for signal in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGHUP] {
            signal_hook::flag::register(signal, Arc::clone(&signals.terminated))?;
        }
        let terminated = Arc::clone(&signals.terminated);
        let child_in_foreground = Arc::clone(&signals.child_in_foreground);
        // SAFETY: the handler only performs atomic loads and stores, which are async-signal-safe.
        unsafe {
            signal_hook::low_level::register(signal_hook::consts::SIGINT, move || {
                if !child_in_foreground.load(Ordering::Relaxed) {
                    terminated.store(true, Ordering::Relaxed);
                }
            })?;
        }
        Ok(signals)
    }

    /// Set once a signal asks the TUI to end.
    pub(super) fn flag(&self) -> &AtomicBool {
        &self.terminated
    }

    /// The same flag, for whatever else may end the TUI.
    pub(super) fn shared_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.terminated)
    }

    /// Marks an in-place child as owning the terminal until the returned scope is dropped.
    fn foreground_child(&self) -> ForegroundChild<'_> {
        self.child_in_foreground.store(true, Ordering::Relaxed);
        ForegroundChild(&self.child_in_foreground)
    }
}

struct ForegroundChild<'a>(&'a AtomicBool);

impl Drop for ForegroundChild<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

/// The terminal operations a session performs, separated so the session's bookkeeping can be
/// tested without a terminal.
trait TerminalControl: Send + Sync {
    fn enable_raw_mode(&self) -> io::Result<()>;
    fn disable_raw_mode(&self) -> io::Result<()>;
    fn enter_screen(&self) -> io::Result<()>;
    fn leave_screen(&self) -> io::Result<()>;
    fn push_keyboard_flags(&self) -> io::Result<()>;
    fn pop_keyboard_flags(&self) -> io::Result<()>;
}

struct CrosstermControl;

impl TerminalControl for CrosstermControl {
    fn enable_raw_mode(&self) -> io::Result<()> {
        enable_raw_mode()
    }

    fn disable_raw_mode(&self) -> io::Result<()> {
        disable_raw_mode()
    }

    fn enter_screen(&self) -> io::Result<()> {
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste,
            Hide
        )
    }

    fn leave_screen(&self) -> io::Result<()> {
        let screen = execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen);
        let cursor = execute!(io::stdout(), Show);
        screen.and(cursor)
    }

    fn push_keyboard_flags(&self) -> io::Result<()> {
        execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )
    }

    fn pop_keyboard_flags(&self) -> io::Result<()> {
        execute!(io::stdout(), PopKeyboardEnhancementFlags)
    }
}

/// The terminal modes Portico has switched on, so each is switched off exactly once.
///
/// `enter` and `leave` are idempotent and each undoes only what is recorded as done. That keeps
/// an error halfway through `enter`, a `leave` before the child runs and the final restore from
/// ever popping keyboard flags that were not pushed (the kitty protocol keeps a stack).
pub(super) struct TerminalSession {
    control: Box<dyn TerminalControl>,
    /// Whether the terminal reports modifiers on keys such as Enter (the kitty keyboard protocol).
    keyboard_enhancement: bool,
    raw: AtomicBool,
    screen: AtomicBool,
    keyboard_flags: AtomicBool,
}

impl TerminalSession {
    pub(super) fn detect() -> Self {
        // A terminal that cannot answer the query simply keeps its legacy key reporting.
        let keyboard_enhancement = supports_keyboard_enhancement().unwrap_or(false);
        Self::new(Box::new(CrosstermControl), keyboard_enhancement)
    }

    fn new(control: Box<dyn TerminalControl>, keyboard_enhancement: bool) -> Self {
        Self {
            control,
            keyboard_enhancement,
            raw: AtomicBool::new(false),
            screen: AtomicBool::new(false),
            keyboard_flags: AtomicBool::new(false),
        }
    }

    pub(super) fn keyboard_enhancement(&self) -> bool {
        self.keyboard_enhancement
    }

    /// Switches on every mode not yet on; a failure leaves the terminal restored.
    pub(super) fn enter(&self) -> Result<(), TuiError> {
        self.enter_modes().map_err(|error| {
            let _ = self.leave();
            error.into()
        })
    }

    fn enter_modes(&self) -> io::Result<()> {
        if !self.raw.load(Ordering::Relaxed) {
            self.control.enable_raw_mode()?;
            self.raw.store(true, Ordering::Relaxed);
        }
        if !self.screen.load(Ordering::Relaxed) {
            // Recorded first: a write that failed partway may still have switched modes on.
            self.screen.store(true, Ordering::Relaxed);
            self.control.enter_screen()?;
        }
        // The kitty keyboard protocol keeps separate flags per screen, so push them after switching.
        if self.keyboard_enhancement && !self.keyboard_flags.load(Ordering::Relaxed) {
            self.control.push_keyboard_flags()?;
            self.keyboard_flags.store(true, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Switches off every mode that is on, reporting the first failure after trying them all.
    pub(super) fn leave(&self) -> Result<(), TuiError> {
        let keys = if self.keyboard_flags.swap(false, Ordering::Relaxed) {
            self.control.pop_keyboard_flags()
        } else {
            Ok(())
        };
        let raw = if self.raw.swap(false, Ordering::Relaxed) {
            self.control.disable_raw_mode()
        } else {
            Ok(())
        };
        let screen = if self.screen.swap(false, Ordering::Relaxed) {
            self.control.leave_screen()
        } else {
            Ok(())
        };
        keys.and(raw).and(screen)?;
        Ok(())
    }
}

/// Restores the terminal when dropped, including while unwinding from a panic.
pub(super) struct TerminalGuard(pub(super) Arc<TerminalSession>);

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.0.leave();
    }
}

type PanicHandler = dyn Fn(&panic::PanicHookInfo<'_>) + Send + Sync + 'static;

/// Runs `restore` before the previous panic hook reports, so the message lands on the normal
/// screen instead of vanishing with the alternate one. Dropping it removes the extra step.
pub(super) struct PanicHook {
    previous: Arc<PanicHandler>,
}

impl PanicHook {
    pub(super) fn install(restore: impl Fn() + Send + Sync + 'static) -> Self {
        let previous: Arc<PanicHandler> = Arc::from(panic::take_hook());
        let delegate = Arc::clone(&previous);
        panic::set_hook(Box::new(move |info| {
            restore();
            delegate(info);
        }));
        Self { previous }
    }
}

impl Drop for PanicHook {
    fn drop(&mut self) {
        // The hook cannot be replaced while unwinding; the process is on its way out then.
        if !thread::panicking() {
            let previous = Arc::clone(&self.previous);
            panic::set_hook(Box::new(move |info| previous(info)));
        }
    }
}

fn suspend_for_child<T, L, C, E>(mut leave: L, child: C, mut enter: E) -> Result<T, TuiError>
where
    L: FnMut() -> Result<(), TuiError>,
    C: FnOnce() -> Result<T, TuiError>,
    E: FnMut() -> Result<(), TuiError>,
{
    leave()?;
    let child_result = child();
    enter()?;
    child_result
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, thread};

    use ratatui::{Terminal, backend::TestBackend, widgets::Paragraph};

    use super::*;

    /// Records the terminal operations it is asked for, optionally failing one.
    #[derive(Clone, Default)]
    struct RecordingControl {
        calls: Arc<std::sync::Mutex<Vec<&'static str>>>,
        failing: Option<&'static str>,
    }

    impl RecordingControl {
        fn call(&self, name: &'static str) -> io::Result<()> {
            self.calls.lock().unwrap().push(name);
            if self.failing == Some(name) {
                Err(io::Error::other(name))
            } else {
                Ok(())
            }
        }

        fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl TerminalControl for RecordingControl {
        fn enable_raw_mode(&self) -> io::Result<()> {
            self.call("raw on")
        }
        fn disable_raw_mode(&self) -> io::Result<()> {
            self.call("raw off")
        }
        fn enter_screen(&self) -> io::Result<()> {
            self.call("screen on")
        }
        fn leave_screen(&self) -> io::Result<()> {
            self.call("screen off")
        }
        fn push_keyboard_flags(&self) -> io::Result<()> {
            self.call("push")
        }
        fn pop_keyboard_flags(&self) -> io::Result<()> {
            self.call("pop")
        }
    }

    fn session(control: &RecordingControl) -> TerminalSession {
        TerminalSession::new(Box::new(control.clone()), true)
    }

    #[test]
    fn session_enter_and_leave_are_idempotent() {
        let control = RecordingControl::default();
        let session = session(&control);

        session.leave().unwrap();
        session.enter().unwrap();
        session.enter().unwrap();
        session.leave().unwrap();
        session.leave().unwrap();
        session.enter().unwrap();

        assert_eq!(
            control.calls(),
            [
                "raw on",
                "screen on",
                "push",
                "pop",
                "raw off",
                "screen off",
                "raw on",
                "screen on",
                "push"
            ]
        );
    }

    #[test]
    fn session_never_pops_flags_it_did_not_push() {
        // The screen switch fails before the push, then the guard restores as well.
        let control = RecordingControl {
            failing: Some("screen on"),
            ..RecordingControl::default()
        };
        let session = session(&control);

        assert!(session.enter().is_err());
        session.leave().unwrap();

        assert!(!control.calls().contains(&"pop"));
        assert!(control.calls().contains(&"raw off"));
    }

    #[test]
    fn session_pops_once_when_reentering_fails_after_the_push() {
        let control = RecordingControl {
            failing: Some("push"),
            ..RecordingControl::default()
        };
        let session = session(&control);

        assert!(session.enter().is_err());
        session.leave().unwrap();

        assert_eq!(
            control.calls(),
            ["raw on", "screen on", "push", "raw off", "screen off"]
        );
    }

    #[test]
    fn session_without_keyboard_enhancement_never_touches_flags() {
        let control = RecordingControl::default();
        let session = TerminalSession::new(Box::new(control.clone()), false);

        session.enter().unwrap();
        session.leave().unwrap();

        assert_eq!(
            control.calls(),
            ["raw on", "screen on", "raw off", "screen off"]
        );
    }

    #[test]
    fn leave_reports_a_failure_but_still_restores_every_mode() {
        let control = RecordingControl {
            failing: Some("pop"),
            ..RecordingControl::default()
        };
        let session = session(&control);
        session.enter().unwrap();

        assert!(session.leave().is_err());

        assert_eq!(&control.calls()[3..], ["pop", "raw off", "screen off"]);
    }

    #[test]
    fn panic_hook_restores_the_terminal_before_the_message_is_reported() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let this_thread = thread::current().id();
        let record = move |log: &Arc<std::sync::Mutex<Vec<&'static str>>>, entry| {
            // The hook is process-wide; ignore panics from other tests' threads.
            if thread::current().id() == this_thread {
                log.lock().unwrap().push(entry);
            }
        };
        let original = panic::take_hook();
        let reported = Arc::clone(&log);
        panic::set_hook(Box::new(move |_| record(&reported, "report")));
        let restoring = Arc::clone(&log);
        let hook = PanicHook::install(move || record(&restoring, "restore"));

        let _ = panic::catch_unwind(|| panic!("boom"));
        assert_eq!(*log.lock().unwrap(), ["restore", "report"]);

        drop(hook);
        let _ = panic::catch_unwind(|| panic!("boom"));
        assert_eq!(*log.lock().unwrap(), ["restore", "report", "report"]);
        panic::set_hook(original);
    }

    /// Serialises the tests that install `Signals` and raise signals.
    ///
    /// Signal handlers are process-wide and are never unregistered, so a signal one test raises
    /// runs the handlers every other test installed too: SIGTERM raised while another test holds
    /// a foreground child would set that test's flag. These tests must not overlap.
    fn signal_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn sigint_ends_the_tui_unless_a_child_owns_the_terminal() {
        let _lock = signal_test_lock();
        let signals = Signals::install().unwrap();
        {
            let _child = signals.foreground_child();
            signal_hook::low_level::raise(signal_hook::consts::SIGINT).unwrap();
            assert!(!signals.flag().load(Ordering::Relaxed));
        }
        signal_hook::low_level::raise(signal_hook::consts::SIGINT).unwrap();
        assert!(signals.flag().load(Ordering::Relaxed));
    }

    #[test]
    fn sigterm_and_sighup_end_the_tui_even_while_a_child_runs() {
        let _lock = signal_test_lock();
        for signal in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGHUP] {
            let signals = Signals::install().unwrap();
            let _child = signals.foreground_child();
            signal_hook::low_level::raise(signal).unwrap();
            assert!(signals.flag().load(Ordering::Relaxed), "{signal}");
        }
    }

    #[test]
    fn restores_terminal_around_child_even_when_child_fails() {
        let events = std::cell::RefCell::new(Vec::new());
        let result: Result<(), TuiError> = suspend_for_child(
            || {
                events.borrow_mut().push("leave");
                Ok(())
            },
            || {
                events.borrow_mut().push("child");
                Err(TuiError::Child("failed".into()))
            },
            || {
                events.borrow_mut().push("enter");
                Ok(())
            },
        );

        assert!(result.is_err());
        assert_eq!(*events.borrow(), ["leave", "child", "enter"]);
    }

    #[test]
    fn redraw_from_scratch_repaints_a_screen_the_terminal_lost() {
        let mut terminal = Terminal::new(TestBackend::new(20, 3)).unwrap();
        let draw = |terminal: &mut Terminal<TestBackend>| {
            terminal
                .draw(|frame| frame.render_widget(Paragraph::new("portico"), frame.area()))
                .unwrap();
        };
        draw(&mut terminal);
        // The terminal forgot the frame, as after leaving and re-entering the alternate screen.
        terminal.backend_mut().clear().unwrap();
        draw(&mut terminal);
        assert_eq!(terminal.backend().buffer()[(0, 0)].symbol(), " ");

        terminal.backend_mut().clear().unwrap();
        redraw_from_scratch(&mut terminal).unwrap();
        draw(&mut terminal);

        assert_eq!(terminal.backend().buffer()[(0, 0)].symbol(), "p");
    }
}
