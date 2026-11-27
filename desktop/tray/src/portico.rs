//! Running `portico`: locating it, one-shot runs, the background worker and the
//! self-update hand-off. Every run is an argument list, never a shell.

use crate::model::{self, Action};
use std::collections::{HashMap, VecDeque};
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(windows)]
const DETACHED_PROCESS: u32 = 0x8;
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x200;

#[cfg(windows)]
const PROGRAM: &str = "portico.exe";
#[cfg(not(windows))]
const PROGRAM: &str = "portico";

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(windows)]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(windows)]
fn well_known(home: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        dirs.push(PathBuf::from(local).join(r"Microsoft\WinGet\Links"));
    }
    dirs.push(home.join(".cargo").join("bin"));
    dirs
}

#[cfg(not(windows))]
fn well_known(home: &Path) -> Vec<PathBuf> {
    vec![
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        home.join(".cargo").join("bin"),
    ]
}

/// The recorded path if it is still an executable file, else `portico` on
/// `path_var`, else the well-known install directories, in that order.
pub fn locate(recorded: Option<&Path>, path_var: Option<&OsStr>, home: &Path) -> Option<PathBuf> {
    if let Some(path) = recorded.filter(|p| is_executable(p)) {
        return Some(path.to_path_buf());
    }
    let on_path = path_var.map(std::env::split_paths).into_iter().flatten();
    on_path
        .filter(|dir| !dir.as_os_str().is_empty())
        .chain(well_known(home))
        .map(|dir| dir.join(PROGRAM))
        .find(|candidate| is_executable(candidate))
}

#[derive(Debug)]
pub struct Output {
    pub stdout: String,
    pub stderr: String,
    pub exit: Option<i32>,
}

fn command(program: &Path, args: &[String]) -> Command {
    let mut command = Command::new(program);
    command.args(args).stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// Runs `program args` to its end and returns what it printed.
pub fn run(program: &Path, args: &[String]) -> io::Result<Output> {
    let output = command(program, args).output()?;
    Ok(Output {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        exit: output.status.code(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Version,
    Settings,
    Hosts,
    Tunnels,
    Containers(Option<String>, Vec<String>),
    Tunnel(Action, String),
    Connect(String),
    Shell(Option<String>, String, String),
}

impl Request {
    /// Requests with the same key run one after another; others run side by side.
    fn key(&self) -> String {
        match self {
            Request::Version => "version".into(),
            Request::Settings => "settings".into(),
            Request::Hosts => "hosts".into(),
            Request::Tunnels => "tunnels".into(),
            Request::Containers(alias, _) => {
                format!("containers:{}", alias.as_deref().unwrap_or(""))
            }
            Request::Tunnel(_, name) => format!("tunnel:{name}"),
            Request::Connect(alias) => format!("connect:{alias}"),
            Request::Shell(alias, _, id) => {
                format!("shell:{}:{id}", alias.as_deref().unwrap_or(""))
            }
        }
    }

    fn args(&self) -> io::Result<Vec<String>> {
        let invalid = |what: &str| io::Error::new(io::ErrorKind::InvalidInput, what.to_string());
        Ok(match self {
            Request::Version => vec!["--version".into()],
            Request::Settings => vec!["settings".into(), "--json".into()],
            Request::Hosts => vec!["hosts".into(), "--json".into()],
            Request::Tunnels => vec!["tunnels".into(), "--json".into()],
            Request::Containers(alias, backends) => {
                let names: Vec<&str> = backends.iter().map(String::as_str).collect();
                model::containers_args(alias.as_deref(), &names)
                    .ok_or_else(|| invalid("no container backend is enabled"))?
            }
            Request::Tunnel(Action::None, _) => return Err(invalid("a tunnel without an action")),
            Request::Tunnel(action, name) => model::tunnel_args(*action, name),
            Request::Connect(alias) => model::connect_args(alias),
            Request::Shell(alias, backend, id) => model::shell_args(alias.as_deref(), backend, id),
        })
    }
}

type Lanes = Arc<Mutex<HashMap<String, VecDeque<Request>>>>;

/// Runs requests on background threads and sends each answer to the channel.
/// Requests of one kind (a tunnel by name, a host's containers, ...) run one at
/// a time, strictly in the order they were sent, on one thread that ends when
/// its queue is empty; requests of different kinds run side by side. The
/// caller never waits.
pub struct Worker {
    program: PathBuf,
    sender: Sender<(Request, io::Result<Output>)>,
    lanes: Lanes,
}

impl Worker {
    pub fn spawn(program: PathBuf, sender: Sender<(Request, io::Result<Output>)>) -> Worker {
        Worker {
            program,
            sender,
            lanes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn send(&self, request: Request) {
        let key = request.key();
        let mut lanes = self.lanes.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(queue) = lanes.get_mut(&key) {
            queue.push_back(request);
            return;
        }
        lanes.insert(key.clone(), VecDeque::from([request]));
        drop(lanes);
        let program = self.program.clone();
        let sender = self.sender.clone();
        let lanes = Arc::clone(&self.lanes);
        std::thread::spawn(move || {
            loop {
                let next = {
                    let mut lanes = lanes.lock().unwrap_or_else(|e| e.into_inner());
                    let next = lanes.get_mut(&key).and_then(VecDeque::pop_front);
                    if next.is_none() {
                        lanes.remove(&key);
                    }
                    next
                };
                let Some(request) = next else { break };
                let result = request.args().and_then(|args| run(&program, &args));
                // The receiver is gone when the app is quitting.
                let _ = sender.send((request, result));
            }
        });
    }
}

/// Starts `program desktop install --yes --tray-only` in a session of its own,
/// so it outlives this app (which it replaces), appending its output to `log`.
pub fn start_update(program: &Path, log: &Path) -> io::Result<()> {
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)?;
    let args = ["desktop", "install", "--yes", "--tray-only"].map(String::from);
    let mut update = command(program, &args);
    update.stdout(file.try_clone()?).stderr(file);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: `setsid` is async-signal-safe and touches no memory of ours.
        unsafe {
            update.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        update.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    update.spawn().map(drop)
}

/// The log's last non-empty line, when the log is newer than the app and that
/// line starts with `error:`: the update ran and failed.
pub fn update_failure(log: &Path, app_modified: SystemTime) -> Option<String> {
    if std::fs::metadata(log).ok()?.modified().ok()? <= app_modified {
        return None;
    }
    let text = std::fs::read_to_string(log).ok()?;
    let last = text.lines().rev().map(str::trim).find(|l| !l.is_empty())?;
    last.starts_with("error:").then(|| last.to_string())
}
