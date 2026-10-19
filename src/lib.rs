pub mod adopt;
pub mod app;
pub mod cli;
pub mod config_error;
pub mod container;
pub mod discovery;
pub mod explain;
pub mod fuzzy;
pub mod host;
pub mod identity;
pub mod managed;
pub mod openssh;
pub mod picker;
pub mod probe;
pub mod suggest;
pub mod tui;
pub mod tunnel;

#[cfg(test)]
#[path = "../tests/support/executable.rs"]
mod test_executable;
