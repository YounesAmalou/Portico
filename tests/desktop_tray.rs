//! `portico desktop` on macOS and Windows: the tray app (the menu bar app on macOS) installed
//! with a fake `curl` and a `tar` that logs and runs the system's own, in a temporary home.
//! The app is never started and `launchctl` never runs (`PORTICO_TRAY_NO_START`), and on
//! Windows the `Run` value goes to a test key of its own (`PORTICO_TRAY_RUN_KEY`).

#![cfg(any(target_os = "macos", windows))]

#[path = "support/executable.rs"]
mod executable;

use std::{
    fs,
    path::PathBuf,
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
};

use executable::write_executable;
use portico::desktop::tray::{self, Expected, System};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn system() -> System {
    System::current().unwrap()
}

struct Tray {
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    bin: PathBuf,
    /// `LOCALAPPDATA` (Windows).
    local: PathBuf,
    /// The key under `HKEY_CURRENT_USER` that takes the `Run` value (Windows).
    run_key: String,
}

impl Tray {
    fn new() -> Self {
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let home = root.join("home");
        let bin = root.join("bin");
        let local = root.join("local");
        for path in [&home, &bin, &local] {
            fs::create_dir_all(path).unwrap();
        }
        let tray = Self {
            run_key: format!(
                r"Software\Portico-test-{}-{}",
                std::process::id(),
                COUNT.fetch_add(1, Ordering::Relaxed)
            ),
            _dir: dir,
            root,
            home,
            bin,
            local,
        };
        // Serves the release asset and SHA256SUMS from the files the test puts in place,
        // or answers 404 while `404` exists.
        write_executable(
            tray.bin.join("curl"),
            &format!(
                "#!/bin/sh\nout=\nprev=\nfor arg do\n  [ \"$prev\" = -o ] && out=$arg\n  prev=$arg\ndone\nprintf '%s\\n' \"$prev\" >> '{root}/curl.log'\nif [ -f '{root}/404' ]; then echo 'curl: (22) The requested URL returned error: 404' >&2; exit 22; fi\ncase \"$prev\" in\n  */SHA256SUMS) cp '{root}/SHA256SUMS' \"$out\" ;;\n  *) cp '{root}/asset' \"$out\" ;;\nesac\n",
                root = tray.root.display()
            ),
        );
        write_executable(
            tray.bin.join("tar"),
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}/tar.log'\nexec {} \"$@\"\n",
                tray.root.display(),
                if cfg!(windows) {
                    "/c/Windows/System32/tar.exe"
                } else {
                    "/usr/bin/tar"
                }
            ),
        );
        tray
    }

    fn command(&self, args: &[&str]) -> Command {
        let inherited = std::env::var_os("PATH").unwrap_or_default();
        let path = std::env::join_paths(
            std::iter::once(self.bin.clone()).chain(std::env::split_paths(&inherited)),
        )
        .unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_portico"));
        command
            .args(args)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("LOCALAPPDATA", &self.local)
            .env("PATH", path)
            .env("PORTICO_TRAY_NO_START", "1")
            .env("PORTICO_TRAY_RUN_KEY", &self.run_key)
            .stdin(Stdio::null());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    /// An archive in the release layout, its app of `version`; its path.
    fn archive(&self, version: &str) -> PathBuf {
        let staging = self.root.join("staging");
        let _ = fs::remove_dir_all(&staging);
        let (archive, entry) = match system() {
            System::MacOs => {
                let contents = staging.join("Portico Tray.app/Contents");
                fs::create_dir_all(contents.join("MacOS")).unwrap();
                fs::write(contents.join("MacOS/portico-tray"), "#!/bin/sh\n").unwrap();
                fs::write(
                    contents.join("Info.plist"),
                    format!(
                        "<plist version=\"1.0\"><dict>\n<key>CFBundleShortVersionString</key>\n<string>{version}</string>\n</dict></plist>\n"
                    ),
                )
                .unwrap();
                (self.root.join("fixture.tar.gz"), "Portico Tray.app")
            }
            System::Windows => {
                fs::create_dir_all(&staging).unwrap();
                fs::write(staging.join("portico-tray.exe"), "not a program").unwrap();
                (self.root.join("fixture.zip"), "portico-tray.exe")
            }
        };
        let (tar, create) = if cfg!(windows) {
            (r"C:\Windows\System32\tar.exe", "-acf")
        } else {
            ("/usr/bin/tar", "-czf")
        };
        let status = Command::new(tar)
            .arg(create)
            .arg(&archive)
            .arg("-C")
            .arg(&staging)
            .arg(entry)
            .status()
            .unwrap();
        assert!(status.success());
        archive
    }

    /// The release asset served at its URL, with a `SHA256SUMS` listing `digest` for it.
    fn serve(&self, digest: Option<&str>) {
        let archive = self.archive(VERSION);
        let name = tray::asset_name(system(), VERSION, tray::target().unwrap());
        let digest = digest
            .map(str::to_string)
            .unwrap_or_else(|| tray::sha256_file(&archive).unwrap());
        fs::copy(&archive, self.root.join("asset")).unwrap();
        fs::write(self.root.join("SHA256SUMS"), format!("{digest}  {name}\n")).unwrap();
    }

    fn paths(&self) -> tray::Paths {
        tray::paths(system(), &self.home, Some(&self.local))
    }

    fn placed(&self) -> PathBuf {
        self.paths().placed(system()).to_path_buf()
    }

    /// The login entry as the system holds it.
    fn login_entry(&self) -> Option<String> {
        match system() {
            System::MacOs => fs::read_to_string(self.paths().launch_agent.unwrap()).ok(),
            System::Windows => {
                let output = Command::new("reg")
                    .args([
                        "query",
                        &format!(r"HKCU\{}", self.run_key),
                        "/v",
                        tray::RUN_VALUE,
                    ])
                    .output()
                    .unwrap();
                output
                    .status
                    .success()
                    .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
            }
        }
    }

    fn read(&self, name: &str) -> String {
        fs::read_to_string(self.root.join(name)).unwrap_or_default()
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        if cfg!(windows) {
            let _ = Command::new("reg")
                .args(["delete", &format!(r"HKCU\{}", self.run_key), "/f"])
                .output();
        }
    }
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// A build with a digest baked in checks against it, not against the fake `SHA256SUMS`.
fn digest_is_baked() -> bool {
    matches!(tray::expected_digest(), Expected::Baked(_))
}

#[test]
fn installs_from_an_archive_shows_status_and_uninstalls() {
    let tray = Tray::new();
    let archive = tray.archive(VERSION);
    let output = tray.run(&[
        "desktop",
        "install",
        "--yes",
        "--no-login",
        "--from",
        archive.to_str().unwrap(),
    ]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        text(&output).contains("its digest is not checked (--from)"),
        "{}",
        text(&output)
    );
    assert!(tray.paths().binary.is_file(), "{}", text(&output));
    assert!(tray.read("curl.log").is_empty(), "nothing was downloaded");
    assert!(
        tray.read("tar.log")
            .contains(if cfg!(windows) { "-xf" } else { "-xzf" }),
        "{}",
        tray.read("tar.log")
    );
    assert!(tray.login_entry().is_none());
    assert!(archive.is_file(), "the archive is left alone");

    let status = text(&tray.run(&["desktop", "status"]));
    assert!(status.contains("installed in "), "{status}");
    if cfg!(target_os = "macos") {
        assert!(status.contains("(matches this portico)"), "{status}");
    }
    assert!(status.contains("At login     not set up"), "{status}");
    assert!(status.contains("Running      no"), "{status}");

    let output = tray.run(&["desktop", "uninstall", "--yes"]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(!tray.placed().exists(), "{}", text(&output));
    let status = text(&tray.run(&["desktop", "status"]));
    assert!(
        status.contains(&format!("{:<12} not installed", system().app_name())),
        "{status}"
    );
}

#[test]
fn downloads_checks_and_sets_up_login() {
    if digest_is_baked() {
        return;
    }
    let tray = Tray::new();
    tray.serve(None);
    let output = tray.run(&["desktop", "install", "--yes"]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        text(&output).contains("checked against the release's SHA256SUMS"),
        "{}",
        text(&output)
    );
    assert!(tray.read("curl.log").contains("/SHA256SUMS"));
    assert!(tray.paths().binary.is_file());
    let entry = tray.login_entry().expect("a login entry");
    assert!(entry.contains("--portico"), "{entry}");

    let status = text(&tray.run(&["desktop", "status"]));
    assert!(status.contains("At login     set up ("), "{status}");

    let output = tray.run(&["desktop", "uninstall", "--yes"]);
    assert!(output.status.success(), "{}", text(&output));
    assert!(tray.login_entry().is_none());
    assert!(!tray.placed().exists());
}

#[test]
fn refuses_a_digest_mismatch() {
    if digest_is_baked() {
        return;
    }
    let tray = Tray::new();
    tray.serve(Some(&"0".repeat(64)));
    let output = tray.run(&["desktop", "install", "--yes", "--no-login"]);
    assert!(!output.status.success());
    assert!(
        text(&output).contains("does not match"),
        "{}",
        text(&output)
    );
    assert!(!tray.placed().exists());
    assert!(tray.read("tar.log").is_empty(), "nothing was unpacked");
}

#[test]
fn a_missing_release_names_from() {
    let tray = Tray::new();
    fs::write(tray.root.join("404"), "").unwrap();
    let output = tray.run(&["desktop", "install", "--yes"]);
    assert!(!output.status.success());
    assert!(text(&output).contains("--from"), "{}", text(&output));
    assert!(text(&output).contains("404"), "{}", text(&output));
    assert!(!tray.placed().exists());
    assert!(tray.login_entry().is_none());
}

#[test]
fn linux_flags_are_refused_and_desktop_is_listed() {
    let tray = Tray::new();
    let output = tray.run(&["desktop", "install", "--yes", "--omarchy"]);
    assert!(!output.status.success());
    assert!(
        text(&output).contains("--omarchy is only on Linux"),
        "{}",
        text(&output)
    );
    let help = text(&tray.run(&["--help"]));
    assert!(help.contains("desktop"), "{help}");
}
