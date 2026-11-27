//! `portico tunnels` and `portico tunnel …`.

use std::{
    io::{BufRead, Write},
    process::ExitCode,
};

use super::{CANCELLED, Interrupts, Step, confirm, hosts::host_argument, json::JsonCommand};
use crate::{
    app::{App, AppError, StartOutcome, StopOutcome, TunnelChanges},
    domain::explain::Explanation,
    domain::openssh::OpenSshError,
    domain::tunnel::{Forward, Kind, Tunnel, TunnelReport, TunnelStatus, saved_name_for},
    picker::{Picker, Row},
};

#[derive(clap::Subcommand, Debug)]
pub(super) enum TunnelCommand {
    /// Show one tunnel, its command line and its status.
    Show {
        name: Option<String>,
        /// Print one JSON document instead of text (see docs/json-output.md).
        #[arg(long)]
        json: bool,
    },
    /// Save a tunnel: one forward through a host.
    Add {
        name: String,
        /// The host the tunnel goes through; without an alias, choose one.
        #[arg(long = "on", value_name = "ALIAS", num_args = 0..=1, required = true)]
        on: Option<Option<String>>,
        #[command(flatten)]
        forward: ForwardArgs,
    },
    /// Change a saved tunnel.
    Edit {
        name: Option<String>,
        #[arg(long = "name", value_name = "NEW")]
        new_name: Option<String>,
        #[arg(long = "on", value_name = "ALIAS", num_args = 0..=1)]
        on: Option<Option<String>>,
        #[command(flatten)]
        forward: ForwardArgs,
    },
    /// Delete a saved tunnel.
    Remove {
        name: Option<String>,
        #[arg(short = 'f', long)]
        force: bool,
    },
    /// Start saved tunnels in the background.
    Start {
        names: Vec<String>,
        /// Print one JSON document instead of text (see docs/json-output.md).
        #[arg(long)]
        json: bool,
    },
    /// Keep a temporary forward as a saved tunnel; a running one restarts under its new name.
    Save {
        /// The temporary forward, as `portico tunnels` lists it (`_web-80`).
        name: String,
        /// The saved tunnel's name; by default the temporary name without its leading `_`.
        #[arg(long = "name", value_name = "NEW")]
        new_name: Option<String>,
        /// Print one JSON document instead of text (see docs/json-output.md).
        #[arg(long)]
        json: bool,
    },
    /// Stop running tunnels.
    Stop {
        names: Vec<String>,
        #[arg(short = 'A', long, conflicts_with = "names")]
        all: bool,
        /// Print one JSON document instead of text (see docs/json-output.md).
        #[arg(long)]
        json: bool,
    },
}

/// The command as a `--json` one, when it was given `--json`.
pub(super) fn json_command(command: &TunnelCommand) -> Option<JsonCommand> {
    Some(match command {
        TunnelCommand::Show { name, json: true } => JsonCommand::TunnelShow(name.clone()),
        TunnelCommand::Start { names, json: true } => JsonCommand::TunnelStart(names.clone()),
        TunnelCommand::Save {
            name,
            new_name,
            json: true,
        } => JsonCommand::TunnelSave {
            name: name.clone(),
            new_name: new_name.clone(),
        },
        TunnelCommand::Stop {
            names,
            all,
            json: true,
        } => JsonCommand::TunnelStop {
            names: names.clone(),
            all: *all,
        },
        _ => return None,
    })
}

/// The forward, in ssh's own syntax.
#[derive(clap::Args, Debug)]
#[group(multiple = false)]
pub(super) struct ForwardArgs {
    /// A local forward: [bind_address:]port:host:hostport
    #[arg(short = 'L', value_name = "SPEC")]
    local: Option<String>,
    /// A remote forward: [bind_address:]port:host:hostport
    #[arg(short = 'R', value_name = "SPEC")]
    remote: Option<String>,
    /// A dynamic (SOCKS) forward: [bind_address:]port
    #[arg(short = 'D', value_name = "SPEC")]
    dynamic: Option<String>,
}

impl ForwardArgs {
    fn forward(self) -> Result<Option<Forward>, String> {
        let (kind, spec) = match (self.local, self.remote, self.dynamic) {
            (Some(spec), _, _) => (Kind::Local, spec),
            (_, Some(spec), _) => (Kind::Remote, spec),
            (_, _, Some(spec)) => (Kind::Dynamic, spec),
            _ => return Ok(None),
        };
        Forward::parse(kind, &spec)
            .map(Some)
            .map_err(|error| error.to_string())
    }
}

fn text(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn print_list<O: Write>(output: &mut O, reports: &[TunnelReport]) -> Result<(), String> {
    if reports.is_empty() {
        writeln!(
            output,
            "No tunnels. Add one with: portico tunnel add <name> --on <alias> -L <port>:<host>:<port>"
        )
        .map_err(text)?;
        return Ok(());
    }
    let rows: Vec<[String; 5]> = reports
        .iter()
        .map(|report| {
            [
                if report.temporary {
                    format!("{} (temporary)", report.tunnel.name)
                } else {
                    report.tunnel.name.clone()
                },
                crate::domain::tunnel::printable(&report.reaches),
                report.tunnel.forward.describe(),
                report.status.name().into(),
                report.status.detail(),
            ]
        })
        .collect();
    let widths: Vec<usize> = (0..4)
        .map(|column| {
            rows.iter()
                .map(|row| row[column].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    for row in &rows {
        let mut line = String::new();
        for (column, width) in widths.iter().enumerate() {
            let cell = &row[column];
            line.push_str(cell);
            line.push_str(&" ".repeat(width - cell.chars().count() + 2));
        }
        line.push_str(&row[4]);
        writeln!(output, "{}", line.trim_end()).map_err(text)?;
    }
    for report in reports {
        for warning in &report.warnings {
            writeln!(output, "Warning: tunnel {}: {warning}", report.tunnel.name).map_err(text)?;
        }
    }
    Ok(())
}

fn print_report<O: Write>(output: &mut O, app: &App, report: &TunnelReport) -> Result<(), String> {
    let tunnel = &report.tunnel;
    let detail = report.status.detail();
    let status = if detail.is_empty() {
        report.status.name().to_owned()
    } else {
        format!("{} \u{2014} {detail}", report.status.name())
    };
    writeln!(output, "Tunnel:  {}", tunnel.name).map_err(text)?;
    writeln!(output, "Host:    {}", tunnel.host).map_err(text)?;
    writeln!(
        output,
        "Forward: {} {} {}",
        tunnel.forward.kind.name(),
        tunnel.forward.kind.flag(),
        tunnel.forward.spec()
    )
    .map_err(text)?;
    writeln!(output, "Status:  {status}").map_err(text)?;
    writeln!(output, "Command: {}", app.tunnel_command(tunnel).join(" ")).map_err(text)?;
    for warning in &report.warnings {
        writeln!(output, "Warning: {warning}").map_err(text)?;
    }
    if matches!(report.status, TunnelStatus::Failed { .. }) {
        let log = app.tunnel_log(&tunnel.name, 20);
        if !log.is_empty() {
            writeln!(output, "Log:").map_err(text)?;
            for line in log {
                writeln!(output, "  {line}").map_err(text)?;
            }
        }
    }
    Ok(())
}

/// A tunnel name from the argument or the picker over `reports` that `wanted` keeps; `None`
/// when the pick is cancelled.
fn pick_tunnel(
    name: Option<String>,
    app: &App,
    picker: &mut dyn Picker,
    interactive: bool,
    usage: &str,
    wanted: impl Fn(&TunnelReport) -> bool,
    none: &str,
) -> Result<Option<String>, String> {
    if let Some(name) = name {
        return Ok(Some(name));
    }
    if !interactive || !picker.available() {
        return Err(format!(
            "choose a tunnel: portico tunnel {usage} <name> (no terminal for the picker)"
        ));
    }
    let reports: Vec<TunnelReport> = app
        .tunnels()
        .map_err(text)?
        .into_iter()
        .filter(|report| wanted(report))
        .collect();
    if reports.is_empty() {
        return Err(none.into());
    }
    let rows = reports
        .iter()
        .map(|report| Row {
            fields: vec![report.tunnel.name.clone(), report.tunnel.host.clone()],
            extra: vec![
                report.tunnel.forward.describe(),
                report.status.name().into(),
            ],
        })
        .collect();
    Ok(picker
        .pick_one("tunnel", rows)?
        .map(|index| reports[index].tunnel.name.clone()))
}

/// The interrupts a tunnel start watches. Where ssh backgrounds itself (Unix), the starting ssh
/// is in Portico's process group and a Ctrl+C ends both, so no handler is installed and a signal
/// acts as it always has. Where Portico starts ssh detached (Windows), a Ctrl+C or a closed
/// console only raises the flag, and the start ends ssh's tree before Portico exits.
pub(super) fn start_interrupts(handle_signals: bool) -> Result<Interrupts, String> {
    Interrupts::install(handle_signals && !crate::platform::process::SSH_BACKGROUNDS)
        .map_err(|error| format!("cannot handle signals: {error}"))
}

/// Starts `names` in turn; a signal stops the start under way, or any after it, and exits with
/// `CANCELLED`.
fn start<O: Write, E: Write>(
    names: &[String],
    app: &App,
    interrupts: &Interrupts,
    output: &mut O,
    error: &mut E,
) -> Result<ExitCode, String> {
    let mut failed = false;
    for name in names {
        let (outcome, late) =
            match interrupts.during(|cancelled| app.start_tunnel(name, Some(cancelled), false)) {
                Step::Done(outcome) => (outcome, false),
                Step::Late(outcome) => (outcome, true),
            };
        match outcome {
            Err(AppError::OpenSsh(OpenSshError::Cancelled)) => {
                writeln!(output, "{name}: cancelled").map_err(text)?;
                return Ok(ExitCode::from(CANCELLED));
            }
            Ok(StartOutcome::Started { pid }) => {
                let line = app
                    .tunnel(name)
                    .map_or(name.clone(), |report| report.tunnel.to_string());
                writeln!(output, "Started {line}, pid {pid}.").map_err(text)?;
            }
            Ok(StartOutcome::AlreadyRunning { pid }) => {
                writeln!(output, "{name} is already running (pid {pid}).").map_err(text)?;
            }
            Ok(StartOutcome::Failed {
                reason,
                explanation,
            }) => {
                failed = true;
                print_failed_start(error, name, &reason, explanation)?;
            }
            Err(refusal) => {
                failed = true;
                writeln!(error, "error: {refusal}").map_err(text)?;
            }
        }
        // A signal after this start finished leaves it standing but starts no other.
        if late {
            return Ok(ExitCode::from(CANCELLED));
        }
    }
    Ok(if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// A failed start of tunnel `name`: its explanation, or the reason in one line.
pub(super) fn print_failed_start<E: Write>(
    error: &mut E,
    name: &str,
    reason: &str,
    explanation: Option<Explanation>,
) -> Result<(), String> {
    match explanation {
        Some(explanation) => {
            for line in explanation.render(name, false) {
                writeln!(error, "{line}").map_err(text)?;
            }
            Ok(())
        }
        None => writeln!(error, "{name}: failed \u{2014} {reason}").map_err(text),
    }
}

/// The name temporary forward `name` is saved under: `new_name`, or by default its name without
/// the leading `_`.
pub(super) fn save_name(name: &str, new_name: Option<String>) -> String {
    new_name.unwrap_or_else(|| saved_name_for(name))
}

pub(super) fn list<O: Write>(app: &App, output: &mut O) -> Result<ExitCode, String> {
    print_list(output, &app.tunnels().map_err(text)?)?;
    Ok(ExitCode::SUCCESS)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn run<I: BufRead, O: Write, E: Write>(
    command: TunnelCommand,
    app: &App,
    picker: &mut dyn Picker,
    input: &mut I,
    output: &mut O,
    error: &mut E,
    interactive: bool,
    handle_signals: bool,
) -> Result<ExitCode, String> {
    match command {
        TunnelCommand::Show { name, .. } => {
            let Some(name) = pick_tunnel(
                name,
                app,
                picker,
                interactive,
                "show",
                |_| true,
                "No tunnels.",
            )?
            else {
                return Ok(ExitCode::from(CANCELLED));
            };
            let report = app.tunnel(&name).map_err(text)?;
            print_report(output, app, &report)?;
            Ok(ExitCode::SUCCESS)
        }
        TunnelCommand::Add { name, on, forward } => {
            let forward = forward
                .forward()?
                .ok_or("give the forward: -L, -R or -D <spec>")?;
            let Some(host) = host_argument(
                on.flatten(),
                app,
                picker,
                interactive,
                "tunnel add <name> --on",
                false,
            )?
            else {
                return Ok(ExitCode::from(CANCELLED));
            };
            let tunnel = Tunnel::new(name, host, forward).map_err(text)?;
            let line = tunnel.to_string();
            app.add_tunnel(tunnel).map_err(text)?;
            writeln!(output, "Added {line}.").map_err(text)?;
            Ok(ExitCode::SUCCESS)
        }
        TunnelCommand::Edit {
            name,
            new_name,
            on,
            forward,
        } => {
            // The flags are checked before a tunnel is picked, so a mistake does not cost a pick.
            let forward = forward.forward()?;
            let Some(name) = pick_tunnel(
                name,
                app,
                picker,
                interactive,
                "edit",
                |report| !report.status.is_running(),
                "No stopped tunnels.",
            )?
            else {
                return Ok(ExitCode::from(CANCELLED));
            };
            let host = match on {
                None => None,
                Some(alias) => {
                    match host_argument(alias, app, picker, interactive, "tunnel edit --on", false)?
                    {
                        Some(host) => Some(host),
                        None => return Ok(ExitCode::from(CANCELLED)),
                    }
                }
            };
            let renamed = new_name.clone().unwrap_or_else(|| name.clone());
            app.edit_tunnel(
                &name,
                TunnelChanges {
                    name: new_name,
                    host,
                    forward,
                },
            )
            .map_err(text)?;
            writeln!(output, "Updated {renamed}.").map_err(text)?;
            Ok(ExitCode::SUCCESS)
        }
        TunnelCommand::Remove { name, force } => {
            let Some(name) = pick_tunnel(
                name,
                app,
                picker,
                interactive,
                "remove",
                |report| !report.status.is_running(),
                "No stopped tunnels.",
            )?
            else {
                return Ok(ExitCode::from(CANCELLED));
            };
            // Refuse before asking, so a running tunnel is not confirmed for nothing.
            let report = app.tunnel(&name).map_err(text)?;
            if report.status.is_running() {
                return Err(format!(
                    "tunnel {} is running; stop it first: portico tunnel stop {}",
                    report.tunnel.name, report.tunnel.name
                ));
            }
            if !force {
                if !interactive {
                    return Err("removal requires --force in non-interactive mode".into());
                }
                if !confirm(input, output, &format!("Remove tunnel {name}? [y/N] "))? {
                    writeln!(output, "Removal cancelled.").map_err(text)?;
                    return Ok(ExitCode::SUCCESS);
                }
            }
            app.remove_tunnel(&name).map_err(text)?;
            writeln!(output, "Removed {}.", report.tunnel.name).map_err(text)?;
            Ok(ExitCode::SUCCESS)
        }
        TunnelCommand::Start { names, .. } => {
            let names = if names.is_empty() {
                match pick_tunnel(
                    None,
                    app,
                    picker,
                    interactive,
                    "start",
                    |report| !report.status.is_running(),
                    "Every tunnel is running.",
                )? {
                    Some(name) => vec![name],
                    None => return Ok(ExitCode::from(CANCELLED)),
                }
            } else {
                names
            };
            let interrupts = start_interrupts(handle_signals)?;
            start(&names, app, &interrupts, output, error)
        }
        TunnelCommand::Save { name, new_name, .. } => {
            let saved = save_name(&name, new_name);
            let outcome = app.save_tunnel(&name, &saved, None).map_err(text)?;
            let name = &outcome.tunnel.name;
            writeln!(output, "Saved {name}.").map_err(text)?;
            let mut exit = ExitCode::SUCCESS;
            match outcome.restarted {
                None => {}
                Some(StartOutcome::Started { .. } | StartOutcome::AlreadyRunning { .. }) => {
                    writeln!(
                        output,
                        "Restarted it as {name}; connections through it were interrupted."
                    )
                    .map_err(text)?;
                }
                Some(StartOutcome::Failed {
                    reason,
                    explanation,
                }) => {
                    exit = ExitCode::FAILURE;
                    print_failed_start(error, name, &reason, explanation)?;
                }
            }
            if let Some(note) = outcome.note {
                writeln!(output, "Note: {note}.").map_err(text)?;
            }
            Ok(exit)
        }
        TunnelCommand::Stop { names, all, .. } => {
            let names = if all {
                let running = app.running_tunnels().map_err(text)?;
                if running.is_empty() {
                    writeln!(output, "No tunnels are running.").map_err(text)?;
                    return Ok(ExitCode::SUCCESS);
                }
                running
            } else if names.is_empty() {
                match pick_tunnel(
                    None,
                    app,
                    picker,
                    interactive,
                    "stop",
                    |report| {
                        matches!(
                            report.status,
                            TunnelStatus::Running { .. } | TunnelStatus::Failed { .. }
                        )
                    },
                    "No tunnels are running.",
                )? {
                    Some(name) => vec![name],
                    None => return Ok(ExitCode::from(CANCELLED)),
                }
            } else {
                names
            };
            let mut failed = false;
            for name in names {
                match app.stop_tunnel(&name) {
                    Ok(StopOutcome::Stopped) => {
                        writeln!(output, "Stopped {name}.").map_err(text)?;
                    }
                    Ok(StopOutcome::NotRunning) => {
                        writeln!(output, "{name} is not running.").map_err(text)?;
                    }
                    Err(refusal) => {
                        failed = true;
                        writeln!(error, "error: {refusal}").map_err(text)?;
                    }
                }
            }
            Ok(if failed {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            })
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;
    use crate::{
        domain::host::Host,
        domain::managed::{ConfigPaths, ManagedStore},
        domain::openssh::OpenSsh,
        domain::tunnel::runtime::RuntimeDir,
        test_executable::write_executable,
    };

    /// An app with tunnels `db` and `web` through a host whose ssh, once started, keeps
    /// connecting.
    pub(in crate::cli) fn two_tunnels() -> (TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join("ssh");
        write_executable(
            &ssh,
            "#!/bin/sh\nif [ \"$1\" = -G ]; then printf 'hostname x\\nport 22\\n'; exit 0; fi\nsleep 60\n",
        );
        fs::create_dir_all(dir.path().join("home/.ssh")).unwrap();
        let app = App::new(
            ManagedStore::new(ConfigPaths::from_home(dir.path().join("home"))),
            OpenSsh::new(ssh, "terminal".into()),
        )
        .with_runtime_dir(RuntimeDir::at(dir.path().join("run")));
        app.initialize(true).unwrap();
        app.add(Host::new("prod", "x", None, None, None, None).unwrap())
            .unwrap();
        for name in ["db", "web"] {
            let port = std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            let forward = Forward::parse(Kind::Dynamic, &format!("127.0.0.1:{port}")).unwrap();
            app.add_tunnel(Tunnel::new(name, "prod", forward).unwrap())
                .unwrap();
        }
        (dir, app)
    }

    #[test]
    fn the_list_prints_what_a_tunnel_reaches_without_escapes() {
        let (_dir, app) = two_tunnels();
        let mut reports = app.tunnels().unwrap();
        reports[0].reaches = "web on \u{1b}]0;owned\u{7}prod".into();
        let mut output = Vec::new();
        print_list(&mut output, &reports).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(!output.contains('\u{1b}'), "{output:?}");
        assert!(output.contains("web on prod"), "{output:?}");
    }

    #[test]
    fn a_signal_during_a_start_cancels_it_and_starts_no_other() {
        let (_dir, app) = two_tunnels();
        let (mut output, mut error) = (Vec::new(), Vec::new());
        let exit = start(
            &["db".into(), "web".into()],
            &app,
            &Interrupts::signalled_during(),
            &mut output,
            &mut error,
        )
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        assert_eq!(exit, ExitCode::from(CANCELLED), "{output}");
        assert_eq!(output, "db: cancelled\n");
        assert!(error.is_empty(), "{}", String::from_utf8_lossy(&error));
        for name in ["db", "web"] {
            assert!(
                !app.tunnel(name).unwrap().status.is_running(),
                "{name} is running"
            );
        }
    }
}
