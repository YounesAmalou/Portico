use std::{
    io::{self, IsTerminal},
    process::ExitCode,
};

use clap::Parser;
use portico::{
    app::App,
    cli::{Cli, fail, run, run_desktop},
    desktop::{DesktopEnv, is_development_binary, panel, reload_omarchy_panel},
    domain::managed::{ConfigPaths, ManagedStore},
    domain::openssh::OpenSsh,
    domain::settings,
    domain::tunnel::runtime::RuntimeDir,
    picker::TerminalPicker,
    platform,
};

fn main() -> ExitCode {
    // Parsed first, so a `--json` command reports even this as a document.
    let cli = Cli::parse();
    let Some(home) = platform::paths::home_dir() else {
        return fail(
            &cli,
            &format!("{} is not set", platform::paths::HOME_VARIABLE),
            &mut io::stdout().lock(),
            &mut io::stderr(),
        );
    };
    let stdin = io::stdin();
    let interactive = stdin.is_terminal();
    let desktop = DesktopEnv::new(
        home.clone(),
        |name| std::env::var_os(name),
        cli.is_desktop()
            .then(|| std::env::current_exe().ok())
            .flatten(),
    );
    if cli.is_desktop() {
        return run_desktop(
            cli,
            &desktop,
            &mut stdin.lock(),
            &mut io::stdout().lock(),
            &mut io::stderr(),
            interactive,
        );
    }
    // Bring an installed panel up to this binary; silent, a no-op without a panel, and never
    // from a cargo build, which would rewrite the installed panel on every run.
    // Omarchy's shell only notices the rewrite through its link, which is re-created for it.
    // Linux only: updating the macOS and Windows tray app needs the network, so it happens
    // only from `portico desktop install`, the settings overlay or the app's own banner.
    if cfg!(target_os = "linux")
        && panel::refresh(&desktop.panel_dir(), || {
            std::env::current_exe().is_ok_and(|exe| is_development_binary(&exe))
        })
    {
        let _ = reload_omarchy_panel(&desktop);
    }
    let runtime = RuntimeDir::locate(&home, std::env::var_os("XDG_RUNTIME_DIR").as_deref());
    let settings_path =
        platform::paths::config_dir(&home, std::env::var_os("XDG_CONFIG_HOME").as_deref())
            .join(settings::FILE_NAME);
    let app = App::new(
        ManagedStore::new(ConfigPaths::from_home(home)),
        OpenSsh::system(),
    )
    .with_runtime_dir(runtime)
    .with_settings_path(settings_path);
    run(
        cli,
        &app,
        &desktop,
        &mut TerminalPicker,
        &mut stdin.lock(),
        &mut io::stdout().lock(),
        // Unlocked: `portico connect` mirrors ssh's stderr from another thread.
        &mut io::stderr(),
        interactive,
        true,
    )
}
