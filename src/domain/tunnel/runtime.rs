//! The runtime state of started tunnels: where records and logs live, how a record's process is
//! recognised in `/proc` (so a recycled PID is never mistaken for a tunnel), how a lost record is
//! rebuilt, whether a local port is free, and how a tunnel's process is stopped.

use std::{
    ffi::OsStr,
    fs, io,
    net::{IpAddr, SocketAddr, TcpListener, ToSocketAddrs},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use super::Forward;
use crate::{
    platform::fs::{create_private_dir, sync_dir, write_unique},
    platform::net::bind_probe,
    platform::paths::runtime_dir,
    platform::process::{Logged, Proc, Uid},
};

/// The directory holding the records and logs of started tunnels.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeDir {
    dir: PathBuf,
}

impl RuntimeDir {
    /// `$XDG_RUNTIME_DIR/portico/tunnels` when `xdg_runtime_dir` is an absolute path to a
    /// directory the current user owns, otherwise `~/.ssh/portico/run/tunnels`.
    pub fn locate(home: &Path, xdg_runtime_dir: Option<&OsStr>) -> Self {
        Self {
            dir: runtime_dir(home, xdg_runtime_dir),
        }
    }

    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    pub fn record(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.run"))
    }

    pub fn log(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.log"))
    }

    /// Where the foreground ssh's stderr goes while a start waits for it.
    pub fn stderr(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.err"))
    }

    pub fn lock(&self) -> PathBuf {
        self.dir.join("start.lock")
    }

    /// Creates the directory, and each missing parent, with mode `0700`.
    pub fn ensure(&self) -> io::Result<()> {
        let mut missing = Vec::new();
        let mut current = self.dir.as_path();
        while !current.exists() {
            missing.push(current.to_path_buf());
            match current.parent() {
                Some(parent) => current = parent,
                None => break,
            }
        }
        for dir in missing.iter().rev() {
            create_private_dir(dir)?;
        }
        Ok(())
    }

    /// The names that have a record, in no particular order.
    pub fn recorded_names(&self) -> Vec<String> {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .and_then(|name| name.strip_suffix(".run"))
                    .map(str::to_owned)
            })
            .collect()
    }
}

/// What was started, kept while it may still run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunningRecord {
    pub pid: u32,
    /// Field 22 of `/proc/<pid>/stat`: clock ticks after boot.
    pub start: u64,
    pub boot: String,
    /// Unix seconds.
    pub started: u64,
    pub host: String,
    /// The flag and spec, as `-L 127.0.0.1:5432:localhost:5432`.
    pub forward: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Record {
    Running(RunningRecord),
    Failed {
        reason: String,
        exit: Option<i32>,
        /// Unix seconds.
        finished: u64,
    },
}

impl Record {
    pub fn render(&self) -> String {
        match self {
            Record::Running(record) => format!(
                "state running\npid {}\nstart {}\nboot {}\nstarted {}\nhost {}\nforward {}\n",
                record.pid, record.start, record.boot, record.started, record.host, record.forward
            ),
            Record::Failed {
                reason,
                exit,
                finished,
            } => {
                let exit = exit.map_or(String::new(), |code| format!("exit {code}\n"));
                // The reason is one line, so the record stays one value per line.
                let reason = super::printable(reason.lines().next().unwrap_or_default());
                format!("state failed\nreason {reason}\n{exit}finished {finished}\n")
            }
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        let value = |key: &str| {
            text.lines().find_map(|line| {
                let (found, value) = line.split_once(' ').unwrap_or((line, ""));
                (found == key).then(|| value.to_owned())
            })
        };
        match value("state")?.as_str() {
            "running" => Some(Record::Running(RunningRecord {
                pid: value("pid")?.parse().ok()?,
                start: value("start")?.parse().ok()?,
                boot: value("boot")?,
                started: value("started")?.parse().ok()?,
                host: value("host")?,
                forward: value("forward")?,
            })),
            "failed" => Some(Record::Failed {
                reason: value("reason")?,
                exit: value("exit").and_then(|code| code.parse().ok()),
                finished: value("finished")?.parse().ok()?,
            }),
            _ => None,
        }
    }

    /// Reads a record; a missing or unreadable one is `None`.
    pub fn read(path: &Path) -> Option<Self> {
        Self::parse(&fs::read_to_string(path).ok()?)
    }

    /// Replaces the record at `path` atomically.
    pub fn write(&self, path: &Path) -> io::Result<()> {
        let dir = path.parent().unwrap_or(Path::new("."));
        write_unique(dir, ".record", self.render().as_bytes(), 0o600)?.rename_to(path)?;
        sync_dir(dir)
    }

    pub fn remove(path: &Path) -> io::Result<()> {
        match fs::remove_file(path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }
}

/// The newest of `processes` that logs to `log`, started no earlier than `since` when given.
pub fn find_in(processes: &[Logged], log: &Path, since: Option<u64>) -> Option<(u32, u64)> {
    processes
        .iter()
        .filter(|process| process.log == log)
        .filter(|process| since.is_none_or(|since| process.start >= since))
        .max_by_key(|process| process.start)
        .map(|process| (process.pid, process.start))
}

impl Proc {
    /// Whether `pid` is a live process of `uid` whose command line names `log` as a tunnel's, and,
    /// when `start` is given, that started at that time.
    pub fn is_tunnel(&self, pid: u32, start: Option<u64>, log: &Path, uid: &Uid) -> bool {
        let Some(stat) = self.stat(pid) else {
            return false;
        };
        !matches!(stat.state, 'Z' | 'X' | 'x')
            && start.is_none_or(|start| start == stat.start)
            && self.uid(pid).as_ref() == Some(uid)
            && self.cmdline(pid).is_some_and(|args| names_log(&args, log))
    }

    /// The newest live process of `uid` whose command line names `log` as a tunnel's, started no
    /// earlier than `since` (clock ticks after boot) when given, with its start time.
    pub fn find_tunnel(&self, log: &Path, uid: &Uid, since: Option<u64>) -> Option<(u32, u64)> {
        find_in(&self.scan(uid), log, since)
    }

    /// Rebuilds the record of a tunnel process found without one.
    pub fn rebuild_record(&self, pid: u32, start: u64) -> Option<RunningRecord> {
        let args = self.cmdline(pid)?;
        let text = |arg: &Vec<u8>| String::from_utf8(arg.clone()).ok();
        let separator = args.iter().position(|arg| arg == b"--")?;
        let host = text(args.get(separator + 1)?)?;
        let flag = args[..separator]
            .iter()
            .rposition(|arg| matches!(arg.as_slice(), b"-L" | b"-R" | b"-D"))?;
        let forward = format!("{} {}", text(&args[flag])?, text(args.get(flag + 1)?)?);
        Some(RunningRecord {
            pid,
            start,
            boot: self.boot_id()?,
            started: self.started_at(start).unwrap_or_else(now),
            host,
            forward,
        })
    }

    /// Whether `record`'s process is still the tunnel it was started as.
    pub fn identify(&self, record: &RunningRecord, log: &Path, uid: &Uid) -> Identity {
        if self.boot_id().as_deref() != Some(record.boot.as_str()) {
            return Identity::Stale;
        }
        if self.is_tunnel(record.pid, Some(record.start), log, uid) {
            Identity::Alive
        } else {
            Identity::Gone
        }
    }
}

/// Whether the arguments name `log` as a tunnel's ssh does (`process::tunnel_marker`).
fn names_log(args: &[Vec<u8>], log: &Path) -> bool {
    crate::platform::process::marked_log(args).as_deref() == Some(log)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Identity {
    /// The recorded process is the tunnel and runs.
    Alive,
    /// The tunnel's process is gone; its PID may belong to another process now.
    Gone,
    /// The record is from another boot.
    Stale,
}

/// Unix seconds now.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Whether a local address can be listened on now.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Bind {
    Free,
    InUse,
    /// A privileged port, or an address the user may not bind.
    Denied,
}

/// Binds `address:port` and closes it at once, as ssh would bind it. A name is resolved and
/// every address it yields is tried. Addresses this machine does not have (no IPv6, say) count
/// as free: ssh skips them too.
///
/// On Linux the probe sets `SO_REUSEADDR`, as ssh does (and the standard library's listener).
/// macOS lets a second `SO_REUSEADDR` socket bind an address another socket listens on, so
/// there the probe goes without it and finds the listener.
pub fn try_bind(address: &str, port: u16) -> Bind {
    let addresses: Vec<SocketAddr> = match address.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, port)],
        Err(_) => (address, port)
            .to_socket_addrs()
            .map(Iterator::collect)
            .unwrap_or_default(),
    };
    for address in addresses {
        let bound = match address {
            SocketAddr::V4(_) if cfg!(target_os = "linux") => TcpListener::bind(address).map(drop),
            _ => bind_probe(address),
        };
        match bound {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => return Bind::InUse,
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => return Bind::Denied,
            Err(_) => {}
        }
    }
    Bind::Free
}

/// How long a foreground ssh must keep running after it authenticated before its forward
/// counts as up: with `ExitOnForwardFailure=yes` a forward that cannot be opened ends ssh well
/// within it (a local listener is opened right after authentication, a remote forward is
/// refused in one round trip).
const SURVIVAL: Duration = Duration::from_secs(1);

/// How long a start waits for a foreground ssh's forward to come up.
pub const READY_BOUND: Duration = Duration::from_secs(30);

/// Whether a foreground ssh (one that cannot background itself, as on Windows) has opened
/// `forward`, given whether its log shows it authenticated and since when it has run so.
///
/// Every kind needs the authentication and ssh alive [`SURVIVAL`] after it; a local or dynamic
/// forward also needs one of its listen addresses bound. Since a start checks that every
/// listen address is free first, a bound address could otherwise only be another program's
/// that took it while ssh connected, and then ssh exits on the forward failure within that
/// time.
pub fn ready(forward: &Forward, authenticated: bool, alive_since_auth: Option<Instant>) -> bool {
    authenticated
        && alive_since_auth.is_some_and(|since| since.elapsed() >= SURVIVAL)
        && (!forward.kind.listens_locally() || listening(forward))
}

/// Whether a forward whose foreground ssh still runs at [`READY_BOUND`] without having been
/// seen [`ready`] counts as up: a local or dynamic forward whose listen address is in use,
/// which the start proved free under its lock before spawning ssh, so ssh holds it. A remote
/// forward has no such evidence and never does.
pub fn up_at_bound(forward: &Forward, listening: bool) -> bool {
    forward.kind.listens_locally() && listening
}

/// Whether one of `forward`'s local listen addresses is in use now.
pub fn listening(forward: &Forward) -> bool {
    forward
        .listen_addresses()
        .iter()
        .any(|address| try_bind(address, forward.bind_port) == Bind::InUse)
}

/// Whether a tunnel's log shows ssh authenticated, which it logs from `LogLevel=VERBOSE` on.
pub fn authenticated(log: &Path) -> bool {
    fs::read(log).is_ok_and(|bytes| {
        String::from_utf8_lossy(&bytes)
            .lines()
            .any(|line| line.starts_with("Authenticated to "))
    })
}

/// The last line of a tunnel's log that says something, debug output left out.
pub fn log_reason(log: &Path) -> Option<String> {
    log_tail(log, 1).pop()
}

/// The last `count` lines of a tunnel's log that say something, debug output left out.
pub fn log_tail(log: &Path, count: usize) -> Vec<String> {
    let Ok(bytes) = fs::read(log) else {
        return Vec::new();
    };
    // Logs are small, but a runaway one must not be read whole into a list.
    let bytes = &bytes[bytes.len().saturating_sub(64 * 1024)..];
    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<String> = text
        .lines()
        .map(|line| super::printable(line).trim_end().to_owned())
        .filter(|line| !line.trim().is_empty() && !line.starts_with("debug"))
        .collect();
    lines[lines.len().saturating_sub(count)..].to_vec()
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::domain::tunnel::Kind;
    #[cfg(target_os = "linux")]
    use crate::platform::process::Stat;

    const BOOT: &str = "3f0d9c1e-0000-4000-8000-000000000001";
    #[cfg(target_os = "linux")]
    const LOG: &str = "/run/user/1000/portico/tunnels/db.log";

    #[cfg(target_os = "linux")]
    /// A fake `/proc` with a boot ID and a boot time of 1000 seconds.
    fn fake_proc() -> (tempfile::TempDir, Proc) {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("sys/kernel/random")).unwrap();
        fs::write(
            dir.path().join("sys/kernel/random/boot_id"),
            format!("{BOOT}\n"),
        )
        .unwrap();
        fs::write(dir.path().join("stat"), "cpu 1 2 3\nbtime 1000\n").unwrap();
        let proc = Proc::at(dir.path());
        (dir, proc)
    }

    #[cfg(target_os = "linux")]
    /// Adds a process that leads its own session, as a backgrounded ssh does.
    fn add_process(root: &Path, pid: u32, start: u64, state: char, uid: u32, args: &[&str]) {
        add_in_session(root, pid, pid, start, state, uid, args);
    }

    #[cfg(target_os = "linux")]
    fn add_in_session(
        root: &Path,
        pid: u32,
        session: u32,
        start: u64,
        state: char,
        uid: u32,
        args: &[&str],
    ) {
        let dir = root.join(pid.to_string());
        fs::create_dir_all(&dir).unwrap();
        // Fields 4 to 6 are the parent, the process group and the session; 7 to 21 are filler;
        // field 22 is the start time.
        let filler = (7..22).map(|_| "0").collect::<Vec<_>>().join(" ");
        fs::write(
            dir.join("stat"),
            format!("{pid} (ssh) a) {state} 1 {pid} {session} {filler} {start} 0 0\n"),
        )
        .unwrap();
        fs::write(
            dir.join("status"),
            format!("Name:\tssh\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\n"),
        )
        .unwrap();
        let mut cmdline = args.join("\0");
        cmdline.push('\0');
        fs::write(dir.join("cmdline"), cmdline).unwrap();
    }

    #[cfg(target_os = "linux")]
    fn tunnel_args() -> Vec<&'static str> {
        vec![
            "/usr/bin/ssh",
            "-F",
            "/tmp/flat",
            "-f",
            "-N",
            "-E",
            LOG,
            "-L",
            "5432:db:5432",
            "--",
            "prod-db",
        ]
    }

    fn record(pid: u32, start: u64) -> RunningRecord {
        RunningRecord {
            pid,
            start,
            boot: BOOT.into(),
            started: 2000,
            host: "prod-db".into(),
            forward: "-L 5432:db:5432".into(),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stat_reads_state_and_start_after_the_command_name() {
        let (dir, proc) = fake_proc();
        add_process(dir.path(), 41, 9218734, 'S', 1000, &tunnel_args());
        assert_eq!(
            proc.stat(41),
            Some(Stat {
                start: 9218734,
                state: 'S',
                session: 41,
            })
        );
        assert_eq!(proc.uid(41), Some(1000));
        assert_eq!(proc.stat(42), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn identity_accepts_the_recorded_process() {
        let (dir, proc) = fake_proc();
        add_process(dir.path(), 41, 500, 'S', 1000, &tunnel_args());
        assert_eq!(
            proc.identify(&record(41, 500), Path::new(LOG), &1000),
            Identity::Alive
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn identity_rejects_another_start_time() {
        let (dir, proc) = fake_proc();
        add_process(dir.path(), 41, 501, 'S', 1000, &tunnel_args());
        assert_eq!(
            proc.identify(&record(41, 500), Path::new(LOG), &1000),
            Identity::Gone
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn identity_rejects_another_boot() {
        let (dir, proc) = fake_proc();
        add_process(dir.path(), 41, 500, 'S', 1000, &tunnel_args());
        let mut old = record(41, 500);
        old.boot = "another".into();
        assert_eq!(proc.identify(&old, Path::new(LOG), &1000), Identity::Stale);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn identity_rejects_another_uid() {
        let (dir, proc) = fake_proc();
        add_process(dir.path(), 41, 500, 'S', 0, &tunnel_args());
        assert_eq!(
            proc.identify(&record(41, 500), Path::new(LOG), &1000),
            Identity::Gone
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn identity_rejects_a_zombie() {
        let (dir, proc) = fake_proc();
        add_process(dir.path(), 41, 500, 'Z', 1000, &tunnel_args());
        assert_eq!(
            proc.identify(&record(41, 500), Path::new(LOG), &1000),
            Identity::Gone
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn identity_rejects_a_command_line_without_the_log() {
        let (dir, proc) = fake_proc();
        add_process(
            dir.path(),
            41,
            500,
            'S',
            1000,
            &["sleep", "-E", "/other.log"],
        );
        assert_eq!(
            proc.identify(&record(41, 500), Path::new(LOG), &1000),
            Identity::Gone
        );
        // A missing process is gone too.
        assert_eq!(
            proc.identify(&record(43, 500), Path::new(LOG), &1000),
            Identity::Gone
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn scan_finds_the_process_with_the_log_argument() {
        let (dir, proc) = fake_proc();
        add_process(dir.path(), 40, 300, 'S', 1000, &["bash"]);
        add_process(dir.path(), 41, 500, 'S', 1000, &tunnel_args());
        assert_eq!(
            proc.find_tunnel(Path::new(LOG), &1000, None),
            Some((41, 500))
        );
        assert_eq!(
            proc.find_tunnel(Path::new(LOG), &1000, Some(400)),
            Some((41, 500))
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn scan_ignores_a_foreground_ssh_that_is_still_connecting() {
        let (dir, proc) = fake_proc();
        add_in_session(dir.path(), 41, 7, 500, 'S', 1000, &tunnel_args());
        assert_eq!(proc.find_tunnel(Path::new(LOG), &1000, None), None);
        add_process(dir.path(), 42, 510, 'S', 1000, &tunnel_args());
        assert_eq!(
            proc.find_tunnel(Path::new(LOG), &1000, None),
            Some((42, 510))
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn scan_ignores_older_and_foreign_processes() {
        let (dir, proc) = fake_proc();
        add_process(dir.path(), 41, 500, 'S', 1000, &tunnel_args());
        add_process(dir.path(), 42, 900, 'S', 0, &tunnel_args());
        add_process(dir.path(), 43, 950, 'Z', 1000, &tunnel_args());
        assert_eq!(proc.find_tunnel(Path::new(LOG), &1000, Some(600)), None);
        assert_eq!(
            proc.find_tunnel(Path::new(LOG), &1000, None),
            Some((41, 500))
        );
        assert_eq!(
            proc.find_tunnel(Path::new("/run/other.log"), &1000, None),
            None
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn rediscovers_a_tunnel_without_a_record() {
        let (dir, proc) = fake_proc();
        add_process(dir.path(), 41, 500, 'S', 1000, &tunnel_args());
        let (pid, start) = proc.find_tunnel(Path::new(LOG), &1000, None).unwrap();
        let rebuilt = proc.rebuild_record(pid, start).unwrap();
        let ticks = u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).unwrap();
        assert_eq!(
            rebuilt,
            RunningRecord {
                started: 1000 + 500 / ticks,
                ..record(41, 500)
            }
        );
        assert_eq!(
            proc.identify(&rebuilt, Path::new(LOG), &1000),
            Identity::Alive
        );
    }

    /// The real process table on every system: a process whose command line looks like a
    /// started tunnel's, detached as a started tunnel is (in its own session as `ssh -f` puts
    /// itself on Unix).
    #[test]
    fn a_live_tunnel_process_is_recognised_rebuilt_and_stopped() {
        use crate::platform::process::{current_uid, detach, stop_process};

        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("db.log");
        #[cfg(unix)]
        let mut command = {
            let mut command = std::process::Command::new("sh");
            command
                .args(["-c", "sleep 30; :", "ssh", "-E"])
                .arg(&log)
                .args(["-L", "5432:db:5432", "--", "prod-db"]);
            command
        };
        // `rem` leaves the rest of cmd's command line, which reads as a tunnel's, unrun.
        #[cfg(windows)]
        let mut command = {
            use std::os::windows::process::CommandExt;
            let mut command = std::process::Command::new("cmd");
            command
                .raw_arg(format!(
                    "/c ping -n 30 127.0.0.1 >nul & rem ssh -o PermitLocalCommand=no -o \"LocalCommand=portico-tunnel {}\" -L 5432:db:5432 -- prod-db",
                    log.display()
                ))
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            command
        };
        let mut child = detach(&mut command).spawn().unwrap();
        let (proc, uid) = (Proc::system(), current_uid());

        let (pid, start) = proc.find_tunnel(&log, &uid, None).unwrap();
        assert_eq!(pid, child.id());
        assert_eq!(proc.find_tunnel(&log, &uid, Some(start + 1)), None);
        let record = proc.rebuild_record(pid, start).unwrap();
        assert_eq!(record.host, "prod-db");
        assert_eq!(record.forward, "-L 5432:db:5432");
        assert!(record.started.abs_diff(now()) < 60, "{record:?}");
        assert_eq!(proc.identify(&record, &log, &uid), Identity::Alive);
        let restarted = RunningRecord {
            start: start + 1,
            ..record.clone()
        };
        assert_eq!(proc.identify(&restarted, &log, &uid), Identity::Gone);
        let rebooted = RunningRecord {
            boot: "another-boot".into(),
            ..record.clone()
        };
        assert_eq!(proc.identify(&rebooted, &log, &uid), Identity::Stale);
        assert_eq!(
            proc.identify(&record, &dir.path().join("other.log"), &uid),
            Identity::Gone
        );

        assert!(stop_process(pid, || proc.is_tunnel(pid, Some(start), &log, &uid)).unwrap());
        child.wait().unwrap();
        assert_eq!(proc.identify(&record, &log, &uid), Identity::Gone);
        assert_eq!(proc.find_tunnel(&log, &uid, None), None);
    }

    #[test]
    fn record_round_trip() {
        let running = Record::Running(record(41, 500));
        assert_eq!(Record::parse(&running.render()), Some(running.clone()));
        let failed = Record::Failed {
            reason: "Address already in use: 127.0.0.1:5432\nsecond line".into(),
            exit: Some(255),
            finished: 1781861043,
        };
        assert_eq!(
            Record::parse(&failed.render()),
            Some(Record::Failed {
                reason: "Address already in use: 127.0.0.1:5432".into(),
                exit: Some(255),
                finished: 1781861043,
            })
        );
        assert_eq!(Record::parse("state running\npid x\n"), None);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.run");
        running.write(&path).unwrap();
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(Record::read(&path), Some(running));
        Record::remove(&path).unwrap();
        Record::remove(&path).unwrap();
        assert_eq!(Record::read(&path), None);
    }

    /// macOS has no runtime directory: its `runtime_dir` is tested in `platform::macos`.
    #[cfg(target_os = "linux")]
    #[test]
    fn runtime_dir_falls_back_without_a_usable_xdg_runtime_dir() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let xdg = dir.path().join("xdg");
        fs::create_dir_all(&xdg).unwrap();
        let fallback = home.join(".ssh/portico/run/tunnels");
        assert_eq!(
            RuntimeDir::locate(&home, Some(xdg.as_os_str())).path(),
            xdg.join("portico/tunnels")
        );
        assert_eq!(RuntimeDir::locate(&home, None).path(), fallback);
        assert_eq!(
            RuntimeDir::locate(&home, Some(OsStr::new("relative/dir"))).path(),
            fallback
        );
        assert_eq!(
            RuntimeDir::locate(&home, Some(dir.path().join("missing").as_os_str())).path(),
            fallback
        );

        let runtime = RuntimeDir::locate(&home, None);
        runtime.ensure().unwrap();
        for path in [
            home.join(".ssh"),
            home.join(".ssh/portico/run"),
            fallback.clone(),
        ] {
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o700,
                "{}",
                path.display()
            );
        }
        fs::write(runtime.record("db"), "").unwrap();
        fs::write(runtime.log("db"), "").unwrap();
        assert_eq!(runtime.recorded_names(), ["db"]);
    }

    #[test]
    fn try_bind_sees_a_listening_port() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert_eq!(try_bind("127.0.0.1", port), Bind::InUse);
        assert_eq!(try_bind("0.0.0.0", port), Bind::InUse);
        // (Whether the port is free once released is not asserted: a parallel test may be given
        // the same port by the kernel at once.)
        drop(listener);
        // An address this machine does not have is skipped, as ssh skips it.
        assert_eq!(try_bind("192.0.2.1", port), Bind::Free);
    }

    #[test]
    fn the_ipv6_wildcard_ignores_ipv4_listeners() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // ssh binds `::` with IPV6_V6ONLY, so an IPv4 listener does not make it busy.
        assert_eq!(try_bind("::", port), Bind::Free);
        if let Ok(v6) = TcpListener::bind("[::1]:0") {
            let port = v6.local_addr().unwrap().port();
            assert_eq!(try_bind("::1", port), Bind::InUse);
            assert_eq!(try_bind("::", port), Bind::InUse);
        }
    }

    /// A log as ssh writes it at `LogLevel=VERBOSE`, authenticated when `authenticated`.
    fn verbose_log(dir: &Path, authenticated: bool) -> PathBuf {
        let log = dir.join("db.log");
        let mut text = String::from("Connecting to prod.example [192.0.2.1] port 22.\r\n");
        if authenticated {
            text.push_str(
                "Authenticated to prod.example ([192.0.2.1]:22) using \"publickey\".\r\n",
            );
        }
        fs::write(&log, text).unwrap();
        log
    }

    fn ago(millis: u64) -> Option<Instant> {
        Instant::now().checked_sub(Duration::from_millis(millis))
    }

    #[test]
    fn ready_for_local_forward_once_bound() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let bound = Forward::parse(Kind::Local, &format!("127.0.0.1:{port}:db:5432")).unwrap();
        assert!(ready(&bound, true, ago(1100)));
        let socks = Forward::parse(Kind::Dynamic, &format!("127.0.0.1:{port}")).unwrap();
        assert!(ready(&socks, true, ago(1100)));
        // A port held before ssh authenticated, or by a program that took it while ssh still
        // could exit on the forward failure, is not ssh's.
        assert!(!ready(&bound, false, ago(1100)));
        assert!(!ready(&bound, true, ago(200)));
        assert!(!ready(&bound, true, None));
        drop(listener);
        let free = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = free.local_addr().unwrap().port();
        drop(free);
        let free = Forward::parse(Kind::Local, &format!("127.0.0.1:{port}:db:5432")).unwrap();
        assert!(!ready(&free, true, ago(1100)));
    }

    #[test]
    fn ready_for_remote_forward_after_auth_and_a_second() {
        let remote = Forward::parse(Kind::Remote, "8080:localhost:3000").unwrap();
        assert!(ready(&remote, true, ago(1100)));
        assert!(!ready(&remote, true, ago(200)));
        assert!(!ready(&remote, false, ago(1100)));
    }

    #[test]
    fn at_the_bound_only_a_listening_local_forward_is_up() {
        let local = Forward::parse(Kind::Local, "5432:db:5432").unwrap();
        let socks = Forward::parse(Kind::Dynamic, "1080").unwrap();
        let remote = Forward::parse(Kind::Remote, "8080:localhost:3000").unwrap();
        assert!(up_at_bound(&local, true));
        assert!(up_at_bound(&socks, true));
        assert!(!up_at_bound(&local, false));
        assert!(!up_at_bound(&socks, false));
        assert!(!up_at_bound(&remote, true));
        assert!(!up_at_bound(&remote, false));
    }

    #[test]
    fn the_log_shows_authentication_from_its_verbose_line() {
        let dir = tempfile::tempdir().unwrap();
        assert!(authenticated(&verbose_log(dir.path(), true)));
        assert!(!authenticated(&verbose_log(dir.path(), false)));
        assert!(!authenticated(&dir.path().join("missing.log")));
    }

    #[test]
    fn the_log_reason_is_its_last_meaningful_line() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("db.log");
        assert_eq!(log_reason(&log), None);
        fs::write(
            &log,
            "Warning: Permanently added 'x' to the list of known hosts.\r\n\
             bind [127.0.0.1]:5432: Address already in use\n\
             debug1: something\n\n",
        )
        .unwrap();
        assert_eq!(
            log_reason(&log).as_deref(),
            Some("bind [127.0.0.1]:5432: Address already in use")
        );
        assert_eq!(log_tail(&log, 5).len(), 2);
    }
}
