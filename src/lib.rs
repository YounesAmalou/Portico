pub mod app;
pub mod cli;
pub mod discovery;
pub mod host;
pub mod managed;
pub mod openssh;
pub mod tui;

#[cfg(test)]
#[path = "../tests/support/executable.rs"]
mod test_executable;
