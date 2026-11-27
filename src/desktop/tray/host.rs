//! The tray app on the system Portico runs on: `curl` and `tar`, the login entry (a LaunchAgent
//! on macOS, a `Run` value on Windows), and finding, stopping and starting the app. Elsewhere
//! (Linux) there is no tray app, and every action fails.
//!
//! Test seam: in debug builds, `PORTICO_TRAY_NO_START=1` neither starts nor stops the app
//! (so tests never stop a developer's own running app) and skips `launchctl`, and on Windows `PORTICO_TRAY_RUN_KEY` names the key that holds the `Run` value.

use std::path::Path;

use super::{Paths, System, TrayHost, paths};
use crate::desktop::DesktopEnv;

/// The tray app on this system.
pub struct SystemHost {
    /// Read on macOS only: the LaunchAgent and the bundle `open` starts.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    paths: Paths,
}

impl SystemHost {
    /// The host for `env`'s home. Outside macOS and Windows the paths are macOS's and unused.
    pub fn new(env: &DesktopEnv) -> Self {
        let system = System::current().unwrap_or(System::MacOs);
        Self {
            paths: paths(system, &env.home, env.local_app_data.as_deref()),
        }
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
impl TrayHost for SystemHost {
    fn download(&self, _url: &str, _to: &Path) -> Result<(), String> {
        Err(unsupported())
    }
    fn unpack(&self, _archive: &Path, _into: &Path) -> Result<(), String> {
        Err(unsupported())
    }
    fn app_version(&self, _app: &Path) -> Option<String> {
        None
    }
    fn login_entry(&self) -> Option<String> {
        None
    }
    fn set_login_entry(&self, _binary: &Path, _portico: &Path) -> Result<(), String> {
        Err(unsupported())
    }
    fn remove_login_entry(&self) -> Result<(), String> {
        Err(unsupported())
    }
    fn running(&self) -> Vec<u32> {
        Vec::new()
    }
    fn stop(&self, _during_update: bool) -> Result<(), String> {
        Err(unsupported())
    }
    fn start(&self, _binary: &Path, _portico: &Path, _via_login: bool) -> Result<(), String> {
        Err(unsupported())
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
fn unsupported() -> String {
    "there is no tray app on this system".into()
}

#[cfg(any(target_os = "macos", windows))]
mod shared {
    use std::{
        ffi::{OsStr, OsString},
        path::{Path, PathBuf},
        process::{Command, Stdio},
        thread,
        time::{Duration, Instant},
    };

    use crate::platform::process::{
        Proc, arg_path, command, current_uid, hide_console, stop_single_process,
    };

    /// In a debug build, whether the tests asked for the app never to be started or stopped
    /// (and on macOS for `launchctl` never to run).
    pub(super) fn no_start() -> bool {
        cfg!(debug_assertions)
            && std::env::var_os("PORTICO_TRAY_NO_START").is_some_and(|v| v == "1")
    }

    /// `program` (a path, or a name found on `PATH`) with `args`, its window hidden on Windows.
    pub(super) fn tool(program: &OsStr, args: &[&OsStr]) -> Command {
        let mut tool = command(program);
        tool.args(args).stdin(Stdio::null());
        // The installer may run without a console (the app's update): a console program it
        // starts would otherwise open a window of its own.
        hide_console(&mut tool);
        tool
    }

    /// Runs `program` with `args`; on failure, the command line with its last stderr line.
    pub(super) fn run(program: impl AsRef<OsStr>, args: &[&OsStr]) -> Result<String, String> {
        let program = program.as_ref();
        let line = std::iter::once(program.to_string_lossy().into_owned())
            .chain(args.iter().map(|arg| arg.to_string_lossy().into_owned()))
            .map(|arg| {
                if arg.contains(' ') {
                    format!("'{arg}'")
                } else {
                    arg
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        let output = tool(program, args).output().map_err(|error| {
            format!(
                "could not run {} ({line}): {error}",
                program.to_string_lossy()
            )
        })?;
        if output.status.success() {
            return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
        }
        let said = String::from_utf8_lossy(&output.stderr)
            .lines()
            .rfind(|line| !line.trim().is_empty())
            .map(|line| format!(": {}", line.trim()))
            .unwrap_or_default();
        Err(format!("{line} failed ({}){said}", output.status))
    }

    pub(super) fn os(text: &str) -> OsString {
        OsString::from(text)
    }

    /// Whether `path` names the app's executable.
    fn is_app(path: &Path) -> bool {
        path.file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| {
                name.eq_ignore_ascii_case("portico-tray")
                    || name.eq_ignore_ascii_case("portico-tray.exe")
            })
    }

    /// Whether `pid` is a live app of this user.
    fn is_running_app(proc: &Proc, pid: u32) -> bool {
        proc.cmdline(pid)
            .and_then(|args| args.first().map(|arg| is_app(&arg_path(arg))))
            .unwrap_or(false)
            && proc.uid(pid) == Some(current_uid())
            && proc
                .stat(pid)
                .is_some_and(|stat| !matches!(stat.state, 'Z' | 'X' | 'x'))
    }

    /// The process IDs of this user's running apps.
    pub(super) fn running() -> Vec<u32> {
        let proc = Proc::system();
        let me = std::process::id();
        proc.pids()
            .into_iter()
            .filter(|&pid| pid != me && is_running_app(&proc, pid))
            .collect()
    }

    /// Stops every running app; first, during an update, gives them 5 seconds to quit.
    pub(super) fn stop_running(during_update: bool) -> Result<(), String> {
        if during_update {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !running().is_empty() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(100));
            }
        }
        let proc = Proc::system();
        for pid in running() {
            // The app alone: during an update this installer is its child, and must outlive it.
            stop_single_process(pid, || is_running_app(&proc, pid))
                .map_err(|error| format!("could not stop the app (process {pid}): {error}"))?;
        }
        Ok(())
    }

    pub(super) fn download(url: &str, to: &Path) -> Result<(), String> {
        let args = [
            "--fail",
            "--location",
            "--proto",
            "=https",
            "--tlsv1.2",
            "--silent",
            "--show-error",
            "-o",
        ]
        .map(os);
        let mut all: Vec<&OsStr> = args.iter().map(OsString::as_os_str).collect();
        all.push(to.as_os_str());
        let url = os(url);
        all.push(&url);
        run("curl", &all).map(drop)
    }

    /// Unpacks with the system's own `tar` (`/usr/bin/tar`, Windows' `tar.exe`, a bsdtar
    /// that reads a zip; another `tar` first on `PATH`, such as Git's GNU tar, cannot), or in
    /// a debug build the one `PORTICO_TRAY_TAR` names.
    pub(super) fn unpack(flags: &str, archive: &Path, into: &Path) -> Result<(), String> {
        let tar = match std::env::var_os("PORTICO_TRAY_TAR") {
            Some(tar) if cfg!(debug_assertions) && !tar.is_empty() => PathBuf::from(tar),
            #[cfg(windows)]
            _ => crate::platform::paths::system_program("tar.exe"),
            #[cfg(not(windows))]
            _ => PathBuf::from("/usr/bin/tar"),
        };
        let (flags, change) = (os(flags), os("-C"));
        run(
            &tar,
            &[&flags, archive.as_os_str(), &change, into.as_os_str()],
        )
        .map(drop)
    }
}

#[cfg(target_os = "macos")]
impl SystemHost {
    fn plist(&self) -> &Path {
        self.paths
            .launch_agent
            .as_deref()
            .unwrap_or(Path::new("/nonexistent"))
    }

    fn domain() -> String {
        format!("gui/{}", crate::platform::process::current_uid())
    }
}

#[cfg(target_os = "macos")]
impl TrayHost for SystemHost {
    fn download(&self, url: &str, to: &Path) -> Result<(), String> {
        shared::download(url, to)
    }

    fn unpack(&self, archive: &Path, into: &Path) -> Result<(), String> {
        shared::unpack("-xzf", archive, into)?;
        // An archive a browser downloaded carries a quarantine attribute that `tar` passes on
        // to what it unpacks, and Gatekeeper would block the ad-hoc-signed app. Clearing it
        // where there is none fails, which is fine.
        let _ = shared::run(
            "xattr",
            &[
                &shared::os("-dr"),
                &shared::os("com.apple.quarantine"),
                into.as_os_str(),
            ],
        );
        Ok(())
    }

    fn app_version(&self, app: &Path) -> Option<String> {
        let text = std::fs::read_to_string(app.join("Contents/Info.plist")).ok()?;
        super::plist_version(&text)
    }

    fn login_entry(&self) -> Option<String> {
        std::fs::read_to_string(self.plist()).ok()
    }

    fn set_login_entry(&self, binary: &Path, portico: &Path) -> Result<(), String> {
        let plist = self.plist();
        let failed = |error: std::io::Error| format!("cannot write {}: {error}", plist.display());
        if let Some(dir) = plist.parent() {
            std::fs::create_dir_all(dir).map_err(failed)?;
        }
        std::fs::write(plist, super::launch_agent_plist(binary, portico)).map_err(failed)
    }

    fn remove_login_entry(&self) -> Result<(), String> {
        match std::fs::remove_file(self.plist()) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("cannot remove {}: {error}", self.plist().display())),
        }
    }

    fn running(&self) -> Vec<u32> {
        shared::running()
    }

    fn stop(&self, during_update: bool) -> Result<(), String> {
        if shared::no_start() {
            return Ok(());
        }
        if !during_update {
            // Unloads the LaunchAgent, which stops the app it started; a LaunchAgent that is
            // not loaded makes this fail, which is fine.
            let service = format!("{}/{}", Self::domain(), super::LABEL);
            let _ = shared::run(
                "launchctl",
                &[&shared::os("bootout"), &shared::os(&service)],
            );
        }
        shared::stop_running(during_update)
    }

    fn start(&self, _binary: &Path, portico: &Path, via_login: bool) -> Result<(), String> {
        if shared::no_start() {
            return Ok(());
        }
        if via_login {
            let service = format!("{}/{}", Self::domain(), super::LABEL);
            // A loaded LaunchAgent (an update) is started again; otherwise it is loaded, and
            // `RunAtLoad` starts the app.
            if shared::run(
                "launchctl",
                &[&shared::os("kickstart"), &shared::os(&service)],
            )
            .is_ok()
            {
                return Ok(());
            }
            return shared::run(
                "launchctl",
                &[
                    &shared::os("bootstrap"),
                    &shared::os(&Self::domain()),
                    self.plist().as_os_str(),
                ],
            )
            .map(drop);
        }
        shared::run(
            "open",
            &[
                &shared::os("-a"),
                self.paths.app.as_os_str(),
                &shared::os("--args"),
                &shared::os("--portico"),
                portico.as_os_str(),
            ],
        )
        .map(drop)
    }
}

#[cfg(windows)]
impl SystemHost {
    /// The key that holds the `Run` value: `RUN_KEY`, or in a debug build the one the tests
    /// name.
    fn run_key() -> String {
        if cfg!(debug_assertions)
            && let Ok(key) = std::env::var("PORTICO_TRAY_RUN_KEY")
            && !key.is_empty()
        {
            return key;
        }
        super::RUN_KEY.to_string()
    }
}

#[cfg(windows)]
impl TrayHost for SystemHost {
    fn download(&self, url: &str, to: &Path) -> Result<(), String> {
        shared::download(url, to)
    }

    fn unpack(&self, archive: &Path, into: &Path) -> Result<(), String> {
        // Windows' own `tar.exe` (bsdtar) reads the zip.
        shared::unpack("-xf", archive, into)
    }

    /// `portico-tray.exe --version` prints `portico-tray <version>`. The app is a GUI program,
    /// so its output is read through a pipe; one that does not answer within 5 seconds is
    /// ended.
    fn app_version(&self, app: &Path) -> Option<String> {
        use std::{
            io::Read,
            process::Stdio,
            time::{Duration, Instant},
        };
        let mut child = shared::tool(app.as_os_str(), &[&shared::os("--version")])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
            }
        }
        let mut text = String::new();
        child.stdout.take()?.read_to_string(&mut text).ok()?;
        let version = text.trim().strip_prefix("portico-tray ")?.trim();
        (!version.is_empty()).then(|| version.to_string())
    }

    fn login_entry(&self) -> Option<String> {
        crate::platform::registry::read_string(&Self::run_key(), super::RUN_VALUE)
            .ok()
            .flatten()
    }

    fn set_login_entry(&self, binary: &Path, portico: &Path) -> Result<(), String> {
        crate::platform::registry::write_string(
            &Self::run_key(),
            super::RUN_VALUE,
            &super::run_value(binary, portico),
        )
        .map_err(|error| {
            format!(
                "cannot write the registry value {}: {error}",
                super::RUN_VALUE
            )
        })
    }

    fn remove_login_entry(&self) -> Result<(), String> {
        crate::platform::registry::delete_value(&Self::run_key(), super::RUN_VALUE).map_err(
            |error| {
                format!(
                    "cannot remove the registry value {}: {error}",
                    super::RUN_VALUE
                )
            },
        )
    }

    fn running(&self) -> Vec<u32> {
        shared::running()
    }

    fn stop(&self, during_update: bool) -> Result<(), String> {
        if shared::no_start() {
            return Ok(());
        }
        shared::stop_running(during_update)
    }

    /// Starts the app detached, as tunnels are; there is no service to start it through.
    fn start(&self, binary: &Path, portico: &Path, _via_login: bool) -> Result<(), String> {
        use std::process::Stdio;
        if shared::no_start() {
            return Ok(());
        }
        let mut app = std::process::Command::new(binary);
        app.arg("--portico")
            .arg(portico)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        crate::platform::process::detach(&mut app);
        app.spawn()
            .map(drop)
            .map_err(|error| format!("could not start {}: {error}", binary.display()))
    }
}
