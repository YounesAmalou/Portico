//! The home directory as Portico shows it (paths under it start with `~`).
//!
//! The home always comes from the caller, ultimately from `App::home()`; only `main` reads
//! `HOME` from the environment, through `home_dir`.

use std::path::PathBuf;

/// The home directory from `HOME`, if it is set.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

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
