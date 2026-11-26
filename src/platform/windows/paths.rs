//! Windows home directory and well-known locations.
//!
//! The home always comes from the caller, ultimately from `App::home()`; only `main` reads
//! `USERPROFILE` from the environment, through `home_dir`.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

/// The home directory from `USERPROFILE`, if it is set: where Windows' OpenSSH looks for
/// `.ssh`. `HOME` is ignored, since Git Bash, MSYS and Cygwin set it to other places.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(PathBuf::from)
}

/// The system-wide `ssh_config`: `%ProgramData%\ssh\ssh_config`, where Windows' OpenSSH
/// reads it.
pub fn system_ssh_config() -> PathBuf {
    absolute_var("ProgramData")
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join(r"ssh\ssh_config")
}

/// Always `~\.ssh\portico\run\tunnels`, as on macOS: Windows has no `XDG_RUNTIME_DIR`.
/// `xdg_runtime_dir` is accepted for the same call as on Linux.
pub fn runtime_dir(home: &Path, _xdg_runtime_dir: Option<&OsStr>) -> PathBuf {
    home.join(r".ssh\portico\run\tunnels")
}

/// `%APPDATA%\portico`, or `~\AppData\Roaming\portico` (its usual place) without it.
pub(in crate::platform) fn default_config_dir(home: &Path) -> PathBuf {
    absolute_var("APPDATA")
        .unwrap_or_else(|| home.join(r"AppData\Roaming"))
        .join("portico")
}

/// The environment variable `name` when it holds an absolute path.
fn absolute_var(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sets `name` to `value` (or removes it) until dropped.
    struct EnvGuard {
        name: &'static str,
        saved: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(name: &'static str, value: Option<&OsStr>) -> Self {
            let saved = std::env::var_os(name);
            // SAFETY: only these tests change these variables, one at a time (`ENV`), and the
            // standard library serialises its own reads; nothing in the test binary reads the
            // environment through the C runtime.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
            Self { name, saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: as in `set`.
            unsafe {
                match &self.saved {
                    Some(value) => std::env::set_var(self.name, value),
                    None => std::env::remove_var(self.name),
                }
            }
        }
    }

    /// Serialises the tests that change the environment.
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn home_ignores_home_variable() {
        let _lock = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile = tempfile::tempdir().unwrap();
        let _home = EnvGuard::set("HOME", Some(OsStr::new(r"C:\elsewhere")));
        let _profile = EnvGuard::set("USERPROFILE", Some(profile.path().as_os_str()));
        assert_eq!(home_dir().as_deref(), Some(profile.path()));
    }

    #[test]
    fn system_config_defaults_under_programdata() {
        let _lock = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        {
            let _data = EnvGuard::set("ProgramData", Some(OsStr::new(r"D:\Data")));
            assert_eq!(system_ssh_config(), Path::new(r"D:\Data\ssh\ssh_config"));
        }
        let _data = EnvGuard::set("ProgramData", None);
        assert_eq!(
            system_ssh_config(),
            Path::new(r"C:\ProgramData\ssh\ssh_config")
        );
    }

    #[test]
    fn runtime_dir_is_under_dot_ssh() {
        let home = Path::new(r"C:\Users\me");
        let expected = Path::new(r"C:\Users\me\.ssh\portico\run\tunnels");
        assert_eq!(runtime_dir(home, None), expected);
        assert_eq!(runtime_dir(home, Some(OsStr::new(r"D:\run"))), expected);
    }

    #[test]
    fn config_dir_defaults_to_appdata() {
        let _lock = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = Path::new(r"C:\Users\me");
        {
            let _data = EnvGuard::set("APPDATA", Some(OsStr::new(r"D:\Roaming")));
            assert_eq!(default_config_dir(home), Path::new(r"D:\Roaming\portico"));
        }
        let _data = EnvGuard::set("APPDATA", None);
        assert_eq!(
            default_config_dir(home),
            Path::new(r"C:\Users\me\AppData\Roaming\portico")
        );
    }
}
