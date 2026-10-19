use std::{
    io::{self, IsTerminal},
    path::PathBuf,
    process::ExitCode,
};

use clap::Parser;
use portico::{
    app::App,
    cli::{Cli, fail, run, run_desktop},
    desktop::{DesktopEnv, panel},
    managed::{ConfigPaths, ManagedStore},
    openssh::OpenSsh,
    picker::TerminalPicker,
    tunnel::runtime::RuntimeDir,
};

fn main() -> ExitCode {
    // Parsed first, so a `--json` command reports even this as a document.
    let cli = Cli::parse();
    let Some(home) = std::env::var_os("HOME") else {
        return fail(
            &cli,
            "HOME is not set",
            &mut io::stdout().lock(),
            &mut io::stderr(),
        );
    };
    let stdin = io::stdin();
    let interactive = stdin.is_terminal();
    let desktop = DesktopEnv::new(
        PathBuf::from(&home),
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
    // Keep an installed panel in step with this binary; silent, and a no-op without a panel.
    panel::refresh(&desktop.panel_dir());
    let runtime = RuntimeDir::locate(
        home.as_ref(),
        std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
    );
    let app = App::new(
        ManagedStore::new(ConfigPaths::from_home(home)),
        OpenSsh::system(),
    )
    .with_runtime_dir(runtime);
    run(
        cli,
        &app,
        &mut TerminalPicker,
        &mut stdin.lock(),
        &mut io::stdout().lock(),
        // Unlocked: `portico connect` mirrors ssh's stderr from another thread.
        &mut io::stderr(),
        interactive,
        true,
    )
}
