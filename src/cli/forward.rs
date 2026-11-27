//! `portico forward`: a temporary forward to a port of a host or of a container on one, or where
//! a container of this machine is already reachable.

use std::{
    io::{BufRead, Write},
    process::ExitCode,
};

use super::{
    BackendArg, CANCELLED, Step,
    containers::{choose_container, on_argument},
    tunnels::{print_failed_start, start_interrupts},
};
use crate::{
    app::{App, AppError, ForwardTarget, StartOutcome, Suggestions},
    backend::{BackendId, Target},
    domain::openssh::OpenSshError,
    domain::tunnel::Tunnel,
    picker::{Picker, Row},
};

/// The exit status of a usage error, as clap's own.
const USAGE: u8 = 2;

#[derive(clap::Args, Debug)]
pub(super) struct ForwardCommand {
    /// The container: its name, `project/service`, service or CT ID. Without it, a port of the
    /// host itself.
    pub container: Option<String>,
    /// The host the port is on; without an alias, choose one. Without --on, CONTAINER is on this
    /// machine, and where its ports are reachable is printed instead.
    #[arg(long = "on", value_name = "ALIAS", num_args = 0..=1)]
    pub on: Option<Option<String>>,
    /// Find the container with this backend only, instead of every installed one.
    #[arg(long, value_enum)]
    pub backend: Option<BackendArg>,
    /// The remote port; without it, choose one of the suggested ports.
    #[arg(long, value_name = "REMOTE")]
    pub port: Option<u16>,
    /// The local port; by default the remote port, or the next free one above it.
    #[arg(long, value_name = "LOCAL")]
    pub local: Option<u16>,
    /// Print one JSON document instead of text (see docs/json-output.md).
    #[arg(long)]
    pub json: bool,
}

impl ForwardCommand {
    /// `--on`, as a bare `--on` when neither a container nor a host is given: the host picker.
    pub fn on(&self) -> Option<Option<String>> {
        match (&self.container, &self.on) {
            (None, None) => Some(None),
            (_, on) => on.clone(),
        }
    }

    pub fn backend(&self) -> Option<BackendId> {
        self.backend.map(BackendId::from)
    }
}

/// Why a forward without `--port` cannot start: the suggestions, or that there are none.
pub(super) fn port_required(suggestions: &Suggestions) -> String {
    if suggestions.ports.is_empty() {
        return "--port is required; no port was suggested".into();
    }
    let ports: Vec<String> = suggestions
        .ports
        .iter()
        .map(|suggestion| suggestion.port.to_string())
        .collect();
    format!("--port is required; suggested: {}", ports.join(", "))
}

/// Where the ports of `target`, a container of this machine, are reachable here: every one, or
/// only `port`. An empty answer is an error that says to publish the port.
pub(super) fn reachable_here(
    app: &App,
    target: &ForwardTarget,
    port: Option<u16>,
) -> Result<Vec<(u16, String)>, String> {
    let ForwardTarget::Container { container, .. } = target else {
        return Err("a host's port is reached through a forward; give --on".into());
    };
    let suggestions = app.forward_suggestions(target, None);
    let Some(reach) = suggestions.reach else {
        return Err(suggestions
            .unavailable
            .unwrap_or_else(|| format!("could not find where {} is reachable", target.short())));
    };
    let addresses: Vec<(u16, String)> = app
        .local_addresses(container, &reach)
        .into_iter()
        .filter(|(reachable, _)| port.is_none_or(|port| port == *reachable))
        .collect();
    if addresses.is_empty() {
        return Err(match port {
            Some(port) => format!(
                "port {port} of {} is not reachable from this machine; publish it",
                target.short()
            ),
            None => format!(
                "no port of {} is reachable from this machine; publish one",
                target.short()
            ),
        });
    }
    Ok(addresses)
}

/// The success copy of a started forward: where it listens and how to stop or keep it.
fn print_started<O: Write>(
    output: &mut O,
    target: &ForwardTarget,
    tunnel: &Tunnel,
    remote: u16,
) -> Result<(), String> {
    let name = &tunnel.name;
    writeln!(
        output,
        "Forwarding 127.0.0.1:{} \u{2192} {}:{remote} (temporary forward {name}).\n\
         Stop it with `portico tunnel stop {name}`, keep it with `portico tunnel save {name}`.",
        tunnel.forward.bind_port,
        target.short(),
    )
    .map_err(|error| error.to_string())
}

/// The remote port chosen in the picker over the suggestions, or typed; `None` when the pick is
/// cancelled.
fn pick_port<I: BufRead, O: Write>(
    suggestions: &Suggestions,
    picker: &mut dyn Picker,
    input: &mut I,
    output: &mut O,
) -> Result<Option<u16>, String> {
    if let Some(unavailable) = &suggestions.unavailable {
        writeln!(output, "{unavailable}").map_err(|error| error.to_string())?;
    }
    if !suggestions.ports.is_empty() {
        let mut rows: Vec<Row> = suggestions
            .ports
            .iter()
            .map(|suggestion| Row {
                fields: vec![suggestion.port.to_string()],
                extra: suggestion.note.iter().cloned().collect(),
            })
            .collect();
        rows.push(Row {
            fields: vec!["another port".into()],
            extra: Vec::new(),
        });
        match picker.pick_one("port", rows)? {
            None => return Ok(None),
            Some(index) => {
                if let Some(suggestion) = suggestions.ports.get(index) {
                    return Ok(Some(suggestion.port));
                }
            }
        }
    }
    write!(output, "Remote port: ").map_err(|error| error.to_string())?;
    output.flush().map_err(|error| error.to_string())?;
    let mut answer = String::new();
    input
        .read_line(&mut answer)
        .map_err(|error| error.to_string())?;
    let answer = answer.trim();
    match answer.parse::<u16>() {
        Ok(port) if port > 0 => Ok(Some(port)),
        _ => Err(format!("not a port: {answer}")),
    }
}

/// Runs `portico forward` in text mode.
#[allow(clippy::too_many_arguments)]
pub(super) fn run<I: BufRead, O: Write, E: Write>(
    command: ForwardCommand,
    app: &App,
    picker: &mut dyn Picker,
    input: &mut I,
    output: &mut O,
    error: &mut E,
    interactive: bool,
    handle_signals: bool,
) -> Result<ExitCode, String> {
    let backend = command.backend();
    let Some(on) = on_argument(command.on(), app, picker, interactive, "forward")? else {
        return Ok(ExitCode::from(CANCELLED));
    };
    let target = match (command.container, on) {
        (None, Target::Host(alias)) => ForwardTarget::Host(alias),
        (None, Target::Local) => {
            return Err("give a container, or a host with --on <alias>".into());
        }
        (reference, on) => {
            let usage = match &on {
                Target::Local => "portico forward <container>".to_owned(),
                Target::Host(alias) => format!("portico forward <container> --on {alias}"),
            };
            match choose_container(
                app,
                &on,
                reference,
                backend,
                &usage,
                picker,
                input,
                output,
                error,
                interactive,
            )? {
                Ok((chosen, container)) => ForwardTarget::Container {
                    target: on,
                    backend: chosen.backend,
                    container,
                    via: chosen.via,
                },
                Err(exit) => return Ok(exit),
            }
        }
    };
    if let ForwardTarget::Container {
        target: Target::Local,
        ..
    } = &target
    {
        for (port, address) in reachable_here(app, &target, command.port)? {
            writeln!(
                output,
                "Port {port} of {} is reachable at {address}.",
                target.short()
            )
            .map_err(|error| error.to_string())?;
        }
        return Ok(ExitCode::SUCCESS);
    }
    let (remote, reach) = match command.port {
        Some(port) => (port, None),
        None => {
            let suggestions = app.forward_suggestions(&target, None);
            if !interactive || !picker.available() {
                writeln!(error, "error: {}", port_required(&suggestions))
                    .map_err(|error| error.to_string())?;
                if let Some(unavailable) = &suggestions.unavailable {
                    writeln!(error, "{unavailable}").map_err(|error| error.to_string())?;
                }
                return Ok(ExitCode::from(USAGE));
            }
            match pick_port(&suggestions, picker, input, output)? {
                Some(port) => (port, suggestions.reach),
                None => return Ok(ExitCode::from(CANCELLED)),
            }
        }
    };
    let interrupts = start_interrupts(handle_signals)?;
    let (result, late) = match interrupts.during(|cancelled| {
        app.quick_forward(
            &target,
            reach.as_ref(),
            remote,
            command.local,
            Some(cancelled),
            false,
        )
    }) {
        Step::Done(result) => (result, false),
        Step::Late(result) => (result, true),
    };
    let exit = match result {
        Err(AppError::OpenSsh(OpenSshError::Cancelled)) => {
            writeln!(output, "cancelled").map_err(|error| error.to_string())?;
            return Ok(ExitCode::from(CANCELLED));
        }
        Err(refusal) => return Err(refusal.to_string()),
        Ok((tunnel, StartOutcome::Started { .. } | StartOutcome::AlreadyRunning { .. })) => {
            print_started(output, &target, &tunnel, remote)?;
            ExitCode::SUCCESS
        }
        Ok((
            tunnel,
            StartOutcome::Failed {
                reason,
                explanation,
            },
        )) => {
            print_failed_start(error, &tunnel.name, &reason, explanation)?;
            ExitCode::FAILURE
        }
    };
    // A signal after the start finished leaves the forward as it is.
    Ok(if late {
        ExitCode::from(CANCELLED)
    } else {
        exit
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::PortSuggestion;

    #[test]
    fn a_missing_port_lists_the_suggestions_or_says_there_are_none() {
        let mut suggestions = Suggestions::default();
        assert_eq!(
            port_required(&suggestions),
            "--port is required; no port was suggested"
        );
        suggestions.ports = [80, 443]
            .map(|port| PortSuggestion { port, note: None })
            .to_vec();
        assert_eq!(
            port_required(&suggestions),
            "--port is required; suggested: 80, 443"
        );
    }
}
