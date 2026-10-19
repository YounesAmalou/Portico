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

/// Rewrites the panel when its stamp is another build's. Runs before every command except
/// `desktop`: with no panel it costs one failed open, and it never reports anything, so a
/// command's output and status stay its own; `portico desktop status` and the panel's banner
/// report a stamp that stayed old.
pub fn refresh(dir: &Path) {
    if let Ok(written) = fs::read_to_string(dir.join(VERSION_FILE))
        && written.trim() != stamp()
    {
        let _ = write(dir);
    }
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
        refresh(&dir);
        assert!(!dir.exists(), "no panel installed, nothing written");

        write(&dir).unwrap();
        assert_eq!(state(&dir), State::Current);

        fs::write(dir.join("shell.qml"), "changed").unwrap();
        assert_eq!(state(&dir), State::Modified);
        refresh(&dir);
        assert_eq!(
            state(&dir),
            State::Modified,
            "a matching stamp is left alone"
        );

        fs::write(dir.join(VERSION_FILE), "0.0.1+0000000000000000\n").unwrap();
        fs::write(dir.join("Old.qml"), "gone").unwrap();
        assert_eq!(state(&dir), State::Stale("0.0.1".into()));
        refresh(&dir);
        assert_eq!(state(&dir), State::Current);
        assert!(!dir.join("Old.qml").exists());
    }

    #[test]
    fn a_failed_refresh_leaves_the_old_stamp() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("panel");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join(VERSION_FILE), "0.0.1+0000000000000000\n").unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
        refresh(&dir);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(state(&dir), State::Stale("0.0.1".into()));
    }
}
