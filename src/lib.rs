pub mod app;
pub mod backend;
pub mod cli;
pub mod desktop;
pub mod domain;
pub mod picker;
pub mod platform;
pub mod tui;

#[cfg(test)]
#[path = "../tests/support/executable.rs"]
mod test_executable;
