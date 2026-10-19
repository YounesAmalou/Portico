//! The launcher entry: a freedesktop `.desktop` file and an SVG icon.

use std::{
    fs,
    path::{Path, PathBuf},
};

use super::DesktopEnv;

/// The entry and icon packages install, committed in `packaging/desktop/`.
pub const DESKTOP: &str = include_str!("../../packaging/desktop/portico.desktop");
pub const ICON: &str = include_str!("../../packaging/desktop/portico.svg");

/// The comment that marks the user copies Portico wrote, so `uninstall` removes only those.
pub const MARKER: &str =
    "Installed by portico desktop install; portico desktop uninstall removes it.";

/// The user's `.desktop` file and icon.
pub fn paths(env: &DesktopEnv) -> (PathBuf, PathBuf) {
    (
        env.data_home.join("applications/portico.desktop"),
        env.data_home
            .join("icons/hicolor/scalable/apps/portico.svg"),
    )
}

/// The `.desktop` file Portico writes for the user: marked, and running `exec`.
pub fn desktop_file(exec: &Path) -> String {
    let mut out = format!("# {MARKER}\n");
    for line in DESKTOP.lines() {
        if line.starts_with("Exec=") {
            out.push_str(&format!("Exec={}\n", quote_exec(&exec.to_string_lossy())));
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// The icon Portico writes for the user: marked after its XML declaration.
pub fn icon_file() -> String {
    let comment = format!("<!-- {MARKER} -->\n");
    match ICON.split_once('\n') {
        Some((first, rest)) if first.starts_with("<?xml") => format!("{first}\n{comment}{rest}"),
        _ => format!("{comment}{ICON}"),
    }
}

/// Whether a file at a launcher path is one Portico wrote.
pub fn is_marked(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
        && fs::read_to_string(path)
            .is_ok_and(|text| text.lines().take(3).any(|line| line.contains(MARKER)))
}

/// The program a marked `.desktop` file runs.
pub fn exec_of(path: &Path) -> Option<String> {
    exec_of_text(&fs::read_to_string(path).ok()?)
}

/// The program a `.desktop` file's text runs.
pub fn exec_of_text(text: &str) -> Option<String> {
    let value = text.lines().find_map(|line| line.strip_prefix("Exec="))?;
    Some(unquote_exec(value))
}

/// A system-wide entry, from a package, in one of `XDG_DATA_DIRS`.
pub fn system_entry(env: &DesktopEnv) -> Option<PathBuf> {
    env.data_dirs
        .iter()
        .map(|dir| dir.join("applications/portico.desktop"))
        .find(|path| path.is_file())
}

/// A path as one argument of a `.desktop` `Exec` key: quoted when it holds a reserved character.
fn quote_exec(path: &str) -> String {
    let reserved = |c: char| " \t\n\"'\\><~|&;$*?#()`=%".contains(c);
    if !path.chars().any(reserved) {
        return path.into();
    }
    let mut out = String::from("\"");
    for c in path.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            out.push('\\');
        }
        if c == '%' {
            out.push('%');
        }
        out.push(c);
    }
    out.push('"');
    // The key's value is itself an escaped string, so each backslash doubles again.
    out.replace('\\', "\\\\")
}

fn unquote_exec(value: &str) -> String {
    let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) else {
        return value.into();
    };
    let inner = inner.replace("\\\\", "\\").replace("%%", "%");
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            out.extend(chars.next());
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embeds_the_committed_files() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("packaging/desktop");
        assert_eq!(
            DESKTOP,
            fs::read_to_string(dir.join("portico.desktop")).unwrap()
        );
        assert_eq!(ICON, fs::read_to_string(dir.join("portico.svg")).unwrap());
        for key in [
            "Name=Portico",
            "Exec=portico",
            "Icon=portico",
            "Terminal=true",
            "Categories=Network;System;",
        ] {
            assert!(DESKTOP.lines().any(|line| line == key), "{key}");
        }
    }

    #[test]
    fn marks_the_user_copies_and_runs_this_binary() {
        let file = desktop_file(Path::new("/home/me/.cargo/bin/portico"));
        assert!(file.starts_with(&format!("# {MARKER}\n[Desktop Entry]\n")));
        assert!(file.contains("\nExec=/home/me/.cargo/bin/portico\n"));
        assert!(!file.contains("Exec=portico"));
        let icon = icon_file();
        assert!(icon.starts_with("<?xml"));
        assert_eq!(icon.lines().nth(1).unwrap(), format!("<!-- {MARKER} -->"));
    }

    #[test]
    fn quotes_an_exec_path_with_reserved_characters() {
        for path in ["/opt/my apps/portico", "/a/$x\\y/portico", "/plain/portico"] {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("portico.desktop");
            fs::write(&file, desktop_file(Path::new(path))).unwrap();
            assert_eq!(exec_of(&file).as_deref(), Some(path));
            assert!(is_marked(&file));
        }
        assert_eq!(
            quote_exec("/opt/my apps/portico"),
            "\"/opt/my apps/portico\""
        );
    }
}
