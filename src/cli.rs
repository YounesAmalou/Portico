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
    app::{App, AppError, HostDetails, Ownership, TestOutcome, TestReport, TestTarget},
    discovery::DiscoveryWarning,
    explain::Explanation,
    host::{Change, ClearField, Host, HostChanges},
    managed::{InitOutcome, InitPlan},
    openssh::{ConnectionOutcome, OpenSshError, PresentedKey},
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
        alias: String,
        #[arg(long = "accept-host-key", value_name = "FINGERPRINT")]
        accept_host_key: Option<String>,
        /// Show what ssh itself said, below the explanation.
        #[arg(long)]
        raw: bool,
    },
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

/// Runs `cli`. `handle_signals` lets `portico test` catch SIGINT, SIGTERM and SIGHUP to stop
/// its `ssh`; only the real program sets it, so tests never install process-wide handlers.
pub fn run<I, O, E>(
    cli: Cli,
    app: &App,
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
    match execute(cli, app, input, output, error, interactive, handle_signals) {
        Ok(exit) => exit,
        Err(message) => {
            let _ = writeln!(error, "error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn execute<I: BufRead, O: Write, E: Write>(
    cli: Cli,
    app: &App,
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
            let updated_alias = new_alias.as_deref().unwrap_or(&alias).to_owned();
            let changes = HostChanges {
                alias: new_alias.map_or(Change::Keep, Change::Set),
                hostname: hostname.map_or(Change::Keep, Change::Set),
                user: field_change(user, "user", ClearField::User, &unset)?,
                port: field_change(port, "port", ClearField::Port, &unset)?,
                identity_file: field_change(identity, "identity", ClearField::Identity, &unset)?,
                proxy_jump: field_change(jump, "jump", ClearField::Jump, &unset)?,
            };
            app.edit(&alias, changes)
                .map_err(|error| error.to_string())?;
            writeln!(output, "Updated {updated_alias}.").map_err(|error| error.to_string())?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Remove { alias, force }) => {
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
            // A held connection already runs in the terminal it was opened in.
            let status = match app
                .connect(&alias, new_terminal && !hold)
                .map_err(|error| error.to_string())?
            {
                ConnectionOutcome::LaunchRequested => return Ok(ExitCode::SUCCESS),
                ConnectionOutcome::Exited {
                    alias,
                    status,
                    explanation,
                } => {
                    if let Some(explanation) = explanation {
                        print_connection_failure(error, &alias, &explanation)?;
                        if hold {
                            writeln!(error, "Press Enter to close")
                                .map_err(|error| error.to_string())?;
                            error.flush().map_err(|error| error.to_string())?;
                            // Best effort: end of input or a read error ends the wait, and
                            // the exit code stays ssh's either way.
                            let _ = input.read_line(&mut String::new());
                        }
                    }
                    status
                }
                ConnectionOutcome::LaunchFailed(status) => status,
            };
            Ok(exit_code(status))
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
            run_migration(&migration, app, &interrupts, input, output, interactive)
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
            run_migration(&migration, app, &interrupts, input, output, interactive)
        }
        Some(Command::Test {
            alias,
            accept_host_key,
            raw,
        }) => {
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
    interrupts: &Interrupts,
    input: &mut I,
    output: &mut O,
    interactive: bool,
) -> Result<ExitCode, String> {
    let verb = migration.verb();
    let named = !migration.aliases.is_empty() || migration.all;
    if !named && !interactive {
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
        } else {
            writeln!(output, "Eligible hosts:").map_err(write_error)?;
            for (index, candidate) in eligible.iter().enumerate() {
                writeln!(
                    output,
                    "  {}. {}  {}",
                    index + 1,
                    candidate.alias,
                    describe_source(migration.direction, candidate, home)
                )
                .map_err(write_error)?;
            }
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
        write!(
            output,
            "{} which hosts? (all, numbers like 1,3-4, or none) ",
            capitalized(verb)
        )
        .map_err(write_error)?;
        output.flush().map_err(write_error)?;
        let mut answer = String::new();
        input.read_line(&mut answer).map_err(write_error)?;
        parse_selection(&answer, eligible.len())?
            .into_iter()
            .map(|index| eligible[index].alias.clone())
            .collect()
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

/// The 0-based indexes an answer to the selection prompt picks out of `count` hosts: `all`,
/// `none` or nothing, or comma-separated 1-based numbers and `a-b` ranges.
fn parse_selection(answer: &str, count: usize) -> Result<Vec<usize>, String> {
    let invalid = || format!("invalid selection: {}", answer.trim());
    let compact = answer
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    match compact.as_str() {
        "" | "none" => return Ok(Vec::new()),
        "all" => return Ok((0..count).collect()),
        _ => {}
    }
    let number = |text: &str| {
        text.parse::<usize>()
            .ok()
            .filter(|number| (1..=count).contains(number))
            .ok_or_else(invalid)
    };
    let mut picked = BTreeSet::new();
    for part in compact.split(',') {
        let (first, last) = match part.split_once('-') {
            Some((first, last)) => (number(first)?, number(last)?),
            None => (number(part)?, number(part)?),
        };
        if first > last {
            return Err(invalid());
        }
        picked.extend(first - 1..last);
    }
    Ok(picked.into_iter().collect())
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
        app::App,
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
            false,
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
            false,
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

    #[test]
    fn parse_selection_accepts_all_none_numbers_and_ranges() {
        assert_eq!(parse_selection("1,3-4", 5), Ok(vec![0, 2, 3]));
        assert_eq!(parse_selection(" 2 , 4 - 5 ", 5), Ok(vec![1, 3, 4]));
        assert_eq!(parse_selection("3,1,3", 5), Ok(vec![0, 2]));
        assert_eq!(parse_selection("", 5), Ok(vec![]));
        assert_eq!(parse_selection("none", 5), Ok(vec![]));
        assert_eq!(parse_selection("all", 3), Ok(vec![0, 1, 2]));
        for bad in ["0", "6", "2-1", "x", "1,,2", "1-", "-1"] {
            assert_eq!(
                parse_selection(bad, 5),
                Err(format!("invalid selection: {bad}"))
            );
        }
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
        let (mut output, mut error) = (Vec::new(), Vec::new());
        let exit = run(
            Cli::try_parse_from(std::iter::once("portico").chain(args.iter().copied())).unwrap(),
            app,
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
    fn interactive_adopt_moves_the_chosen_hosts() {
        let (_dir, app) = adoption_app(THREE_HOSTS);
        let (exit, output, error) = run_command(&app, &["adopt"], "2\ny\n", true);
        assert_eq!(exit, std::process::ExitCode::SUCCESS, "{error}");
        assert!(output.starts_with(
            "Eligible hosts:\n  1. one  ~/.ssh/config\n  2. two  ~/.ssh/config\n  3. three  ~/.ssh/config\n\
             Not eligible:\n  multi  ~/.ssh/config: Host line declares 2 aliases\n  other  ~/.ssh/config: Host line declares 2 aliases\n\
             Adopt which hosts? (all, numbers like 1,3-4, or none) "
        ), "{output}");
        assert!(output.contains("Adopt 1 host? [y/N] "));
        assert!(output.ends_with("Adopted: two\n"));
        assert_eq!(aliases_of(&app, Ownership::Managed), ["two"]);
    }

    #[test]
    fn interactive_adopt_accepts_ranges_and_all() {
        let (_dir, app) = adoption_app(THREE_HOSTS);
        let (_, output, _) = run_command(&app, &["adopt"], "1-2\ny\n", true);
        assert!(output.contains("Adopt 2 hosts? [y/N] "));
        assert!(output.ends_with("Adopted: one, two\n"));
        let (_, output, _) = run_command(&app, &["adopt"], "all\ny\n", true);
        assert!(output.ends_with("Adopted: three\n"), "{output}");
    }

    #[test]
    fn interactive_adopt_changes_nothing_on_none_decline_or_invalid_answers() {
        let (dir, app) = adoption_app(THREE_HOSTS);
        let entry = fs::read(dir.path().join(".ssh/config")).unwrap();
        for (input, success, ending) in [
            ("none\n", true, "Nothing to adopt.\n"),
            ("\n", true, "Nothing to adopt.\n"),
            ("", true, "Nothing to adopt.\n"),
            ("1\nn\n", true, "Adoption cancelled.\n"),
            ("1\n", true, "Adoption cancelled.\n"),
        ] {
            let (exit, output, _) = run_command(&app, &["adopt"], input, true);
            assert_eq!(exit == std::process::ExitCode::SUCCESS, success, "{input}");
            assert!(output.ends_with(ending), "{input}: {output}");
        }
        let (exit, _, error) = run_command(&app, &["adopt"], "9\n", true);
        assert_eq!(exit, std::process::ExitCode::FAILURE);
        assert_eq!(error, "error: invalid selection: 9\n");
        assert_eq!(fs::read(dir.path().join(".ssh/config")).unwrap(), entry);
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
    fn interactive_export_moves_managed_hosts_back() {
        let (_dir, app) = adoption_app("");
        app.add(Host::new("dev", "dev.example", None, None, None, None).unwrap())
            .unwrap();
        let (exit, output, error) = run_command(&app, &["export"], "all\ny\n", true);
        assert_eq!(exit, std::process::ExitCode::SUCCESS, "{error}");
        assert!(
            output.starts_with("Eligible hosts:\n  1. dev  managed\n"),
            "{output}"
        );
        assert!(output.contains("Export which hosts?"));
        assert!(output.contains("Export 1 host? [y/N] "));
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
