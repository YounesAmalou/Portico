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
/// The self-update log, in Portico's runtime directory.
pub const UPDATE_LOG: &str = "tray-update.log";

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

/// Fails on a symlink at `path` or anywhere below it. Never follows a link.
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
        .map(|dir| dir.join(name))
        .find(|candidate| fs::canonicalize(candidate).is_ok_and(|c| c == real))
        .unwrap_or_else(|| exe.to_path_buf())
}

/// `CFBundleShortVersionString` from a bundle's `Info.plist` text: the `<string>` that follows
/// the key.
pub fn plist_version(info_plist: &str) -> Option<String> {
    let after = info_plist
        .split_once("<key>CFBundleShortVersionString</key>")?
        .1;
    let value = after.split_once("<string>")?.1;
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
        symlink(&real, &bin.join("portico")).unwrap();
        let path = std::env::join_paths([&bin]).unwrap();
        assert_eq!(stable_portico(&real, Some(&path)), bin.join("portico"));
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
    fn plist_version_reads_short_version() {
        let text = "<key>CFBundleVersion</key>\n<string>9</string>\n<key>CFBundleShortVersionString</key>\n\t<string>0.8.0</string>";
        assert_eq!(plist_version(text).as_deref(), Some("0.8.0"));
        assert_eq!(plist_version("<key>Other</key><string>1</string>"), None);
    }
}
