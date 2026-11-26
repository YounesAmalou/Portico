#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
compile_error!("Portico supports Linux, macOS and Windows only.");

mod common;

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

/// The implementation for the target system.
#[cfg(target_os = "linux")]
use linux as os;
#[cfg(target_os = "macos")]
use macos as os;

pub mod fs;
pub mod launcher;
pub mod paths;
pub mod process;
