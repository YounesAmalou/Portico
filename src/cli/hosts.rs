use std::{
    io::{BufRead, Write},
    path::Path,
    process::ExitCode,
};

use crate::{
    app::{
        App, AppError, HostDetails, HostSummary, Ownership, TestOutcome, TestReport, TestTarget,
    },
    domain::discovery::DiscoveryWarning,
    domain::explain::Explanation,
    domain::host::{Change, ClearField},
    domain::identity::{self, Agent, HostIdentities, IdentityInventory},
    domain::managed::InitPlan,
    domain::openssh::{ConnectionOutcome, OpenSshError, PresentedKey},
    picker::{Picker, Row},
};

use super::{CANCELLED, Interrupts, Step, confirm, exit_code};

/// `portico test`: tests `alias`, offers to trust an unknown host key and tests again.
#[allow(clippy::too_many_arguments)]
pub(super) fn test_command<I: BufRead, O: Write>(
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
pub(super) fn finish_connection<I: BufRead, E: Write>(
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

/// The host `alias` names, otherwise one chosen in the picker; `None` when the choice is
/// cancelled. `managed_only` offers only the hosts Portico manages, for commands that change one.
pub(super) fn host_argument(
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
pub(super) fn print_connection_failure(
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

pub(super) fn print_report(
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
pub(super) fn field_change<T>(
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

pub(super) fn print_init_plan(output: &mut impl Write, plan: &InitPlan) -> Result<(), String> {
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

pub(super) fn print_details(
    output: &mut impl Write,
    details: &HostDetails,
    home: &Path,
) -> Result<(), String> {
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
    if let Some(identities) = &details.identities {
        print_identities(output, identities, home)?;
    }
    for warning in &details.warnings {
        writeln!(output, "Warning: {}", format_warning(warning))
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// The identities block of `portico show`, after every resolved field.
fn print_identities(
    output: &mut impl Write,
    identities: &HostIdentities,
    home: &Path,
) -> Result<(), String> {
    let mut lines = vec![format!("Agent: {}", identities.agent.describe(home))];
    let default = if identities.defaults {
        " (OpenSSH default)"
    } else {
        ""
    };
    for identity in &identities.identities {
        lines.push(format!(
            "Identity: {}{default}: {}",
            crate::platform::paths::tilde_text(&identity.configured, home),
            identity.describe()
        ));
    }
    if identities.defaults {
        lines.push(if identities.identities.is_empty() {
            "Identity: OpenSSH defaults: none present".into()
        } else {
            "Identity: other OpenSSH defaults: none present".into()
        });
    } else if identities.identities.is_empty() {
        lines.push("Identity: none".into());
    }
    if identities.identities_only {
        lines.push("IdentitiesOnly: yes".into());
    } else if identities.agent.keys().is_some() {
        let mut line = format!(
            "Also offered by the agent: {}",
            identities.also_offered.len()
        );
        if !identities.also_offered.is_empty() {
            let names = identities
                .also_offered
                .iter()
                .map(|key| {
                    if key.comment.is_empty() {
                        key.fingerprint.clone()
                    } else {
                        key.comment.clone()
                    }
                })
                .collect::<Vec<_>>();
            line.push_str(&format!(" ({})", names.join(", ")));
        }
        lines.push(line);
    }
    if let Some(provider) = &identities.pkcs11_provider {
        lines.push(format!("PKCS#11 provider: {provider}"));
    }
    for line in lines {
        writeln!(output, "{line}").map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// `portico identities`: the agents with their keys, then every identity file.
pub(super) fn print_inventory(
    output: &mut impl Write,
    inventory: &IdentityInventory,
    home: &Path,
) -> Result<(), String> {
    let mut text = String::from("Agents\n");
    if inventory.agents.is_empty() {
        text.push_str("  none (SSH_AUTH_SOCK is not set and no host names an agent)\n");
    }
    for known in &inventory.agents {
        let socket = known
            .agent
            .socket()
            .map(|socket| crate::platform::paths::tilde(socket, home))
            .unwrap_or_default();
        let (name, state) = match &known.agent {
            Agent::Listed { provider, keys, .. } => {
                (provider.name(), identity::count(keys.len(), "key"))
            }
            Agent::Unavailable {
                provider, reason, ..
            } => (
                provider.name(),
                format!("unavailable: {}", reason.describe()),
            ),
            Agent::Disabled | Agent::NotConfigured { .. } => continue,
        };
        let mut uses = Vec::new();
        if known.environment {
            uses.push("SSH_AUTH_SOCK".to_owned());
        }
        if !known.used_by.is_empty() {
            uses.push(format!(
                "used by {}",
                identity::count(known.used_by.len(), "host")
            ));
        }
        text.push_str(&format!("  {name}  {socket}  {state}"));
        if !uses.is_empty() {
            text.push_str(&format!("  ({})", uses.join(", ")));
        }
        text.push('\n');
        let keys = known.agent.keys().unwrap_or_default();
        let types = keys.iter().map(|key| key.key_type.len()).max().unwrap_or(0);
        let fingerprints = keys
            .iter()
            .map(|key| key.fingerprint.len())
            .max()
            .unwrap_or(0);
        let comments = keys
            .iter()
            .map(|key| key.comment.chars().count())
            .max()
            .unwrap_or(0);
        for key in keys {
            let paired = inventory.files.iter().find(|file| {
                file.public_key
                    .as_ref()
                    .is_some_and(|own| own.fingerprint == key.fingerprint)
            });
            let line = format!(
                "    {:<types$}  {:<fingerprints$}  {:<comments$}  {}",
                key.key_type,
                key.fingerprint,
                key.comment,
                paired.map_or("", |file| file.configured.as_str())
            );
            text.push_str(line.trim_end());
            text.push('\n');
        }
    }
    text.push_str("\nIdentity files\n");
    if inventory.files.is_empty() {
        text.push_str("  none\n");
    }
    let paths = inventory
        .files
        .iter()
        .map(|file| file.configured.chars().count())
        .max()
        .unwrap_or(0);
    let words = inventory
        .files
        .iter()
        .map(|file| file.availability.word().len())
        .max()
        .unwrap_or(0);
    for file in &inventory.files {
        let used_by = if file.used_by.is_empty() {
            "-".to_owned()
        } else {
            file.used_by.join(", ")
        };
        text.push_str(&format!(
            "  {:<paths$}  {:<words$}  used by: {used_by}\n",
            file.configured,
            file.availability.word(),
        ));
    }
    for warning in &inventory.warnings {
        text.push_str(&format!("Warning: {warning}\n"));
    }
    output
        .write_all(text.as_bytes())
        .map_err(|error| error.to_string())
}

fn print_optional(output: &mut impl Write, label: &str, value: Option<&str>) -> Result<(), String> {
    writeln!(output, "{label}: {}", value.unwrap_or("-")).map_err(|error| error.to_string())
}

pub(super) fn ownership(ownership: Ownership) -> &'static str {
    match ownership {
        Ownership::Managed => "managed",
        Ownership::External => "external",
    }
}

pub(super) fn format_warning(warning: &DiscoveryWarning) -> String {
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
    use super::*;

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

    fn key(fingerprint: &str, comment: &str) -> identity::PublicKey {
        identity::PublicKey {
            key_type: "ED25519".into(),
            bits: 256,
            fingerprint: fingerprint.into(),
            comment: comment.into(),
        }
    }

    fn shown_details(identities: Option<HostIdentities>) -> String {
        let details = HostDetails {
            summary: HostSummary {
                alias: "dev".into(),
                ownership: Ownership::External,
                source: "/home/me/.ssh/config".into(),
            },
            declared: None,
            resolution: Ok(crate::domain::openssh::ResolvedConfig {
                hostname: "dev.example".into(),
                user: Some("deploy".into()),
                port: Some(22),
                identity_files: vec!["~/.ssh/github.pub".into(), "~/.ssh/id_work".into()],
                proxy_jump: None,
                proxy_command: None,
                proxy_use_fdpass: false,
                known_hosts_files: Vec::new(),
                identity_agent: None,
                identities_only: true,
                pkcs11_provider: None,
            }),
            identities,
            warnings: Vec::new(),
        };
        let mut output = Vec::new();
        print_details(&mut output, &details, Path::new("/home/me")).unwrap();
        String::from_utf8(output).unwrap()
    }

    #[test]
    fn show_keeps_existing_lines_unchanged() {
        let before = "Alias: dev\nOwnership: external\nSource: /home/me/.ssh/config\n\
                      Resolved HostName: dev.example\nResolved User: deploy\nResolved Port: 22\n\
                      Resolved IdentityFile: ~/.ssh/github.pub, ~/.ssh/id_work\n\
                      Resolved ProxyJump: -\n";
        assert_eq!(shown_details(None), before);
        let identities = HostIdentities {
            agent: Agent::Listed {
                socket: "/home/me/.1password/agent.sock".into(),
                provider: identity::Provider::OnePassword,
                keys: vec![key("SHA256:gh", "GitHub"), key("SHA256:vps", "VPS")],
            },
            defaults: false,
            identities: vec![
                identity::Identity {
                    configured: "~/.ssh/github.pub".into(),
                    path: Some("/home/me/.ssh/github.pub".into()),
                    public_key: Some(key("SHA256:gh", "GitHub")),
                    availability: identity::Availability::InAgent,
                },
                identity::Identity {
                    configured: "/home/me/.ssh/id_work".into(),
                    path: Some("/home/me/.ssh/id_work".into()),
                    public_key: None,
                    availability: identity::Availability::Missing(
                        identity::MissingReason::NoSuchFile,
                    ),
                },
            ],
            identities_only: true,
            also_offered: Vec::new(),
            pkcs11_provider: Some("/usr/lib/p11.so".into()),
        };
        assert_eq!(
            shown_details(Some(identities)),
            format!(
                "{before}Agent: 1Password, ~/.1password/agent.sock, 2 keys\n\
                 Identity: ~/.ssh/github.pub: in agent (ED25519 SHA256:gh \"GitHub\")\n\
                 Identity: ~/.ssh/id_work: missing (no such file)\n\
                 IdentitiesOnly: yes\n\
                 PKCS#11 provider: /usr/lib/p11.so\n"
            )
        );
    }

    #[test]
    fn show_names_defaults_and_keys_the_agent_also_offers() {
        let identities = HostIdentities {
            agent: Agent::Listed {
                socket: "/run/agent".into(),
                provider: identity::Provider::Generic,
                keys: vec![key("SHA256:vps", "VPS"), key("SHA256:x", "")],
            },
            defaults: true,
            identities: Vec::new(),
            identities_only: false,
            also_offered: vec![key("SHA256:vps", "VPS"), key("SHA256:x", "")],
            pkcs11_provider: None,
        };
        let shown = shown_details(Some(identities));
        assert!(
            shown.ends_with(
                "Agent: SSH agent, /run/agent, 2 keys\n\
                 Identity: OpenSSH defaults: none present\n\
                 Also offered by the agent: 2 (VPS, SHA256:x)\n"
            ),
            "{shown}"
        );
    }

    #[test]
    fn identities_lists_agents_then_files() {
        let inventory = IdentityInventory {
            agents: vec![
                identity::InventoryAgent {
                    agent: Agent::Listed {
                        socket: "/home/me/.1password/agent.sock".into(),
                        provider: identity::Provider::OnePassword,
                        keys: vec![key("SHA256:gh", "GitHub"), key("SHA256:vps", "VPS")],
                    },
                    environment: true,
                    used_by: vec!["a".into(), "b".into()],
                },
                identity::InventoryAgent {
                    agent: Agent::Unavailable {
                        socket: "/run/gone".into(),
                        provider: identity::Provider::Generic,
                        reason: identity::AgentProblem::NoSuchSocket,
                    },
                    environment: false,
                    used_by: vec!["c".into()],
                },
            ],
            files: vec![
                identity::InventoryFile {
                    configured: "~/.ssh/github.pub".into(),
                    path: Some("/home/me/.ssh/github.pub".into()),
                    public_key: Some(key("SHA256:gh", "GitHub")),
                    availability: identity::Availability::InAgent,
                    used_by: vec!["a".into(), "b".into()],
                },
                identity::InventoryFile {
                    configured: "~/.ssh/old".into(),
                    path: Some("/home/me/.ssh/old".into()),
                    public_key: None,
                    availability: identity::Availability::KeyFile {
                        public_key_found: false,
                    },
                    used_by: Vec::new(),
                },
            ],
            warnings: vec!["z: could not resolve: boom".into()],
        };
        let mut output = Vec::new();
        print_inventory(&mut output, &inventory, Path::new("/home/me")).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "Agents\n\
             \x20 1Password  ~/.1password/agent.sock  2 keys  (SSH_AUTH_SOCK, used by 2 hosts)\n\
             \x20   ED25519  SHA256:gh   GitHub  ~/.ssh/github.pub\n\
             \x20   ED25519  SHA256:vps  VPS\n\
             \x20 SSH agent  /run/gone  unavailable: no such socket  (used by 1 host)\n\
             \n\
             Identity files\n\
             \x20 ~/.ssh/github.pub  in agent  used by: a, b\n\
             \x20 ~/.ssh/old         key file  used by: -\n\
             Warning: z: could not resolve: boom\n"
        );
    }
}
