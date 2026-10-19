//! End-to-end tests: the real `portico` binary against a real, unprivileged
//! `sshd`, through a real pseudo-terminal where a terminal is involved.
//!
//! Every test is ignored by a plain `cargo test`; run the suite with
//! `scripts/ci-checks.sh e2e`. It needs OpenSSH's server installed and runs as
//! a regular user. Each test owns its server, its keys and its home, all in
//! temporary directories, and proves that the real `~/.ssh/known_hosts` was
//! not touched.

#[path = "support/executable.rs"]
mod executable;

#[path = "e2e/client.rs"]
mod client;
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
#[path = "e2e/tui_first_run.rs"]
mod tui_first_run;
#[path = "e2e/tui_hangup.rs"]
mod tui_hangup;
#[path = "e2e/tui_maintenance.rs"]
mod tui_maintenance;
