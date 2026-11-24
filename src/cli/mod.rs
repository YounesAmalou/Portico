use std::{
    io::{self, BufRead, Write},
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::{
    app::{App, AppError, ContainerError, Ownership},
    backend::{BackendId, ResolveError, Via, resolve},
    domain::adopt::Direction,
    domain::host::{Change, ClearField, Host, HostChanges},
    domain::managed::InitOutcome,
    picker::{Picker, Row},
};

mod containers;
mod desktop;
mod hosts;
mod json;
mod migrate;
mod tunnels;

use self::{
    containers::{
        container_rows, list_with_offers, on_argument, print_listing, report_container_failure,
    },
    hosts::{
        field_change, finish_connection, format_warning, host_argument, ownership, print_details,
        print_init_plan, print_inventory, test_command,
    },
    migrate::{Migration, run_migration},
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
    Hosts {
        /// Print one JSON document instead of text (see docs/json-output.md).
        #[arg(long)]
        json: bool,
    },
    Show {
        alias: Option<String>,
        /// Print one JSON document instead of text (see docs/json-output.md).
        #[arg(long)]
        json: bool,
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
        alias: Option<String>,
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
        alias: Option<String>,
        #[arg(short = 'f', long)]
        force: bool,
    },
    Connect {
        alias: Option<String>,
        #[arg(short = 'n', long = "new-terminal")]
        new_terminal: bool,
        /// Wait for Enter after an explained failure; used in a new terminal.
        #[arg(long, hide = true)]
        hold: bool,
    },
    /// Move external hosts into Portico's managed configuration.
    Adopt {
        aliases: Vec<String>,
        #[arg(short = 'A', long, conflicts_with = "aliases")]
        all: bool,
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Move managed hosts back into the entry configuration.
    Export {
        aliases: Vec<String>,
        #[arg(short = 'A', long, conflicts_with = "aliases")]
        all: bool,
        #[arg(short = 'y', long)]
        yes: bool,
    },
    Test {
        alias: Option<String>,
        #[arg(long = "accept-host-key", value_name = "FINGERPRINT")]
        accept_host_key: Option<String>,
        /// Show what ssh itself said, below the explanation.
        #[arg(long)]
        raw: bool,
    },
    /// List the agents and identity files Portico knows of, and whether each key is available.
    Identities {
        /// Print one JSON document instead of text (see docs/json-output.md).
        #[arg(long)]
        json: bool,
    },
    Containers {
        /// List on this host; without an alias, choose one.
        #[arg(long = "on", value_name = "ALIAS", num_args = 0..=1)]
        on: Option<Option<String>>,
        /// List with this backend only; without it, Docker, then Proxmox when Docker is not
        /// installed.
        #[arg(long, value_enum)]
        backend: Option<BackendArg>,
        /// Print one JSON document instead of text (see docs/json-output.md).
        #[arg(long)]
        json: bool,
    },
    Shell {
        container: Option<String>,
        /// Open the shell on this host; without an alias, choose one.
        #[arg(long = "on", value_name = "ALIAS", num_args = 0..=1)]
        on: Option<Option<String>>,
        /// List with this backend only; with --hold, the backend of the given ID (Docker by
        /// default).
        #[arg(long, value_enum)]
        backend: Option<BackendArg>,
        /// Run the shell as this user (Docker only: a Proxmox container's shell runs as root).
        #[arg(short = 'u', long)]
        user: Option<String>,
        #[arg(short = 'n', long = "new-terminal")]
        new_terminal: bool,
        /// Open the given container ID without listing, and wait for Enter after an explained
        /// failure; used in a new terminal.
        #[arg(long, hide = true)]
        hold: bool,
        /// Run the backend's tool through sg or sudo, as the listing before the new terminal did.
        #[arg(long, hide = true, value_enum)]
        via: Option<ViaArg>,
    },
    /// List saved tunnels and their status.
    Tunnels {
        /// Print one JSON document instead of text (see docs/json-output.md).
        #[arg(long)]
        json: bool,
    },
    /// Save, start and stop tunnels.
    Tunnel {
        #[command(subcommand)]
        command: tunnels::TunnelCommand,
    },
    /// Install the desktop panel, a launcher entry and a keybinding, or show or remove them.
    Desktop {
        #[command(subcommand)]
        command: desktop::DesktopCommand,
    },
}

impl Cli {
    /// Whether this is a `portico desktop` command, which `main` runs with the desktop
    /// environment and which must not refresh the panel first.
    pub fn is_desktop(&self) -> bool {
        matches!(self.command, Some(Command::Desktop { .. }))
    }
}

/// Runs a `portico desktop` command.
pub fn run_desktop(
    cli: Cli,
    env: &crate::desktop::DesktopEnv,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
    error: &mut dyn Write,
    interactive: bool,
) -> ExitCode {
    let Some(Command::Desktop { command }) = cli.command else {
        return ExitCode::FAILURE;
    };
    match desktop::run(command, env, input, output, interactive) {
        Ok(exit) => exit,
        Err(message) => {
            let _ = output.flush();
            let _ = writeln!(error, "error: {message}");
            ExitCode::FAILURE
        }
    }
}

/// `--backend`: the container backend to use.
#[derive(clap::ValueEnum, Clone, Copy, Debug, Eq, PartialEq)]
enum BackendArg {
    Docker,
    Proxmox,
}

impl From<BackendArg> for BackendId {
    fn from(backend: BackendArg) -> Self {
        match backend {
            BackendArg::Docker => BackendId::Docker,
            BackendArg::Proxmox => BackendId::Proxmox,
        }
    }
}

/// `--via`: the tool the backend's command ran through in the listing.
#[derive(clap::ValueEnum, Clone, Copy, Debug, Eq, PartialEq)]
enum ViaArg {
    Sg,
    Sudo,
}

impl From<ViaArg> for Via {
    fn from(via: ViaArg) -> Self {
        match via {
            ViaArg::Sg => Via::Sg,
            ViaArg::Sudo => Via::Sudo,
        }
    }
}

/// ssh's exit status as Portico's: its code, or 128 plus the signal that killed it, as a shell
/// reports it.
fn exit_code(status: std::process::ExitStatus) -> ExitCode {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .and_then(|code| u8::try_from(code).ok())
        .map_or(ExitCode::FAILURE, ExitCode::from)
}

/// The exit status of a `portico test` that a signal cancelled, as for SIGINT.
const CANCELLED: u8 = 130;

/// SIGINT, SIGTERM and SIGHUP during `portico test`.
///
/// `ssh` runs in its own process group, out of reach of the terminal's Ctrl+C, so while it runs
/// these signals only set `cancelled`, and the test kills it and returns. Anywhere else, such as at
/// the trust prompt, they end Portico as they would without these handlers. One that arrives after
/// a step stopped checking, but before the default action is back, stops Portico before the next
/// step; it never carries over into that step.
struct Interrupts {
    cancelled: Arc<AtomicBool>,
    default_action: Arc<AtomicBool>,
    /// Counts steps down to the one just after which `during` raises the flag, as a signal in
    /// that window would; 0 never does.
    #[cfg(test)]
    signal_after: std::cell::Cell<usize>,
}

/// A step's result, `Late` when a signal arrived after the step stopped checking for one.
enum Step<T> {
    Done(T),
    Late(T),
}

impl Interrupts {
    fn install(handle_signals: bool) -> io::Result<Self> {
        let interrupts = Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            default_action: Arc::new(AtomicBool::new(true)),
            #[cfg(test)]
            signal_after: std::cell::Cell::new(0),
        };
        if handle_signals {
            for signal in [
                signal_hook::consts::SIGINT,
                signal_hook::consts::SIGTERM,
                signal_hook::consts::SIGHUP,
            ] {
                signal_hook::flag::register_conditional_default(
                    signal,
                    Arc::clone(&interrupts.default_action),
                )?;
                signal_hook::flag::register(signal, Arc::clone(&interrupts.cancelled))?;
            }
        }
        Ok(interrupts)
    }

    /// Interrupts without handlers whose flag is raised just after the `step`th step (1-based).
    #[cfg(test)]
    fn signalled_after(step: usize) -> Self {
        let interrupts = Self::install(false).expect("no handlers to install");
        interrupts.signal_after.set(step);
        interrupts
    }

    /// Runs `step` with the cancel flag the signals set.
    fn during<T>(&self, step: impl FnOnce(&AtomicBool) -> T) -> Step<T> {
        // A signal acted on after an earlier step must not cancel this one.
        self.cancelled.store(false, Ordering::SeqCst);
        self.default_action.store(false, Ordering::SeqCst);
        let result = step(&self.cancelled);
        #[cfg(test)]
        match self.signal_after.get() {
            0 => {}
            1 => {
                self.signal_after.set(0);
                self.cancelled.store(true, Ordering::SeqCst);
            }
            later => self.signal_after.set(later - 1),
        }
        self.default_action.store(true, Ordering::SeqCst);
        if self.cancelled.swap(false, Ordering::SeqCst) {
            Step::Late(result)
        } else {
            Step::Done(result)
        }
    }
}

/// Runs `cli`. An error with an empty message was already reported. `handle_signals` lets
/// `portico test` catch SIGINT, SIGTERM and SIGHUP to stop its `ssh`; only the real program sets
/// it, so tests never install process-wide handlers.
#[allow(clippy::too_many_arguments)]
pub fn run<I, O, E>(
    cli: Cli,
    app: &App,
    picker: &mut dyn Picker,
    input: &mut I,
    output: &mut O,
    error: &mut E,
    interactive: bool,
    handle_signals: bool,
) -> ExitCode
where
    I: BufRead,
    O: Write,
    E: Write,
{
    if let Some(command) = cli.command.as_ref().and_then(json_command) {
        return json::run(command, app, output);
    }
    match execute(
        cli,
        app,
        picker,
        input,
        output,
        error,
        interactive,
        handle_signals,
    ) {
        Ok(exit) => exit,
        Err(message) if message.is_empty() => ExitCode::FAILURE,
        Err(message) => {
            let _ = writeln!(error, "error: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Reports `message` as the command's failure before anything ran: an error document on
/// `output` for a `--json` command, otherwise `error: …` on `error`.
pub fn fail(cli: &Cli, message: &str, output: &mut impl Write, error: &mut impl Write) -> ExitCode {
    if cli.command.as_ref().and_then(json_command).is_some() {
        return json::fail(message, output);
    }
    let _ = writeln!(error, "error: {message}");
    ExitCode::FAILURE
}

/// The command as a `--json` one, when it was given `--json`.
fn json_command(command: &Command) -> Option<json::JsonCommand> {
    use json::JsonCommand;
    Some(match command {
        Command::Hosts { json: true } => JsonCommand::Hosts,
        Command::Show { alias, json: true } => JsonCommand::Show(alias.clone()),
        Command::Identities { json: true } => JsonCommand::Identities,
        Command::Containers {
            on,
            backend,
            json: true,
        } => JsonCommand::Containers(on.clone(), backend.map(BackendId::from)),
        Command::Tunnels { json: true } => JsonCommand::Tunnels,
        Command::Tunnel { command } => return tunnels::json_command(command),
        _ => return None,
    })
}

#[allow(clippy::too_many_arguments)]
fn execute<I: BufRead, O: Write, E: Write>(
    cli: Cli,
    app: &App,
    picker: &mut dyn Picker,
    input: &mut I,
    output: &mut O,
    error: &mut E,
    interactive: bool,
    handle_signals: bool,
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
        Some(Command::Hosts { .. }) => {
            let inventory = app.hosts().map_err(|error| error.to_string())?;
            for host in inventory.hosts {
                writeln!(
                    output,
                    "{}  {}  {}",
                    host.alias,
                    ownership(host.ownership),
                    host.source.display()
                )
                .map_err(|error| error.to_string())?;
            }
            for warning in &inventory.warnings {
                writeln!(output, "Warning: {}", format_warning(warning))
                    .map_err(|error| error.to_string())?;
            }
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Show { alias, .. }) => {
            let Some(alias) = host_argument(alias, app, picker, interactive, "show", false)? else {
                return Ok(ExitCode::from(CANCELLED));
            };
            let details = app.show(&alias).map_err(|error| error.to_string())?;
            print_details(output, &details, app.home())?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Identities { .. }) => {
            let inventory = app
                .identity_inventory()
                .map_err(|error| error.to_string())?;
            print_inventory(output, &inventory, app.home())?;
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
            // The flags are checked before a host is picked, so a mistake does not cost a pick.
            let renamed = new_alias.clone();
            let changes = HostChanges {
                alias: new_alias.map_or(Change::Keep, Change::Set),
                hostname: hostname.map_or(Change::Keep, Change::Set),
                user: field_change(user, "user", ClearField::User, &unset)?,
                port: field_change(port, "port", ClearField::Port, &unset)?,
                identity_file: field_change(identity, "identity", ClearField::Identity, &unset)?,
                proxy_jump: field_change(jump, "jump", ClearField::Jump, &unset)?,
            };
            let Some(alias) = host_argument(alias, app, picker, interactive, "edit", true)? else {
                return Ok(ExitCode::from(CANCELLED));
            };
            let updated_alias = renamed.unwrap_or_else(|| alias.clone());
            app.edit(&alias, changes)
                .map_err(|error| error.to_string())?;
            writeln!(output, "Updated {updated_alias}.").map_err(|error| error.to_string())?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Remove { alias, force }) => {
            let Some(alias) = host_argument(alias, app, picker, interactive, "remove", true)?
            else {
                return Ok(ExitCode::from(CANCELLED));
            };
            let inventory = app.hosts().map_err(|error| error.to_string())?;
            match inventory
                .hosts
                .iter()
                .find(|host| host.alias.eq_ignore_ascii_case(&alias))
            {
                None => return Err(AppError::UnknownHost(alias).to_string()),
                Some(host) if host.ownership == Ownership::External => {
                    return Err(AppError::ReadOnlyExternal(alias).to_string());
                }
                Some(_) => {}
            }
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
            hold,
        }) => {
            let Some(alias) = host_argument(alias, app, picker, interactive, "connect", false)?
            else {
                return Ok(ExitCode::from(CANCELLED));
            };
            // A held connection already runs in the terminal it was opened in.
            let outcome = app
                .connect(&alias, new_terminal && !hold)
                .map_err(|error| error.to_string())?;
            finish_connection(outcome, hold, input, error)
        }
        Some(Command::Containers { on, backend, .. }) => {
            let Some(target) = on_argument(on, app, picker, interactive, "containers")? else {
                return Ok(ExitCode::from(CANCELLED));
            };
            let backend = backend.map(BackendId::from);
            let list = list_with_offers(app, &target, backend, input, output, error, interactive)?;
            print_listing(output, &list)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Shell {
            container,
            on,
            backend,
            user,
            new_terminal,
            hold,
            via,
        }) => {
            let backend = backend.map(BackendId::from);
            let Some(target) = on_argument(on, app, picker, interactive, "shell")? else {
                return Ok(ExitCode::from(CANCELLED));
            };
            // A held shell was resolved before its terminal was opened.
            let (backend, id, via) = match container {
                Some(id) if hold => (backend.unwrap_or(BackendId::Docker), id, via.map(Via::from)),
                container => {
                    let list =
                        list_with_offers(app, &target, backend, input, output, error, interactive)?;
                    let containers = &list.listing.containers;
                    if containers.is_empty() {
                        writeln!(error, "No running containers on {}.", list.target)
                            .map_err(|error| error.to_string())?;
                        return Ok(ExitCode::FAILURE);
                    }
                    let id = match container {
                        Some(reference) => match resolve(&reference, containers) {
                            Ok(container) => container.id.clone(),
                            Err(ResolveError::NotFound) => {
                                return Err(ContainerError::NoMatch {
                                    target: list.target,
                                    reference,
                                }
                                .to_string());
                            }
                            Err(ResolveError::Ambiguous(candidates)) => {
                                return Err(ContainerError::Ambiguous {
                                    target: list.target,
                                    reference,
                                    candidates,
                                }
                                .to_string());
                            }
                        },
                        None if interactive && picker.available() => {
                            let (ids, rows): (Vec<String>, Vec<Row>) =
                                container_rows(&list).into_iter().unzip();
                            match picker.pick_one("container", rows)? {
                                Some(index) => ids[index].clone(),
                                None => return Ok(ExitCode::from(CANCELLED)),
                            }
                        }
                        None => {
                            return Err("choose a container: portico shell <container> (no terminal for the picker)".into());
                        }
                    };
                    (list.backend, id, list.via)
                }
            };
            let outcome = match app.container_shell(
                &target,
                backend,
                &id,
                user.as_deref(),
                via,
                new_terminal && !hold,
                None,
                false,
            ) {
                Ok(outcome) => outcome,
                Err(failure) => return Err(report_container_failure(error, failure)?),
            };
            finish_connection(outcome, hold, input, error)
        }
        Some(Command::Tunnels { .. }) => tunnels::list(app, output),
        Some(Command::Tunnel { command }) => {
            tunnels::run(command, app, picker, input, output, error, interactive)
        }
        // `main` runs these through `run_desktop`, with the desktop environment.
        Some(Command::Desktop { .. }) => {
            Err("portico desktop needs the desktop environment".into())
        }
        Some(Command::Adopt { aliases, all, yes }) => {
            let interrupts = Interrupts::install(handle_signals)
                .map_err(|error| format!("cannot handle signals: {error}"))?;
            let migration = Migration {
                direction: Direction::Adopt,
                aliases,
                all,
                yes,
            };
            run_migration(
                &migration,
                app,
                picker,
                &interrupts,
                input,
                output,
                interactive,
            )
        }
        Some(Command::Export { aliases, all, yes }) => {
            let interrupts = Interrupts::install(handle_signals)
                .map_err(|error| format!("cannot handle signals: {error}"))?;
            let migration = Migration {
                direction: Direction::Export,
                aliases,
                all,
                yes,
            };
            run_migration(
                &migration,
                app,
                picker,
                &interrupts,
                input,
                output,
                interactive,
            )
        }
        Some(Command::Test {
            alias,
            accept_host_key,
            raw,
        }) => {
            let Some(alias) = host_argument(alias, app, picker, interactive, "test", false)? else {
                return Ok(ExitCode::from(CANCELLED));
            };
            let interrupts = Interrupts::install(handle_signals)
                .map_err(|error| format!("cannot handle signals: {error}"))?;
            test_command(
                app,
                alias,
                accept_host_key,
                raw,
                input,
                output,
                interactive,
                &interrupts,
            )
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

#[cfg(test)]
mod tests {
    use std::{fs, io::Cursor};

    use clap::Parser;
    use tempfile::TempDir;

    use crate::test_executable::write_executable;

    use super::*;
    use crate::{
        app::{App, ContainerPrograms},
        backend::{
            docker::{SHELL_SCRIPT, join_group_words},
            remote_command,
        },
        domain::host::Host,
        domain::managed::{ConfigPaths, ManagedStore},
        domain::openssh::OpenSsh,
        picker::Row,
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
            &mut Scripted::default(),
            &mut Cursor::new(b"y\n"),
            &mut output,
            &mut error,
            true,
            false,
        );
        assert_eq!(exit, std::process::ExitCode::SUCCESS);
        app.add(Host::new("dev", "dev.example", None, None, None, None).unwrap())
            .unwrap();

        let exit = run(
            Cli::try_parse_from(["portico", "remove", "dev"]).unwrap(),
            &app,
            &mut Scripted::default(),
            &mut Cursor::new(b"n\n"),
            &mut output,
            &mut error,
            true,
            false,
        );
        assert_eq!(exit, std::process::ExitCode::SUCCESS);
        assert!(app.show("dev").is_ok());

        let _ = run(
            Cli::try_parse_from(["portico", "remove", "dev"]).unwrap(),
            &app,
            &mut Scripted::default(),
            &mut Cursor::new(b"y\n"),
            &mut output,
            &mut error,
            true,
            false,
        );
        assert!(matches!(app.show("dev"), Err(AppError::UnknownHost(_))));
    }

    #[test]
    fn remove_does_not_prompt_for_hosts_that_cannot_be_removed() {
        let (dir, app) = test_app();
        app.initialize(true).unwrap();
        fs::write(
            dir.path().join(".ssh/config"),
            "Include ~/.ssh/portico/config\nHost legacy\n HostName legacy.example\n",
        )
        .unwrap();

        for alias in ["missing", "legacy"] {
            let mut output = Vec::new();
            let mut error = Vec::new();
            let exit = run(
                Cli::try_parse_from(["portico", "remove", alias]).unwrap(),
                &app,
                &mut Scripted::default(),
                &mut Cursor::new(b"y\n"),
                &mut output,
                &mut error,
                true,
                false,
            );
            assert_eq!(exit, std::process::ExitCode::FAILURE);
            assert!(output.is_empty(), "prompted for {alias}");
        }
    }

    /// An initialized app with a saved `dev` whose key is unknown until
    /// `~/.ssh/known_hosts` exists.
    fn unknown_key_app() -> (TempDir, App) {
        let (dir, app) = test_app();
        app.initialize(true).unwrap();
        app.add(Host::new("dev", "h.example", None, None, None, None).unwrap())
            .unwrap();
        write_executable(
            dir.path().join("ssh"),
            &format!(
                "#!/bin/sh\nif [ \"$1\" = '-G' ]; then printf 'hostname h\\nuser deploy\\nport 22\\nuserknownhostsfile ~/.ssh/known_hosts\\n'; exit 0; fi\nkh=; prev=\nfor a do\n  case \"$prev$a\" in -oUserKnownHostsFile=*) kh=${{a#UserKnownHostsFile=}} ;; esac\n  prev=; [ \"$a\" = -o ] && prev=-o\ndone\nif [ -n \"$kh\" ]; then echo '|1|salt|hash ssh-ed25519 AAAAkey' >> \"$kh\"; exit 255; fi\nif [ -f '{}' ]; then\n  printf 'Authenticated to 10.0.0.5 ([10.0.0.5]:22) using \"publickey\".\\n' >&2\nelse\n  echo 'Host key verification failed.' >&2; exit 255\nfi\n",
                dir.path().join(".ssh/known_hosts").display()
            ),
        );
        write_executable(
            dir.path().join("ssh-keygen"),
            "#!/bin/sh\nprintf '256 SHA256:abc |1|salt|hash (ED25519)\\n'\n",
        );
        let app = App::new(
            ManagedStore::new(ConfigPaths::from_home(dir.path())),
            OpenSsh::new(dir.path().join("ssh"), dir.path().join("ssh"))
                .with_keygen(dir.path().join("ssh-keygen")),
        );
        (dir, app)
    }

    fn run_test_command(app: &App, answer: &[u8]) -> (std::process::ExitCode, String) {
        let mut output = Vec::new();
        let exit = run(
            Cli::try_parse_from(["portico", "test", "dev"]).unwrap(),
            app,
            &mut Scripted::default(),
            &mut Cursor::new(answer),
            &mut output,
            &mut Vec::new(),
            true,
            false,
        );
        (exit, String::from_utf8(output).unwrap())
    }

    #[test]
    fn answering_y_trusts_the_key_and_retests() {
        let (dir, app) = unknown_key_app();
        let (exit, output) = run_test_command(&app, b"y\n");
        assert_eq!(exit, std::process::ExitCode::SUCCESS);
        assert_eq!(
            output,
            "dev: host key not trusted\n\
             dev presents an ED25519 key with fingerprint SHA256:abc\n\
             Compare it with the fingerprint from the server's administrator or console.\n\
             Trust this host key? [y/N] \
             dev: authenticated as deploy@10.0.0.5:22 (publickey)\n"
        );
        assert!(dir.path().join(".ssh/known_hosts").exists());
    }

    #[test]
    fn declining_trusts_nothing_and_exits_one() {
        let (dir, app) = unknown_key_app();
        let (exit, output) = run_test_command(&app, b"n\n");
        assert_eq!(exit, std::process::ExitCode::from(1));
        assert!(output.ends_with("Trust this host key? [y/N] Host key not trusted.\n"));
        assert!(!dir.path().join(".ssh/known_hosts").exists());
    }

    const LISTING: &str = r#"{"id":"aaaa1111","name":"web","image":"nginx","status":"Up"}"#;
    const DENIED: &str = "permission denied while trying to connect to the Docker daemon socket at unix:///var/run/docker.sock";

    /// An app whose local container programs are fakes in a temporary directory. Each fake
    /// appends its arguments to `<name>.calls`, one call per line as `[arg][arg]…`, then
    /// sources `<name>.behaviour` when it exists.
    fn container_app() -> (TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        for name in ["docker", "sudo", "sg", "sh", "ssh"] {
            let base = dir.path().join(name);
            write_executable(
                &base,
                &format!(
                    "#!/bin/sh\nprintf '[%s]' \"$@\" >> '{base}.calls'\necho >> '{base}.calls'\nif [ -f '{base}.behaviour' ]; then . '{base}.behaviour'; fi\nexit 0\n",
                    base = base.display()
                ),
            );
        }
        let program = |name: &str| dir.path().join(name);
        let app = App::new(
            ManagedStore::new(ConfigPaths::from_home(dir.path())),
            OpenSsh::new(program("ssh"), program("terminal")),
        )
        .with_container_programs(ContainerPrograms {
            docker: program("docker"),
            sudo: program("sudo"),
            sg: program("sg"),
            sh: program("sh"),
            pct: program("pct"),
        });
        (dir, app)
    }

    fn behave(dir: &TempDir, name: &str, script: &str) {
        fs::write(dir.path().join(format!("{name}.behaviour")), script).unwrap();
    }

    fn calls(dir: &TempDir, name: &str) -> String {
        fs::read_to_string(dir.path().join(format!("{name}.calls"))).unwrap_or_default()
    }

    /// Docker refuses the socket; the probe reports `me` outside the docker group until
    /// `sudo` has run, and `sg` lists once it has.
    fn refused_until_joined(dir: &TempDir) {
        let joined = dir.path().join("joined");
        behave(dir, "docker", &format!("echo '{DENIED}' >&2\nexit 1\n"));
        behave(
            dir,
            "sh",
            &format!(
                "database=me\n[ -f '{joined}' ] && database='me docker'\nprintf 'user=me\\nprocess=me\\ndatabase=%s\\ngroup=yes\\nsocket=/var/run/docker.sock\\nsocket_group=docker\\nsg=yes\\nnewgrp=yes\\nsystemctl=yes\\n' \"$database\"\n",
                joined = joined.display()
            ),
        );
        behave(dir, "sudo", &format!("touch '{}'\n", joined.display()));
        behave(dir, "sg", &format!("echo '{LISTING}'\n"));
    }

    fn run_containers(app: &App, answer: &[u8]) -> (ExitCode, String, String) {
        let mut output = Vec::new();
        let mut error = Vec::new();
        let exit = run(
            Cli::try_parse_from(["portico", "containers"]).unwrap(),
            app,
            &mut Scripted::default(),
            &mut Cursor::new(answer),
            &mut output,
            &mut error,
            true,
            false,
        );
        (
            exit,
            String::from_utf8(output).unwrap(),
            String::from_utf8(error).unwrap(),
        )
    }

    #[test]
    fn join_offer_accepted_runs_sudo_then_lists() {
        let (dir, app) = container_app();
        refused_until_joined(&dir);

        let (exit, output, error) = run_containers(&app, b"y\n");

        assert_eq!(exit, ExitCode::SUCCESS, "{error}");
        let join = join_group_words("me", "/var/run/docker.sock");
        let expected_sudo: String = join[1..].iter().map(|word| format!("[{word}]")).collect();
        assert_eq!(calls(&dir, "sudo"), format!("{expected_sudo}\n"));
        assert_eq!(
            output,
            format!(
                "Add me to the docker group on this machine? Members of the docker group have root-equivalent access. This runs: {} [y/N] \
                 Other\n  web  web  nginx  Up\n\
                 Ran docker through `sg docker`: other shells need `newgrp docker` or a new login.\n",
                remote_command(&join)
            )
        );
        assert_eq!(
            error,
            "error: me cannot use Docker on this machine: permission denied on /var/run/docker.sock.\n"
        );
    }

    #[test]
    fn join_offer_declined_lists_options() {
        let (dir, app) = container_app();
        refused_until_joined(&dir);

        let (exit, output, error) = run_containers(&app, b"n\n");

        assert_eq!(exit, ExitCode::FAILURE);
        assert!(calls(&dir, "sudo").is_empty());
        assert!(output.ends_with("[y/N] "), "{output}");
        assert!(
            error.starts_with(
                "error: me cannot use Docker on this machine: permission denied on /var/run/docker.sock.\n  Add me to the docker group (root-equivalent access): sudo sh -c "
            ),
            "{error}"
        );
        assert!(error.contains("\n  Or use rootless Docker: "), "{error}");
    }

    #[test]
    fn a_failed_fix_stops_without_listing_again() {
        let (dir, app) = container_app();
        refused_until_joined(&dir);
        behave(&dir, "sudo", "exit 1\n");

        let (exit, _, error) = run_containers(&app, b"y\n");

        assert_eq!(exit, ExitCode::FAILURE);
        assert_eq!(calls(&dir, "docker").lines().count(), 1, "listed again");
        assert!(
            error.ends_with(&format!(
                "error: {} exited with exit status: 1\n",
                remote_command(&join_group_words("me", "/var/run/docker.sock"))
            )),
            "{error}"
        );
    }

    #[test]
    fn start_daemon_offer_runs_systemctl_through_sudo() {
        let (dir, app) = container_app();
        behave(
            &dir,
            "docker",
            "echo 'Cannot connect to the Docker daemon at unix:///var/run/docker.sock.' >&2\nexit 1\n",
        );
        behave(
            &dir,
            "sh",
            "printf 'user=me\\nprocess=me\\ndatabase=me\\ngroup=yes\\nsocket=/var/run/docker.sock\\nsocket_group=docker\\nsg=yes\\nnewgrp=yes\\nsystemctl=yes\\n'\n",
        );

        let (exit, output, _) = run_containers(&app, b"y\n");

        // The fake daemon stays down, so the second listing fails without a second offer.
        assert_eq!(exit, ExitCode::FAILURE);
        assert_eq!(calls(&dir, "sudo"), "[systemctl][start][docker]\n");
        assert_eq!(
            output,
            "Start the Docker service on this machine? This runs: sudo systemctl start docker [y/N] "
        );
    }

    /// Two containers outside any project: `alpha` (`aaaa1111`) then `beta` (`bbbb2222`).
    fn two_containers(dir: &TempDir) {
        behave(
            dir,
            "docker",
            r#"if [ "$1" = ps ]; then
echo '{"id":"bbbb2222","name":"beta","image":"redis","status":"Up 1 hour"}'
echo '{"id":"aaaa1111","name":"alpha","image":"nginx","status":"Up 2 hours"}'
fi
"#,
        );
    }

    fn run_shell(app: &App, picker: &mut Scripted, interactive: bool) -> (ExitCode, String) {
        let mut output = Vec::new();
        let exit = run(
            Cli::try_parse_from(["portico", "shell"]).unwrap(),
            app,
            picker,
            &mut Cursor::new(b""),
            &mut output,
            &mut Vec::new(),
            interactive,
            false,
        );
        (exit, String::from_utf8(output).unwrap())
    }

    fn exec_call(id: &str) -> String {
        format!("[exec][-it][{id}][sh][-c][{SHELL_SCRIPT}]\n")
    }

    /// A picker that answers from a script, by the first field of a row, and records what it
    /// was shown. `None` cancels; `pick_many` takes a comma-separated list.
    #[derive(Default)]
    struct Scripted {
        answers: std::collections::VecDeque<Option<&'static str>>,
        seen: Vec<(String, Vec<Row>)>,
        /// Stands for output that is not a terminal, so no picker can be drawn.
        unavailable: bool,
    }

    impl Scripted {
        fn new(answers: &[Option<&'static str>]) -> Self {
            Self {
                answers: answers.iter().copied().collect(),
                ..Self::default()
            }
        }

        fn answer(&mut self, prompt: &str, rows: Vec<Row>) -> Option<Vec<usize>> {
            let answer = self.answers.pop_front().expect("an unexpected pick");
            let picked = answer.map(|labels| {
                labels
                    .split(',')
                    .map(|label| {
                        rows.iter()
                            .position(|row| row.fields[0] == label)
                            .unwrap_or_else(|| panic!("no row {label}"))
                    })
                    .collect()
            });
            self.seen.push((prompt.into(), rows));
            picked
        }

        fn prompts(&self) -> Vec<&str> {
            self.seen
                .iter()
                .map(|(prompt, _)| prompt.as_str())
                .collect()
        }

        /// The first field of each row shown by the `pick`th call.
        fn labels(&self, pick: usize) -> Vec<&str> {
            self.seen[pick]
                .1
                .iter()
                .map(|row| row.fields[0].as_str())
                .collect()
        }
    }

    impl Picker for Scripted {
        fn available(&self) -> bool {
            !self.unavailable
        }

        fn pick_one(&mut self, prompt: &str, rows: Vec<Row>) -> Result<Option<usize>, String> {
            Ok(self.answer(prompt, rows).map(|picked| picked[0]))
        }

        fn pick_many(&mut self, title: &str, rows: Vec<Row>) -> Result<Option<Vec<usize>>, String> {
            Ok(self.answer(title, rows))
        }
    }

    fn row(fields: &[&str], extra: &[&str]) -> Row {
        Row {
            fields: fields.iter().map(|field| (*field).into()).collect(),
            extra: extra.iter().map(|extra| (*extra).into()).collect(),
        }
    }

    #[test]
    fn shell_picks_a_container_through_the_picker() {
        let (dir, app) = container_app();
        two_containers(&dir);
        let mut picker = Scripted::new(&[Some("beta")]);

        let (exit, output) = run_shell(&app, &mut picker, true);

        assert_eq!(exit, ExitCode::SUCCESS);
        assert!(output.is_empty(), "{output}");
        assert!(calls(&dir, "docker").ends_with(&exec_call("bbbb2222")));
    }

    #[test]
    fn shell_container_rows_search_names_project_and_image() {
        let (dir, app) = container_app();
        two_containers(&dir);
        let mut picker = Scripted::new(&[None]);

        let _ = run_shell(&app, &mut picker, true);

        assert_eq!(
            picker.seen,
            vec![(
                "container".to_owned(),
                vec![
                    row(
                        &["alpha", "alpha", "", "", "nginx"],
                        &["Other", "Up 2 hours"]
                    ),
                    row(&["beta", "beta", "", "", "redis"], &["Other", "Up 1 hour"]),
                ]
            )]
        );
    }

    #[test]
    fn shell_picker_cancelled_exits_130_and_runs_nothing() {
        let (dir, app) = container_app();
        two_containers(&dir);

        let (exit, _) = run_shell(&app, &mut Scripted::new(&[None]), true);

        assert_eq!(exit, ExitCode::from(130));
        assert!(!calls(&dir, "docker").contains("[exec]"));
    }

    #[test]
    fn shell_without_terminal_still_needs_a_container() {
        let (dir, app) = container_app();
        two_containers(&dir);
        let mut error = Vec::new();

        let exit = run(
            Cli::try_parse_from(["portico", "shell"]).unwrap(),
            &app,
            &mut Scripted::default(),
            &mut Cursor::new(b""),
            &mut Vec::new(),
            &mut error,
            false,
            false,
        );

        assert_eq!(exit, ExitCode::FAILURE);
        assert_eq!(
            String::from_utf8(error).unwrap(),
            "error: choose a container: portico shell <container> (no terminal for the picker)\n"
        );
    }

    /// A managed `dev` and an external `legacy`.
    fn two_hosts() -> (TempDir, App) {
        let (dir, app) = adoption_app("Host legacy\n  HostName legacy.example\n");
        let (exit, _, error) = run_command(&app, &["add", "dev", "-H", "dev.example"], "", false);
        assert_eq!(exit, ExitCode::SUCCESS, "{error}");
        (dir, app)
    }

    #[test]
    fn parses_optional_aliases_and_bare_on() {
        let parse = |args: &[&str]| {
            Cli::try_parse_from(std::iter::once("portico").chain(args.iter().copied()))
                .unwrap()
                .command
        };
        assert!(matches!(
            parse(&["connect"]),
            Some(Command::Connect { alias: None, .. })
        ));
        assert!(matches!(
            parse(&["shell", "--on"]),
            Some(Command::Shell { on: Some(None), .. })
        ));
        assert!(matches!(
            parse(&["shell", "--on", "box"]),
            Some(Command::Shell { on: Some(Some(alias)), .. }) if alias == "box"
        ));
        assert!(matches!(
            parse(&["containers"]),
            Some(Command::Containers { on: None, .. })
        ));
    }

    #[test]
    fn connect_test_show_list_every_host() {
        let (_dir, app) = two_hosts();
        let mut picker = Scripted::new(&[Some("legacy")]);

        let (exit, output, error) = run_picking(&app, &["show"], "", true, &mut picker);

        assert_eq!(exit, ExitCode::SUCCESS, "{error}");
        assert!(output.starts_with("Alias: legacy\n"), "{output}");
        assert_eq!(picker.prompts(), ["host"]);
        assert_eq!(picker.labels(0), ["dev", "legacy"]);
        let extra: Vec<&str> = picker.seen[0]
            .1
            .iter()
            .map(|row| row.extra[0].as_str())
            .collect();
        assert_eq!(extra, ["managed", "external"]);
        assert!(picker.seen[0].1[1].extra[1].ends_with(".ssh/config"));
    }

    #[test]
    fn edit_and_remove_list_managed_hosts_only() {
        let (_dir, app) = two_hosts();
        for args in [&["edit", "-u", "x"][..], &["remove"]] {
            let mut picker = Scripted::new(&[None]);
            let _ = run_picking(&app, args, "", true, &mut picker);
            assert_eq!(picker.labels(0), ["dev"], "{args:?}");
        }
    }

    #[test]
    fn edit_without_alias_applies_flags_to_the_pick() {
        let (_dir, app) = two_hosts();
        let mut picker = Scripted::new(&[Some("dev")]);

        let (exit, output, error) =
            run_picking(&app, &["edit", "-u", "root"], "", true, &mut picker);

        assert_eq!(exit, ExitCode::SUCCESS, "{error}");
        assert_eq!(output, "Updated dev.\n");
        let details = app.show("dev").unwrap();
        assert_eq!(details.declared.unwrap().user(), Some("root"));
    }

    #[test]
    fn edit_without_alias_rejects_conflicting_flags_before_picking() {
        let (_dir, app) = two_hosts();
        let mut picker = Scripted::new(&[]);

        let (exit, _, error) = run_picking(
            &app,
            &["edit", "-u", "x", "--unset", "user"],
            "",
            true,
            &mut picker,
        );

        assert_eq!(exit, ExitCode::FAILURE);
        assert_eq!(error, "error: --unset user conflicts with --user\n");
        assert!(picker.prompts().is_empty());
    }

    #[test]
    fn remove_without_alias_still_confirms() {
        let (_dir, app) = two_hosts();
        let mut picker = Scripted::new(&[Some("dev")]);

        let (exit, output, _) = run_picking(&app, &["remove"], "n\n", true, &mut picker);

        assert_eq!(exit, ExitCode::SUCCESS);
        assert_eq!(output, "Remove dev? [y/N] Removal cancelled.\n");
        assert_eq!(aliases_of(&app, Ownership::Managed), ["dev"]);
    }

    #[test]
    fn cancelled_host_pick_exits_130_and_changes_nothing() {
        let (_dir, app) = two_hosts();
        let mut picker = Scripted::new(&[None]);

        let (exit, output, _) = run_picking(&app, &["remove", "-f"], "", true, &mut picker);

        assert_eq!(exit, ExitCode::from(130));
        assert!(output.is_empty(), "{output}");
        assert_eq!(aliases_of(&app, Ownership::Managed), ["dev"]);
    }

    #[test]
    fn no_hosts_and_no_managed_hosts() {
        let (_dir, app) = adoption_app("");
        let (exit, _, error) = run_command(&app, &["show"], "", true);
        assert_eq!(exit, ExitCode::FAILURE);
        assert_eq!(error, "error: No hosts.\n");

        let (_dir, app) = adoption_app("Host legacy\n  HostName legacy.example\n");
        let (exit, _, error) = run_command(&app, &["edit", "-u", "x"], "", true);
        assert_eq!(exit, ExitCode::FAILURE);
        assert_eq!(error, "error: No managed hosts.\n");
    }

    #[test]
    fn single_host_still_opens_the_picker() {
        let (_dir, app) = adoption_app("Host legacy\n  HostName legacy.example\n");
        let mut picker = Scripted::new(&[None]);

        let (exit, _, _) = run_picking(&app, &["show"], "", true, &mut picker);

        assert_eq!(exit, ExitCode::from(130));
        assert_eq!(picker.labels(0), ["legacy"]);
    }

    #[test]
    fn without_terminal_the_alias_is_required() {
        let (_dir, app) = two_hosts();
        for (args, message) in [
            (
                &["connect"][..],
                "choose a host: portico connect <alias> (no terminal for the picker)",
            ),
            (
                &["edit", "-u", "x"],
                "choose a host: portico edit <alias> (no terminal for the picker)",
            ),
            (
                &["shell", "--on"],
                "choose a host: portico shell --on <alias> (no terminal for the picker)",
            ),
            (
                &["containers", "--on"],
                "choose a host: portico containers --on <alias> (no terminal for the picker)",
            ),
        ] {
            let (exit, _, error) = run_command(&app, args, "", false);
            assert_eq!(exit, ExitCode::FAILURE, "{args:?}");
            assert_eq!(error, format!("error: {message}\n"));
        }
    }

    #[test]
    fn without_a_terminal_for_the_picker_nothing_is_asked() {
        let (dir, app) = two_hosts();
        two_containers(&dir);
        for (args, message) in [
            (
                &["show"][..],
                "choose a host: portico show <alias> (no terminal for the picker)",
            ),
            (
                &["containers", "--on"],
                "choose a host: portico containers --on <alias> (no terminal for the picker)",
            ),
            (
                &["adopt"],
                "adopt requires host aliases or --all in non-interactive mode",
            ),
        ] {
            let mut picker = Scripted {
                unavailable: true,
                ..Scripted::default()
            };
            let (exit, _, error) = run_picking(&app, args, "", true, &mut picker);
            assert_eq!(exit, ExitCode::FAILURE, "{args:?}");
            assert_eq!(error, format!("error: {message}\n"));
            assert!(picker.seen.is_empty());
        }
    }

    #[test]
    fn bare_on_picks_the_host_then_the_container() {
        let (dir, app) = container_app();
        app.initialize(true).unwrap();
        let (exit, _, error) = run_command(&app, &["add", "box", "-H", "box.example"], "", false);
        assert_eq!(exit, ExitCode::SUCCESS, "{error}");
        behave(
            &dir,
            "ssh",
            &format!("case \"$*\" in *' ps '*) echo '{LISTING}' ;; esac\n"),
        );
        let mut picker = Scripted::new(&[Some("box"), Some("web")]);

        let (exit, _, error) = run_picking(&app, &["shell", "--on"], "", true, &mut picker);

        assert_eq!(exit, ExitCode::SUCCESS, "{error}");
        assert_eq!(picker.prompts(), ["host", "container"]);
        let ssh = calls(&dir, "ssh");
        let last = ssh.lines().last().unwrap();
        assert!(last.contains("[box]") && last.contains("aaaa1111"), "{ssh}");
        assert!(calls(&dir, "docker").is_empty());
    }

    #[test]
    fn parses_shell_arguments() {
        let cli = Cli::try_parse_from([
            "portico", "shell", "--hold", "--on", "dev", "-u", "root", "--via", "sg", "--", "abcd",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Shell {
                container: Some(container),
                on: Some(on),
                user: Some(user),
                new_terminal: false,
                hold: true,
                via: Some(ViaArg::Sg),
                backend: None,
            }) if container == "abcd" && on.as_deref() == Some("dev") && user == "root"
        ));
    }

    /// An initialized app whose entry config declares `hosts` below the Portico include.
    fn adoption_app(hosts: &str) -> (TempDir, App) {
        let (dir, app) = test_app();
        app.initialize(true).unwrap();
        fs::write(
            dir.path().join(".ssh/config"),
            format!("Include ~/.ssh/portico/config\n{hosts}"),
        )
        .unwrap();
        (dir, app)
    }

    const THREE_HOSTS: &str = "Host one\n  HostName one.example\n\nHost two\n  HostName two.example\n\nHost three\n  HostName three.example\n\nHost multi other\n  HostName m.example\n";

    fn run_command(
        app: &App,
        args: &[&str],
        input: &str,
        interactive: bool,
    ) -> (std::process::ExitCode, String, String) {
        run_picking(app, args, input, interactive, &mut Scripted::default())
    }

    fn run_picking(
        app: &App,
        args: &[&str],
        input: &str,
        interactive: bool,
        picker: &mut Scripted,
    ) -> (std::process::ExitCode, String, String) {
        let (mut output, mut error) = (Vec::new(), Vec::new());
        let exit = run(
            Cli::try_parse_from(std::iter::once("portico").chain(args.iter().copied())).unwrap(),
            app,
            picker,
            &mut Cursor::new(input.as_bytes().to_vec()),
            &mut output,
            &mut error,
            interactive,
            false,
        );
        (
            exit,
            String::from_utf8(output).unwrap(),
            String::from_utf8(error).unwrap(),
        )
    }

    fn aliases_of(app: &App, ownership: Ownership) -> Vec<String> {
        app.hosts()
            .unwrap()
            .hosts
            .into_iter()
            .filter(|host| host.ownership == ownership)
            .map(|host| host.alias)
            .collect()
    }

    #[test]
    fn adopt_parses_aliases_flags_and_rejects_all_with_aliases() {
        let cli = Cli::try_parse_from(["portico", "adopt", "a", "b", "-y"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Adopt { aliases, all: false, yes: true }) if aliases == ["a", "b"]
        ));
        let cli = Cli::try_parse_from(["portico", "export", "-A"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Export { all: true, .. })
        ));
        assert!(Cli::try_parse_from(["portico", "adopt", "a", "--all"]).is_err());
    }

    #[test]
    fn interactive_adopt_moves_the_picked_hosts() {
        let (_dir, app) = adoption_app(THREE_HOSTS);
        let mut picker = Scripted::new(&[Some("one,three")]);

        let (exit, output, error) = run_picking(&app, &["adopt"], "y\n", true, &mut picker);

        assert_eq!(exit, ExitCode::SUCCESS, "{error}");
        assert_eq!(picker.prompts(), ["adopt"]);
        assert_eq!(
            picker.seen[0].1,
            vec![
                row(&["one"], &["~/.ssh/config"]),
                row(&["two"], &["~/.ssh/config"]),
                row(&["three"], &["~/.ssh/config"]),
            ]
        );
        assert!(output.contains("Adopt 2 hosts? [y/N] "), "{output}");
        assert!(output.ends_with("Adopted: one, three\n"), "{output}");
        assert_eq!(aliases_of(&app, Ownership::Managed), ["one", "three"]);
    }

    #[test]
    fn ineligible_hosts_are_printed_before_the_picker() {
        let (_dir, app) = adoption_app(THREE_HOSTS);
        let mut picker = Scripted::new(&[None]);

        let (_, output, _) = run_picking(&app, &["adopt"], "", true, &mut picker);

        assert_eq!(
            output,
            "Not eligible:\n  multi  ~/.ssh/config: Host line declares 2 aliases\n  other  ~/.ssh/config: Host line declares 2 aliases\n"
        );
        assert_eq!(picker.labels(0), ["one", "two", "three"]);
    }

    #[test]
    fn cancelled_adoption_exits_130_and_changes_nothing() {
        let (dir, app) = adoption_app(THREE_HOSTS);
        let entry = fs::read(dir.path().join(".ssh/config")).unwrap();

        let (exit, _, _) = run_picking(&app, &["adopt"], "", true, &mut Scripted::new(&[None]));

        assert_eq!(exit, ExitCode::from(130));
        assert_eq!(fs::read(dir.path().join(".ssh/config")).unwrap(), entry);
        assert!(aliases_of(&app, Ownership::Managed).is_empty());
    }

    #[test]
    fn declined_preview_changes_nothing() {
        let (_dir, app) = adoption_app(THREE_HOSTS);
        let mut picker = Scripted::new(&[Some("one")]);

        let (exit, output, _) = run_picking(&app, &["adopt"], "n\n", true, &mut picker);

        assert_eq!(exit, ExitCode::SUCCESS);
        assert!(output.ends_with("Adoption cancelled.\n"), "{output}");
        assert!(aliases_of(&app, Ownership::Managed).is_empty());
    }

    #[test]
    fn adopt_without_eligible_hosts_says_so_and_succeeds() {
        let (_dir, app) = adoption_app("Host multi other\n  HostName m.example\n");
        let (exit, output, _) = run_command(&app, &["adopt"], "", true);
        assert_eq!(exit, std::process::ExitCode::SUCCESS);
        assert_eq!(
            output,
            "No host can be adopted.\nNot eligible:\n  multi  ~/.ssh/config: Host line declares 2 aliases\n  other  ~/.ssh/config: Host line declares 2 aliases\n"
        );
        let (exit, output, _) = run_command(&app, &["adopt", "--all", "-y"], "", false);
        assert_eq!(exit, std::process::ExitCode::SUCCESS);
        assert_eq!(
            output,
            "Skipped multi: Host line declares 2 aliases\nSkipped other: Host line declares 2 aliases\nNo host can be adopted.\n"
        );
    }

    #[test]
    fn non_interactive_adopt_needs_aliases_or_all_and_yes() {
        let (_dir, app) = adoption_app(THREE_HOSTS);
        let (exit, output, error) = run_command(&app, &["adopt"], "", false);
        assert_eq!(exit, std::process::ExitCode::FAILURE);
        assert!(output.is_empty());
        assert_eq!(
            error,
            "error: adopt requires host aliases or --all in non-interactive mode\n"
        );
        let (_, _, error) = run_command(&app, &["adopt", "one"], "y\n", false);
        assert_eq!(
            error,
            "error: adopt requires --yes in non-interactive mode\n"
        );
        assert!(aliases_of(&app, Ownership::Managed).is_empty());
    }

    #[test]
    fn interactive_export_moves_picked_hosts_back() {
        let (_dir, app) = adoption_app("");
        app.add(Host::new("dev", "dev.example", None, None, None, None).unwrap())
            .unwrap();
        let mut picker = Scripted::new(&[Some("dev")]);

        let (exit, output, error) = run_picking(&app, &["export"], "y\n", true, &mut picker);

        assert_eq!(exit, ExitCode::SUCCESS, "{error}");
        assert_eq!(picker.prompts(), ["export"]);
        assert_eq!(picker.seen[0].1, vec![row(&["dev"], &["managed"])]);
        assert!(output.contains("Export 1 host? [y/N] "), "{output}");
        assert!(output.ends_with("Exported: dev\n"));
        assert_eq!(aliases_of(&app, Ownership::External), ["dev"]);

        let (_, output, _) = run_command(&app, &["export"], "", true);
        assert_eq!(output, "No host can be exported.\n");
    }

    #[test]
    fn each_step_starts_with_a_clear_flag_and_a_late_flag_is_reported() {
        let interrupts = Interrupts::install(false).unwrap();
        interrupts.cancelled.store(true, Ordering::SeqCst);
        assert!(matches!(
            interrupts.during(|cancelled| cancelled.load(Ordering::SeqCst)),
            Step::Done(false)
        ));
        assert!(matches!(
            interrupts.during(|cancelled| cancelled.store(true, Ordering::SeqCst)),
            Step::Late(())
        ));
        assert!(matches!(
            interrupts.during(|cancelled| cancelled.load(Ordering::SeqCst)),
            Step::Done(false)
        ));
    }

    fn run_interrupted(
        app: &App,
        accept_host_key: Option<&str>,
        input: &[u8],
        interrupts: &Interrupts,
    ) -> (ExitCode, String) {
        let mut output = Vec::new();
        let exit = test_command(
            app,
            "dev".into(),
            accept_host_key.map(String::from),
            false,
            &mut Cursor::new(input),
            &mut output,
            true,
            interrupts,
        )
        .unwrap();
        (exit, String::from_utf8(output).unwrap())
    }

    #[test]
    fn a_late_signal_after_an_unknown_key_cancels_before_the_prompt() {
        let (dir, app) = unknown_key_app();

        let (exit, output) = run_interrupted(&app, None, b"y\n", &Interrupts::signalled_after(1));

        assert_eq!(exit, ExitCode::from(CANCELLED), "{output}");
        assert!(output.contains("SHA256:abc"), "{output}");
        assert!(output.ends_with("dev: cancelled\n"), "{output}");
        assert!(!output.contains("Trust this host key?"), "{output}");
        assert!(!dir.path().join(".ssh/known_hosts").exists());
    }

    #[test]
    fn a_late_signal_after_the_retest_keeps_its_result() {
        let (dir, app) = unknown_key_app();

        let (exit, output) = run_interrupted(
            &app,
            Some("SHA256:abc"),
            b"",
            &Interrupts::signalled_after(2),
        );

        assert_eq!(exit, ExitCode::SUCCESS, "{output}");
        assert!(output.contains("authenticated"), "{output}");
        assert!(dir.path().join(".ssh/known_hosts").exists());
    }

    #[test]
    fn a_late_signal_after_an_ordinary_result_keeps_it() {
        let (dir, app) = unknown_key_app();
        fs::write(dir.path().join(".ssh/known_hosts"), "").unwrap();

        let (exit, output) = run_interrupted(&app, None, b"", &Interrupts::signalled_after(1));

        assert_eq!(exit, ExitCode::SUCCESS, "{output}");
        assert!(output.contains("authenticated"), "{output}");
    }

    /// A picker that fails the test when it is asked anything.
    struct Forbidden;

    impl Picker for Forbidden {
        fn available(&self) -> bool {
            true
        }

        fn pick_one(&mut self, prompt: &str, _: Vec<Row>) -> Result<Option<usize>, String> {
            panic!("--json opened the {prompt} picker")
        }

        fn pick_many(&mut self, title: &str, _: Vec<Row>) -> Result<Option<Vec<usize>>, String> {
            panic!("--json opened the {title} checklist")
        }
    }

    #[test]
    fn json_commands_never_pick_or_prompt_even_on_a_terminal() {
        let (_dir, app) = test_app();
        app.initialize(true).unwrap();
        app.add(Host::new("dev", "dev.example", None, None, None, None).unwrap())
            .unwrap();
        for args in [
            &["show", "--json"][..],
            &["containers", "--on", "--json"],
            &["tunnel", "show", "--json"],
            &["tunnel", "start", "--json"],
            &["tunnel", "stop", "--json"],
        ] {
            let cli = Cli::try_parse_from(std::iter::once("portico").chain(args.iter().copied()))
                .unwrap();
            let mut output = Vec::new();
            let mut error = Vec::new();
            let exit = run(
                cli,
                &app,
                &mut Forbidden,
                // A confirmation would read this.
                &mut Cursor::new(b"y\n"),
                &mut output,
                &mut error,
                true,
                false,
            );
            assert_eq!(exit, ExitCode::FAILURE, "{args:?}");
            assert!(error.is_empty(), "{args:?}");
            let document: serde_json::Value = serde_json::from_slice(&output).unwrap();
            assert_eq!(document["schema"], 1, "{args:?}");
            assert_eq!(document["error"]["kind"], "usage", "{args:?}");
        }
    }

    fn test_app() -> (TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join("ssh");
        write_executable(
            &ssh,
            "#!/bin/sh\nif [ \"$1\" = '-G' ]; then printf 'hostname resolved.example\\nport 22\\n'; fi\n",
        );
        let app = App::new(
            ManagedStore::new(ConfigPaths::from_home(dir.path())),
            OpenSsh::new(ssh.clone(), ssh),
        );
        (dir, app)
    }
}
