//! Paths every system shares: `~` display and expansion, the configuration directory and the
//! spelling of paths written into OpenSSH files.
//!
//! Paths shown to people use the system's separator (`\` on Windows); paths written into
//! OpenSSH files use `/`, which Windows' OpenSSH reads too.

use std::{
    ffi::OsStr,
    path::{MAIN_SEPARATOR, MAIN_SEPARATOR_STR, Path, PathBuf, is_separator},
};

/// `$XDG_CONFIG_HOME/portico` when `xdg_config_home` is an absolute path, otherwise the
/// system's default: `~/.config/portico` on Unix, `%APPDATA%\portico` on Windows.
pub fn config_dir(home: &Path, xdg_config_home: Option<&OsStr>) -> PathBuf {
    match xdg_config_home
        .map(Path::new)
        .filter(|path| path.is_absolute())
    {
        Some(path) => path.join("portico"),
        None => default_config_dir(home),
    }
}

#[cfg(unix)]
fn default_config_dir(home: &Path) -> PathBuf {
    home.join(".config/portico")
}

#[cfg(windows)]
use super::super::windows::paths::default_config_dir;

/// `path` with a leading `home` shown as `~`.
pub fn tilde(path: &Path, home: &Path) -> String {
    if home.as_os_str().is_empty() {
        return native(path.display().to_string());
    }
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => native(format!("~{MAIN_SEPARATOR}{}", rest.display())),
        Err(_) => native(path.display().to_string()),
    }
}

/// `text` with the system's separator: Windows paths often mix `/` into them (`home.join`
/// of a literal with `/`), which people should not see. Unix text is kept as it is.
fn native(text: String) -> String {
    if MAIN_SEPARATOR == '/' {
        text
    } else {
        text.replace('/', MAIN_SEPARATOR_STR)
    }
}

/// `text`, a path written with `/`, as Portico shows it to people on this system: lets a test
/// write one expectation for every system.
#[cfg(test)]
pub fn shown(text: &str) -> String {
    native(text.into())
}

/// A configured value with a leading `home` shown as `~`; other spellings are kept.
pub fn tilde_text(value: &str, home: &Path) -> String {
    let home = home.to_string_lossy();
    let home = home.trim_end_matches(is_separator);
    match value.strip_prefix(home) {
        Some(rest) if !home.is_empty() && rest.starts_with(is_separator) => format!("~{rest}"),
        _ => value.into(),
    }
}

/// A `~/` or `~\` path with `home` in place of the `~`, the way `ssh -G` reports it; other
/// values are kept.
pub fn expand(value: &str, home: &Path) -> String {
    match value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
    {
        Some(rest) => format!(
            "{}{MAIN_SEPARATOR}{rest}",
            home.to_string_lossy().trim_end_matches(is_separator)
        ),
        None => value.into(),
    }
}

/// `path` as written into an OpenSSH file (an `IdentityFile` Portico suggests, say): `~/`
/// in place of a leading `home`, and `/` between the parts, which OpenSSH reads on every
/// system. On Unix this is what `tilde` shows.
pub fn openssh_path(path: &Path, home: &Path) -> String {
    let shown = tilde(path, home);
    if MAIN_SEPARATOR == '/' {
        shown
    } else {
        shown.replace(MAIN_SEPARATOR, "/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn shortens_paths_under_home() {
        let home = Path::new("/home/me");
        assert_eq!(tilde(Path::new("/home/me/.ssh/a"), home), "~/.ssh/a");
        assert_eq!(tilde(Path::new("/home/me"), home), "~");
        assert_eq!(tilde(Path::new("/home/meow/a"), home), "/home/meow/a");
        assert_eq!(tilde_text("/home/me/.ssh/a", home), "~/.ssh/a");
        assert_eq!(tilde_text("/home/meow/a", home), "/home/meow/a");
        assert_eq!(tilde_text("~/.ssh/a", home), "~/.ssh/a");
        assert_eq!(tilde(Path::new("/a/b"), Path::new("")), "/a/b");
        assert_eq!(tilde_text("/a/b", Path::new("")), "/a/b");
    }

    #[cfg(unix)]
    #[test]
    fn config_dir_follows_absolute_xdg_only() {
        let home = Path::new("/home/me");
        assert_eq!(
            config_dir(home, Some(OsStr::new("/x"))),
            Path::new("/x/portico")
        );
        for xdg in [None, Some(OsStr::new("rel"))] {
            assert_eq!(config_dir(home, xdg), Path::new("/home/me/.config/portico"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn expands_home_relative_values() {
        assert_eq!(
            expand("~/.ssh/id", Path::new("/home/me/")),
            "/home/me/.ssh/id"
        );
        assert_eq!(
            expand("~/.ssh/id", Path::new("/home/me")),
            "/home/me/.ssh/id"
        );
        assert_eq!(expand("/etc/id", Path::new("/home/me")), "/etc/id");
        assert_eq!(expand("~other/id", Path::new("/home/me")), "~other/id");
    }

    #[test]
    fn expand_accepts_both_separators() {
        let home = Path::new("/home/me");
        assert_eq!(expand("~\\a", home), home.join("a").to_str().unwrap());
        assert_eq!(expand("~/a", home), home.join("a").to_str().unwrap());
    }

    #[test]
    fn openssh_path_uses_forward_slashes() {
        let home = Path::new("/home/me");
        assert_eq!(
            openssh_path(&home.join(".ssh").join("id"), home),
            "~/.ssh/id"
        );
        assert_eq!(openssh_path(home, home), "~");
        assert_eq!(openssh_path(Path::new("/keys/id"), home), "/keys/id");
    }

    #[cfg(windows)]
    #[test]
    fn tilde_keeps_spaces() {
        let home = Path::new(r"C:\Users\Jane Doe");
        let key = Path::new(r"C:\Users\Jane Doe\.ssh\x");
        assert_eq!(tilde(key, home), r"~\.ssh\x");
        assert_eq!(tilde(&home.join(".ssh/x"), home), r"~\.ssh\x");
        assert_eq!(tilde_text(r"C:\Users\Jane Doe\.ssh\x", home), r"~\.ssh\x");
        assert_eq!(
            tilde_text("C:/Users/Jane Doe/.ssh/x", Path::new(r"C:\Users\Jane Doe\")),
            "C:/Users/Jane Doe/.ssh/x"
        );
        assert_eq!(openssh_path(key, home), "~/.ssh/x");
        assert_eq!(openssh_path(Path::new(r"D:\keys\x"), home), "D:/keys/x");
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_shown_and_expanded_with_backslashes() {
        let home = Path::new(r"C:\Users\me");
        assert_eq!(tilde(Path::new("D:/keys/x"), home), r"D:\keys\x");
        assert_eq!(
            expand(r"~\.ssh\id", Path::new(r"C:\Users\me\")),
            r"C:\Users\me\.ssh\id"
        );
        assert_eq!(
            config_dir(home, Some(OsStr::new(r"D:\xdg"))),
            Path::new(r"D:\xdg\portico")
        );
        // The default reads `%APPDATA%`, which `windows::paths`' tests change.
        assert!(config_dir(home, Some(OsStr::new("relative"))).ends_with("portico"));
        assert_ne!(
            config_dir(home, Some(OsStr::new("relative"))),
            Path::new(r"relative\portico")
        );
    }
}
