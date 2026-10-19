//! `--json`: one document per run on stdout, rendered from the same reports as the human
//! output. The field names of the contract (`docs/json-output.md`) are spelled here and only
//! here; the domain types know nothing of JSON.

use std::{io::Write, path::Path, process::ExitCode};

use serde_json::{Map, Value, json};

use super::{format_warning, ownership};
use crate::{
    app::{
        App, AppError, ContainerError, ContainerList, DockerIssue, HostDetails, HostInventory,
        StartOutcome, StopOutcome, Target, TunnelRefusal,
    },
    container::{DockerProblem, GroupTool, display_name, group},
    discovery::DiscoveryWarning,
    explain::Explanation,
    identity::{
        Agent, Availability, HostIdentities, Identity, IdentityInventory, Provider, PublicKey,
    },
    tunnel::{Forward, TunnelReport, TunnelStatus, TunnelWarning, printable},
};

/// The contract's version: raised only by a breaking change.
pub const SCHEMA: u64 = 1;

/// How many log lines `tunnel show` includes, as the human output.
const LOG_LINES: usize = 20;

/// A command run with `--json`. A name is `None` when it was not given.
#[derive(Debug)]
pub(super) enum JsonCommand {
    Hosts,
    Show(Option<String>),
    Identities,
    Containers(Option<Option<String>>),
    Tunnels,
    TunnelShow(Option<String>),
    TunnelStart(Vec<String>),
    TunnelStop { names: Vec<String>, all: bool },
}

/// Why a JSON command produced an error document.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Failure {
    kind: &'static str,
    message: String,
    next: Vec<String>,
    /// Docker's problem, for kind `docker`.
    problem: Option<&'static str>,
}

impl Failure {
    fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            next: Vec::new(),
            problem: None,
        }
    }

    fn usage(usage: &str) -> Self {
        Self::new("usage", format!("give the name: portico {usage} --json"))
    }

    fn explained(kind: &'static str, explanation: &Explanation) -> Self {
        Self {
            next: explanation.next.clone(),
            ..Self::new(kind, explanation.headline.clone())
        }
    }

    fn value(&self) -> Value {
        let mut object = json!({
            "kind": self.kind,
            "message": line(&self.message),
            "next": self.next.iter().map(|next| line(next)).collect::<Vec<_>>(),
        });
        if let Some(problem) = self.problem {
            object["problem"] = json!(problem);
        }
        object
    }
}

impl From<AppError> for Failure {
    fn from(error: AppError) -> Self {
        let kind = match &error {
            AppError::UnknownHost(_) | AppError::Tunnel(TunnelRefusal::UnknownHost { .. }) => {
                "unknown-host"
            }
            AppError::Tunnel(TunnelRefusal::Unknown(_)) => "unknown-tunnel",
            _ => "other",
        };
        Self::new(kind, error.to_string())
    }
}

impl From<ContainerError> for Failure {
    fn from(error: ContainerError) -> Self {
        match error {
            ContainerError::App(error) => error.into(),
            ContainerError::Ssh {
                alias,
                explanation,
                stderr,
            } => match explanation {
                Some(explanation) => Self::explained("ssh", &explanation),
                // Unexplained: ssh's own last word, as the human output shows its stderr.
                None => Self::new(
                    "ssh",
                    match stderr.lines().map(line).rfind(|said| !said.is_empty()) {
                        Some(said) => format!("ssh to {alias} failed: {said}"),
                        None => format!("ssh to {alias} failed"),
                    },
                ),
            },
            ContainerError::Docker(issue) => docker_failure(&issue),
            error => Self::new("other", error.to_string()),
        }
    }
}

fn docker_failure(issue: &DockerIssue) -> Failure {
    let mut next = issue.next.clone();
    if issue.offer.is_some() {
        let command = match &issue.target {
            Target::Local => "portico containers".to_owned(),
            Target::Host(alias) => format!("portico containers --on {alias}"),
        };
        next.push(format!(
            "Run {command} in a terminal to be offered this fix."
        ));
    }
    Failure {
        kind: "docker",
        message: issue.headline.clone(),
        next,
        problem: Some(match issue.problem {
            DockerProblem::NotInstalled => "not-installed",
            DockerProblem::DaemonDown => "daemon-down",
            DockerProblem::PermissionDenied => "permission-denied",
            DockerProblem::Other => "other",
        }),
    }
}

/// Runs `command` and prints its document, or an error document; never reads a terminal.
pub(super) fn run(command: JsonCommand, app: &App, output: &mut impl Write) -> ExitCode {
    let (document, exit) = match document(command, app) {
        Ok(result) => result,
        Err(failure) => (json!({ "error": failure.value() }), ExitCode::FAILURE),
    };
    match print(output, document) {
        Ok(()) => exit,
        Err(_) => ExitCode::FAILURE,
    }
}

/// Prints an error document of kind `other` for a failure before any command ran.
pub(super) fn fail(message: &str, output: &mut impl Write) -> ExitCode {
    let _ = print(
        output,
        json!({ "error": Failure::new("other", message).value() }),
    );
    ExitCode::FAILURE
}

/// Writes `document` with the schema version, pretty-printed, with a final newline.
fn print(output: &mut impl Write, mut document: Value) -> std::io::Result<()> {
    document["schema"] = json!(SCHEMA);
    serde_json::to_writer_pretty(&mut *output, &document)?;
    output.write_all(b"\n")?;
    output.flush()
}

fn document(command: JsonCommand, app: &App) -> Result<(Value, ExitCode), Failure> {
    let success = |document| Ok((document, ExitCode::SUCCESS));
    match command {
        JsonCommand::Hosts => success(hosts(&app.hosts()?)),
        JsonCommand::Show(alias) => {
            let alias = alias.ok_or_else(|| Failure::usage("show <alias>"))?;
            success(details(&app.show(&alias)?))
        }
        JsonCommand::Identities => success(inventory(&app.identity_inventory()?)),
        JsonCommand::Containers(on) => {
            let target = match on {
                None => Target::Local,
                Some(Some(alias)) => Target::Host(alias),
                Some(None) => return Err(Failure::usage("containers --on <alias>")),
            };
            // Batch: ssh must not ask for a password nobody sees.
            success(containers(&app.containers(&target, true, None)?))
        }
        JsonCommand::Tunnels => {
            let reports = app.tunnels()?;
            success(json!({ "tunnels": reports.iter().map(report).collect::<Vec<_>>() }))
        }
        JsonCommand::TunnelShow(name) => {
            let name = name.ok_or_else(|| Failure::usage("tunnel show <name>"))?;
            let report = app.tunnel(&name)?;
            let log = if matches!(report.status, TunnelStatus::Failed { .. }) {
                app.tunnel_log(&report.tunnel.name, LOG_LINES)
                    .iter()
                    .map(|logged| line(logged))
                    .collect()
            } else {
                Vec::new()
            };
            success(json!({
                "command": app.tunnel_command(&report.tunnel),
                "log": log,
                "tunnel": self::report(&report),
            }))
        }
        JsonCommand::TunnelStart(names) => {
            if names.is_empty() {
                return Err(Failure::usage("tunnel start <name>..."));
            }
            Ok(results(
                names.iter().map(|name| start_result(app, name)).collect(),
            ))
        }
        JsonCommand::TunnelStop { names, all } => {
            let names = if all {
                app.running_tunnels()?
            } else if names.is_empty() {
                return Err(Failure::usage("tunnel stop <name>..."));
            } else {
                names
            };
            Ok(results(
                names.iter().map(|name| stop_result(app, name)).collect(),
            ))
        }
    }
}

/// The results document of `start` or `stop`: exit 1 when any failed.
fn results(results: Vec<Value>) -> (Value, ExitCode) {
    let failed = results.iter().any(|result| result["outcome"] == "failed");
    (
        json!({ "results": results }),
        if failed {
            ExitCode::FAILURE
        } else {
            ExitCode::SUCCESS
        },
    )
}

/// One result: what happened to `name`, and its report as it stands now.
fn result(
    app: &App,
    name: &str,
    outcome: &str,
    pid: Option<u32>,
    failure: Option<Failure>,
) -> Value {
    let tunnel = app.tunnel(name).ok();
    json!({
        "error": failure.map(|failure| failure.value()),
        "name": tunnel.as_ref().map_or(name, |report| report.tunnel.name.as_str()),
        "outcome": outcome,
        "pid": pid,
        "tunnel": tunnel.as_ref().map(report),
    })
}

fn start_result(app: &App, name: &str) -> Value {
    match app.start_tunnel(name, None, false) {
        Ok(StartOutcome::Started { pid }) => result(app, name, "started", Some(pid), None),
        Ok(StartOutcome::AlreadyRunning { pid }) => {
            result(app, name, "already-running", Some(pid), None)
        }
        Ok(StartOutcome::Failed {
            reason,
            explanation,
        }) => {
            let failure = match explanation {
                Some(explanation) => Failure::explained("ssh", &explanation),
                None => Failure::new("ssh", reason),
            };
            result(app, name, "failed", None, Some(failure))
        }
        Err(error) => result(app, name, "failed", None, Some(error.into())),
    }
}

fn stop_result(app: &App, name: &str) -> Value {
    match app.stop_tunnel(name) {
        Ok(StopOutcome::Stopped) => result(app, name, "stopped", None, None),
        Ok(StopOutcome::NotRunning) => result(app, name, "not-running", None, None),
        Err(error) => result(app, name, "failed", None, Some(error.into())),
    }
}

/// Free text from Portico or a child program as one printable line: lines joined with a
/// space, terminal escape sequences and control characters removed.
fn line(text: &str) -> String {
    text.lines()
        .map(|part| printable(part).trim().to_owned())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// An optional free text as one printable line.
fn optional_line(text: Option<&str>) -> Value {
    json!(text.map(line))
}

/// A path as a string; one that is not UTF-8 is converted lossily.
fn path(path: &Path) -> Value {
    json!(path.to_string_lossy())
}

fn warnings(warnings: &[DiscoveryWarning]) -> Value {
    warnings
        .iter()
        .map(|warning| json!({ "message": line(&format_warning(warning)) }))
        .collect()
}

pub(super) fn hosts(inventory: &HostInventory) -> Value {
    let hosts: Vec<Value> = inventory
        .hosts
        .iter()
        .map(|host| {
            json!({
                "alias": host.alias,
                "ownership": ownership(host.ownership),
                "source": path(&host.source),
            })
        })
        .collect();
    json!({ "hosts": hosts, "warnings": warnings(&inventory.warnings) })
}

pub(super) fn details(details: &HostDetails) -> Value {
    let declared = details.declared.as_ref().map(|host| {
        json!({
            "hostname": host.hostname(),
            "identity_file": host.identity_file(),
            "port": host.port(),
            "proxy_jump": host.proxy_jump(),
            "user": host.user(),
        })
    });
    let (resolved, resolution_error) = match &details.resolution {
        Ok(resolved) => (
            json!({
                "hostname": resolved.hostname,
                "identity_files": resolved.identity_files,
                "port": resolved.port,
                "proxy_jump": resolved.proxy_jump,
                "user": resolved.user,
            }),
            Value::Null,
        ),
        Err(error) => (Value::Null, json!(line(error))),
    };
    json!({
        "host": {
            "alias": details.summary.alias,
            "declared": declared,
            "identities": details.identities.as_ref().map(host_identities),
            "ownership": ownership(details.summary.ownership),
            "resolution_error": resolution_error,
            "resolved": resolved,
            "source": path(&details.summary.source),
        },
        "warnings": warnings(&details.warnings),
    })
}

fn public_key(key: &PublicKey) -> Value {
    json!({
        "bits": key.bits,
        "comment": line(&key.comment),
        "fingerprint": key.fingerprint,
        "type": key.key_type,
    })
}

fn provider(provider: Provider) -> &'static str {
    match provider {
        Provider::OnePassword => "1password",
        Provider::GpgAgent => "gpg-agent",
        Provider::GnomeKeyring => "gnome-keyring",
        Provider::Generic => "generic",
    }
}

/// The agent's state and what identifies it; `keys` lists the keys, or only counts them.
fn agent(agent: &Agent, list_keys: bool) -> Map<String, Value> {
    let value = match agent {
        Agent::Disabled => json!({ "state": "disabled" }),
        Agent::NotConfigured { variable } => {
            json!({ "state": "not-configured", "variable": variable })
        }
        Agent::Unavailable {
            socket,
            provider: name,
            reason,
        } => {
            let mut value = json!({
                "provider": provider(*name),
                "reason": line(&reason.describe()),
                "socket": path(socket),
                "state": "unavailable",
            });
            if list_keys {
                value["keys"] = json!([]);
            }
            value
        }
        Agent::Listed {
            socket,
            provider: name,
            keys,
        } => {
            let mut value = json!({
                "provider": provider(*name),
                "socket": path(socket),
                "state": "listed",
            });
            if list_keys {
                value["keys"] = keys.iter().map(public_key).collect();
            } else {
                value["key_count"] = json!(keys.len());
            }
            value
        }
    };
    match value {
        Value::Object(map) => map,
        _ => unreachable!("an agent renders as an object"),
    }
}

fn availability(availability: &Availability) -> &'static str {
    match availability {
        Availability::InAgent => "in-agent",
        Availability::KeyFile { .. } => "key-file",
        Availability::Missing(_) => "missing",
        Availability::Unchecked { .. } => "unchecked",
    }
}

fn identity(identity: &Identity) -> Value {
    json!({
        "availability": availability(&identity.availability),
        "configured": identity.configured,
        "path": identity.path.as_deref().map(path),
        "public_key": identity.public_key.as_ref().map(public_key),
        "reason": optional_line(identity.availability.reason().as_deref()),
    })
}

pub(super) fn host_identities(identities: &HostIdentities) -> Value {
    let also_offered: Vec<Value> =
        if identities.identities_only || identities.agent.keys().is_none() {
            Vec::new()
        } else {
            identities.also_offered.iter().map(public_key).collect()
        };
    json!({
        "agent": agent(&identities.agent, false),
        "also_offered": also_offered,
        "defaults": identities.defaults,
        "identities": identities.identities.iter().map(identity).collect::<Vec<_>>(),
        "identities_only": identities.identities_only,
        "pkcs11_provider": identities.pkcs11_provider,
    })
}

pub(super) fn inventory(inventory: &IdentityInventory) -> Value {
    let agents: Vec<Value> = inventory
        .agents
        .iter()
        // As the human output: only agents with a socket.
        .filter(|known| known.agent.socket().is_some())
        .map(|known| {
            let mut map = agent(&known.agent, true);
            map.insert("environment".into(), json!(known.environment));
            map.insert("used_by".into(), json!(known.used_by));
            Value::Object(map)
        })
        .collect();
    let files: Vec<Value> = inventory
        .files
        .iter()
        .map(|file| {
            json!({
                "availability": availability(&file.availability),
                "configured": file.configured,
                "path": file.path.as_deref().map(path),
                "public_key": file.public_key.as_ref().map(public_key),
                "reason": optional_line(file.availability.reason().as_deref()),
                "used_by": file.used_by,
            })
        })
        .collect();
    let warnings: Vec<Value> = inventory
        .warnings
        .iter()
        .map(|warning| json!({ "message": line(warning) }))
        .collect();
    json!({ "agents": agents, "files": files, "warnings": warnings })
}

pub(super) fn containers(list: &ContainerList) -> Value {
    let all = &list.listing.containers;
    // The human output's order: grouped by project, then the others.
    let containers: Vec<Value> = group(all)
        .into_iter()
        .flat_map(|group| group.containers)
        .map(|container| {
            json!({
                "display_name": line(&display_name(container, all)),
                "id": container.id,
                "image": line(&container.image),
                "name": line(&container.name),
                "project": container.project,
                "service": container.service,
                "status": line(&container.status),
            })
        })
        .collect();
    let target = match &list.target {
        Target::Local => json!({ "alias": null, "kind": "local" }),
        Target::Host(alias) => json!({ "alias": alias, "kind": "host" }),
    };
    json!({
        "containers": containers,
        "note": optional_line(list.note.as_deref()),
        "skipped": list.listing.skipped,
        "target": target,
        "via": list.via.map(|tool| match tool {
            GroupTool::Sg => "sg",
        }),
    })
}

fn forward(forward: &Forward) -> Value {
    json!({
        "bind_address": forward.bind_address,
        "bind_port": forward.bind_port,
        "description": forward.describe(),
        "kind": forward.kind.name(),
        "spec": forward.spec(),
        "target_host": forward.destination.as_ref().map(|destination| &destination.host),
        "target_port": forward.destination.as_ref().map(|destination| destination.port),
    })
}

pub(super) fn status(status: &TunnelStatus) -> Value {
    let mut value = match status {
        TunnelStatus::Stopped => json!({}),
        TunnelStatus::Running { pid, started } => json!({ "pid": pid, "started": started }),
        TunnelStatus::Failed { reason, finished } => {
            json!({ "finished": finished, "reason": line(reason) })
        }
        TunnelStatus::PortInUse { address, by } => json!({ "address": address, "by": by }),
    };
    value["state"] = json!(status.name());
    value
}

fn tunnel_warning(warning: &TunnelWarning) -> Value {
    let mut value = match warning {
        TunnelWarning::UnknownHost { host } => json!({ "host": host, "kind": "unknown-host" }),
        TunnelWarning::CollidesWith { tunnel } => {
            json!({ "kind": "collides-with", "tunnel": tunnel })
        }
    };
    value["message"] = json!(line(&warning.to_string()));
    value
}

pub(super) fn report(report: &TunnelReport) -> Value {
    json!({
        "forward": forward(&report.tunnel.forward),
        "host": report.tunnel.host,
        "name": report.tunnel.name,
        "status": status(&report.status),
        "warnings": report.warnings.iter().map(tunnel_warning).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt, path::PathBuf};

    use super::*;
    use crate::{
        app::{HostSummary, Ownership},
        container::{Container, Listing},
        identity::MissingReason,
        tunnel::{Kind, Tunnel},
    };

    fn text(document: Value) -> String {
        let mut output = Vec::new();
        print(&mut output, document).unwrap();
        String::from_utf8(output).unwrap()
    }

    #[test]
    fn keys_come_out_sorted() {
        // The golden files rely on it; serde_json's `preserve_order` feature would break it.
        assert_eq!(
            json!({ "b": 1, "a": 2, "c": 3 }).to_string(),
            r#"{"a":2,"b":1,"c":3}"#
        );
    }

    #[test]
    fn documents_carry_the_schema_and_end_with_a_newline() {
        assert_eq!(
            text(json!({ "x": [] })),
            "{\n  \"schema\": 1,\n  \"x\": []\n}\n"
        );
    }

    #[test]
    fn every_status_has_its_keys() {
        assert_eq!(
            status(&TunnelStatus::Stopped),
            json!({ "state": "stopped" })
        );
        assert_eq!(
            status(&TunnelStatus::Running {
                pid: 7,
                started: 100
            }),
            json!({ "state": "running", "pid": 7, "started": 100 })
        );
        assert_eq!(
            status(&TunnelStatus::Failed {
                reason: "exited".into(),
                finished: None
            }),
            json!({ "state": "failed", "reason": "exited", "finished": null })
        );
        assert_eq!(
            status(&TunnelStatus::PortInUse {
                address: "127.0.0.1:5432".into(),
                by: Some("db".into())
            }),
            json!({ "state": "port-in-use", "address": "127.0.0.1:5432", "by": "db" })
        );
    }

    #[test]
    fn a_dynamic_forward_has_no_target() {
        let forward = Forward::parse(Kind::Dynamic, "1080").unwrap();
        let value = self::forward(&forward);
        assert_eq!(value["kind"], "dynamic");
        assert_eq!(value["bind_address"], Value::Null);
        assert_eq!(value["target_host"], Value::Null);
        assert_eq!(value["target_port"], Value::Null);
        assert_eq!(value["spec"], "1080");
    }

    #[test]
    fn a_report_names_its_warnings() {
        let tunnel = Tunnel::new(
            "db",
            "prod",
            Forward::parse(Kind::Local, "5432:localhost:5432").unwrap(),
        )
        .unwrap();
        let value = report(&TunnelReport {
            tunnel,
            status: TunnelStatus::Stopped,
            warnings: vec![
                TunnelWarning::UnknownHost {
                    host: "prod".into(),
                },
                TunnelWarning::CollidesWith {
                    tunnel: "pg".into(),
                },
            ],
        });
        assert_eq!(value["forward"]["target_port"], 5432);
        assert_eq!(value["warnings"][0]["kind"], "unknown-host");
        assert_eq!(value["warnings"][0]["host"], "prod");
        assert_eq!(value["warnings"][1]["kind"], "collides-with");
        assert_eq!(value["warnings"][1]["tunnel"], "pg");
        assert!(
            value["warnings"][1]["message"]
                .as_str()
                .unwrap()
                .contains("pg")
        );
    }

    #[test]
    fn every_agent_state_has_its_keys() {
        let socket = PathBuf::from("/run/agent.sock");
        let key = PublicKey {
            key_type: "ED25519".into(),
            bits: 256,
            fingerprint: "SHA256:abc".into(),
            comment: "me".into(),
        };
        let listed = Agent::Listed {
            socket: socket.clone(),
            provider: Provider::OnePassword,
            keys: vec![key.clone()],
        };
        assert_eq!(
            Value::Object(agent(&listed, false)),
            json!({ "state": "listed", "provider": "1password", "socket": "/run/agent.sock", "key_count": 1 })
        );
        assert_eq!(
            Value::Object(agent(&listed, true))["keys"][0],
            json!({ "type": "ED25519", "bits": 256, "fingerprint": "SHA256:abc", "comment": "me" })
        );
        let unavailable = Agent::Unavailable {
            socket,
            provider: Provider::Generic,
            reason: crate::identity::AgentProblem::NoSuchSocket,
        };
        assert_eq!(
            Value::Object(agent(&unavailable, false)),
            json!({ "state": "unavailable", "provider": "generic", "socket": "/run/agent.sock", "reason": "no such socket" })
        );
        assert_eq!(
            Value::Object(agent(&Agent::Disabled, false)),
            json!({ "state": "disabled" })
        );
        assert_eq!(
            Value::Object(agent(
                &Agent::NotConfigured {
                    variable: "SSH_AUTH_SOCK".into()
                },
                false
            )),
            json!({ "state": "not-configured", "variable": "SSH_AUTH_SOCK" })
        );
    }

    #[test]
    fn every_availability_has_a_name_and_a_reason_when_needed() {
        for (availability_, name, reason) in [
            (Availability::InAgent, "in-agent", None),
            (
                Availability::KeyFile {
                    public_key_found: true,
                },
                "key-file",
                None,
            ),
            (
                Availability::Missing(MissingReason::NoSuchFile),
                "missing",
                Some("no such file"),
            ),
            (
                Availability::Unchecked { token: "%C".into() },
                "unchecked",
                Some("Portico does not expand %C"),
            ),
        ] {
            let value = identity(&Identity {
                configured: "~/.ssh/id".into(),
                path: None,
                public_key: None,
                availability: availability_,
            });
            assert_eq!(value["availability"], name);
            assert_eq!(value["reason"], json!(reason));
            assert_eq!(value["path"], Value::Null);
        }
    }

    #[test]
    fn an_external_host_declares_nothing_and_a_failed_resolution_says_why() {
        let value = details(&HostDetails {
            summary: HostSummary {
                alias: "legacy".into(),
                ownership: Ownership::External,
                source: PathBuf::from(OsStr::from_bytes(b"/home/me/.ssh/conf\xffig")),
            },
            declared: None,
            resolution: Err("ssh -G failed".into()),
            identities: None,
            warnings: Vec::new(),
        });
        let host = &value["host"];
        assert_eq!(host["declared"], Value::Null);
        assert_eq!(host["resolved"], Value::Null);
        assert_eq!(host["identities"], Value::Null);
        assert_eq!(host["resolution_error"], "ssh -G failed");
        assert_eq!(host["ownership"], "external");
        // Not UTF-8: still shown, lossily.
        assert_eq!(host["source"], "/home/me/.ssh/conf\u{fffd}ig");
    }

    #[test]
    fn control_characters_are_escaped() {
        let document = text(json!({ "reason": "\u{1b}[31mred\u{7}" }));
        assert!(document.contains(r#""\u001b[31mred\u0007""#), "{document}");
        assert!(!document.contains('\u{1b}'));
    }

    #[test]
    fn free_text_is_one_printable_line() {
        let raw = "ssh: \u{1b}[31mConnection refused\u{1b}[0m\r\n\tby\u{7} 10.0.0.5\n\n";
        assert_eq!(line(raw), "ssh: Connection refused by 10.0.0.5");
        let failure = Failure::from(AppError::Tunnel(TunnelRefusal::Rejected {
            name: "db".into(),
            stderr: "Bad local forwarding\nspecification \u{1b}]0;title\u{7}'1:x'".into(),
        }));
        let message = failure.value()["message"].as_str().unwrap().to_owned();
        assert_eq!(
            message,
            "OpenSSH rejected tunnel db: Bad local forwarding specification '1:x'"
        );
        let value = Value::Object(agent(
            &Agent::Unavailable {
                socket: PathBuf::from("/run/agent.sock"),
                provider: Provider::Generic,
                reason: crate::identity::AgentProblem::Refused(
                    "error fetching identities:\n\u{1b}[1mcommunication\u{1b}[0m".into(),
                ),
            },
            false,
        ));
        assert_eq!(value["reason"], "error fetching identities: communication");
        let value = details(&HostDetails {
            summary: HostSummary {
                alias: "x".into(),
                ownership: Ownership::External,
                source: PathBuf::from("/c"),
            },
            declared: None,
            resolution: Err("ssh exited\nline 2: \u{1b}[2Kbad".into()),
            identities: None,
            warnings: Vec::new(),
        });
        assert_eq!(value["host"]["resolution_error"], "ssh exited line 2: bad");
    }

    #[test]
    fn an_unexplained_ssh_failure_keeps_ssh_s_last_word() {
        let failure = Failure::from(ContainerError::Ssh {
            alias: "dev".into(),
            explanation: None,
            stderr: "debug noise\n\u{1b}[31mkex_exchange_identification: read: Connection reset\u{1b}[0m\n\n"
                .into(),
        });
        assert_eq!(
            failure.value()["message"],
            "ssh to dev failed: kex_exchange_identification: read: Connection reset"
        );
        let failure = Failure::from(ContainerError::Ssh {
            alias: "dev".into(),
            explanation: None,
            stderr: String::new(),
        });
        assert_eq!(failure.value()["message"], "ssh to dev failed");
    }

    #[test]
    fn docker_failures_name_their_problem_and_how_to_be_offered_the_fix() {
        let failure = docker_failure(&DockerIssue {
            target: Target::Host("dev".into()),
            problem: DockerProblem::PermissionDenied,
            headline: "me cannot use Docker on dev".into(),
            next: vec!["Add me to the docker group".into()],
            offer: Some(crate::app::Offer::StartDaemon),
            stderr: String::new(),
        });
        assert_eq!(
            failure.value(),
            json!({
                "kind": "docker",
                "problem": "permission-denied",
                "message": "me cannot use Docker on dev",
                "next": [
                    "Add me to the docker group",
                    "Run portico containers --on dev in a terminal to be offered this fix.",
                ],
            })
        );
    }

    #[test]
    fn errors_have_kinds() {
        assert_eq!(
            Failure::from(AppError::UnknownHost("x".into())).value()["kind"],
            "unknown-host"
        );
        assert_eq!(
            Failure::from(AppError::Tunnel(TunnelRefusal::Unknown("x".into()))).value()["kind"],
            "unknown-tunnel"
        );
        assert_eq!(
            Failure::from(AppError::ReadOnlyExternal("x".into())).value(),
            json!({ "kind": "other", "message": "external host is read-only: x", "next": [] })
        );
        assert_eq!(Failure::usage("show <alias>").value()["kind"], "usage");
    }

    #[test]
    fn containers_follow_the_human_order_and_leave_out_their_command() {
        let container = |id: &str, name: &str, project: Option<&str>| Container {
            id: id.into(),
            name: name.into(),
            image: "img".into(),
            command: "run --password=s3cret".into(),
            running_for: String::new(),
            status: "Up".into(),
            ports: "80/tcp".into(),
            networks: String::new(),
            mounts: "/srv".into(),
            project: project.map(String::from),
            service: project.map(|_| "web".into()),
            number: project.map(|_| 1),
            working_dir: None,
        };
        let value = containers(&ContainerList {
            target: Target::Local,
            listing: Listing {
                containers: vec![
                    container("c", "cache", None),
                    container("a", "shop-web-1", Some("shop")),
                ],
                skipped: 2,
            },
            via: Some(GroupTool::Sg),
            note: Some("note".into()),
        });
        assert_eq!(value["containers"][0]["id"], "a");
        assert_eq!(value["containers"][1]["id"], "c");
        assert_eq!(value["containers"][1]["project"], Value::Null);
        assert_eq!(value["target"], json!({ "kind": "local", "alias": null }));
        assert_eq!(value["via"], "sg");
        assert_eq!(value["skipped"], 2);
        assert!(!value.to_string().contains("s3cret"));
        assert!(!value.to_string().contains("80/tcp"));
    }
}
