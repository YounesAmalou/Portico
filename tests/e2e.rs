//! End-to-end tests: the real `portico` binary against a real, unprivileged
//! `sshd`, through a real pseudo-terminal where a terminal is involved.
//!
//! Every test is ignored by a plain `cargo test`; run the suite with
//! `scripts/ci-checks.sh e2e`. It needs OpenSSH's server installed and runs as
//! a regular user. Each test owns its server, its keys and its home, all in
//! temporary directories, and proves that the real `~/.ssh/known_hosts` was
//! not touched.
//!
//! On Windows the server is Win32-OpenSSH's `sshd.exe` and the terminal is a
//! ConPTY. The tests that are about Unix alone are left out there, each with
//! its reason.

#[path = "support/executable.rs"]
#[cfg_attr(
    windows,
    allow(dead_code, reason = "Windows' ssh wrapper is compiled, not a script")
)]
mod executable;

#[path = "e2e/client.rs"]
mod client;
#[path = "e2e/procs.rs"]
mod procs;
#[path = "e2e/sshd.rs"]
mod sshd;
#[path = "e2e/terminal.rs"]
mod terminal;

#[path = "e2e/cli_connect.rs"]
mod cli_connect;
#[path = "e2e/cli_test.rs"]
mod cli_test;
#[path = "e2e/explanations.rs"]
mod explanations;
// The tests run a private `ssh-agent` on a Unix socket; Windows' agent is a system service.
#[cfg(unix)]
#[path = "e2e/identities.rs"]
mod identities;
#[path = "e2e/json.rs"]
mod json;
#[path = "e2e/tui_first_run.rs"]
mod tui_first_run;
// Hanging up is a Unix terminal's SIGHUP, and the tests drive termios and signals directly.
#[cfg(unix)]
#[path = "e2e/tui_hangup.rs"]
mod tui_hangup;
#[path = "e2e/tui_maintenance.rs"]
mod tui_maintenance;
#[path = "e2e/tunnels.rs"]
mod tunnels;
