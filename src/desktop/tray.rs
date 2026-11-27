//! The plan for installing the separate `portico-tray` app on macOS and Windows: which release
//! asset to download, how to check it, where it goes and how it starts at login.
//!
//! Everything here is pure or reads only the paths it is given, so it compiles and is tested on
//! every system; the CLI flow that downloads and installs lives elsewhere.

use std::{
    ffi::OsStr,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

/// The LaunchAgent's label, also the tray app's bundle identifier.
pub const LABEL: &str = "io.github.younesamalou.portico.tray";
/// The value name under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.
pub const RUN_VALUE: &str = "Portico Tray";
/// The self-update log, in the app's support directory (`support_dir`).
pub const UPDATE_LOG: &str = "tray-update.log";
/// The app's name, as its bundle and the previews call it.
pub const PRODUCT: &str = "Portico Tray";
/// The registry key, under `HKEY_CURRENT_USER`, that holds `RUN_VALUE`.
pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

const RELEASES: &str = "https://github.com/YounesAmalou/Portico/releases/download";
const APP_DIR: &str = "Portico Tray.app";
const MAC_BINARY: &str = "Contents/MacOS/portico-tray";
const WIN_BINARY: &str = "portico-tray.exe";

/// A system the tray app is installed on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum System {
    MacOs,
    Windows,
}

impl System {
    /// The system Portico runs on, `None` where there is no tray app (Linux and the rest).
    pub fn current() -> Option<System> {
        if cfg!(target_os = "macos") {
            Some(System::MacOs)
        } else if cfg!(windows) {
            Some(System::Windows)
        } else {
            None
        }
    }

    /// What the system calls the app, for messages.
    pub fn app_name(self) -> &'static str {
        match self {
            System::MacOs => "Menu bar app",
            System::Windows => "Tray app",
        }
    }

    /// `app_name` inside a sentence.
    pub fn app_noun(self) -> &'static str {
        match self {
            System::MacOs => "menu bar app",
            System::Windows => "tray app",
        }
    }

    /// The system's name, for messages.
    pub fn name(self) -> &'static str {
        match self {
            System::MacOs => "macOS",
            System::Windows => "Windows",
        }
    }

    /// What starts the app at login, for messages.
    pub fn login_name(self) -> String {
        match self {
            System::MacOs => format!("LaunchAgent {LABEL}"),
            System::Windows => format!("registry value {RUN_VALUE} (HKCU\\{RUN_KEY})"),
        }
    }
}

/// The release target of this build, `None` where no tray app is released.
pub fn target() -> Option<&'static str> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("aarch64-apple-darwin")
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some("x86_64-apple-darwin")
    } else if cfg!(all(windows, target_arch = "x86_64")) {
        Some("x86_64-pc-windows-msvc")
    } else if cfg!(all(windows, target_arch = "aarch64")) {
        Some("aarch64-pc-windows-msvc")
    } else {
        None
    }
}

/// The release asset's file name: a `tar.gz` on macOS, a `zip` on Windows.
pub fn asset_name(system: System, version: &str, target: &str) -> String {
    let ext = match system {
        System::MacOs => "tar.gz",
        System::Windows => "zip",
    };
    format!("portico-tray-v{version}-{target}.{ext}")
}

/// Where a release's file is downloaded from.
pub fn asset_url(version: &str, name: &str) -> String {
    format!("{RELEASES}/v{version}/{name}")
}

/// Where a release's `SHA256SUMS` file is downloaded from.
pub fn sums_url(version: &str) -> String {
    asset_url(version, "SHA256SUMS")
}

/// Where the digest to check a download against comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expected {
    /// Baked into this build at release time.
    Baked(String),
    /// Not baked in: read it from the release's `SHA256SUMS`.
    Sums,
}

/// The digest baked in by `PORTICO_TRAY_SHA256` at build time (lowercased), else `Sums`.
pub fn expected_digest() -> Expected {
    match option_env!("PORTICO_TRAY_SHA256") {
        Some(digest) if !digest.trim().is_empty() => {
            Expected::Baked(digest.trim().to_ascii_lowercase())
        }
        _ => Expected::Sums,
    }
}

/// The lowercase digest `sums` lists for `name`, in the `<hex>  <name>` or `<hex> *<name>` form.
pub fn sums_digest(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (digest, file) = line.trim().split_once(char::is_whitespace)?;
        let file = file.trim_start();
        let file = file.strip_prefix('*').unwrap_or(file);
        (file == name).then(|| digest.to_ascii_lowercase())
    })
}

/// The lowercase hex SHA-256 of the file at `path`.
pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Where the tray app is installed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    /// The app: the bundle on macOS, its directory on Windows.
    pub app: PathBuf,
    /// The app's executable.
    pub binary: PathBuf,
    /// The LaunchAgent's plist (macOS only; Windows uses a registry value).
    pub launch_agent: Option<PathBuf>,
}

/// The install locations: `~/Applications/Portico Tray.app` on macOS,
/// `%LOCALAPPDATA%\Programs\Portico` on Windows (`<home>/AppData/Local` when it is unset).
pub fn paths(system: System, home: &Path, local_app_data: Option<&Path>) -> Paths {
    match system {
        System::MacOs => {
            let app = home.join("Applications").join(APP_DIR);
            Paths {
                binary: app.join(MAC_BINARY),
                app,
                launch_agent: Some(
                    home.join("Library/LaunchAgents")
                        .join(format!("{LABEL}.plist")),
                ),
            }
        }
        System::Windows => {
            let local = local_app_data
                .map(Path::to_path_buf)
                .unwrap_or_else(|| home.join("AppData").join("Local"));
            let app = local.join("Programs").join("Portico");
            Paths {
                binary: app.join(WIN_BINARY),
                app,
                launch_agent: None,
            }
        }
    }
}

impl Paths {
    /// What install puts in place and uninstall removes: the bundle on macOS, the executable
    /// on Windows (whose directory may hold nothing else of Portico's).
    pub fn placed(&self, system: System) -> &Path {
        match system {
            System::MacOs => &self.app,
            System::Windows => &self.binary,
        }
    }
}

/// Where the app keeps its single-instance lock and the self-update log, as the app computes
/// it: `~/Library/Application Support/Portico` on macOS, `%LOCALAPPDATA%\Portico` on Windows
/// (`<home>/AppData/Local` when it is unset).
pub fn support_dir(system: System, home: &Path, local_app_data: Option<&Path>) -> PathBuf {
    match system {
        System::MacOs => home.join("Library/Application Support/Portico"),
        System::Windows => local_app_data
            .map(Path::to_path_buf)
            .unwrap_or_else(|| home.join("AppData").join("Local"))
            .join("Portico"),
    }
}

/// The login entry's text: the LaunchAgent's plist on macOS, the `Run` value on Windows.
pub fn login_entry(system: System, binary: &Path, portico: &Path) -> String {
    match system {
        System::MacOs => launch_agent_plist(binary, portico),
        System::Windows => run_value(binary, portico),
    }
}

/// The `portico` a login entry (`login_entry`'s text, or one like it) passes the app with
/// `--portico`, `None` when the entry is not in that form.
pub fn entry_portico(system: System, entry: &str) -> Option<PathBuf> {
    let path = match system {
        System::MacOs => {
            let after = entry.split_once("<string>--portico</string>")?.1;
            let value = after.trim_start().strip_prefix("<string>")?;
            xml_unescape(value.split_once("</string>")?.0)
        }
        System::Windows => entry
            .split_once("--portico \"")?
            .1
            .split_once('"')?
            .0
            .to_string(),
    };
    (!path.is_empty()).then(|| PathBuf::from(path))
}

fn xml_unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// What installing, starting and stopping the app does on the system: run `curl` and `tar`,
/// read the app's version, write the login entry, and find, stop and start the app. The real
/// one is `SystemHost`; the CLI's tests replace it.
pub trait TrayHost {
    /// Downloads `url` to the file `to`.
    fn download(&self, url: &str, to: &Path) -> Result<(), String>;
    /// Unpacks the release archive `archive` into the directory `into`.
    fn unpack(&self, archive: &Path, into: &Path) -> Result<(), String>;
    /// The version of the app at `app` (`Paths::placed`, or the same in an unpacked archive).
    fn app_version(&self, app: &Path) -> Option<String>;
    /// The login entry's text (`login_entry`), when there is one.
    fn login_entry(&self) -> Option<String>;
    /// Writes the login entry that starts `binary` with `--portico <portico>`.
    fn set_login_entry(&self, binary: &Path, portico: &Path) -> Result<(), String>;
    /// Removes the login entry; no entry is not an error.
    fn remove_login_entry(&self) -> Result<(), String>;
    /// The process IDs of this user's running apps.
    fn running(&self) -> Vec<u32>;
    /// Stops the running app. `during_update`: the app is quitting on its own, so wait for it
    /// first, and never unload its LaunchAgent.
    fn stop(&self, during_update: bool) -> Result<(), String>;
    /// Starts `binary` with `--portico <portico>`, through its login entry when `via_login`.
    fn start(&self, binary: &Path, portico: &Path, via_login: bool) -> Result<(), String>;
}

mod host;
pub use host::SystemHost;

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The LaunchAgent that starts the app at login, passing it the `portico` to run.
pub fn launch_agent_plist(binary: &Path, portico: &Path) -> String {
    let binary = xml_escape(&binary.to_string_lossy());
    let portico = xml_escape(&portico.to_string_lossy());
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n\
<dict>\n\
\t<key>Label</key>\n\
\t<string>{LABEL}</string>\n\
\t<key>ProgramArguments</key>\n\
\t<array>\n\
\t\t<string>{binary}</string>\n\
\t\t<string>--portico</string>\n\
\t\t<string>{portico}</string>\n\
\t</array>\n\
\t<key>RunAtLoad</key>\n\
\t<true/>\n\
\t<key>AbandonProcessGroup</key>\n\
\t<true/>\n\
\t<key>LimitLoadToSessionType</key>\n\
\t<string>Aqua</string>\n\
</dict>\n\
</plist>\n"
    )
}

/// The registry `Run` value: `"<binary>" --portico "<portico>"`. Each path is wrapped in double
/// quotes and not escaped further, since a Windows path cannot contain `"`.
pub fn run_value(binary: &Path, portico: &Path) -> String {
    format!(
        "\"{}\" --portico \"{}\"",
        binary.display(),
        portico.display()
    )
}

/// Checks an extracted archive and returns the app's path. The archive must hold exactly one
/// top-level entry: `Portico Tray.app` (a directory with `Contents/MacOS/portico-tray`) on
/// macOS or `portico-tray.exe` (a file) on Windows, and no symlink anywhere in it.
pub fn check_layout(dir: &Path, system: System) -> Result<PathBuf, String> {
    let (entry_name, binary) = match system {
        System::MacOs => (APP_DIR, PathBuf::from(APP_DIR).join(MAC_BINARY)),
        System::Windows => (WIN_BINARY, PathBuf::from(WIN_BINARY)),
    };
    let unreadable = |path: &Path, e: io::Error| format!("cannot read {}: {e}", path.display());
    let mut names = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| unreadable(dir, e))? {
        names.push(entry.map_err(|e| unreadable(dir, e))?.file_name());
    }
    if names.len() != 1 || names[0] != OsStr::new(entry_name) {
        return Err(format!("the archive must hold only {entry_name}"));
    }
    let entry = dir.join(entry_name);
    no_symlinks(&entry)?;
    let is_dir = fs::symlink_metadata(&entry)
        .map_err(|e| unreadable(&entry, e))?
        .is_dir();
    if is_dir != (system == System::MacOs) {
        return Err(format!("{entry_name} has the wrong type"));
    }
    match fs::symlink_metadata(dir.join(&binary)) {
        Ok(meta) if meta.is_file() => Ok(entry),
        _ => Err(format!("{} is missing", binary.display())),
    }
}

/// Fails on a symlink or a special file (FIFO, device) at `path` or anywhere below it. Never
/// follows a link.
fn no_symlinks(path: &Path) -> Result<(), String> {
    let meta =
        fs::symlink_metadata(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if meta.file_type().is_symlink() {
        return Err(format!("{} is a symlink", path.display()));
    }
    if meta.is_dir() {
        let entries =
            fs::read_dir(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            no_symlinks(&entry.path())?;
        }
    } else if !meta.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    Ok(())
}

/// The path to give the app for running this `portico`: the first `portico` on `path_var` that
/// resolves to the same file as `exe` (so a package manager's stable link survives upgrades),
/// else `exe` itself.
pub fn stable_portico(exe: &Path, path_var: Option<&OsStr>) -> PathBuf {
    let name = if cfg!(windows) {
        "portico.exe"
    } else {
        "portico"
    };
    let Some(real) = fs::canonicalize(exe).ok() else {
        return exe.to_path_buf();
    };
    path_var
        .into_iter()
        .flat_map(std::env::split_paths)
        // An empty or relative entry would resolve against the working directory and put a
        // relative path in the login entry.
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|candidate| fs::canonicalize(candidate).is_ok_and(|c| c == real))
        .unwrap_or_else(|| exe.to_path_buf())
}

/// `CFBundleShortVersionString` from a bundle's `Info.plist` text: the `<string>` that follows
/// the key (only whitespace may sit between them). Reads text plists only, not binary ones.
pub fn plist_version(info_plist: &str) -> Option<String> {
    let after = info_plist
        .split_once("<key>CFBundleShortVersionString</key>")?
        .1;
    let value = after.trim_start().strip_prefix("<string>")?;
    Some(value.split_once("</string>")?.0.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::fs::symlink;
    use std::path::Path;

    #[test]
    fn asset_names_per_system() {
        assert_eq!(
            asset_name(System::MacOs, "0.8.0", "aarch64-apple-darwin"),
            "portico-tray-v0.8.0-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            asset_name(System::Windows, "0.8.0", "x86_64-pc-windows-msvc"),
            "portico-tray-v0.8.0-x86_64-pc-windows-msvc.zip"
        );
    }

    #[test]
    fn asset_url_points_at_the_release() {
        assert_eq!(
            asset_url("0.8.0", "portico-tray-v0.8.0-x.zip"),
            "https://github.com/YounesAmalou/Portico/releases/download/v0.8.0/portico-tray-v0.8.0-x.zip"
        );
        assert_eq!(
            sums_url("0.8.0"),
            "https://github.com/YounesAmalou/Portico/releases/download/v0.8.0/SHA256SUMS"
        );
    }

    #[test]
    fn sums_digest_both_forms() {
        let sums = "aaaa  other.zip\nBBBB  a.zip\ncccc *b.zip\n";
        assert_eq!(sums_digest(sums, "a.zip").as_deref(), Some("bbbb"));
        assert_eq!(sums_digest(sums, "b.zip").as_deref(), Some("cccc"));
    }

    #[test]
    fn sums_digest_ignores_look_alike_names_and_reads_crlf() {
        let sums = "aaaa  xa.zip\nbbbb  a.zip.sig\ncccc  a.zip\r\n";
        assert_eq!(sums_digest(sums, "a.zip").as_deref(), Some("cccc"));
        assert_eq!(
            sums_digest("aaaa  xa.zip\nbbbb  a.zip.sig\n", "a.zip"),
            None
        );
    }

    #[test]
    fn sums_digest_missing_line() {
        assert_eq!(sums_digest("aaaa  other.zip\n", "a.zip"), None);
        assert_eq!(sums_digest("", "a.zip"), None);
    }

    #[test]
    fn sha256_known_vector() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        std::fs::write(&file, "abc").unwrap();
        assert_eq!(
            sha256_file(&file).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn plist_has_label_arguments_and_abandon() {
        let plist = launch_agent_plist(Path::new("/a/portico-tray"), Path::new("/b/portico"));
        assert!(plist.contains("<string>io.github.younesamalou.portico.tray</string>"));
        assert!(plist.contains(
            "<array>\n\t\t<string>/a/portico-tray</string>\n\t\t<string>--portico</string>\n\t\t<string>/b/portico</string>\n\t</array>"
        ));
        assert!(plist.contains("<key>RunAtLoad</key>\n\t<true/>"));
        assert!(plist.contains("<key>AbandonProcessGroup</key>\n\t<true/>"));
        assert!(plist.contains("<key>LimitLoadToSessionType</key>\n\t<string>Aqua</string>"));
    }

    #[test]
    fn plist_escapes_xml() {
        let plist = launch_agent_plist(Path::new("/a&b/tray"), Path::new("/c<d>/portico"));
        assert!(plist.contains("/a&amp;b/tray"));
        assert!(plist.contains("/c&lt;d&gt;/portico"));
    }

    #[test]
    fn run_value_quotes_paths_with_spaces() {
        assert_eq!(
            run_value(
                Path::new(r"C:\Users\A B\portico-tray.exe"),
                Path::new(r"C:\Program Files\portico.exe")
            ),
            r#""C:\Users\A B\portico-tray.exe" --portico "C:\Program Files\portico.exe""#
        );
    }

    fn make_app(dir: &Path) {
        let macos = dir.join("Portico Tray.app/Contents/MacOS");
        std::fs::create_dir_all(&macos).unwrap();
        std::fs::write(macos.join("portico-tray"), "x").unwrap();
    }

    #[test]
    fn layout_accepts_app_bundle() {
        let dir = tempfile::tempdir().unwrap();
        make_app(dir.path());
        assert_eq!(
            check_layout(dir.path(), System::MacOs).unwrap(),
            dir.path().join("Portico Tray.app")
        );
    }

    #[test]
    fn layout_accepts_exe() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("portico-tray.exe"), "x").unwrap();
        assert_eq!(
            check_layout(dir.path(), System::Windows).unwrap(),
            dir.path().join("portico-tray.exe")
        );
    }

    #[test]
    fn layout_rejects_extra_entry() {
        let dir = tempfile::tempdir().unwrap();
        make_app(dir.path());
        std::fs::write(dir.path().join("extra"), "x").unwrap();
        assert!(check_layout(dir.path(), System::MacOs).is_err());
    }

    #[test]
    fn layout_rejects_symlink() {
        let dir = tempfile::tempdir().unwrap();
        make_app(dir.path());
        symlink(
            Path::new("/etc/passwd"),
            &dir.path().join("Portico Tray.app/Contents/link"),
        )
        .unwrap();
        assert!(check_layout(dir.path(), System::MacOs).is_err());
    }

    #[test]
    fn layout_rejects_missing_binary() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("Portico Tray.app/Contents/MacOS")).unwrap();
        assert!(check_layout(dir.path(), System::MacOs).is_err());
        let empty = tempfile::tempdir().unwrap();
        assert!(check_layout(empty.path(), System::Windows).is_err());
    }

    #[test]
    fn stable_portico_prefers_path_link() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("cellar/portico");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, "x").unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        // The name `stable_portico` looks for on this system.
        let name = if cfg!(windows) {
            "portico.exe"
        } else {
            "portico"
        };
        symlink(&real, &bin.join(name)).unwrap();
        let path = std::env::join_paths([&bin]).unwrap();
        assert_eq!(stable_portico(&real, Some(&path)), bin.join(name));
    }

    #[cfg(unix)]
    #[test]
    fn stable_portico_skips_empty_and_relative_path_entries() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("portico");
        std::fs::write(&real, "x").unwrap();
        // A relative entry that reaches the exe from the working directory, without changing it.
        let cwd = std::env::current_dir().unwrap();
        let mut rel = PathBuf::new();
        for _ in cwd.components().skip(1) {
            rel.push("..");
        }
        rel.push(dir.path().strip_prefix("/").unwrap());
        let path = std::env::join_paths([Path::new(""), rel.as_path()]).unwrap();
        let result = stable_portico(&real, Some(&path));
        assert!(result.is_absolute());
        assert_eq!(result, real);
    }

    #[cfg(unix)]
    #[test]
    fn layout_rejects_special_file() {
        let dir = tempfile::tempdir().unwrap();
        make_app(dir.path());
        let fifo = dir.path().join("Portico Tray.app/Contents/fifo");
        let ok = std::process::Command::new("mkfifo").arg(&fifo).status();
        if ok.is_ok_and(|s| s.success()) {
            assert!(check_layout(dir.path(), System::MacOs).is_err());
        }
    }

    #[test]
    fn stable_portico_falls_back_to_exe() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("portico");
        std::fs::write(&real, "x").unwrap();
        let other = tempfile::tempdir().unwrap();
        std::fs::write(other.path().join("portico"), "y").unwrap();
        let path = std::env::join_paths([other.path()]).unwrap();
        assert_eq!(stable_portico(&real, Some(&path)), real);
        assert_eq!(stable_portico(&real, None), real);
    }

    #[test]
    fn paths_per_system() {
        let home = Path::new("/h");
        let mac = paths(System::MacOs, home, None);
        assert_eq!(mac.app, Path::new("/h/Applications/Portico Tray.app"));
        assert_eq!(
            mac.binary,
            Path::new("/h/Applications/Portico Tray.app/Contents/MacOS/portico-tray")
        );
        assert_eq!(
            mac.launch_agent.unwrap(),
            Path::new("/h/Library/LaunchAgents/io.github.younesamalou.portico.tray.plist")
        );
        let win = paths(System::Windows, home, Some(Path::new("/lad")));
        assert_eq!(win.app, Path::new("/lad/Programs/Portico"));
        assert_eq!(
            win.binary,
            Path::new("/lad/Programs/Portico/portico-tray.exe")
        );
        assert!(win.launch_agent.is_none());
        let fallback = paths(System::Windows, home, None);
        assert_eq!(fallback.app, Path::new("/h/AppData/Local/Programs/Portico"));
    }

    #[test]
    fn entry_portico_reads_both_forms() {
        let plist = launch_agent_plist(Path::new("/a/tray"), Path::new("/b&c/portico"));
        assert_eq!(
            entry_portico(System::MacOs, &plist).unwrap(),
            Path::new("/b&c/portico")
        );
        let value = run_value(
            Path::new(r"C:\A B\portico-tray.exe"),
            Path::new(r"C:\P Q\portico.exe"),
        );
        assert_eq!(
            entry_portico(System::Windows, &value).unwrap(),
            Path::new(r"C:\P Q\portico.exe")
        );
        assert_eq!(entry_portico(System::Windows, "other.exe"), None);
        assert_eq!(entry_portico(System::MacOs, "<plist/>"), None);
    }

    #[test]
    fn support_dir_per_system() {
        let home = Path::new("/h");
        assert_eq!(
            support_dir(System::MacOs, home, None),
            Path::new("/h/Library/Application Support/Portico")
        );
        assert_eq!(
            support_dir(System::Windows, home, Some(Path::new("/lad"))),
            Path::new("/lad/Portico")
        );
        assert_eq!(
            support_dir(System::Windows, home, None),
            Path::new("/h/AppData/Local/Portico")
        );
    }

    #[test]
    fn plist_version_reads_short_version() {
        let text = "<key>CFBundleVersion</key>\n<string>9</string>\n<key>CFBundleShortVersionString</key>\n\t<string>0.8.0</string>";
        assert_eq!(plist_version(text).as_deref(), Some("0.8.0"));
        assert_eq!(plist_version("<key>Other</key><string>1</string>"), None);
    }
}
