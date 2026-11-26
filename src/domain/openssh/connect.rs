use std::{
    collections::VecDeque,
    ffi::OsString,
    io::{self, Read, Write},
    path::Path,
    process::{Command, ExitStatus, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use super::pipes::{push_capped, read_lines};
use super::{ConnectionOutcome, OpenSsh, OpenSshError};
use crate::domain::probe::{
    Authenticated, ProbeResult, Route, TEST_DEADLINE, TEST_STDERR_LIMIT, authenticated, diagnose,
    test_args,
};
use crate::platform::launcher::{Launch, launch};
use crate::platform::process::{
    has_exited, own_group, terminate_process_group, terminate_process_tree,
};

/// How a watched ssh run ended.
pub(super) enum Watched {
    Authenticated(Authenticated),
    Ended {
        stderr: String,
        deadline_reached: bool,
    },
}

/// The most of a connection's stderr kept for diagnosis, counted from the end.
pub const CONNECT_TAIL_LIMIT: usize = 64 * 1024;

/// How long the stderr copier may keep running after ssh exits: anything ssh wrote is already in
/// the pipe, so only a descendant still holding the pipe open makes it wait this long.
const MIRROR_DRAIN: Duration = Duration::from_secs(1);

/// A connection whose stderr was mirrored: ssh's exit status and the newest bytes it wrote there.
#[derive(Debug)]
pub struct Mirrored {
    pub status: ExitStatus,
    pub tail: Vec<u8>,
}

impl OpenSsh {
    /// Tries to log in to `alias` without a terminal, stopping as soon as
    /// OpenSSH reports success; a failure comes with ssh's own lines. `route` says whether the alias goes through a
    /// jump host or proxy, which decides the success line to wait for and lets
    /// a failure at the jump host be told apart from one at the target.
    pub(crate) fn probe(
        &self,
        entry: Option<&Path>,
        alias: &str,
        route: Route<'_>,
        cancelled: &AtomicBool,
    ) -> Result<(ProbeResult, Vec<String>), OpenSshError> {
        self.probe_with_deadline(entry, alias, route, cancelled, TEST_DEADLINE)
    }

    fn probe_with_deadline(
        &self,
        entry: Option<&Path>,
        alias: &str,
        route: Route<'_>,
        cancelled: &AtomicBool,
        deadline: Duration,
    ) -> Result<(ProbeResult, Vec<String>), OpenSshError> {
        Ok(
            match self.watch_ssh(test_args(entry, alias), route, cancelled, deadline)? {
                Watched::Authenticated(success) => {
                    (ProbeResult::Authenticated(success), Vec::new())
                }
                Watched::Ended {
                    stderr,
                    deadline_reached,
                } => {
                    let diagnosis = diagnose(&stderr, deadline_reached, route);
                    (ProbeResult::Failed(diagnosis.failure), diagnosis.raw)
                }
            },
        )
    }

    /// Runs ssh with `args` in its own process group, reading stderr line by
    /// line until a success line, its exit, `deadline` or cancellation. The
    /// whole group is killed whichever way the run ends.
    pub(super) fn watch_ssh(
        &self,
        args: Vec<OsString>,
        route: Route<'_>,
        cancelled: &AtomicBool,
        deadline: Duration,
    ) -> Result<Watched, OpenSshError> {
        let mut command = Command::new(&self.ssh);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = own_group(&mut command)
            .spawn()
            .map_err(|source| OpenSshError::Io {
                program: self.ssh.clone(),
                source,
            })?;
        let lines = read_lines(child.stderr.take().expect("stderr is piped"));
        let deadline = Instant::now() + deadline;
        let mut stderr = String::new();
        let deadline_reached = loop {
            if cancelled.load(Ordering::Relaxed) {
                terminate_process_group(&mut child);
                return Err(OpenSshError::Cancelled);
            }
            match lines.recv_timeout(Duration::from_millis(20)) {
                Ok(line) => {
                    if let Some(success) = authenticated(&line, route) {
                        terminate_process_group(&mut child);
                        return Ok(Watched::Authenticated(success));
                    }
                    push_capped(&mut stderr, &line, TEST_STDERR_LIMIT);
                    // A steady trickle of output must not hold off the deadline.
                    if Instant::now() < deadline {
                        continue;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    thread::sleep(Duration::from_millis(20));
                }
            }
            match has_exited(&child) {
                Ok(true) => break false,
                Ok(false) if Instant::now() >= deadline => break true,
                Ok(false) => {}
                Err(source) => {
                    terminate_process_group(&mut child);
                    return Err(OpenSshError::Io {
                        program: self.ssh.clone(),
                        source,
                    });
                }
            }
        };
        // Leftover processes in the group (a ProxyCommand, say) would hold
        // stderr open; once they are gone the rest of it can be read. ssh is
        // not reaped yet, so its PID still names this group.
        terminate_process_group(&mut child);
        let drain_deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if cancelled.load(Ordering::Relaxed) {
                return Err(OpenSshError::Cancelled);
            }
            match lines.recv_timeout(Duration::from_millis(20)) {
                Ok(line) => {
                    // ssh may log success just before the server drops it.
                    if let Some(success) = authenticated(&line, route) {
                        return Ok(Watched::Authenticated(success));
                    }
                    push_capped(&mut stderr, &line, TEST_STDERR_LIMIT);
                }
                Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() < drain_deadline => {}
                Err(_) => break,
            }
        }
        Ok(Watched::Ended {
            stderr,
            deadline_reached,
        })
    }

    /// Asks `xdg-terminal-exec` to open the connection in a new terminal: `portico connect
    /// --hold -- <alias>` when Portico's path is known, so a failure is explained and held on
    /// screen, otherwise `ssh -- <alias>`.
    ///
    /// The launcher's own exit status is the only signal available: it exits
    /// non-zero when it cannot find or start a terminal, but a terminal that
    /// stays open (or hands the window to a running instance later) is
    /// indistinguishable from one that is still starting. A launcher that
    /// fails within the launch grace period is reported as `LaunchFailed`; anything
    /// else is `LaunchRequested`, which promises only that the request was
    /// accepted, never that the SSH session itself succeeded.
    pub fn launch_in_new_terminal(&self, alias: &str) -> Result<ConnectionOutcome, OpenSshError> {
        self.launch_new_terminal_with(
            &["connect".into(), "--hold".into(), "--".into(), alias.into()],
            &[self.ssh.clone().into(), "--".into(), alias.into()],
        )
    }

    /// Asks the terminal launcher to run `<portico> <portico_args…>` when Portico's path is
    /// known, otherwise `fallback`, with the same launch semantics as `launch_in_new_terminal`.
    pub fn launch_new_terminal_with(
        &self,
        portico_args: &[OsString],
        fallback: &[OsString],
    ) -> Result<ConnectionOutcome, OpenSshError> {
        let command: Vec<OsString> = match &self.portico {
            Some(portico) => std::iter::once(portico.clone().into())
                .chain(portico_args.iter().cloned())
                .collect(),
            None => fallback.to_vec(),
        };
        match launch(&self.terminal, &command) {
            Ok(Launch::Requested) => Ok(ConnectionOutcome::LaunchRequested),
            Ok(Launch::Failed(status)) => Ok(ConnectionOutcome::LaunchFailed(status)),
            Err(source) => Err(OpenSshError::Io {
                program: self.terminal.clone(),
                source,
            }),
        }
    }

    /// Runs `ssh -- <alias>` with stdin and stdout inherited and stderr piped through a copier
    /// thread that writes every chunk to `sink` as soon as it is read and keeps the newest
    /// `CONNECT_TAIL_LIMIT` bytes.
    ///
    /// Setting `cancelled` stops ssh and every process it started.
    pub(crate) fn connect_mirrored(
        &self,
        alias: &str,
        sink: Box<dyn Write + Send>,
        cancelled: Option<&AtomicBool>,
    ) -> Result<Mirrored, OpenSshError> {
        self.connect_mirrored_command(alias, None, sink, cancelled)
    }

    /// Like `connect_mirrored`, but with `Some(command)` runs `ssh -t -o RemoteCommand=none --
    /// <alias> <command>`, forcing a terminal for the remote command.
    pub(crate) fn connect_mirrored_command(
        &self,
        alias: &str,
        command: Option<&str>,
        sink: Box<dyn Write + Send>,
        cancelled: Option<&AtomicBool>,
    ) -> Result<Mirrored, OpenSshError> {
        let io_error = |source| OpenSshError::Io {
            program: self.ssh.clone(),
            source,
        };
        let mut ssh = Command::new(&self.ssh);
        if command.is_some() {
            ssh.args(["-t", "-o", "RemoteCommand=none"]);
        }
        ssh.arg("--").arg(alias);
        ssh.args(command);
        let mut child = ssh.stderr(Stdio::piped()).spawn().map_err(io_error)?;
        let copier = Copier::start(child.stderr.take().expect("stderr is piped"), sink);
        let status = loop {
            if cancelled.is_some_and(|cancelled| cancelled.load(Ordering::Relaxed)) {
                let stopped = terminate_process_tree(&mut child);
                copier.finish();
                stopped.map_err(io_error)?;
                return Err(OpenSshError::Cancelled);
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => thread::sleep(Duration::from_millis(20)),
                Err(source) => {
                    let stopped = terminate_process_tree(&mut child);
                    copier.finish();
                    stopped.map_err(io_error)?;
                    return Err(io_error(source));
                }
            }
        };
        Ok(Mirrored {
            status,
            tail: copier.finish(),
        })
    }
}

/// Copies a child's stderr to a sink chunk by chunk, keeping the newest bytes.
struct Copier {
    thread: thread::JoinHandle<()>,
    done: mpsc::Receiver<()>,
    detached: Arc<AtomicBool>,
    tail: Arc<Mutex<VecDeque<u8>>>,
}

impl Copier {
    fn start(mut source: impl Read + Send + 'static, mut sink: Box<dyn Write + Send>) -> Self {
        let (finished, done) = mpsc::channel();
        let detached = Arc::new(AtomicBool::new(false));
        let tail = Arc::new(Mutex::new(VecDeque::new()));
        let thread = thread::spawn({
            let detached = Arc::clone(&detached);
            let tail = Arc::clone(&tail);
            move || {
                let mut buffer = [0; 8 * 1024];
                loop {
                    let read = match source.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(read) => read,
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    };
                    // Once Portico has moved on, a leftover descendant's output is not ssh's.
                    if detached.load(Ordering::Relaxed) {
                        break;
                    }
                    let chunk = &buffer[..read];
                    // A sink that fails must not stop the reading, or ssh would block on a full
                    // pipe; the tail still gets every byte.
                    let _ = sink.write_all(chunk).and_then(|()| sink.flush());
                    let mut tail = tail.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    tail.extend(chunk);
                    let excess = tail.len().saturating_sub(CONNECT_TAIL_LIMIT);
                    tail.drain(..excess);
                }
                let _ = finished.send(());
            }
        });
        Self {
            thread,
            done,
            detached,
            tail,
        }
    }

    /// Waits for the copy to reach the end of the pipe, which comes when ssh and everything it
    /// started have exited; a descendant that still holds the pipe after `MIRROR_DRAIN` is left
    /// behind, and the copier stops at its next write. Returns the tail.
    fn finish(self) -> Vec<u8> {
        if self.done.recv_timeout(MIRROR_DRAIN).is_ok() {
            let _ = self.thread.join();
        } else {
            self.detached.store(true, Ordering::Relaxed);
        }
        let tail = self
            .tail
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        tail.iter().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        thread,
        time::{Duration, Instant},
    };

    use super::*;
    use crate::domain::openssh::tests::{
        FakeProgram, fake_failure, fake_ssh, fixture_path, wait_for_contents,
    };
    use crate::domain::probe::{Failure, KeySource, OfferedKey};
    use crate::platform::process::{fail_pin_after, pin_failure_injected};
    use crate::test_executable::write_executable;

    #[test]
    fn passes_alias_as_one_argument_without_a_shell() {
        let fake = fake_ssh();
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
        let mirrored = openssh
            .connect_mirrored("-external", Box::new(io::sink()), None)
            .unwrap();

        assert!(mirrored.status.success());
        assert_eq!(fs::read_to_string(fake.log()).unwrap(), "--\n-external\n");
    }

    #[test]
    fn connect_mirrored_command_requests_a_tty() {
        let fake = fake_ssh();
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
        let mirrored = openssh
            .connect_mirrored_command(
                "dev",
                Some("docker exec -it web sh"),
                Box::new(io::sink()),
                None,
            )
            .unwrap();

        assert!(mirrored.status.success());
        assert_eq!(
            fs::read_to_string(fake.log()).unwrap(),
            "-t\n-o\nRemoteCommand=none\n--\ndev\ndocker exec -it web sh\n"
        );
    }

    #[test]
    fn new_terminal_with_custom_portico_args() {
        let ssh = fake_ssh();
        let terminal = fake_ssh();
        let openssh =
            OpenSsh::new(ssh.path(), terminal.path()).with_portico(Some("/opt/portico".into()));
        let args = ["shell".into(), "--hold".into(), "x".into()];
        let fallback = ["fallback".into(), "y".into()];

        assert_eq!(
            openssh.launch_new_terminal_with(&args, &fallback).unwrap(),
            ConnectionOutcome::LaunchRequested
        );
        wait_for_contents(terminal.log(), "/opt/portico\nshell\n--hold\nx\n");

        let terminal = fake_ssh();
        let openssh = OpenSsh::new(ssh.path(), terminal.path()).with_portico(None);
        openssh.launch_new_terminal_with(&args, &fallback).unwrap();
        wait_for_contents(terminal.log(), "fallback\ny\n");
        assert_eq!(openssh.ssh_program(), ssh.path());
    }

    #[test]
    fn passes_terminal_arguments_without_a_shell() {
        let ssh = fake_ssh();
        let terminal = fake_ssh();
        let openssh = OpenSsh::new(ssh.path(), terminal.path());

        assert_eq!(
            openssh.launch_in_new_terminal("-external").unwrap(),
            ConnectionOutcome::LaunchRequested
        );
        wait_for_contents(
            terminal.log(),
            &format!("{}\n--\n-external\n", ssh.path().display()),
        );
    }

    #[test]
    fn a_new_terminal_runs_portico_when_its_path_is_known() {
        let ssh = fake_ssh();
        let terminal = fake_ssh();
        let openssh =
            OpenSsh::new(ssh.path(), terminal.path()).with_portico(Some("/opt/portico".into()));

        assert_eq!(
            openssh.launch_in_new_terminal("dev").unwrap(),
            ConnectionOutcome::LaunchRequested
        );
        wait_for_contents(terminal.log(), "/opt/portico\nconnect\n--hold\n--\ndev\n");

        let terminal = fake_ssh();
        let openssh = OpenSsh::new(ssh.path(), terminal.path()).with_portico(None);
        openssh.launch_in_new_terminal("dev").unwrap();
        wait_for_contents(
            terminal.log(),
            &format!("{}\n--\ndev\n", ssh.path().display()),
        );
    }

    #[test]
    fn early_launcher_failure_is_reported() {
        let ssh = fake_ssh();
        let terminal = fake_ssh();
        write_executable(&terminal.path, "#!/bin/sh\nexit 7\n");
        let openssh = OpenSsh::new(ssh.path(), terminal.path());

        assert!(matches!(
            openssh.launch_in_new_terminal("dev").unwrap(),
            ConnectionOutcome::LaunchFailed(status) if status.code() == Some(7)
        ));
    }

    #[test]
    fn missing_launcher_is_an_error() {
        let openssh = OpenSsh::new("ssh".into(), "/nonexistent/xdg-terminal-exec".into());

        assert!(matches!(
            openssh.launch_in_new_terminal("dev"),
            Err(OpenSshError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound
        ));
    }

    #[test]
    fn new_terminal_connection_returns_after_launch() {
        let ssh = fake_ssh();
        let terminal = fake_ssh();
        let release = terminal.log.with_file_name("release");
        let done = terminal.log.with_file_name("done");
        write_executable(
            &terminal.path,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\ni=0\nwhile [ ! -f '{}' ] && [ \"$i\" -lt 200 ]; do i=$((i + 1)); sleep 0.01; done\n: > '{}'\n",
                terminal.log.display(),
                release.display(),
                done.display(),
            ),
        );
        let openssh = OpenSsh::new(ssh.path(), terminal.path());
        let started = std::time::Instant::now();

        assert_eq!(
            openssh.launch_in_new_terminal("dev").unwrap(),
            ConnectionOutcome::LaunchRequested
        );

        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "new-terminal launch blocked for {:?}",
            started.elapsed()
        );
        wait_for_contents(
            terminal.log(),
            &format!("{}\n--\ndev\n", ssh.path().display()),
        );
        fs::write(release, "").unwrap();
        wait_for_contents(&done, "");
    }

    #[test]
    fn cancellation_without_a_pin_still_kills_and_reports_cancelled() {
        let fake = fake_ssh();
        let marker = fake.log.with_file_name("connection-started");
        write_executable(
            &fake.path,
            &format!(
                "#!/bin/sh\nprintf '%s' \"$$\" > '{}'\nsleep 2\n",
                marker.display()
            ),
        );
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
        let cancelled = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&cancelled);
        let cancel = thread::spawn({
            let marker = marker.clone();
            move || {
                let deadline = Instant::now() + Duration::from_secs(1);
                while !fs::read_to_string(&marker).is_ok_and(|pid| !pid.is_empty()) {
                    assert!(Instant::now() < deadline, "connection did not start");
                    thread::sleep(Duration::from_millis(5));
                }
                signal.store(true, Ordering::Relaxed);
            }
        });
        fail_pin_after(0);
        let started = Instant::now();

        let result = openssh.connect_mirrored("dev", Box::new(io::sink()), Some(&cancelled));

        cancel.join().unwrap();
        let pid = fs::read_to_string(marker).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "cancellation blocked for {:?}",
            started.elapsed()
        );
        assert!(matches!(result, Err(OpenSshError::Cancelled)));
        assert!(pin_failure_injected(), "ssh's pin opened");
        assert!(!Path::new(&format!("/proc/{pid}")).exists());
    }

    #[test]
    fn cancellation_without_a_descendant_pin_still_kills_the_whole_tree() {
        let fake = fake_ssh();
        let marker = fake.log.with_file_name("tree-started");
        write_executable(
            &fake.path,
            &format!(
                "#!/bin/sh\nsleep 30 &\nfirst=$!\nsleep 30 &\nprintf '%s %s' \"$first\" \"$!\" > '{}.tmp'\nmv '{0}.tmp' '{0}'\nwait\n",
                marker.display()
            ),
        );
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
        let cancelled = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&cancelled);
        let cancel = thread::spawn({
            let marker = marker.clone();
            move || {
                let deadline = Instant::now() + Duration::from_secs(2);
                while !marker.exists() {
                    assert!(Instant::now() < deadline, "connection did not start");
                    thread::sleep(Duration::from_millis(5));
                }
                signal.store(true, Ordering::Relaxed);
            }
        });
        // The root's descriptor opens; the first descendant's does not.
        fail_pin_after(1);

        let result = openssh.connect_mirrored("dev", Box::new(io::sink()), Some(&cancelled));

        cancel.join().unwrap();
        assert!(matches!(result, Err(OpenSshError::Cancelled)));
        assert!(pin_failure_injected(), "every descendant's pin opened");
        for pid in fs::read_to_string(marker).unwrap().split_whitespace() {
            assert_dies(pid);
        }
    }

    /// Asserts that `pid` is gone or a zombie, allowing it time to die. Teardown sends SIGKILL to
    /// every process in the tree but cannot wait for descendants, and a process with SIGKILL
    /// pending is still runnable for a moment, longer on a loaded machine.
    fn assert_dies(pid: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let state = fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| {
                    stat.rsplit(')')
                        .next()
                        .map(|rest| rest.trim_start().to_owned())
                });
            if state.is_none_or(|state| state.starts_with('Z')) {
                return;
            }
            assert!(Instant::now() < deadline, "descendant {pid} survived");
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// A fake ssh that starts `sleep 30` in its group, records the sleep's pid
    /// at `<dir>/pid`, then prints `fixture` (if any) to stderr and waits.
    ///
    /// The sleep records its own pid just before it becomes `sleep`, so if the
    /// pid file is missing, no sleep was ever started that could outlive ssh.
    fn fake_hanging_ssh(fixture: Option<&str>) -> (FakeProgram, PathBuf) {
        let fake = fake_ssh();
        let pid = fake.log.with_file_name("pid");
        let print = fixture.map_or(String::new(), |name| {
            format!("cat '{}' >&2\n", fixture_path(name).display())
        });
        write_executable(
            &fake.path,
            &format!(
                "#!/bin/sh\nsh -c 'printf %s \"$$\" > \"$1.tmp\" && mv \"$1.tmp\" \"$1\" && exec sleep 30' sh '{0}' &\nwhile [ ! -s '{0}' ]; do sleep 0.01; done\n{print}wait\n",
                pid.display()
            ),
        );
        (fake, pid)
    }

    /// Checks that whatever a fake ssh recorded at `pid` is gone, once the run
    /// that a deadline ended has returned.
    ///
    /// A deadline counts from the spawn, so on a loaded machine it can end the
    /// fake before the fake records anything. The fakes record a pid before
    /// starting anything that could outlive them, so then there is nothing to
    /// check.
    fn assert_recorded_process_dies(pid: &Path) {
        if let Ok(pid) = fs::read_to_string(pid) {
            assert_dies(&pid);
        }
    }

    fn wait_for_file(path: &Path) -> String {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(contents) = fs::read_to_string(path)
                && !contents.is_empty()
            {
                return contents;
            }
            assert!(Instant::now() < deadline, "missing {}", path.display());
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn probe_stops_at_the_success_line_and_kills_the_group() {
        let (fake, pid) = fake_hanging_ssh(Some("authenticated"));
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
        let started = Instant::now();

        let result = openssh
            .probe(None, "dev", Route::Direct, &AtomicBool::new(false))
            .unwrap()
            .0;

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "probe blocked for {:?}",
            started.elapsed()
        );
        assert_eq!(
            result,
            ProbeResult::Authenticated(Authenticated {
                host: "127.0.0.1".into(),
                address: "127.0.0.1".into(),
                port: 22999,
                method: "publickey".into(),
                via: None,
            })
        );
        assert_dies(&fs::read_to_string(pid).unwrap());
    }

    #[test]
    fn a_proxied_probe_stops_at_the_targets_line_not_the_jump_hosts() {
        let (fake, pid) = fake_hanging_ssh(Some("authenticated-via-verbose-jump"));
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
        let route = Route::Proxied {
            jump: Some("jumpv"),
            hostname: "127.0.0.1",
            port: 2222,
            fd_passing: false,
        };

        let result = openssh
            .probe(None, "dev", route, &AtomicBool::new(false))
            .unwrap()
            .0;

        assert_eq!(
            result,
            ProbeResult::Authenticated(Authenticated {
                host: "127.0.0.1".into(),
                address: "127.0.0.1".into(),
                port: 2222,
                method: "publickey".into(),
                via: Some("jumpv".into()),
            })
        );
        assert_dies(&wait_for_file(&pid));
    }

    #[test]
    fn a_multi_hop_probe_waits_for_the_targets_line() {
        let (fake, pid) = fake_hanging_ssh(Some("authenticated-via-two-jumps"));
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
        let route = Route::Proxied {
            jump: Some("hopa,hopb"),
            hostname: "127.0.0.3",
            port: 22999,
            fd_passing: false,
        };

        let result = openssh
            .probe(None, "dev", route, &AtomicBool::new(false))
            .unwrap()
            .0;

        assert!(
            matches!(&result, ProbeResult::Authenticated(success) if success.host == "127.0.0.3"),
            "{result:?}"
        );
        assert_dies(&wait_for_file(&pid));
    }

    #[test]
    fn a_fd_passing_probe_succeeds_instead_of_timing_out() {
        let (fake, pid) = fake_hanging_ssh(Some("authenticated-via-fdpass"));
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
        let route = Route::Proxied {
            jump: None,
            hostname: "127.0.0.3",
            port: 22999,
            fd_passing: true,
        };
        let started = Instant::now();

        let result = openssh
            .probe(None, "dev", route, &AtomicBool::new(false))
            .unwrap()
            .0;

        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(
            matches!(&result, ProbeResult::Authenticated(success) if success.address == "127.0.0.3"),
            "{result:?}"
        );
        assert_dies(&wait_for_file(&pid));
    }

    #[test]
    fn a_hops_success_before_the_targets_failure_is_a_failure() {
        let refused = fs::read_to_string(fixture_path("auth-refused")).unwrap();
        let fake = fake_failure(&format!(
            "Authenticated to 127.0.0.2 (via proxy) using \"publickey\".\n{refused}"
        ));
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
        let route = Route::Proxied {
            jump: Some("hopa,hopb"),
            hostname: "127.0.0.3",
            port: 22999,
            fd_passing: false,
        };

        let result = openssh
            .probe(None, "dev", route, &AtomicBool::new(false))
            .unwrap()
            .0;

        assert!(
            matches!(result, ProbeResult::Failed(Failure::AuthRefused { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn a_jump_hosts_own_success_is_not_the_targets() {
        let fake =
            fake_failure("Authenticated to 127.0.0.1 ([127.0.0.1]:22999) using \"publickey\".\n");
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
        let route = Route::Proxied {
            jump: Some("jumpv"),
            hostname: "127.0.0.1",
            port: 22999,
            fd_passing: false,
        };

        let result = openssh
            .probe(None, "dev", route, &AtomicBool::new(false))
            .unwrap()
            .0;

        assert!(matches!(result, ProbeResult::Failed(_)), "{result:?}");
    }

    #[test]
    fn probe_passes_the_exact_options() {
        let fake = fake_ssh();
        write_executable(
            &fake.path,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nexit 255\n",
                fake.log.display()
            ),
        );
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());

        openssh
            .probe(None, "dev", Route::Direct, &AtomicBool::new(false))
            .unwrap();

        let expected: String = test_args(None, "dev")
            .iter()
            .map(|arg| format!("{}\n", arg.to_string_lossy()))
            .collect();
        assert_eq!(fs::read_to_string(fake.log()).unwrap(), expected);
    }

    #[test]
    fn probe_classifies_output_when_ssh_exits() {
        let fake = fake_ssh();
        write_executable(
            &fake.path,
            &format!(
                "#!/bin/sh\ncat '{}' >&2\nexit 255\n",
                fixture_path("auth-refused").display()
            ),
        );
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());

        assert_eq!(
            openssh
                .probe(None, "dev", Route::Direct, &AtomicBool::new(false))
                .unwrap()
                .0,
            ProbeResult::Failed(Failure::AuthRefused {
                methods: "publickey".into(),
                offered: vec![OfferedKey {
                    path: "/tmp/portico-fixture/wrong".into(),
                    fingerprint: "SHA256:DEPp8rJWkCfro2273hHcjabe3DX963MQEi2ggf+UeW4".into(),
                    source: KeySource::File,
                }],
                unusable: Vec::new(),
            })
        );
    }

    #[test]
    fn probe_reports_a_timeout_at_the_deadline() {
        let (fake, pid) = fake_hanging_ssh(None);
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());

        let probe = thread::spawn(move || {
            openssh.probe_with_deadline(
                None,
                "dev",
                Route::Direct,
                &AtomicBool::new(false),
                Duration::from_secs(1),
            )
        });
        let result = probe.join().unwrap().unwrap().0;

        assert_eq!(
            result,
            ProbeResult::Failed(Failure::TimedOut { deadline: true })
        );
        assert_recorded_process_dies(&pid);
    }

    #[test]
    fn the_deadline_holds_while_stderr_keeps_arriving() {
        let fake = fake_ssh();
        let pid = fake.log.with_file_name("pid");
        write_executable(
            &fake.path,
            &format!(
                "#!/bin/sh\nprintf '%s' \"$$\" > '{0}.tmp'\nmv '{0}.tmp' '{0}'\nwhile :; do echo still trying >&2; sleep 0.01; done\n",
                pid.display()
            ),
        );
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
        let (sender, results) = mpsc::channel();
        thread::spawn(move || {
            let _ = sender.send(openssh.probe_with_deadline(
                None,
                "dev",
                Route::Direct,
                &AtomicBool::new(false),
                Duration::from_millis(300),
            ));
        });

        let result = results
            .recv_timeout(Duration::from_secs(5))
            .expect("the probe ran past its deadline")
            .unwrap()
            .0;

        assert_eq!(
            result,
            ProbeResult::Failed(Failure::TimedOut { deadline: true })
        );
        assert_recorded_process_dies(&pid);
    }

    #[test]
    fn probe_cancellation_kills_ssh() {
        let (fake, pid) = fake_hanging_ssh(None);
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
        let cancelled = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&cancelled);
        let cancel = thread::spawn({
            let pid = pid.clone();
            move || {
                wait_for_file(&pid);
                thread::sleep(Duration::from_millis(100));
                signal.store(true, Ordering::Relaxed);
            }
        });
        let started = Instant::now();

        let result = openssh.probe(None, "dev", Route::Direct, &cancelled);

        cancel.join().unwrap();
        assert!(matches!(result, Err(OpenSshError::Cancelled)));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "cancellation blocked for {:?}",
            started.elapsed()
        );
        assert_dies(&fs::read_to_string(pid).unwrap());
    }

    /// A sink the tests can read while a connection still writes to it.
    #[derive(Clone, Default)]
    struct SharedSink(Arc<std::sync::Mutex<Vec<u8>>>);

    impl SharedSink {
        fn contents(&self) -> Vec<u8> {
            self.0.lock().unwrap().clone()
        }
    }

    impl Write for SharedSink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn mirrored_ssh(script: &str) -> FakeProgram {
        let fake = fake_ssh();
        write_executable(&fake.path, &format!("#!/bin/sh\n{script}\n"));
        fake
    }

    #[test]
    fn mirrored_bytes_arrive_unchanged_and_in_order() {
        let fake = fake_ssh();
        let release = fake.log.with_file_name("release");
        write_executable(
            &fake.path,
            &format!(
                "#!/bin/sh\nprintf 'Password:' >&2\ni=0\nwhile [ ! -f '{}' ] && [ \"$i\" -lt 1000 ]; do i=$((i + 1)); sleep 0.01; done\nprintf '\\377\\r\\nbye\\n' >&2\nexit 0\n",
                release.display()
            ),
        );
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
        let sink = SharedSink::default();
        let connection = thread::spawn({
            let sink = sink.clone();
            move || openssh.connect_mirrored("dev", Box::new(sink), None)
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        while sink.contents() != b"Password:" {
            assert!(Instant::now() < deadline, "the prompt was not mirrored");
            thread::sleep(Duration::from_millis(5));
        }
        assert!(!connection.is_finished(), "ssh exited before its release");
        fs::write(&release, "").unwrap();
        let mirrored = connection.join().unwrap().unwrap();

        assert!(mirrored.status.success());
        assert_eq!(sink.contents(), b"Password:\xff\r\nbye\n");
        assert_eq!(mirrored.tail, b"Password:\xff\r\nbye\n");
    }

    #[test]
    fn the_tail_keeps_the_newest_bytes() {
        let fake = mirrored_ssh("head -c 102400 /dev/zero | tr '\\000' a >&2\nprintf 'END\\n' >&2");
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
        let sink = SharedSink::default();

        let mirrored = openssh
            .connect_mirrored("dev", Box::new(sink.clone()), None)
            .unwrap();

        assert_eq!(sink.contents().len(), 102400 + 4);
        assert_eq!(mirrored.tail.len(), CONNECT_TAIL_LIMIT);
        assert!(mirrored.tail.ends_with(b"aaaEND\n"));
    }

    #[test]
    fn the_exit_status_passes_through() {
        let fake = mirrored_ssh("exit 3");
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());

        let mirrored = openssh
            .connect_mirrored("dev", Box::new(SharedSink::default()), None)
            .unwrap();

        assert_eq!(mirrored.status.code(), Some(3));
        assert!(mirrored.tail.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_descendant_holding_stderr_open_does_not_block_the_return() {
        let fake = fake_ssh();
        let pid = fake.log.with_file_name("pid");
        write_executable(
            &fake.path,
            &format!(
                "#!/bin/sh\nprintf 'gone\\n' >&2\nsleep 30 &\nprintf '%s' \"$!\" > '{}'\nexit 255\n",
                pid.display()
            ),
        );
        let openssh = OpenSsh::new(fake.path(), "xdg-terminal-exec".into());
        let sink = SharedSink::default();
        let started = Instant::now();

        let mirrored = openssh
            .connect_mirrored("dev", Box::new(sink.clone()), None)
            .unwrap();

        assert!(
            started.elapsed() < Duration::from_secs(10),
            "waited {:?} for a descendant",
            started.elapsed()
        );
        assert_eq!(mirrored.status.code(), Some(255));
        assert_eq!(mirrored.tail, b"gone\n");
        assert_eq!(sink.contents(), b"gone\n");
        let pid: libc::pid_t = fs::read_to_string(pid).unwrap().parse().unwrap();
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
}
