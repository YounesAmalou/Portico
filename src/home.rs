//! The home directory as Portico shows it: paths under it start with `~`.
//!
//! The home always comes from the caller, ultimately from `App::home()`; only `main` reads
//! `HOME` from the environment.

use std::path::Path;

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
    /// `--json` output; `main` is the one place that reads it.
    #[test]
    fn only_main_reads_home_from_the_environment() {
        let patterns = ["var", "var_os"].map(|call| format!("{call}(\"{}\")", "HOME"));
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        let mut pending = vec![src.clone()];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    pending.push(path);
                } else if path.extension().is_some_and(|ext| ext == "rs")
                    && path != src.join("main.rs")
                {
                    let text = std::fs::read_to_string(&path).unwrap();
                    if patterns.iter().any(|pattern| text.contains(pattern)) {
                        offenders.push(path.display().to_string());
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "HOME read outside main.rs: {offenders:?}"
        );
    }
}
