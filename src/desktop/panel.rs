//! The Quickshell panel, embedded in the binary and written to Portico's own directory.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

macro_rules! panel_files {
    ($($name:literal),* $(,)?) => {
        /// Every file of `desktop/quickshell/` except its tests and preview image.
        pub const FILES: &[(&str, &[u8])] = &[
            $(($name, include_bytes!(concat!("../../desktop/quickshell/", $name)))),*
        ];
    };
}

panel_files!(
    "manifest.json",
    "OmarchyBarWidget.qml",
    "OmarchyPanel.qml",
    "portico.js",
    "PorticoModel.qml",
    "PorticoPopup.qml",
    "PorticoRun.qml",
    "PorticoView.qml",
    "README.md",
    "shell.qml",
);

/// The stamp file in the panel directory.
pub const VERSION_FILE: &str = "VERSION";

/// This binary's stamp: its version and a hash of the embedded files, `0.6.0+0123456789abcdef`.
pub fn stamp() -> String {
    // FNV-1a, 64 bits: stable across builds and platforms, unlike std's hasher.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for (name, bytes) in FILES {
        for byte in name
            .bytes()
            .chain([0])
            .chain(bytes.iter().copied())
            .chain([0])
        {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    format!("{}+{hash:016x}", env!("CARGO_PKG_VERSION"))
}

/// The version part of a stamp.
pub fn stamp_version(stamp: &str) -> &str {
    stamp.trim().split('+').next().unwrap_or_default()
}

/// The panel directory as this binary sees it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum State {
    /// A link or a file where the directory belongs: not Portico's, never written or removed.
    Blocked,
    /// No stamp: no panel installed.
    Missing,
    /// Written by this binary, every file intact.
    Current,
    /// Written by another build: the version in its stamp.
    Stale(String),
    /// This binary's stamp, but a file is missing or differs.
    Modified,
}

pub fn state(dir: &Path) -> State {
    if is_blocked(dir) {
        return State::Blocked;
    }
    let Ok(written) = fs::read_to_string(dir.join(VERSION_FILE)) else {
        return State::Missing;
    };
    if written.trim() != stamp() {
        return State::Stale(stamp_version(&written).to_string());
    }
    if FILES
        .iter()
        .all(|(name, bytes)| fs::read(dir.join(name)).is_ok_and(|found| found == *bytes))
    {
        State::Current
    } else {
        State::Modified
    }
}

/// Writes the panel into `dir`: every file atomically, files not embedded removed, the stamp
/// last. Two writers at once write the same bytes, so either result is the same.
pub fn write(dir: &Path) -> io::Result<()> {
    if is_blocked(dir) {
        return Err(io::Error::other(
            "it is a link or a file, not Portico's directory",
        ));
    }
    fs::create_dir_all(dir)?;
    for (name, bytes) in FILES {
        write_atomically(&dir.join(name), bytes)?;
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let kept = name == VERSION_FILE || FILES.iter().any(|(file, _)| *file == name);
        // Another writer's temporary file is its own to rename.
        if !kept && !name.contains(".tmp-") && !entry.file_type()?.is_dir() {
            fs::remove_file(entry.path())?;
        }
    }
    write_atomically(&dir.join(VERSION_FILE), format!("{}\n", stamp()).as_bytes())
}

/// Rewrites the panel when it was written by an older Portico, or by another build of this
/// version. Runs before every command except `desktop`, and never from a development build
/// (asked only once a refresh is due): with no panel it costs one failed open, and it never reports anything, so a
/// command's output and status stay its own. A panel from a newer Portico is left alone, so two
/// installed binaries never take turns rewriting it; `portico desktop status` and the panel's
/// banner report a stamp that differs.
/// Whether it rewrote the panel.
pub fn refresh(dir: &Path, is_development_build: impl FnOnce() -> bool) -> bool {
    if let Ok(written) = fs::read_to_string(dir.join(VERSION_FILE))
        && should_refresh(written.trim())
        && !is_blocked(dir)
        && !is_development_build()
    {
        return write(dir).is_ok();
    }
    false
}

/// Whether a panel stamped `written` is behind this binary.
fn should_refresh(written: &str) -> bool {
    let ours = stamp();
    match compare_versions(env!("CARGO_PKG_VERSION"), stamp_version(written)) {
        Some(std::cmp::Ordering::Greater) => true,
        Some(std::cmp::Ordering::Equal) => written != ours,
        _ => false,
    }
}

/// `a` against `b` as semantic versions (`1.2.3`, `1.2.3-rc.1`); `None` when either does not
/// parse. A pre-release sorts before its release; two pre-releases compare as text.
pub fn compare_versions(a: &str, b: &str) -> Option<std::cmp::Ordering> {
    type Parsed<'a> = ((u64, u64, u64), Option<&'a str>);
    fn parse(version: &str) -> Option<Parsed<'_>> {
        let (core, pre) = match version.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (version, None),
        };
        let mut parts = core.split('.').map(|part| part.parse::<u64>().ok());
        let triple = (parts.next()??, parts.next()??, parts.next()??);
        parts.next().is_none().then_some((triple, pre))
    }
    let (a, a_pre) = parse(a)?;
    let (b, b_pre) = parse(b)?;
    Some(a.cmp(&b).then_with(|| match (a_pre, b_pre) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(a), Some(b)) => a.cmp(b),
    }))
}

/// Whether something other than a real directory is at `dir`. Following a link there would
/// write into, and remove files from, a directory that is not Portico's.
pub fn is_blocked(dir: &Path) -> bool {
    fs::symlink_metadata(dir).is_ok_and(|meta| !meta.is_dir())
}

fn write_atomically(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut temporary = PathBuf::from(path);
    temporary
        .as_mut_os_string()
        .push(format!(".tmp-{}", std::process::id()));
    let result = fs::write(&temporary, bytes).and_then(|()| fs::rename(&temporary, path));
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn embeds_every_panel_file_except_tests_and_the_preview() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("desktop/quickshell");
        let on_disk: BTreeSet<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name != "tests" && name != "preview.png")
            .collect();
        let embedded: BTreeSet<String> = FILES.iter().map(|(name, _)| name.to_string()).collect();
        assert_eq!(embedded, on_disk);
    }

    #[test]
    fn stamp_carries_the_version_and_a_hash() {
        let stamp = stamp();
        assert_eq!(stamp_version(&stamp), env!("CARGO_PKG_VERSION"));
        assert_eq!(stamp.split('+').nth(1).unwrap().len(), 16);
        assert_eq!(stamp_version("0.5.1+abc\n"), "0.5.1");
    }

    #[test]
    fn writes_refreshes_and_reports_the_panel() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("panel");
        assert_eq!(state(&dir), State::Missing);
        refresh(&dir, || false);
        assert!(!dir.exists(), "no panel installed, nothing written");

        write(&dir).unwrap();
        assert_eq!(state(&dir), State::Current);

        fs::write(dir.join("shell.qml"), "changed").unwrap();
        assert_eq!(state(&dir), State::Modified);
        refresh(&dir, || false);
        assert_eq!(
            state(&dir),
            State::Modified,
            "a matching stamp is left alone"
        );

        fs::write(dir.join(VERSION_FILE), "0.0.1+0000000000000000\n").unwrap();
        fs::write(dir.join("Old.qml"), "gone").unwrap();
        assert_eq!(state(&dir), State::Stale("0.0.1".into()));
        refresh(&dir, || false);
        assert_eq!(state(&dir), State::Current);
        assert!(!dir.join("Old.qml").exists());
    }

    #[test]
    fn refreshes_only_a_panel_from_an_older_build() {
        use std::cmp::Ordering::*;
        assert_eq!(compare_versions("0.6.0", "0.5.9"), Some(Greater));
        assert_eq!(compare_versions("0.10.0", "0.9.0"), Some(Greater));
        assert_eq!(compare_versions("0.6.0", "0.6.0-rc.1"), Some(Greater));
        assert_eq!(compare_versions("0.6.0-rc.1", "0.6.0-rc.2"), Some(Less));
        assert_eq!(compare_versions("1.0.0", "1.0.0"), Some(Equal));
        assert_eq!(compare_versions("1.0", "1.0.0"), None);

        let ours = env!("CARGO_PKG_VERSION");
        assert!(should_refresh("0.0.1+0000000000000000"));
        assert!(should_refresh(&format!("{ours}+0000000000000000")));
        assert!(!should_refresh(&stamp()));
        assert!(
            !should_refresh("999.0.0+0000000000000000"),
            "a newer panel is left alone"
        );
        assert!(!should_refresh("garbage"));

        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("panel");
        write(&dir).unwrap();
        fs::write(dir.join(VERSION_FILE), "999.0.0+0000000000000000\n").unwrap();
        refresh(&dir, || false);
        assert_eq!(state(&dir), State::Stale("999.0.0".into()));
    }

    #[test]
    fn never_follows_a_link_at_the_panel_directory() {
        let home = tempfile::tempdir().unwrap();
        let checkout = home.path().join("checkout");
        fs::create_dir(&checkout).unwrap();
        fs::write(checkout.join("preview.png"), "image").unwrap();
        fs::write(checkout.join(VERSION_FILE), "0.0.1+0000000000000000\n").unwrap();
        let dir = home.path().join("panel");
        std::os::unix::fs::symlink(&checkout, &dir).unwrap();
        assert_eq!(state(&dir), State::Blocked);
        refresh(&dir, || false);
        assert!(write(&dir).is_err());
        assert!(checkout.join("preview.png").exists());
        assert!(!checkout.join("shell.qml").exists());
    }

    #[test]
    fn a_failed_refresh_leaves_the_old_stamp() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("panel");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join(VERSION_FILE), "0.0.1+0000000000000000\n").unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
        refresh(&dir, || false);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(state(&dir), State::Stale("0.0.1".into()));
    }
}
