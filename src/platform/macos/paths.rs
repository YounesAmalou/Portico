//! macOS locations: the system ssh configuration and the runtime directory of started tunnels.

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

/// The system-wide ssh client configuration.
pub const SYSTEM_SSH_CONFIG: &str = "/etc/ssh/ssh_config";

/// Always `~/.ssh/portico/run/tunnels`. macOS has no `XDG_RUNTIME_DIR`, and its per-user
/// temporary directory is cleaned of files untouched for three days, which would delete the
/// record of a long-running tunnel. `xdg_runtime_dir` is accepted for the same call as on Linux.
pub fn runtime_dir(home: &Path, _xdg_runtime_dir: Option<&OsStr>) -> PathBuf {
    home.join(".ssh/portico/run/tunnels")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_dir_is_under_the_home() {
        let home = Path::new("/Users/me");
        assert_eq!(
            runtime_dir(home, Some(OsStr::new("/tmp"))),
            home.join(".ssh/portico/run/tunnels")
        );
    }
}
