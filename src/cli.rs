use std::{
    io::{BufRead, Write},
    process::ExitCode,
};

use crate::{
    app::{App, HostDetails, Ownership},
    discovery::DiscoveryWarning,
    host::{Change, ClearField, Host, HostChanges},
    managed::{InitOutcome, InitPlan},
};

#[derive(clap::Parser, Debug)]
#[command(name = "portico", version)]
pub struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    Init {
        #[arg(short = 'y', long)]
        yes: bool,
    },
    Hosts,
    Show {
        alias: String,
    },
    Add {
        alias: String,
        #[arg(short = 'H', long)]
        hostname: String,
        #[arg(short = 'u', long)]
        user: Option<String>,
        #[arg(short = 'p', long)]
        port: Option<u16>,
        #[arg(short = 'i', long)]
        identity: Option<String>,
        #[arg(short = 'J', long)]
        jump: Option<String>,
    },
    Edit {
        alias: String,
        #[arg(short = 'a', long = "alias")]
        new_alias: Option<String>,
        #[arg(short = 'H', long)]
        hostname: Option<String>,
        #[arg(short = 'u', long)]
        user: Option<String>,
        #[arg(short = 'p', long)]
        port: Option<u16>,
        #[arg(short = 'i', long)]
        identity: Option<String>,
        #[arg(short = 'J', long)]
        jump: Option<String>,
        #[arg(short = 'U', long = "unset")]
        unset: Vec<ClearField>,
    },
    Remove {
        alias: String,
        #[arg(short = 'f', long)]
        force: bool,
    },
    Connect {
        alias: String,
        #[arg(short = 'n', long = "new-terminal")]
        new_terminal: bool,
    },
}

pub fn run<I, O, E>(
    cli: Cli,
    app: &App,
    input: &mut I,
    output: &mut O,
    error: &mut E,
    interactive: bool,
) -> ExitCode
where
    I: BufRead,
    O: Write,
    E: Write,
{
    match execute(cli, app, input, output, interactive) {
        Ok(exit) => exit,
        Err(message) => {
            let _ = writeln!(error, "error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn execute<I: BufRead, O: Write>(
    cli: Cli,
    app: &App,
    input: &mut I,
    output: &mut O,
    interactive: bool,
) -> Result<ExitCode, String> {
    match cli.command {
        None => crate::tui::run(app)
            .map(|()| ExitCode::SUCCESS)
            .map_err(|error| error.to_string()),
        Some(Command::Init { yes }) => {
            let plan = app.init_plan().map_err(|error| error.to_string())?;
            print_init_plan(output, &plan)?;
            if plan.requires_confirmation()
                && !yes
                && (!interactive || !confirm(input, output, "Apply initialization? [y/N] ")?)
            {
                if interactive {
                    writeln!(output, "Initialization cancelled.")
                        .map_err(|error| error.to_string())?;
                    return Ok(ExitCode::SUCCESS);
                }
                return Err("initialization requires --yes in non-interactive mode".into());
            }
            match app
                .initialize(yes || interactive)
                .map_err(|error| error.to_string())?
            {
                InitOutcome::Initialized => writeln!(output, "Initialized Portico."),
                InitOutcome::Unchanged => writeln!(output, "Portico is already initialized."),
            }
            .map_err(|error| error.to_string())?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Hosts) => {
            for host in app.hosts().map_err(|error| error.to_string())? {
                writeln!(
                    output,
                    "{}  {}  {}",
                    host.alias,
                    ownership(host.ownership),
                    host.source.display()
                )
                .map_err(|error| error.to_string())?;
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Show { alias }) => {
            let details = app.show(&alias).map_err(|error| error.to_string())?;
            print_details(output, &details)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Add {
            alias,
            hostname,
            user,
            port,
            identity,
            jump,
        }) => {
            let host = Host::new(alias, hostname, user, port, identity, jump)
                .map_err(|error| error.to_string())?;
            let alias = host.alias().to_owned();
            app.add(host).map_err(|error| error.to_string())?;
            writeln!(output, "Added {alias}.").map_err(|error| error.to_string())?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Edit {
            alias,
            new_alias,
            hostname,
            user,
            port,
            identity,
            jump,
            unset,
        }) => {
            let mut changes = HostChanges::from_clear_fields(unset);
            let updated_alias = new_alias.as_deref().unwrap_or(&alias).to_owned();
            if let Some(value) = new_alias {
                changes.alias = Change::Set(value);
            }
            if let Some(value) = hostname {
                changes.hostname = Change::Set(value);
            }
            if let Some(value) = user {
                changes.user = Change::Set(value);
            }
            if let Some(value) = port {
                changes.port = Change::Set(value);
            }
            if let Some(value) = identity {
                changes.identity_file = Change::Set(value);
            }
            if let Some(value) = jump {
                changes.proxy_jump = Change::Set(value);
            }
            app.edit(&alias, changes)
                .map_err(|error| error.to_string())?;
            writeln!(output, "Updated {updated_alias}.").map_err(|error| error.to_string())?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Remove { alias, force }) => {
            if !force {
                if !interactive {
                    return Err("removal requires --force in non-interactive mode".into());
                }
                if !confirm(input, output, &format!("Remove {alias}? [y/N] "))? {
                    writeln!(output, "Removal cancelled.").map_err(|error| error.to_string())?;
                    return Ok(ExitCode::SUCCESS);
                }
            }
            app.remove(&alias).map_err(|error| error.to_string())?;
            writeln!(output, "Removed {alias}.").map_err(|error| error.to_string())?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Connect {
            alias,
            new_terminal,
        }) => {
            let status = app
                .connect(&alias, new_terminal)
                .map_err(|error| error.to_string())?;
            Ok(status
                .code()
                .and_then(|code| u8::try_from(code).ok())
                .map_or(ExitCode::FAILURE, ExitCode::from))
        }
    }
}

fn confirm<I: BufRead, O: Write>(
    input: &mut I,
    output: &mut O,
    prompt: &str,
) -> Result<bool, String> {
    write!(output, "{prompt}").map_err(|error| error.to_string())?;
    output.flush().map_err(|error| error.to_string())?;
    let mut answer = String::new();
    input
        .read_line(&mut answer)
        .map_err(|error| error.to_string())?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn print_init_plan(output: &mut impl Write, plan: &InitPlan) -> Result<(), String> {
    writeln!(output, "Initialization plan:").map_err(|error| error.to_string())?;
    let changes = [
        (plan.create_entry, "Create ~/.ssh/config"),
        (plan.create_managed, "Create managed config"),
        (plan.insert_include, "Add Portico Include"),
        (plan.reposition_include, "Reposition Portico Include"),
        (plan.backup_entry, "Back up ~/.ssh/config"),
        (plan.fix_portico_permissions, "Fix Portico permissions"),
        (plan.fix_managed_permissions, "Fix managed permissions"),
    ];
    if plan.requires_confirmation() {
        for (_, change) in changes.into_iter().filter(|(needed, _)| *needed) {
            writeln!(output, "- {change}").map_err(|error| error.to_string())?;
        }
    } else {
        writeln!(output, "- No changes").map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn print_details(output: &mut impl Write, details: &HostDetails) -> Result<(), String> {
    writeln!(output, "Alias: {}", details.summary.alias).map_err(|error| error.to_string())?;
    writeln!(
        output,
        "Ownership: {}",
        ownership(details.summary.ownership)
    )
    .map_err(|error| error.to_string())?;
    writeln!(output, "Source: {}", details.summary.source.display())
        .map_err(|error| error.to_string())?;
    if let Some(host) = &details.declared {
        writeln!(output, "Declared HostName: {}", host.hostname())
            .map_err(|error| error.to_string())?;
        print_optional(output, "Declared User", host.user())?;
        print_optional(
            output,
            "Declared Port",
            host.port().map(|port| port.to_string()).as_deref(),
        )?;
        print_optional(output, "Declared IdentityFile", host.identity_file())?;
        print_optional(output, "Declared ProxyJump", host.proxy_jump())?;
    }
    writeln!(output, "Resolved HostName: {}", details.resolved.hostname)
        .map_err(|error| error.to_string())?;
    print_optional(output, "Resolved User", details.resolved.user.as_deref())?;
    print_optional(
        output,
        "Resolved Port",
        details
            .resolved
            .port
            .map(|port| port.to_string())
            .as_deref(),
    )?;
    print_optional(
        output,
        "Resolved IdentityFile",
        (!details.resolved.identity_files.is_empty())
            .then(|| details.resolved.identity_files.join(", "))
            .as_deref(),
    )?;
    print_optional(
        output,
        "Resolved ProxyJump",
        details.resolved.proxy_jump.as_deref(),
    )?;
    for warning in &details.warnings {
        writeln!(output, "Warning: {}", format_warning(warning))
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn print_optional(output: &mut impl Write, label: &str, value: Option<&str>) -> Result<(), String> {
    writeln!(output, "{label}: {}", value.unwrap_or("-")).map_err(|error| error.to_string())
}

fn ownership(ownership: Ownership) -> &'static str {
    match ownership {
        Ownership::Managed => "managed",
        Ownership::External => "external",
    }
}

fn format_warning(warning: &DiscoveryWarning) -> String {
    match warning {
        DiscoveryWarning::Include { path, message } => format!("{}: {message}", path.display()),
        DiscoveryWarning::Parse {
            path,
            line,
            message,
        } => format!("{}:{line}: {message}", path.display()),
        DiscoveryWarning::DuplicateAlias { alias, sources } => format!(
            "duplicate alias {alias}: {}",
            sources
                .iter()
                .map(|source| source.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Cursor, os::unix::fs::PermissionsExt};

    use clap::Parser;
    use tempfile::TempDir;

    use super::*;
    use crate::{
        app::{App, AppError},
        host::Host,
        managed::{ConfigPaths, ManagedStore},
        openssh::OpenSsh,
    };

    #[test]
    fn parses_short_host_flags_and_new_terminal() {
        let cli = Cli::try_parse_from([
            "portico",
            "add",
            "dev",
            "-H",
            "dev.example",
            "-u",
            "deploy",
            "-p",
            "2222",
            "-i",
            "~/.ssh/id_dev",
            "-J",
            "bastion",
        ])
        .unwrap();
        assert!(
            matches!(cli.command, Some(Command::Add { alias, port: Some(2222), .. }) if alias == "dev")
        );

        let cli = Cli::try_parse_from(["portico", "connect", "dev", "-n"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Connect {
                new_terminal: true,
                ..
            })
        ));
    }

    #[test]
    fn parses_repeatable_unset_and_force() {
        let cli =
            Cli::try_parse_from(["portico", "edit", "dev", "-U", "user", "-U", "port"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Edit { unset, .. }) if unset.len() == 2));
        let cli = Cli::try_parse_from(["portico", "remove", "dev", "-f"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Remove { force: true, .. })
        ));
    }

    #[test]
    fn interactive_confirmation_controls_initialization_and_removal() {
        let (_dir, app) = test_app();
        let mut output = Vec::new();
        let mut error = Vec::new();

        let exit = run(
            Cli::try_parse_from(["portico", "init"]).unwrap(),
            &app,
            &mut Cursor::new(b"y\n"),
            &mut output,
            &mut error,
            true,
        );
        assert_eq!(exit, std::process::ExitCode::SUCCESS);
        app.add(Host::new("dev", "dev.example", None, None, None, None).unwrap())
            .unwrap();

        let exit = run(
            Cli::try_parse_from(["portico", "remove", "dev"]).unwrap(),
            &app,
            &mut Cursor::new(b"n\n"),
            &mut output,
            &mut error,
            true,
        );
        assert_eq!(exit, std::process::ExitCode::SUCCESS);
        assert!(app.show("dev").is_ok());

        run(
            Cli::try_parse_from(["portico", "remove", "dev"]).unwrap(),
            &app,
            &mut Cursor::new(b"y\n"),
            &mut output,
            &mut error,
            true,
        );
        assert!(matches!(app.show("dev"), Err(AppError::UnknownHost(_))));
    }

    fn test_app() -> (TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join("ssh");
        fs::write(
            &ssh,
            "#!/bin/sh\nif [ \"$1\" = '-G' ]; then printf 'hostname resolved.example\\nport 22\\n'; fi\n",
        )
        .unwrap();
        fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
        let app = App::new(
            ManagedStore::new(ConfigPaths::from_home(dir.path())),
            OpenSsh::new(ssh.clone(), ssh),
        );
        (dir, app)
    }
}
