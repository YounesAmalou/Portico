use std::{
    io::{self, IsTerminal},
    process::ExitCode,
};

use clap::Parser;
use portico::{
    app::App,
    cli::{Cli, fail, run},
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
    let runtime = RuntimeDir::locate(
        home.as_ref(),
        std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
    );
    let app = App::new(
        ManagedStore::new(ConfigPaths::from_home(home)),
        OpenSsh::system(),
    )
    .with_runtime_dir(runtime);
    let stdin = io::stdin();
    let interactive = stdin.is_terminal();
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
