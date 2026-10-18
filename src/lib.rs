pub mod app;
pub mod cli;
pub mod config_error;
pub mod container;
pub mod discovery;
pub mod explain;
pub mod host;
pub mod managed;
pub mod openssh;
pub mod probe;
pub mod suggest;
pub mod tui;

#[cfg(test)]
#[path = "../tests/support/executable.rs"]
mod test_executable;
