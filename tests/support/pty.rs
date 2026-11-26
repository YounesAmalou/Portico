//! The real binary/terminal boundary: Portico in a pseudo-terminal (ConPTY on Windows), driven
//! through scenarios the way a user would.
//!
//! The terminal backend only redraws changed cells, so words are not contiguous in the raw
//! output: everything Portico prints goes through a `vt100` parser that keeps the screen, and
//! scenarios wait for text on that screen. Raw mode switches and keyboard flags are checked in
//! the raw output, and on Unix the terminal's own settings must come back as they were.
//! Windows' console has no such escape sequences or settings to check, nor signals, bracketed
//! paste or the kitty keyboard protocol, so the scenarios about them are Unix-only.

use std::{
    fs,
    io::{Read, Write},
    num::NonZeroU16,
    panic::{self, AssertUnwindSafe},
    path::Path,
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread,
    time::{Duration, Instant},
};

use portable_pty::{Child, CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system};

use super::TestEnv;

const ROWS: u16 = 24;
const COLUMNS: u16 = 120;

/// Whether the raw output carries the escape sequences the checks look for.
const RAW_SEQUENCES: bool = cfg!(unix);
/// The Esc key: with the kitty keyboard protocol a lone escape byte is not a key, on Windows
/// the console turns the byte into the key.
const ESC: &[u8] = if cfg!(unix) { b"\x1b[27u" } else { b"\x1b" };

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Scenario {
    Paste,
    PasteFilterEdit,
    BlockedResolution,
    BlockedMutationLock,
    BlockedInitLock,
    BlockedConnection,
    InterruptConnection,
    InputRules,
    LauncherFailure,
    CtrlEnter,
    Refresh,
    LiveReload,
    LegacyKeyboard,
    ResponsiveNavigation,
    NormalExit,
    ChildFailure,
    ConnectionTest,
    Tabs,
    RememberedUser,
    RememberedUserRestored,
    Settings,
    DesktopAction,
    /// Portico is sent this signal instead of a key.
    #[cfg(unix)]
    Signal(libc::c_int),
}

use Scenario::*;

/// The terminal lifecycle scenarios, which share one home, in order.
pub fn lifecycle() -> Vec<Scenario> {
    let mut scenarios = Vec::new();
    #[cfg(unix)]
    scenarios.extend([
        Paste,
        PasteFilterEdit,
        BlockedResolution,
        BlockedMutationLock,
        BlockedInitLock,
        BlockedConnection,
        InterruptConnection,
        InputRules,
    ]);
    scenarios.push(LauncherFailure);
    #[cfg(unix)]
    scenarios.push(CtrlEnter);
    scenarios.extend([Refresh, LiveReload]);
    #[cfg(unix)]
    scenarios.push(LegacyKeyboard);
    scenarios.extend([ResponsiveNavigation, NormalExit]);
    #[cfg(unix)]
    scenarios.extend([
        ChildFailure,
        Signal(libc::SIGINT),
        Signal(libc::SIGTERM),
        Signal(libc::SIGHUP),
    ]);
    scenarios
}

/// Runs Portico in `env` through `scenario`, from its first screen until it has exited and
/// left the terminal as it found it.
pub fn check_terminal(env: &TestEnv, scenario: Scenario) {
    let home = env.home();
    let scenario_home = if scenario == BlockedInitLock {
        home.join("uninitialized")
    } else {
        home.to_owned()
    };
    let held_lock = matches!(scenario, BlockedMutationLock | BlockedInitLock).then(|| {
        let lock_path = scenario_home.join(".ssh/portico/config.lock");
        fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
        let lock = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(lock_path)
            .unwrap();
        fs2::FileExt::try_lock_exclusive(&lock).unwrap();
        lock
    });
    let marker = match scenario {
        BlockedResolution | ResponsiveNavigation => Some("block-resolver"),
        BlockedConnection => Some("block-connection"),
        InterruptConnection => Some("interrupt-connection"),
        LauncherFailure => Some("fail-launcher"),
        _ => None,
    };
    if let Some(marker) = marker {
        fs::write(home.join(marker), "").unwrap();
    }
    let managed_before = (scenario == LiveReload)
        .then(|| fs::read_to_string(scenario_home.join(".ssh/portico/config")).unwrap());

    let mut terminal = Terminal::spawn(env, &scenario_home, scenario);
    let result = panic::catch_unwind(AssertUnwindSafe(|| run(&mut terminal, home, scenario)));
    if result.is_err() {
        terminal.kill();
    }
    clean_up(home, scenario, managed_before.as_deref());
    drop(held_lock);
    if let Err(payload) = result {
        panic::resume_unwind(payload);
    }
    println!("PTY restored: {scenario:?}");
}

fn run(terminal: &mut Terminal, home: &Path, scenario: Scenario) {
    let kitty = terminal.kitty;
    match scenario {
        BlockedResolution | ResponsiveNavigation => {
            if RAW_SEQUENCES {
                terminal.read_until(b"\x1b[?1049h", 0, Duration::from_secs(5));
            }
            wait_for_file(&home.join("resolver-started"), "resolver did not start");
        }
        BlockedInitLock => {
            terminal.read_screen("y initialize");
            terminal.write(b"y");
            thread::sleep(Duration::from_millis(100));
        }
        // Crossterm positions words independently, so do not expect literal spaces.
        _ => terminal.read_screen("quit"),
    }
    #[cfg(unix)]
    assert!(
        terminal.is_raw(),
        "{scenario:?}: the terminal is not in raw mode"
    );
    if RAW_SEQUENCES {
        assert!(terminal.transcript_has(b"\x1b[?1049h", 0));
    }
    // The kitty keyboard flags are pushed only where the terminal reported support.
    if kitty {
        terminal.read_until(b"\x1b[>1u", 0, Duration::from_secs(5));
    }

    act(terminal, home, scenario);

    let start = terminal.transcript.len();
    match scenario {
        #[cfg(unix)]
        BlockedResolution | BlockedMutationLock | BlockedInitLock | BlockedConnection => {
            terminal.signal(libc::SIGTERM)
        }
        #[cfg(unix)]
        Signal(signal) => terminal.signal(signal),
        _ => terminal.write(b"q"),
    }
    let timeout = if matches!(
        scenario,
        BlockedResolution
            | ResponsiveNavigation
            | BlockedMutationLock
            | BlockedInitLock
            | BlockedConnection
    ) {
        Duration::from_secs(1)
    } else {
        Duration::from_secs(5)
    };
    let status = if RAW_SEQUENCES {
        terminal.read_until(b"\x1b[?1049l", start, timeout);
        let left = terminal.find(b"\x1b[?1049l", start).unwrap();
        if kitty {
            assert!(
                terminal
                    .find(b"\x1b[<1u", start)
                    .is_some_and(|pop| pop < left),
                "{scenario:?}: keyboard flags were not popped"
            );
        } else {
            assert!(!terminal.transcript_has(b"\x1b[<1u", 0));
        }
        terminal.read_until(b"\x1b[?25h", left, Duration::from_secs(5));
        terminal.wait_draining(Duration::from_secs(5))
    } else {
        terminal.wait_draining(timeout)
    };
    assert!(
        status.success(),
        "{scenario:?}: exit status {status}: {:?}",
        terminal.transcript.escape_ascii().to_string()
    );
    #[cfg(unix)]
    assert!(
        terminal.termios_restored(),
        "{scenario:?}: termios was not restored"
    );
    if RAW_SEQUENCES {
        assert!(terminal.transcript_has(b"\x1b[?1049l", start));
    }
    #[cfg(unix)]
    if scenario == BlockedConnection {
        for pid in connection_pids(home) {
            let deadline = Instant::now() + Duration::from_secs(1);
            while is_running(pid) {
                assert!(
                    Instant::now() < deadline,
                    "connection process survived: {pid}"
                );
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// What the user does in `scenario` once Portico shows its first screen.
fn act(terminal: &mut Terminal, home: &Path, scenario: Scenario) {
    let managed = home.join(".ssh/portico/config");
    match scenario {
        Paste => {
            terminal.write(b"a");
            terminal.read_screen("Enter save");
            terminal.write(b"\x1b[200~pasted\x1b[201~");
            terminal.write(b"\t");
            terminal.write(b"\x1b[200~pasted.example\x1b[201~");
            terminal.write(b"\r");
            terminal.read_screen("Saved");
            assert!(
                fs::read_to_string(&managed)
                    .unwrap()
                    .contains("Host pasted\n")
            );
        }
        PasteFilterEdit => {
            terminal.write(b"/");
            terminal.write(b"\x1b[200~beta\x1b[201~");
            terminal.write(b"\r");
            terminal.write(b"e");
            terminal.read_screen("Enter save");
            terminal.write(b"\x1b[200~x\x1b[201~");
            terminal.write(b"\r");
            terminal.read_screen("Saved");
            let config = fs::read_to_string(&managed).unwrap();
            assert!(config.contains("Host alpha\n"), "{config}");
            assert!(config.contains("Host betax\n"), "{config}");
        }
        BlockedMutationLock => {
            terminal.write(b"a");
            terminal.read_screen("Enter save");
            terminal.write(b"\x1b[200~locked\x1b[201~");
            terminal.write(b"\t");
            terminal.write(b"\x1b[200~locked.example\x1b[201~");
            terminal.write(b"\r");
            thread::sleep(Duration::from_millis(100));
        }
        #[cfg(unix)]
        BlockedConnection => {
            terminal.write(b"\r");
            let [_, parent, worker, child] = connection_pids(home);
            // Linux lists children per thread: the connection's child belongs to the worker.
            if Path::new("/proc").is_dir() {
                let children = |thread: u32| {
                    fs::read_to_string(format!("/proc/{parent}/task/{thread}/children"))
                        .unwrap()
                        .split_whitespace()
                        .map(|pid| pid.parse::<u32>().unwrap())
                        .collect::<Vec<_>>()
                };
                assert!(!children(parent).contains(&child));
                assert!(children(worker).contains(&child));
            }
        }
        #[cfg(unix)]
        InterruptConnection => {
            // Ctrl+C while ssh connects goes to the whole foreground group: ssh ends, Portico
            // stays.
            let left_at = terminal.transcript.len();
            terminal.write(b"\r");
            // Read the switch away from the TUI first: a terminal's Ctrl+C discards output not
            // yet read, which on macOS can still hold it.
            terminal.read_until(b"\x1b[?1049l", left_at, Duration::from_secs(5));
            wait_for_file(&home.join("interrupt-ready"), "connection did not start");
            terminal.write(b"\x03");
            terminal.read_screen("exited");
            assert!(terminal.screen().contains("alpha"));
            thread::sleep(Duration::from_millis(200));
            assert!(
                terminal.is_running(),
                "Ctrl+C during the connection quit the TUI"
            );
            let left = terminal
                .rfind(b"\x1b[?1049l")
                .expect("the TUI never left the screen");
            assert!(
                terminal.transcript_has(b"\x1b[?1049h", left),
                "the TUI did not come back"
            );
        }
        InputRules => {
            // Whitespace never enters a field, however it arrives; the saved host proves it.
            terminal.write(b"a");
            terminal.read_screen("Enter save");
            terminal.write(b"my host\t");
            terminal.write(b"\x1b[200~my.exa mple\x1b[201~\t");
            terminal.write(b"de ploy\t2 2\t");
            // A typed leading or trailing space is refused; a pasted inner one is kept.
            terminal.write(b" ~/.ssh/my \x1b[200~ key\x1b[201~ \t");
            terminal.write(b"\x1b[200~bas tion, jump\n\x1b[201~\r");
            terminal.read_screen("Saved");
            let config = fs::read_to_string(&managed).unwrap();
            let block = &config[config.find("Host myhost\n").expect(&config)..];
            for line in [
                "HostName my.example",
                "User deploy",
                "Port 22",
                "ProxyJump bastion,jump",
            ] {
                assert!(block.contains(line), "{block}");
            }
            assert!(
                block.contains("IdentityFile \"~/.ssh/my key\"\n"),
                "{block}"
            );
        }
        Refresh => {
            // What `ssh -G` resolves can change without any config file changing: no watch sees
            // it, so it stays invisible until `r` re-reads everything.
            terminal.read_screen("fail.example");
            fs::write(home.join("resolved-hostname"), "changed.example").unwrap();
            thread::sleep(Duration::from_millis(1500));
            assert!(!terminal.screen().contains("changed.example"));
            terminal.write(b"r");
            terminal.read_screen("Refreshed: ");
            terminal.read_screen("changed.example");
            // A refresh that cannot read the inventory keeps the screen as it was.
            super::deny_read(&managed);
            terminal.write(b"\x1b[15~");
            terminal.read_screen("Refresh failed");
            assert!(terminal.screen().contains("alpha"));
            super::allow_read(&managed);
            terminal.write(b"r");
            terminal.read_screen("Refreshed: ");
        }
        LiveReload => {
            let config = home.join(".ssh/config");
            let extra = home.join(".ssh/extra.conf");
            let append = |text: &str| {
                fs::OpenOptions::new()
                    .append(true)
                    .open(&config)
                    .unwrap()
                    .write_all(text.as_bytes())
                    .unwrap();
            };
            // Edits made outside Portico appear by themselves, without a key.
            terminal.read_screen("alpha");
            append("\nHost watched-direct\n    HostName direct.example\n");
            terminal.read_screen("watched-direct");
            terminal.read_screen("Config changed: reloaded");
            // A new Include is followed, and its file is watched from then on, also when an
            // editor saves it by renaming a new file over it.
            fs::write(&extra, "Host watched-included\n").unwrap();
            append("Include extra.conf\n");
            terminal.read_screen("watched-included");
            let partial = home.join(".ssh/extra.conf.tmp");
            fs::write(&partial, "Host watched-renamed\n").unwrap();
            fs::rename(&partial, &extra).unwrap();
            terminal.read_screen("watched-renamed");
            terminal.wait_gone("watched-included");
            // Under an open form the list stays as it is and says why; leaving the form
            // applies it.
            terminal.write(b"a");
            terminal.read_screen("Enter save");
            append("Host watched-later\n");
            terminal.read_screen("the list reloads when this closes");
            assert!(!terminal.screen().contains("watched-later"));
            terminal.write(ESC);
            terminal.read_screen("watched-later");
            // Portico's own save is read back at once, and the watch takes no notice of it.
            terminal.write(b"a");
            terminal.read_screen("Enter save");
            terminal.enter_text("own");
            terminal.write(b"\t");
            terminal.enter_text("own.example");
            terminal.write(b"\r");
            terminal.read_screen("Saved own");
            thread::sleep(Duration::from_secs(2));
            let screen = terminal.screen();
            assert!(
                screen.contains("Saved own") && !screen.contains("Config changed"),
                "{screen}"
            );
        }
        ConnectionTest => {
            // The fake ssh answers after a second, so the test is visibly in flight first.
            terminal.write(b"t");
            terminal.read_screen("Testing dev\u{2026} (Esc to cancel)");
            terminal.read_screen("\u{2713} dev");
            assert!(!terminal.screen().contains("Testing dev"));
        }
        Tabs => {
            let docker_called = home.join("docker-called");
            terminal.read_screen("1 Hosts");
            // The container tabs list this machine only once one is shown.
            thread::sleep(Duration::from_millis(300));
            assert!(!docker_called.exists(), "listed before the tab was shown");
            terminal.write(b"2");
            terminal.read_screen("Tunnels /");
            terminal.write(b"3");
            terminal.read_screen("Target: this machine");
            wait_for_file(&docker_called, "the Docker tab listed nothing");
            // Tab goes on through the Podman and Proxmox tabs, then back to Hosts.
            terminal.write(b"\t");
            terminal.read_screen("Podman /");
            terminal.write(b"\t\t");
            terminal.read_screen("Hosts /");
            // A digit in the filter is text, not a tab switch.
            terminal.write(b"/2");
            terminal.read_screen("Hosts /2");
            terminal.write(b"\r");
        }
        RememberedUser => {
            terminal.read_screen("1 Hosts");
            terminal.write(b"3");
            terminal.read_screen("api");
            terminal.write(b"u");
            terminal.enter_text("deploy");
            terminal.write(b"\r");
            terminal.read_screen("user: deploy");
            // Opening the shell (the fake docker exits at once) remembers the user.
            terminal.write(b"\r");
            let settings_path = home.join(".config/portico/settings.toml");
            let deadline = Instant::now() + Duration::from_secs(5);
            while !fs::read_to_string(&settings_path)
                .is_ok_and(|settings| settings.contains("\"shop/api\" = \"deploy\""))
            {
                assert!(Instant::now() < deadline, "the user was not remembered");
                thread::sleep(Duration::from_millis(10));
            }
            let settings = fs::read_to_string(&settings_path).unwrap();
            assert!(settings.contains("[users.local.docker]"), "{settings}");
            terminal.read_screen("user: deploy");
        }
        RememberedUserRestored => {
            terminal.read_screen("1 Hosts");
            terminal.write(b"3");
            terminal.read_screen("api");
            terminal.read_screen("user: deploy");
        }
        Settings => {
            terminal.read_screen("4 Podman");
            terminal.write(b",");
            // The overlay's settings path; the desktop integration is offered only on Linux.
            terminal.read_screen("settings.toml");
            // Down twice reaches Podman; Space turns it off, written at once.
            terminal.write(b"\x1b[B\x1b[B ");
            terminal.read_screen("[ ] Podman");
            terminal.write(ESC);
            terminal.wait_gone("settings.toml");
            let screen = terminal.screen();
            let tab_bar = screen.lines().next().unwrap_or_default();
            assert!(!tab_bar.contains("Podman"), "{tab_bar}");
            assert!(tab_bar.contains("4 Proxmox"), "{tab_bar}");
            let settings = fs::read_to_string(home.join(".config/portico/settings.toml")).unwrap();
            assert!(settings.contains("podman = false"), "{settings}");
        }
        DesktopAction => {
            terminal.write(b",");
            terminal.read_screen("Install or update");
            // Past the four toggles to "Install or update", which runs `portico desktop
            // install` with the TUI suspended.
            terminal.write(&b"\x1b[B".repeat(4));
            terminal.write(b"\r");
            let start = terminal.transcript.len();
            terminal.read_until(b"\x1b[?1049l", start, Duration::from_secs(5));
            // Decline whatever it offers, which depends on the tools installed, until it is
            // done.
            let mut declined = 0;
            let prompt: &[u8] = b"Press Enter to return to Portico.";
            let deadline = Instant::now() + Duration::from_secs(10);
            while !terminal.transcript_has(prompt, start) {
                terminal.pump(deadline);
                let asked = count(&terminal.transcript[start..], b"[y/N]");
                while declined < asked {
                    terminal.write(b"n\r");
                    declined += 1;
                }
            }
            let resumed = terminal.transcript.len();
            terminal.write(b"\r");
            // The TUI comes back with the overlay, and still takes keys.
            terminal.read_until(b"\x1b[?1049h", resumed, Duration::from_secs(5));
            terminal.read_screen("Install or update");
            terminal.write(ESC);
            terminal.wait_gone("Install or update");
        }
        LauncherFailure => {
            terminal.write(b"n");
            terminal.read_screen("launcher");
        }
        ResponsiveNavigation => {
            // Resolution is stuck in a slow ssh -G, yet keys are still handled and quitting is
            // immediate.
            terminal.write(b"j");
            terminal.write(b"k");
        }
        CtrlEnter => {
            // Terminals with the kitty keyboard protocol report Ctrl+Enter as CSI 13;5u.
            terminal.write(b"a");
            terminal.read_screen("Enter save");
            terminal.write(b"\x1b[13;5u");
            terminal.write(b"\x1b[27u");
            terminal.write(b"\x1b[13;5u");
            terminal.read_screen("requested");
            let launched = fs::read_to_string(home.join("launcher-args")).unwrap();
            let launched: Vec<&str> = launched.split_whitespace().collect();
            assert_eq!(
                launched[launched.len().saturating_sub(4)..],
                ["connect", "--hold", "--", "alpha"],
                "{launched:?}"
            );
            assert!(!home.join("child-terminal-state").exists());
        }
        LegacyKeyboard => {
            assert!(!terminal.transcript_has(b"\x1b[>1u", 0));
            assert!(!terminal.screen().contains("Ctrl+Enter"));
        }
        #[cfg(unix)]
        ChildFailure => {
            let start = terminal.transcript.len();
            terminal.write(b"\r");
            terminal.read_screen("exited");
            let child_output = &terminal.transcript[start..];
            let left = find(child_output, b"\x1b[?1049l", 0).unwrap();
            let entered = find(child_output, b"\x1b[?1049h", left).unwrap();
            // The child gets a terminal with the original key reporting, and the TUI re-arms
            // it.
            assert!(find(child_output, b"\x1b[<1u", 0).unwrap() < left);
            assert!(find(child_output, b"\x1b[>1u", entered).is_some());
            assert!(find(&child_output[..entered], b"\x1b[?25h", 0).is_some());
            let state = fs::read_to_string(home.join("child-terminal-state")).unwrap();
            assert_eq!(state.trim(), terminal.before_stty);
            assert!(terminal.is_raw());
        }
        _ => {}
    }
}

/// Removes what `scenario` left behind, so the next one starts from the same home.
fn clean_up(home: &Path, scenario: Scenario, managed_before: Option<&str>) {
    let remove = |name: &str| {
        let _ = fs::remove_file(home.join(name));
    };
    match scenario {
        BlockedResolution | ResponsiveNavigation => {
            remove("block-resolver");
            remove("resolver-started");
        }
        BlockedConnection => {
            #[cfg(unix)]
            if home.join("connection-started").exists() {
                for pid in connection_pids(home) {
                    // SAFETY: plain kill(2) of processes this scenario's fake started.
                    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
                }
            }
            remove("block-connection");
            remove("connection-started");
        }
        InterruptConnection => {
            remove("interrupt-connection");
            remove("interrupt-ready");
        }
        Refresh => {
            remove("resolved-hostname");
            super::allow_read(&home.join(".ssh/portico/config"));
        }
        LiveReload => {
            let config = home.join(".ssh/config");
            let text = fs::read_to_string(&config).unwrap();
            let original = text.split("\nHost watched-direct\n").next().unwrap();
            fs::write(&config, original).unwrap();
            remove(".ssh/extra.conf");
            remove(".ssh/extra.conf.tmp");
            if let Some(managed) = managed_before {
                fs::write(home.join(".ssh/portico/config"), managed).unwrap();
            }
        }
        LauncherFailure => {
            remove("fail-launcher");
            remove("launcher-args");
        }
        CtrlEnter => remove("launcher-args"),
        _ => {}
    }
}

/// Waits up to five seconds for the file at `path`.
fn wait_for_file(path: &Path, failure: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(Instant::now() < deadline, "{failure}");
        thread::sleep(Duration::from_millis(10));
    }
}

/// The shell, parent, worker thread and child of the blocked connection, as its fake records
/// them once it has started.
#[cfg(unix)]
fn connection_pids(home: &Path) -> [u32; 4] {
    let marker = home.join("connection-started");
    wait_for_file(&marker, "connection did not start");
    let text = fs::read_to_string(marker).unwrap();
    let pids: Vec<u32> = text
        .split_whitespace()
        .map(|pid| pid.parse().unwrap())
        .collect();
    pids.try_into().unwrap()
}

/// Whether `pid` can still run. A zombie is dead: containers whose PID 1 never reaps orphans
/// (as in CI) leave killed processes behind as zombies that signals still reach.
#[cfg(unix)]
fn is_running(pid: u32) -> bool {
    if !Path::new("/proc").is_dir() {
        // macOS: ps prints nothing for a process that is gone.
        let output = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let state = String::from_utf8_lossy(&output.stdout);
        let state = state.trim();
        return !state.is_empty() && !state.starts_with('Z');
    }
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .is_some_and(|state| state != "Z"),
        Err(_) => false,
    }
}

/// Prints what a Portico that does not exit is doing, for the CI log.
fn report_hang(pid: u32) {
    #[cfg(unix)]
    {
        let mut commands = vec![vec![
            "ps".to_owned(),
            "-axo".into(),
            "pid,ppid,pgid,stat,command".into(),
        ]];
        if cfg!(target_os = "macos") {
            commands.push(vec!["sample".into(), pid.to_string(), "1".into()]);
        } else {
            commands.push(vec!["cat".into(), format!("/proc/{pid}/stack")]);
        }
        for command in commands {
            if let Ok(output) = std::process::Command::new(&command[0])
                .args(&command[1..])
                .output()
            {
                eprintln!(
                    "$ {}\n{}{}",
                    command.join(" "),
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
    }
    #[cfg(windows)]
    let _ = pid;
}

fn find(haystack: &[u8], needle: &[u8], start: usize) -> Option<usize> {
    haystack
        .get(start..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|at| at + start)
}

fn count(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}

/// Portico in a pseudo-terminal, with what it printed so far.
struct Terminal {
    // Dropping the master closes the terminal, so it lives as long as the child.
    _master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    output: Receiver<Vec<u8>>,
    child: Box<dyn Child + Send + Sync>,
    parser: vt100::Parser,
    transcript: Vec<u8>,
    scenario: Scenario,
    /// Whether the terminal answers crossterm's query as one with the kitty keyboard protocol.
    kitty: bool,
    answered: bool,
    /// The terminal's other end, kept open as a terminal emulator would: its settings are read
    /// through it, and its output does not end when Portico exits.
    #[cfg(unix)]
    slave: fs::File,
    #[cfg(unix)]
    before: libc::termios,
    #[cfg(unix)]
    before_stty: String,
}

impl Terminal {
    fn spawn(env: &TestEnv, home: &Path, scenario: Scenario) -> Self {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: ROWS,
                cols: COLUMNS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        #[cfg(unix)]
        let slave = {
            use std::os::unix::fs::OpenOptionsExt;
            let name = pair.master.tty_name().expect("the terminal has a name");
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NOCTTY)
                .open(name)
                .unwrap()
        };
        #[cfg(unix)]
        let before = termios(&slave);
        #[cfg(unix)]
        let before_stty = {
            let output = std::process::Command::new("stty")
                .arg("-g")
                .stdin(slave.try_clone().unwrap())
                .output()
                .unwrap();
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };

        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_portico"));
        command.env("PATH", env.path());
        for (key, value) in env.vars_in(home) {
            match value {
                Some(value) => command.env(key, value),
                None => command.env_remove(key),
            }
        }
        if scenario == DesktopAction {
            // `portico desktop install` must look only at the temporary home, and never offer
            // to bind a key in the developer's own compositor.
            for name in [
                "XDG_DATA_HOME",
                "XDG_DATA_DIRS",
                "HYPRLAND_INSTANCE_SIGNATURE",
                "SWAYSOCK",
            ] {
                command.env_remove(name);
            }
        }
        command.cwd(env.home());
        let child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().unwrap();
        let (sender, output) = mpsc::channel();
        thread::spawn(move || {
            let mut buffer = [0; 65536];
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
            parser: vt100::Parser::new(
                NonZeroU16::new(ROWS).expect("the screen has rows"),
                NonZeroU16::new(COLUMNS).expect("the screen has columns"),
                0,
            ),
            transcript: Vec::new(),
            scenario,
            kitty: cfg!(unix) && scenario != LegacyKeyboard,
            answered: false,
            #[cfg(unix)]
            slave,
            #[cfg(unix)]
            before,
            #[cfg(unix)]
            before_stty,
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
    }

    /// Enters `text` into a field: as a bracketed paste where terminals have one, else typed.
    fn enter_text(&mut self, text: &str) {
        if cfg!(unix) {
            self.write(format!("\x1b[200~{text}\x1b[201~").as_bytes());
        } else {
            self.write(text.as_bytes());
        }
    }

    fn screen(&self) -> String {
        self.parser.screen().contents()
    }

    fn find(&self, needle: &[u8], start: usize) -> Option<usize> {
        find(&self.transcript, needle, start)
    }

    fn rfind(&self, needle: &[u8]) -> Option<usize> {
        self.transcript
            .windows(needle.len())
            .rposition(|window| window == needle)
    }

    fn transcript_has(&self, needle: &[u8], start: usize) -> bool {
        self.find(needle, start).is_some()
    }

    /// Like a terminal, answers crossterm's keyboard-protocol query; legacy terminals only
    /// answer the device-attributes request that follows it.
    fn answer_keyboard_query(&mut self) {
        if !self.answered && self.transcript_has(b"\x1b[?u\x1b[c", 0) {
            let answer: &[u8] = if self.kitty {
                b"\x1b[?0u\x1b[?62;c"
            } else {
                b"\x1b[?62;c"
            };
            self.write(answer);
            self.answered = true;
        }
    }

    /// Reads what Portico prints until `deadline`, which must not have passed.
    fn pump(&mut self, deadline: Instant) {
        self.answer_keyboard_query();
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            self.fail("timed out");
        }
        match self.output.recv_timeout(remaining) {
            Ok(data) => self.feed(&data),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => self.fail("unexpected terminal EOF"),
        }
    }

    fn feed(&mut self, data: &[u8]) {
        self.transcript.extend_from_slice(data);
        self.parser.process(data);
    }

    /// Waits for raw terminal output, such as a mode switch.
    fn read_until(&mut self, marker: &[u8], start: usize, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while !self.transcript_has(marker, start) {
            self.pump(deadline);
        }
    }

    /// Waits until `text` is visible on the screen.
    fn read_screen(&mut self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.screen().contains(text) {
            self.pump(deadline);
        }
    }

    /// Waits until `text` has left the screen.
    fn wait_gone(&mut self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.screen().contains(text) {
            self.pump(deadline);
        }
    }

    fn is_running(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }

    /// Waits for Portico to exit while reading what it still writes: macOS holds an exiting
    /// process that controls the terminal until its last output has been read.
    fn wait_draining(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                while let Ok(data) = self.output.try_recv() {
                    self.feed(&data);
                }
                return status;
            }
            if Instant::now() >= deadline {
                report_hang(self.child.process_id().unwrap_or_default());
                self.fail("portico did not exit");
            }
            if let Ok(data) = self.output.recv_timeout(Duration::from_millis(50)) {
                self.feed(&data);
            }
        }
    }

    fn kill(&mut self) {
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    #[cfg(unix)]
    fn signal(&mut self, signal: libc::c_int) {
        let pid = self.child.process_id().unwrap() as libc::pid_t;
        // SAFETY: plain kill(2) of the child this terminal started.
        assert_eq!(unsafe { libc::kill(pid, signal) }, 0);
    }

    /// Whether the terminal reads keys one at a time, without echoing them.
    #[cfg(unix)]
    fn is_raw(&self) -> bool {
        termios(&self.slave).c_lflag & (libc::ICANON | libc::ECHO) == 0
    }

    #[cfg(unix)]
    fn termios_restored(&self) -> bool {
        let after = termios(&self.slave);
        let before = &self.before;
        (
            after.c_iflag,
            after.c_oflag,
            after.c_cflag,
            after.c_lflag,
            after.c_cc,
        ) == (
            before.c_iflag,
            before.c_oflag,
            before.c_cflag,
            before.c_lflag,
            before.c_cc,
        )
    }

    #[track_caller]
    fn fail(&self, what: &str) -> ! {
        panic!(
            "{:?}: {what}\n--- screen ---\n{}\n--- output ---\n{}",
            self.scenario,
            self.screen(),
            self.transcript.escape_ascii()
        );
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(unix)]
fn termios(terminal: &fs::File) -> libc::termios {
    use std::os::fd::AsRawFd;
    // SAFETY: `termios` is plain data that `tcgetattr` fills in.
    let mut termios: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: the descriptor is open for the call and `termios` is a valid location.
    assert_eq!(
        unsafe { libc::tcgetattr(terminal.as_raw_fd(), &mut termios) },
        0,
        "tcgetattr: {}",
        std::io::Error::last_os_error()
    );
    termios
}
