//! A real pseudo-terminal running Portico, read the way a user sees it.
//!
//! The terminal backend redraws only the cells that change, so words are not
//! contiguous in the raw output: everything the child prints goes through a
//! `vt100` parser that keeps the screen as a terminal would, and tests wait
//! for text to appear on that screen.

use std::{
    io::{Read, Write},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread,
    time::{Duration, Instant},
};

use portable_pty::{Child, CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system};

use crate::client::Client;

const ROWS: u16 = 24;
const COLUMNS: u16 = 120;
/// Long enough for a loaded CI runner; a passing test never waits this long.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// crossterm asks for the kitty keyboard protocol, then for the device
/// attributes that every terminal answers.
const KEYBOARD_QUERY: &[u8] = b"\x1b[?u\x1b[c";
/// The answer of a terminal without the kitty protocol.
const DEVICE_ATTRIBUTES: &[u8] = b"\x1b[?62;c";

#[derive(Clone, Copy, Debug)]
pub enum Key {
    Enter,
    Tab,
    Esc,
    Backspace,
    Up,
    Down,
    CtrlC,
}

impl Key {
    fn bytes(self) -> &'static [u8] {
        match self {
            Key::Enter => b"\r",
            Key::Tab => b"\t",
            Key::Esc => b"\x1b",
            Key::Backspace => b"\x7f",
            Key::Up => b"\x1b[A",
            Key::Down => b"\x1b[B",
            Key::CtrlC => b"\x03",
        }
    }
}

pub struct Terminal {
    // Dropping the master closes the terminal, so it lives as long as the child.
    _master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    output: Receiver<Vec<u8>>,
    child: Box<dyn Child + Send + Sync>,
    parser: vt100::Parser,
    transcript: Vec<u8>,
    answered: bool,
}

impl Terminal {
    /// Starts Portico with `args` in a 120×24 terminal, in `client`'s home.
    pub fn spawn(client: &Client, args: &[&str]) -> Self {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: ROWS,
                cols: COLUMNS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new(Client::binary());
        command.args(args);
        command.env_clear();
        command.env("TERM", "xterm-256color");
        for (key, value) in client.env() {
            command.env(key, value);
        }
        command.cwd(client.home());
        let child = pair.slave.spawn_command(command).unwrap();
        // Only the child may hold the terminal's other end, or its exit
        // would never reach the reader as end of file.
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().unwrap();
        let (sender, output) = mpsc::channel();
        thread::spawn(move || {
            let mut buffer = [0; 4096];
            while let Ok(read) = reader.read(&mut buffer) {
                if read == 0 || sender.send(buffer[..read].to_vec()).is_err() {
                    break;
                }
            }
        });
        let writer = pair.master.take_writer().unwrap();
        Self {
            _master: pair.master,
            writer,
            output,
            child,
            parser: vt100::Parser::new(ROWS, COLUMNS, 0),
            transcript: Vec::new(),
            answered: false,
        }
    }

    pub fn pid(&self) -> u32 {
        self.child.process_id().unwrap()
    }

    pub fn type_text(&mut self, text: &str) {
        self.write(text.as_bytes());
    }

    pub fn key(&mut self, key: Key) {
        self.write(key.bytes());
    }

    /// Sends `text` as a bracketed paste.
    pub fn paste(&mut self, text: &str) {
        self.write(format!("\x1b[200~{text}\x1b[201~").as_bytes());
    }

    /// What the screen shows now, one line per row.
    pub fn screen(&self) -> String {
        self.parser.screen().contents()
    }

    #[track_caller]
    pub fn wait_for(&mut self, text: &str) {
        self.wait_for_within(text, DEFAULT_TIMEOUT);
    }

    #[track_caller]
    pub fn wait_for_within(&mut self, text: &str, timeout: Duration) {
        self.wait_until(&format!("{text:?} to appear"), timeout, |screen| {
            screen.contains(text)
        });
    }

    #[track_caller]
    pub fn wait_gone(&mut self, text: &str) {
        self.wait_until(&format!("{text:?} to go"), DEFAULT_TIMEOUT, |screen| {
            !screen.contains(text)
        });
    }

    /// Waits for Portico to exit, reading what it prints until then.
    #[track_caller]
    pub fn wait_exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + DEFAULT_TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                self.drain();
                return status;
            }
            if Instant::now() > deadline {
                self.fail("portico to exit");
            }
            self.pump(Duration::from_millis(20));
        }
    }

    #[track_caller]
    fn wait_until(&mut self, what: &str, timeout: Duration, done: impl Fn(&str) -> bool) {
        let deadline = Instant::now() + timeout;
        while !done(&self.screen()) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                self.fail(what);
            }
            self.pump(remaining);
        }
    }

    /// Reads output for at most `timeout`, feeding it to the screen.
    fn pump(&mut self, timeout: Duration) {
        match self.output.recv_timeout(timeout) {
            Ok(data) => self.feed(&data),
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                // Without output to wait for, do not spin.
                if timeout > Duration::ZERO {
                    thread::sleep(timeout.min(Duration::from_millis(10)));
                }
            }
        }
        self.drain();
    }

    fn drain(&mut self) {
        while let Ok(data) = self.output.try_recv() {
            self.feed(&data);
        }
    }

    fn feed(&mut self, data: &[u8]) {
        self.transcript.extend_from_slice(data);
        self.parser.process(data);
        if !self.answered
            && self
                .transcript
                .windows(KEYBOARD_QUERY.len())
                .any(|window| window == KEYBOARD_QUERY)
        {
            self.answered = true;
            self.write(DEVICE_ATTRIBUTES);
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }

    #[track_caller]
    fn fail(&self, what: &str) -> ! {
        let tail = &self.transcript[self.transcript.len().saturating_sub(2048)..];
        panic!(
            "timed out waiting for {what}\n--- screen ---\n{}\n--- last output ---\n{}",
            self.screen(),
            tail.escape_ascii()
        );
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
