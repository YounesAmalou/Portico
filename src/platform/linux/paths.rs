//! Linux locations: the system ssh configuration and the runtime directory of started tunnels.

use std::{
    ffi::OsStr,
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

/// The system-wide ssh client configuration.
pub const SYSTEM_SSH_CONFIG: &str = "/etc/ssh/ssh_config";

/// `$XDG_RUNTIME_DIR/portico/tunnels` when `xdg_runtime_dir` is an absolute path to a
/// directory the current user owns, otherwise `~/.ssh/portico/run/tunnels`.
pub fn runtime_dir(home: &Path, xdg_runtime_dir: Option<&OsStr>) -> PathBuf {
    let usable = xdg_runtime_dir.map(Path::new).filter(|path| {
        path.is_absolute()
            && fs::metadata(path).is_ok_and(|metadata| {
                // SAFETY: `getuid` has no preconditions.
                metadata.is_dir() && metadata.uid() == unsafe { libc::getuid() }
            })
    });
    match usable {
        Some(path) => path.join("portico/tunnels"),
        None => home.join(".ssh/portico/run/tunnels"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_dir_falls_back_under_the_home_without_a_usable_xdg_dir() {
        let home = Path::new("/home/me");
        assert_eq!(
            runtime_dir(home, None),
            home.join(".ssh/portico/run/tunnels")
        );
        assert_eq!(
            runtime_dir(home, Some(OsStr::new("relative"))),
            home.join(".ssh/portico/run/tunnels")
        );
    }
}
