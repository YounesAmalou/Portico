use std::{
    io::{self, IsTerminal},
    process::ExitCode,
};

use clap::Parser;
use portico::{
    app::App,
    cli::{Cli, run},
    managed::{ConfigPaths, ManagedStore},
    openssh::OpenSsh,
    picker::TerminalPicker,
};

fn main() -> ExitCode {
    let Some(home) = std::env::var_os("HOME") else {
        eprintln!("error: HOME is not set");
        return ExitCode::FAILURE;
    };
    let app = App::new(
        ManagedStore::new(ConfigPaths::from_home(home)),
        OpenSsh::system(),
    );
    let cli = Cli::parse();
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
