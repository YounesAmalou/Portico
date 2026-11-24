//! The home directory as Portico shows it (paths under it start with `~`).
//!
//! The home always comes from the caller, ultimately from `App::home()`; only `main` reads
//! `HOME` from the environment, through `home_dir`.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

/// The home directory from `HOME`, if it is set.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

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

/// A `~/` path with `home` in place of the `~`, the way `ssh -G` reports it; other values are kept.
pub fn expand(value: &str, home: &Path) -> String {
    match value.strip_prefix("~/") {
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

    /// Code that reads `HOME` itself would disagree with `App::home()` in tests and in
    /// `--json` output; `main` is the one place that reads it, through `home_dir`.
    #[test]
    fn only_main_reads_home_from_the_environment() {
        let patterns = ["var", "var_os"].map(|call| format!("{call}(\"{}\")", "HOME"));
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let paths = src.join("platform/unix/paths.rs");
        let mut offenders = Vec::new();
        let mut pending = vec![src.clone()];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    pending.push(path);
                } else if path.extension().is_some_and(|ext| ext == "rs") && path != paths {
                    let text = std::fs::read_to_string(&path).unwrap();
                    if patterns.iter().any(|pattern| text.contains(pattern)) {
                        offenders.push(path.display().to_string());
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "HOME read outside platform::paths::home_dir: {offenders:?}"
        );
    }
}
