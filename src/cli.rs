use std::{
    collections::BTreeSet,
    io::{self, BufRead, Write},
    path::Path,
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::{
    adopt::{Candidate, Direction},
    app::{
        App, AppError, ContainerError, ContainerList, DockerIssue, HostDetails, HostSummary, Offer,
        Ownership, Target, TestOutcome, TestReport, TestTarget,
    },
    container::{GroupTool, ResolveError, display_name, group, remote_command, resolve},
    discovery::DiscoveryWarning,
    explain::Explanation,
    host::{Change, ClearField, Host, HostChanges},
    managed::{InitOutcome, InitPlan},
    openssh::{ConnectionOutcome, OpenSshError, PresentedKey},
    picker::{Picker, Row},
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
        alias: Option<String>,
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
    Containers {
        /// List on this host; without an alias, choose one.
        #[arg(long = "on", value_name = "ALIAS", num_args = 0..=1)]
        on: Option<Option<String>>,
    },
    Shell {
        container: Option<String>,
        /// Open the shell on this host; without an alias, choose one.
        #[arg(long = "on", value_name = "ALIAS", num_args = 0..=1)]
        on: Option<Option<String>>,
        #[arg(short = 'u', long)]
        user: Option<String>,
        #[arg(short = 'n', long = "new-terminal")]
        new_terminal: bool,
        /// Open the given container ID without listing, and wait for Enter after an explained
        /// failure; used in a new terminal.
        #[arg(long, hide = true)]
        hold: bool,
        /// Run docker inside the docker group, as the listing before the new terminal did.
        #[arg(long, hide = true, value_enum)]
        via: Option<Via>,
    },
}

/// `--via`: the tool that runs docker inside the docker group.
#[derive(clap::ValueEnum, Clone, Copy, Debug, Eq, PartialEq)]
enum Via {
    Sg,
}

impl From<Via> for GroupTool {
    fn from(via: Via) -> Self {
        match via {
            Via::Sg => GroupTool::Sg,
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
        Some(Command::Hosts) => {
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
        Some(Command::Show { alias }) => {
            let Some(alias) = host_argument(alias, app, picker, interactive, "show", false)? else {
                return Ok(ExitCode::from(CANCELLED));
            };
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
        Some(Command::Containers { on }) => {
            let Some(target) = on_argument(on, app, picker, interactive, "containers")? else {
                return Ok(ExitCode::from(CANCELLED));
            };
            let list = list_with_offers(app, &target, input, output, error, interactive)?;
            print_listing(output, &list)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Shell {
            container,
            on,
            user,
            new_terminal,
            hold,
            via,
        }) => {
            let Some(target) = on_argument(on, app, picker, interactive, "shell")? else {
                return Ok(ExitCode::from(CANCELLED));
            };
            // A held shell was resolved before its terminal was opened.
            let (id, via) = match container {
                Some(id) if hold => (id, via.map(GroupTool::from)),
                container => {
                    let list = list_with_offers(app, &target, input, output, error, interactive)?;
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
                    (id, list.via)
                }
            };
            let outcome = match app.container_shell(
                &target,
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

/// `portico test`: tests `alias`, offers to trust an unknown host key and tests again.
#[allow(clippy::too_many_arguments)]
fn test_command<I: BufRead, O: Write>(
    app: &App,
    alias: String,
    accept_host_key: Option<String>,
    raw: bool,
    input: &mut I,
    output: &mut O,
    interactive: bool,
    interrupts: &Interrupts,
) -> Result<ExitCode, String> {
    let target = TestTarget::Saved(alias);
    let (outcome, late) =
        match interrupts.during(|cancelled| app.test_cancellable(&target, cancelled)) {
            Step::Done(outcome) => (outcome, false),
            Step::Late(outcome) => (outcome, true),
        };
    let outcome = match outcome {
        Err(AppError::OpenSsh(OpenSshError::Cancelled)) => {
            return print_cancelled(output, target.alias());
        }
        outcome => outcome.map_err(|error| error.to_string())?,
    };
    let (report, key) = match outcome {
        TestOutcome::Done(report) => return print_report(output, &report, raw),
        TestOutcome::UnknownHostKey { report, key } => (report, key),
    };
    print_untrusted_key(output, &report.alias, &key)?;
    if late {
        return print_cancelled(output, target.alias());
    }
    let trusted = match accept_host_key {
        Some(expected) => {
            let matches = !key.fingerprints.is_empty()
                && key
                    .fingerprints
                    .iter()
                    .all(|presented| presented.fingerprint == expected);
            if !matches {
                writeln!(
                    output,
                    "{}: failed \u{2014} the host key fingerprint does not match --accept-host-key",
                    report.alias
                )
                .map_err(|error| error.to_string())?;
                return Ok(ExitCode::from(1));
            }
            true
        }
        None if interactive => confirm(input, output, "Trust this host key? [y/N] ")?,
        None => {
            // --accept-host-key needs one fingerprint that every key shares.
            let shared = match key.fingerprints.split_first() {
                Some((first, rest))
                    if rest.iter().all(|key| key.fingerprint == first.fingerprint) =>
                {
                    first.fingerprint.as_str()
                }
                _ => "<fingerprint>",
            };
            writeln!(output, "hint: pass --accept-host-key {shared} to trust it")
                .map_err(|error| error.to_string())?;
            return Ok(ExitCode::from(1));
        }
    };
    if !trusted {
        writeln!(output, "Host key not trusted.").map_err(|error| error.to_string())?;
        return Ok(ExitCode::from(1));
    }
    // The key is trusted by now, so a signal after the retest leaves its result standing.
    let (Step::Done(retested) | Step::Late(retested)) =
        interrupts.during(|cancelled| app.trust_and_retest_cancellable(&target, &key, cancelled));
    match match retested {
        Err(AppError::OpenSsh(OpenSshError::Cancelled)) => {
            return print_cancelled(output, target.alias());
        }
        retested => retested.map_err(|error| error.to_string())?,
    } {
        TestOutcome::Done(report) | TestOutcome::UnknownHostKey { report, .. } => {
            print_report(output, &report, raw)
        }
    }
}

/// Exits as a connection did: with ssh's (or docker's) status, after explaining a failed
/// connection, and with `hold` waiting for Enter before the terminal closes.
fn finish_connection<I: BufRead, E: Write>(
    outcome: ConnectionOutcome,
    hold: bool,
    input: &mut I,
    error: &mut E,
) -> Result<ExitCode, String> {
    let status = match outcome {
        ConnectionOutcome::LaunchRequested => return Ok(ExitCode::SUCCESS),
        ConnectionOutcome::Exited {
            alias,
            status,
            explanation,
        } => {
            if let Some(explanation) = explanation {
                print_connection_failure(error, &alias, &explanation)?;
                if hold {
                    writeln!(error, "Press Enter to close").map_err(|error| error.to_string())?;
                    error.flush().map_err(|error| error.to_string())?;
                    // Best effort: end of input or a read error ends the wait, and the exit
                    // code stays ssh's either way.
                    let _ = input.read_line(&mut String::new());
                }
            }
            status
        }
        ConnectionOutcome::LaunchFailed(status) => status,
    };
    Ok(exit_code(status))
}

/// The picker's rows for the containers of `list`, grouped and ordered as they are listed.
fn container_rows(list: &ContainerList) -> Vec<(String, Row)> {
    let containers = &list.listing.containers;
    group(containers)
        .into_iter()
        .flat_map(|group| {
            let project = group.project.unwrap_or("Other");
            group.containers.into_iter().map(move |container| {
                let row = Row {
                    fields: vec![
                        display_name(container, containers),
                        container.name.clone(),
                        container.project.clone().unwrap_or_default(),
                        container.service.clone().unwrap_or_default(),
                        container.image.clone(),
                    ],
                    extra: vec![project.to_owned(), container.status.clone()],
                };
                (container.id.clone(), row)
            })
        })
        .collect()
}

/// The host `alias` names, otherwise one chosen in the picker; `None` when the choice is
/// cancelled. `managed_only` offers only the hosts Portico manages, for commands that change one.
fn host_argument(
    alias: Option<String>,
    app: &App,
    picker: &mut dyn Picker,
    interactive: bool,
    usage: &str,
    managed_only: bool,
) -> Result<Option<String>, String> {
    if let Some(alias) = alias {
        return Ok(Some(alias));
    }
    if !interactive || !picker.available() {
        return Err(format!(
            "choose a host: portico {usage} <alias> (no terminal for the picker)"
        ));
    }
    let hosts: Vec<HostSummary> = app
        .hosts()
        .map_err(|error| error.to_string())?
        .hosts
        .into_iter()
        .filter(|host| !managed_only || host.ownership == Ownership::Managed)
        .collect();
    if hosts.is_empty() {
        return Err(if managed_only {
            "No managed hosts."
        } else {
            "No hosts."
        }
        .into());
    }
    let rows = hosts
        .iter()
        .map(|host| Row {
            fields: vec![host.alias.clone()],
            extra: vec![
                ownership(host.ownership).into(),
                host.source.display().to_string(),
            ],
        })
        .collect();
    Ok(picker
        .pick_one("host", rows)?
        .map(|index| hosts[index].alias.clone()))
}

/// This machine without `--on`, the host it names, or one chosen in the picker for a bare
/// `--on`; `None` when the choice is cancelled.
fn on_argument(
    on: Option<Option<String>>,
    app: &App,
    picker: &mut dyn Picker,
    interactive: bool,
    command: &str,
) -> Result<Option<Target>, String> {
    match on {
        None => Ok(Some(Target::Local)),
        Some(alias) => Ok(host_argument(
            alias,
            app,
            picker,
            interactive,
            &format!("{command} --on"),
            false,
        )?
        .map(Target::Host)),
    }
}

/// Lists the containers of `target`. When docker refuses with a fix Portico can run, an
/// interactive session is asked whether to run it, and on yes the listing runs once more.
fn list_with_offers<I: BufRead, O: Write, E: Write>(
    app: &App,
    target: &Target,
    input: &mut I,
    output: &mut O,
    error: &mut E,
    interactive: bool,
) -> Result<ContainerList, String> {
    let mut offered = false;
    loop {
        let issue = match app.containers(target, false, None) {
            Ok(list) => return Ok(list),
            Err(ContainerError::Docker(issue)) => issue,
            Err(failure) => return Err(report_container_failure(error, failure)?),
        };
        let offer = match &issue.offer {
            Some(offer) if interactive && !offered => offer,
            _ => {
                print_docker_issue(error, &issue, true)?;
                return Err(String::new());
            }
        };
        print_docker_issue(error, &issue, false)?;
        let prompt = format!("{} [y/N] ", offer_prompt(&issue.target, offer));
        if !confirm(input, output, &prompt)? {
            print_docker_issue_next(error, &issue)?;
            return Err(String::new());
        }
        let status = match app.docker_fix(target, offer) {
            Ok(status) => status,
            Err(failure) => return Err(report_container_failure(error, failure)?),
        };
        if !status.success() {
            return Err(format!(
                "{} exited with {status}",
                remote_command(&offer.words())
            ));
        }
        offered = true;
    }
}

/// The question asked before running `offer` on `target`.
fn offer_prompt(target: &Target, offer: &Offer) -> String {
    match offer {
        Offer::JoinGroup { user, .. } => format!(
            "Add {user} to the docker group on {target}? Members of the docker group have root-equivalent access. This runs: {}",
            remote_command(&offer.words())
        ),
        Offer::StartDaemon => format!(
            "Start the Docker service on {target}? This runs: {}",
            remote_command(&offer.words())
        ),
    }
}

/// Prints the issue's headline, and with `next` what to do about it.
fn print_docker_issue(
    error: &mut impl Write,
    issue: &DockerIssue,
    next: bool,
) -> Result<(), String> {
    writeln!(error, "error: {}", issue.headline).map_err(|error| error.to_string())?;
    if next {
        print_docker_issue_next(error, issue)?;
    }
    Ok(())
}

fn print_docker_issue_next(error: &mut impl Write, issue: &DockerIssue) -> Result<(), String> {
    for line in &issue.next {
        writeln!(error, "  {line}").map_err(|error| error.to_string())?;
    }
    error.flush().map_err(|error| error.to_string())
}

/// Reports a container failure on `error` and returns what is left for `run` to print: the
/// message, or nothing when the failure was explained here.
fn report_container_failure(
    error: &mut impl Write,
    failure: ContainerError,
) -> Result<String, String> {
    match failure {
        ContainerError::Docker(issue) => {
            print_docker_issue(error, &issue, true)?;
            Ok(String::new())
        }
        ContainerError::Ssh {
            alias,
            explanation,
            stderr,
        } => {
            // ssh's stderr was captured, so show it above the explanation.
            write!(error, "{stderr}").map_err(|error| error.to_string())?;
            match explanation {
                Some(explanation) => {
                    print_connection_failure(error, &alias, &explanation)?;
                    Ok(String::new())
                }
                None => Ok(format!("ssh to {alias} failed")),
            }
        }
        failure => Ok(failure.to_string()),
    }
}

/// Prints the containers grouped by project, each group's columns aligned, then the note
/// and the count of skipped lines.
fn print_listing(output: &mut impl Write, list: &ContainerList) -> Result<(), String> {
    let containers = &list.listing.containers;
    let mut text = String::new();
    if containers.is_empty() {
        text.push_str(&format!("No running containers on {}.\n", list.target));
    }
    for group in group(containers) {
        text.push_str(group.project.unwrap_or("Other"));
        text.push('\n');
        let rows: Vec<[String; 4]> = group
            .containers
            .iter()
            .map(|container| {
                [
                    display_name(container, containers),
                    container.name.clone(),
                    container.image.clone(),
                    container.status.clone(),
                ]
            })
            .collect();
        let width = |column: usize| {
            rows.iter()
                .map(|row| row[column].chars().count())
                .max()
                .unwrap_or(0)
        };
        let widths = [width(0), width(1), width(2)];
        for [display, name, image, status] in &rows {
            text.push_str(&format!(
                "  {display:<w0$}  {name:<w1$}  {image:<w2$}  {status}\n",
                w0 = widths[0],
                w1 = widths[1],
                w2 = widths[2]
            ));
        }
    }
    if let Some(note) = &list.note {
        text.push_str(note);
        text.push('\n');
    }
    if list.listing.skipped > 0 {
        text.push_str(&format!(
            "Skipped {} lines docker printed that Portico could not read.\n",
            list.listing.skipped
        ));
    }
    write!(output, "{text}").map_err(|error| error.to_string())
}

/// What `portico adopt` or `portico export` was asked to do.
struct Migration {
    direction: Direction,
    aliases: Vec<String>,
    all: bool,
    yes: bool,
}

impl Migration {
    fn verb(&self) -> &'static str {
        match self.direction {
            Direction::Adopt => "adopt",
            Direction::Export => "export",
        }
    }

    fn participle(&self) -> &'static str {
        match self.direction {
            Direction::Adopt => "adopted",
            Direction::Export => "exported",
        }
    }

    fn past(&self) -> &'static str {
        match self.direction {
            Direction::Adopt => "Adopted",
            Direction::Export => "Exported",
        }
    }

    fn cancelled(&self) -> &'static str {
        match self.direction {
            Direction::Adopt => "Adoption cancelled.",
            Direction::Export => "Export cancelled.",
        }
    }
}

/// `portico adopt` and `portico export`: choose the hosts, show the exact preview, ask, move.
fn run_migration<I: BufRead, O: Write>(
    migration: &Migration,
    app: &App,
    picker: &mut dyn Picker,
    interrupts: &Interrupts,
    input: &mut I,
    output: &mut O,
    interactive: bool,
) -> Result<ExitCode, String> {
    let verb = migration.verb();
    let named = !migration.aliases.is_empty() || migration.all;
    if !named && !(interactive && picker.available()) {
        return Err(format!(
            "{verb} requires host aliases or --all in non-interactive mode"
        ));
    }
    let plan = match migration.direction {
        Direction::Adopt => app.adoption_plan(),
        Direction::Export => app.export_plan(),
    }
    .map_err(|error| error.to_string())?;
    let home = app.home();
    let eligible = plan.eligible().collect::<Vec<_>>();
    let ineligible = plan
        .candidates
        .iter()
        .filter(|candidate| candidate.reason.is_some())
        .collect::<Vec<_>>();
    let write_error = |error: io::Error| error.to_string();

    let selection = if !migration.aliases.is_empty() {
        migration.aliases.clone()
    } else if migration.all {
        for candidate in &ineligible {
            let reason = candidate.reason.as_deref().unwrap_or_default();
            writeln!(output, "Skipped {}: {reason}", candidate.alias).map_err(write_error)?;
        }
        if eligible.is_empty() {
            writeln!(output, "No host can be {}.", migration.participle()).map_err(write_error)?;
            return Ok(ExitCode::SUCCESS);
        }
        eligible
            .iter()
            .map(|candidate| candidate.alias.clone())
            .collect()
    } else {
        if eligible.is_empty() {
            writeln!(output, "No host can be {}.", migration.participle()).map_err(write_error)?;
        }
        if !ineligible.is_empty() {
            writeln!(output, "Not eligible:").map_err(write_error)?;
            for candidate in &ineligible {
                writeln!(
                    output,
                    "  {}  {}: {}",
                    candidate.alias,
                    describe_source(migration.direction, candidate, home),
                    candidate.reason.as_deref().unwrap_or_default()
                )
                .map_err(write_error)?;
            }
        }
        if eligible.is_empty() {
            return Ok(ExitCode::SUCCESS);
        }
        output.flush().map_err(write_error)?;
        let rows = eligible
            .iter()
            .map(|candidate| Row {
                fields: vec![candidate.alias.clone()],
                extra: vec![describe_source(migration.direction, candidate, home)],
            })
            .collect();
        match picker.pick_many(verb, rows)? {
            Some(picked) => picked
                .into_iter()
                .map(|index| eligible[index].alias.clone())
                .collect(),
            None => return Ok(ExitCode::from(CANCELLED)),
        }
    };

    if selection.is_empty() {
        writeln!(output, "Nothing to {verb}.").map_err(write_error)?;
        return Ok(ExitCode::SUCCESS);
    }
    let preview = plan
        .preview(&selection)
        .map_err(|error| error.to_string())?;
    write!(output, "{}", preview.render(Some(home))).map_err(write_error)?;
    if !migration.yes {
        if !interactive {
            return Err(format!("{verb} requires --yes in non-interactive mode"));
        }
        let count = selection
            .iter()
            .map(|alias| alias.to_ascii_lowercase())
            .collect::<BTreeSet<_>>()
            .len();
        let noun = if count == 1 { "host" } else { "hosts" };
        if !confirm(
            input,
            output,
            &format!("{} {count} {noun}? [y/N] ", capitalized(verb)),
        )? {
            writeln!(output, "{}", migration.cancelled()).map_err(write_error)?;
            return Ok(ExitCode::SUCCESS);
        }
    }
    // A signal after the migration finished leaves its result standing.
    let (Step::Done(moved) | Step::Late(moved)) =
        interrupts.during(|cancelled| app.migrate_cancellable(&plan, &selection, cancelled));
    let moved = moved.map_err(|error| error.to_string())?;
    writeln!(output, "{}: {}", migration.past(), moved.join(", ")).map_err(write_error)?;
    Ok(ExitCode::SUCCESS)
}

/// The file a candidate comes from, with the home directory as `~`; `managed` for export.
fn describe_source(direction: Direction, candidate: &Candidate, home: &Path) -> String {
    match direction {
        Direction::Export => "managed".into(),
        Direction::Adopt => match candidate.source.strip_prefix(home) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => candidate.source.display().to_string(),
        },
    }
}

fn capitalized(word: &str) -> String {
    let mut characters = word.chars();
    characters
        .next()
        .map(|first| first.to_ascii_uppercase().to_string() + characters.as_str())
        .unwrap_or_default()
}

fn print_untrusted_key(
    output: &mut impl Write,
    alias: &str,
    key: &PresentedKey,
) -> Result<(), String> {
    let mut text = format!("{alias}: host key not trusted\n");
    for presented in &key.fingerprints {
        text.push_str(&presented.describe(alias));
        text.push('\n');
    }
    text.push_str("Compare it with the fingerprint from the server's administrator or console.\n");
    write!(output, "{text}").map_err(|error| error.to_string())
}

fn print_cancelled(output: &mut impl Write, alias: &str) -> Result<ExitCode, String> {
    writeln!(output, "{alias}: cancelled").map_err(|error| error.to_string())?;
    Ok(ExitCode::from(CANCELLED))
}

/// Explains a failed connection below what ssh already wrote to the same stream.
fn print_connection_failure(
    error: &mut impl Write,
    alias: &str,
    explanation: &Explanation,
) -> Result<(), String> {
    writeln!(error).map_err(|error| error.to_string())?;
    for line in explanation.render(alias, false) {
        writeln!(error, "{line}").map_err(|error| error.to_string())?;
    }
    error.flush().map_err(|error| error.to_string())
}

fn print_report(
    output: &mut impl Write,
    report: &TestReport,
    raw: bool,
) -> Result<ExitCode, String> {
    let lines = match &report.explanation {
        Some(explanation) => explanation.render(&report.alias, raw),
        None => vec![format!("{}: {}", report.alias, report.summary())],
    };
    for line in lines {
        writeln!(output, "{line}").map_err(|error| error.to_string())?;
    }
    Ok(if report.passed() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

/// Resolves `--<flag> value` and `--unset <flag>` for one field, rejecting both together.
fn field_change<T>(
    value: Option<T>,
    flag: &str,
    field: ClearField,
    unset: &[ClearField],
) -> Result<Change<T>, String> {
    match (value, unset.contains(&field)) {
        (Some(_), true) => Err(format!("--unset {flag} conflicts with --{flag}")),
        (Some(value), false) => Ok(Change::Set(value)),
        (None, true) => Ok(Change::Clear),
        (None, false) => Ok(Change::Keep),
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
    match &details.resolution {
        Ok(resolved) => {
            writeln!(output, "Resolved HostName: {}", resolved.hostname)
                .map_err(|error| error.to_string())?;
            print_optional(output, "Resolved User", resolved.user.as_deref())?;
            print_optional(
                output,
                "Resolved Port",
                resolved.port.map(|port| port.to_string()).as_deref(),
            )?;
            print_optional(
                output,
                "Resolved IdentityFile",
                (!resolved.identity_files.is_empty())
                    .then(|| resolved.identity_files.join(", "))
                    .as_deref(),
            )?;
            print_optional(output, "Resolved ProxyJump", resolved.proxy_jump.as_deref())?;
        }
        Err(error) => {
            writeln!(output, "Resolution error: {error}").map_err(|error| error.to_string())?;
        }
    }
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
    use std::{fs, io::Cursor};

    use clap::Parser;
    use tempfile::TempDir;

    use crate::test_executable::write_executable;

    use super::*;
    use crate::{
        app::{App, ContainerPrograms},
        container::{SHELL_SCRIPT, join_group_words, remote_command},
        host::Host,
        managed::{ConfigPaths, ManagedStore},
        openssh::OpenSsh,
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

        run(
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

    #[test]
    fn field_change_rejects_setting_and_unsetting_together() {
        assert_eq!(
            field_change(Some(1), "port", ClearField::Port, &[]),
            Ok(Change::Set(1))
        );
        assert_eq!(
            field_change::<u16>(None, "port", ClearField::Port, &[ClearField::Port]),
            Ok(Change::Clear)
        );
        assert_eq!(
            field_change::<u16>(None, "port", ClearField::Port, &[ClearField::User]),
            Ok(Change::Keep)
        );
        assert_eq!(
            field_change(Some(1), "port", ClearField::Port, &[ClearField::Port]),
            Err("--unset port conflicts with --port".into())
        );
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

        run_shell(&app, &mut picker, true);

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
            Some(Command::Containers { on: None })
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
            run_picking(&app, args, "", true, &mut picker);
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
                via: Some(Via::Sg),
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
