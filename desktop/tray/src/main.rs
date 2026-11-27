// No console window on Windows: the app lives in the notification area.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(any(target_os = "macos", windows))]
mod app;
#[cfg(any(target_os = "macos", windows))]
mod menu;

/// The release version `scripts/build-tray.sh` builds with, else `dev`.
const VERSION: &str = match option_env!("PORTICO_TRAY_VERSION") {
    Some(version) => version,
    None => "dev",
};

const USAGE: &str = "usage: portico-tray [--portico <path>] | --version";

/// `--portico <path>`, or `None` for `--version`.
fn arguments() -> Result<Option<Option<std::path::PathBuf>>, String> {
    let mut portico = None;
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--version" {
            return Ok(None);
        } else if arg == "--portico" {
            let path = args.next().ok_or("--portico needs a path")?;
            portico = Some(std::path::PathBuf::from(path));
        } else if let Some(path) = arg.to_str().and_then(|a| a.strip_prefix("--portico=")) {
            portico = Some(std::path::PathBuf::from(path));
        } else {
            return Err(format!("unknown argument {}", arg.to_string_lossy()));
        }
    }
    Ok(Some(portico))
}

fn main() {
    let portico = match arguments() {
        Ok(Some(portico)) => portico,
        Ok(None) => {
            println!("portico-tray {VERSION}");
            return;
        }
        Err(error) => {
            eprintln!("portico-tray: {error}\n{USAGE}");
            std::process::exit(2);
        }
    };
    #[cfg(any(target_os = "macos", windows))]
    app::run(VERSION, portico);
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = portico;
        eprintln!(
            "portico-tray runs on macOS and Windows; on Linux use the Quickshell panel (portico desktop install)"
        );
        std::process::exit(1);
    }
}
