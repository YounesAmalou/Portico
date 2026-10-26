//! Closing the terminal Portico runs in, and how much processor it uses while
//! it waits for keys.
//!
//! The shared `Terminal` keeps a second handle on its pseudo-terminal for the
//! thread that reads it, so dropping it never hangs the terminal up. These
//! tests open their own: the only handle on the terminal's side is the one
//! they close.

use std::{
    fs,
    io::{self, Read, Write},
    num::NonZeroU16,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    os::unix::process::CommandExt,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use crate::client::Client;

const ROWS: u16 = 24;
const COLUMNS: u16 = 120;
const TIMEOUT: Duration = Duration::from_secs(10);
/// A closed terminal must end Portico well within this.
const EXIT_DEADLINE: Duration = Duration::from_secs(3);
const KEYBOARD_QUERY: &[u8] = b"\x1b[?u\x1b[c";
const DEVICE_ATTRIBUTES: &[u8] = b"\x1b[?62;c";

struct HangupTerminal {
    master: Option<fs::File>,
    child: Child,
    parser: vt100::Parser,
    transcript: Vec<u8>,
    answered: bool,
}

impl HangupTerminal {
    /// Starts Portico as the leader of a new session whose controlling
    /// terminal is a fresh pseudo-terminal, the way a terminal emulator does.
    fn spawn(client: &Client) -> Self {
        Self::spawn_with(client, false)
    }

    /// Like `spawn`; with `block_sighup`, SIGHUP stays blocked in Portico, as if nothing
    /// sent it, so only the closed terminal itself can end it.
    fn spawn_with(client: &Client, block_sighup: bool) -> Self {
        let size = libc::winsize {
            ws_row: ROWS,
            ws_col: COLUMNS,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let (mut master, mut slave) = (-1, -1);
        // SAFETY: `openpty` writes two descriptors that are then owned here.
        let (master, slave) = unsafe {
            let opened = libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                &size,
            );
            assert_eq!(opened, 0, "openpty: {}", io::Error::last_os_error());
            (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave))
        };
        // SAFETY: plain flag changes on a descriptor owned here.
        unsafe {
            // The child must not inherit the master, or closing it here would
            // not hang the terminal up.
            assert_ne!(
                libc::fcntl(master.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC),
                -1
            );
            let flags = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
            assert_ne!(
                libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK),
                -1
            );
        }

        let mut command = Command::new(Client::binary());
        command
            .env_clear()
            .env("TERM", "xterm-256color")
            .envs(client.env())
            .current_dir(client.home())
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                if block_sighup {
                    let mut set = std::mem::zeroed();
                    libc::sigemptyset(&mut set);
                    libc::sigaddset(&mut set, libc::SIGHUP);
                    if libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        // The command's copies of the slave close with it.
        drop(command);

        Self {
            master: Some(fs::File::from(master)),
            child,
            parser: vt100::Parser::new(
                NonZeroU16::new(ROWS).expect("the screen has rows"),
                NonZeroU16::new(COLUMNS).expect("the screen has columns"),
                0,
            ),
            transcript: Vec::new(),
            answered: false,
        }
    }

    fn type_text(&mut self, text: &str) {
        let master = self.master.as_mut().unwrap();
        master.write_all(text.as_bytes()).unwrap();
    }

    #[track_caller]
    fn wait_for(&mut self, text: &str) {
        let deadline = Instant::now() + TIMEOUT;
        while !self.parser.screen().contents().contains(text) {
            if Instant::now() > deadline {
                panic!(
                    "timed out waiting for {text:?}\n--- screen ---\n{}",
                    self.parser.screen().contents()
                );
            }
            self.pump(Duration::from_millis(20));
        }
    }

    /// Reads output for at most `timeout`, answering crossterm's keyboard query.
    fn pump(&mut self, timeout: Duration) {
        let master = self.master.as_mut().unwrap();
        let mut poll = libc::pollfd {
            fd: master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid `pollfd`.
        unsafe { libc::poll(&mut poll, 1, timeout.as_millis() as libc::c_int) };
        let mut buffer = [0; 4096];
        while let Ok(read) = master.read(&mut buffer) {
            if read == 0 {
                break;
            }
            self.transcript.extend_from_slice(&buffer[..read]);
            self.parser.process(&buffer[..read]);
        }
        if !self.answered
            && self
                .transcript
                .windows(KEYBOARD_QUERY.len())
                .any(|window| window == KEYBOARD_QUERY)
        {
            self.answered = true;
            master.write_all(DEVICE_ATTRIBUTES).unwrap();
        }
    }

    /// Processor time Portico has used so far, all its threads together.
    fn cpu_time(&self) -> Duration {
        cpu_time(self.child.id())
    }

    /// Closes the terminal, as closing a terminal emulator's window does.
    fn hang_up(&mut self) {
        self.master = None;
    }

    /// Fails unless Portico exits within `EXIT_DEADLINE` of the hang-up.
    #[track_caller]
    fn assert_exits(&mut self) {
        let started = Instant::now();
        let used_before = self.cpu_time();
        while started.elapsed() < EXIT_DEADLINE {
            if self.child.try_wait().unwrap().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let used = self.cpu_time().saturating_sub(used_before);
        panic!(
            "portico still runs {EXIT_DEADLINE:?} after its terminal closed, having used \
             {used:?} of processor time since"
        );
    }
}

impl Drop for HangupTerminal {
    fn drop(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// The user and system time of process `pid`, from `/proc/<pid>/stat`.
fn cpu_time(pid: u32) -> Duration {
    let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return Duration::ZERO;
    };
    // The command name may hold spaces; the fields after it do not.
    let fields: Vec<&str> = stat[stat.rfind(')').unwrap() + 2..]
        .split_whitespace()
        .collect();
    // utime and stime are the 14th and 15th fields, so the 12th and 13th here.
    let ticks: u64 = fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap();
    // SAFETY: `sysconf` only reads a configuration value.
    let per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as u64;
    Duration::from_millis(ticks * 1000 / per_second)
}

#[test]
#[ignore = "end-to-end: run with scripts/ci-checks.sh e2e"]
fn closing_the_terminal_ends_the_host_list() {
    let client = Client::new();
    client.init();
    let mut terminal = HangupTerminal::spawn(&client);
    terminal.wait_for("quit");

    terminal.hang_up();
    terminal.assert_exits();
}

#[test]
#[ignore = "end-to-end: run with scripts/ci-checks.sh e2e"]
fn closing_the_terminal_ends_the_host_list_without_sighup() {
    let client = Client::new();
    client.init();
    let mut terminal = HangupTerminal::spawn_with(&client, true);
    terminal.wait_for("quit");

    terminal.hang_up();
    terminal.assert_exits();
}

#[test]
#[ignore = "end-to-end: run with scripts/ci-checks.sh e2e"]
fn closing_the_terminal_ends_the_first_run_prompt() {
    let client = Client::new();
    let mut terminal = HangupTerminal::spawn(&client);
    terminal.wait_for("y initialize");

    terminal.hang_up();
    terminal.assert_exits();
}

#[test]
#[ignore = "end-to-end: run with scripts/ci-checks.sh e2e"]
fn closing_the_terminal_ends_an_open_form() {
    let client = Client::new();
    client.init();
    let mut terminal = HangupTerminal::spawn(&client);
    terminal.wait_for("quit");
    terminal.type_text("a");
    terminal.wait_for("Enter save");

    terminal.hang_up();
    terminal.assert_exits();
}

#[test]
#[ignore = "end-to-end: run with scripts/ci-checks.sh e2e"]
fn an_idle_host_list_barely_uses_the_processor() {
    let client = Client::new();
    client.init();
    let mut terminal = HangupTerminal::spawn(&client);
    terminal.wait_for("quit");
    // Let start-up work (the first resolution, the containers listing) settle.
    terminal.pump(Duration::from_millis(500));

    let window = Duration::from_secs(3);
    let before = terminal.cpu_time();
    let started = Instant::now();
    while started.elapsed() < window {
        terminal.pump(Duration::from_millis(100));
    }
    let used = terminal.cpu_time().saturating_sub(before);
    // A wake-up every 100 ms costs microseconds; 2% of one core leaves room
    // for a loaded runner while catching any busy loop.
    assert!(
        used < window / 50,
        "an idle Portico used {used:?} of processor time in {window:?}"
    );

    terminal.type_text("q");
    let deadline = Instant::now() + TIMEOUT;
    while terminal.child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "portico did not quit on q");
        terminal.pump(Duration::from_millis(20));
    }
}
