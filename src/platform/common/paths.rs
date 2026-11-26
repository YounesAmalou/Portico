//! Paths every system shares: `~` display and expansion, and the configuration directory.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

/// `$XDG_CONFIG_HOME/portico` when `xdg_config_home` is an absolute path, otherwise
/// `~/.config/portico`.
pub fn config_dir(home: &Path, xdg_config_home: Option<&OsStr>) -> PathBuf {
    match xdg_config_home
        .map(Path::new)
        .filter(|path| path.is_absolute())
    {
        Some(path) => path.join("portico"),
        None => home.join(".config/portico"),
    }
}

/// `path` with a leading `home` shown as `~`.
pub fn tilde(path: &Path, home: &Path) -> String {
    if home.as_os_str().is_empty() {
        return path.display().to_string();
    }
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// A configured value with a leading `home` shown as `~`; other spellings are kept.
pub fn tilde_text(value: &str, home: &Path) -> String {
    let home = home.to_string_lossy();
    let home = home.trim_end_matches('/');
    match value.strip_prefix(home) {
        Some(rest) if !home.is_empty() && rest.starts_with('/') => format!("~{rest}"),
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
        Some(rest) => format!("{}/{rest}", home.to_string_lossy().trim_end_matches('/')),
        None => value.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
