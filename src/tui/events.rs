//! What the TUI waits for: the terminal's input, and a steady tick for everything else.
//!
//! The loop handles one `Step` at a time. Input is passed on as it arrives; a tick comes once
//! its period has passed, even while input keeps arriving, so background work (host details,
//! tests, listings, the config watch) is looked after however busy the keyboard is. The terminal
//! is read in `read_terminal` alone, the only code that knows about the `hangup` workaround.

use std::{
    io,
    time::{Duration, Instant},
};

use crossterm::event::Event;

use super::hangup::{self, Input};

#[derive(Debug)]
pub(super) enum Step {
    Input(Event),
    /// A tick, taken at this instant.
    Tick(Instant),
    /// The terminal is gone: the TUI ends as on a hang-up.
    Closed,
}

/// Steps from a terminal reader, timed by a clock.
pub(super) struct Events<R, C> {
    read: R,
    clock: C,
    last_tick: Instant,
}

/// The terminal's events, on the real clock.
pub(super) type TerminalEvents = Events<fn(Duration) -> io::Result<Input>, fn() -> Instant>;

impl TerminalEvents {
    pub(super) fn terminal() -> Self {
        Self::new(read_terminal, Instant::now)
    }
}

/// Waits at most `timeout` for the terminal's next event.
fn read_terminal(timeout: Duration) -> io::Result<Input> {
    // Becomes crossterm's own `poll` and `read` once crossterm-rs/crossterm#1067 lets the
    // workaround go.
    hangup::next_event(timeout)
}

impl<R, C> Events<R, C>
where
    R: FnMut(Duration) -> io::Result<Input>,
    C: Fn() -> Instant,
{
    pub(super) fn new(read: R, clock: C) -> Self {
        let last_tick = clock();
        Self {
            read,
            clock,
            last_tick,
        }
    }

    /// The next step: input that arrives before the tick due `every` after the previous one,
    /// otherwise that tick.
    pub(super) fn next(&mut self, every: Duration) -> io::Result<Step> {
        loop {
            let now = (self.clock)();
            let due = self.last_tick + every;
            if now >= due {
                self.last_tick = now;
                return Ok(Step::Tick(now));
            }
            match (self.read)(due - now)? {
                Input::Event(event) => return Ok(Step::Input(event)),
                Input::Idle => {}
                Input::Closed => return Ok(Step::Closed),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;

    const EVERY: Duration = Duration::from_millis(100);

    fn key() -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE))
    }

    fn ms(milliseconds: u64) -> Duration {
        Duration::from_millis(milliseconds)
    }

    /// A terminal that answers every read `after` it starts, input waiting or not, with what
    /// `answer` gives, moving the clock on by that time.
    fn terminal<'a>(
        clock: &'a Cell<Instant>,
        after: Duration,
        mut answer: impl FnMut() -> io::Result<Input> + 'a,
    ) -> Events<impl FnMut(Duration) -> io::Result<Input> + 'a, impl Fn() -> Instant + 'a> {
        Events::new(
            move |_| {
                clock.set(clock.get() + after);
                answer()
            },
            move || clock.get(),
        )
    }

    /// A terminal where nothing happens: every wait lasts as long as it may.
    fn quiet(
        clock: &Cell<Instant>,
    ) -> Events<impl FnMut(Duration) -> io::Result<Input> + '_, impl Fn() -> Instant + '_> {
        Events::new(
            move |timeout| {
                clock.set(clock.get() + timeout);
                Ok(Input::Idle)
            },
            move || clock.get(),
        )
    }

    #[test]
    fn input_before_the_tick_is_passed_on() {
        let start = Instant::now();
        let clock = Cell::new(start);
        let mut events = terminal(&clock, ms(10), || Ok(Input::Event(key())));

        let step = events.next(EVERY).unwrap();

        assert!(matches!(step, Step::Input(event) if event == key()));
        assert_eq!(clock.get(), start + ms(10));
    }

    #[test]
    fn a_quiet_wait_ends_in_a_tick_and_the_next_one_waits_a_full_period() {
        let start = Instant::now();
        let clock = Cell::new(start);
        let mut events = quiet(&clock);

        let first = events.next(EVERY).unwrap();
        let second = events.next(EVERY).unwrap();

        assert!(matches!(first, Step::Tick(at) if at == start + EVERY));
        assert!(matches!(second, Step::Tick(at) if at == start + EVERY * 2));
    }

    #[test]
    fn ticks_keep_coming_while_input_keeps_arriving() {
        let start = Instant::now();
        let clock = Cell::new(start);
        // A held key: there is always input waiting, and each takes 30 ms to handle.
        let mut events = terminal(&clock, ms(30), || Ok(Input::Event(key())));

        let steps = (0..12)
            .map(|_| events.next(EVERY).unwrap())
            .collect::<Vec<_>>();

        let ticks = steps
            .iter()
            .filter_map(|step| match step {
                Step::Tick(at) => Some(at.duration_since(start)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(ticks, [ms(120), ms(240)]);
    }

    #[test]
    fn the_period_may_change_from_one_wait_to_the_next() {
        let start = Instant::now();
        let clock = Cell::new(start);
        let mut events = quiet(&clock);

        events.next(EVERY).unwrap();
        let busy = events.next(ms(25)).unwrap();

        assert!(matches!(busy, Step::Tick(at) if at == start + ms(125)));
    }

    #[test]
    fn a_closed_terminal_ends_the_steps_and_errors_are_passed_on() {
        let clock = Cell::new(Instant::now());
        let mut closed = terminal(&clock, ms(10), || Ok(Input::Closed));
        assert!(matches!(closed.next(EVERY).unwrap(), Step::Closed));

        let mut failing = terminal(&clock, ms(10), || {
            Err(io::Error::other("Input/output error"))
        });
        let error = failing.next(EVERY).unwrap_err();
        assert_eq!(error.to_string(), "Input/output error");
    }
}
