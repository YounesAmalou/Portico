#[cfg(not(target_os = "linux"))]
compile_error!(
    "Portico supports Linux only for now; macOS and Windows are planned in ROADMAP.md (v5)."
);
#[cfg(target_os = "linux")]
mod linux;

pub mod process;
