//! Portico's `--json` documents (docs/json-output.md) as the rows the menu
//! shows, and the argument lists the app runs. Pure functions: no GUI types and
//! no I/O. It follows desktop/quickshell/portico.js rule for rule.

use serde_json::Value;

/// The one schema this app reads.
const SCHEMA: i64 = 1;

const OLD_PORTICO: &str =
    "This Portico does not print the JSON this app reads; it needs a Portico release with --json.";

/// A document that is schema 1 (it may still be an error document), or why not.
#[derive(Debug, Clone, PartialEq)]
pub enum Parsed {
    Doc(Value),
    Problem(String),
}

/// The only reader of raw stdout.
pub fn parse(stdout: &str, _exit: Option<i32>) -> Parsed {
    let doc = match serde_json::from_str::<Value>(stdout) {
        Ok(doc) if doc.is_object() && doc.get("schema").is_some() => doc,
        _ => return Parsed::Problem(OLD_PORTICO.to_string()),
    };
    match &doc["schema"] {
        Value::Number(n) if n.as_i64() == Some(SCHEMA) => Parsed::Doc(doc),
        Value::Number(n) if n.as_f64().is_some_and(|n| n > SCHEMA as f64) => {
            Parsed::Problem(format!(
                "This Portico prints schema {n}; this app reads schema {SCHEMA}. Update the tray app."
            ))
        }
        other => {
            let shown = other
                .as_str()
                .map_or_else(|| other.to_string(), str::to_string);
            Parsed::Problem(format!(
                "This Portico prints schema {shown}; this app reads schema {SCHEMA}. Update Portico."
            ))
        }
    }
}

fn text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn list(value: &Value) -> &[Value] {
    value.as_array().map_or(&[], Vec::as_slice)
}

/// The `error` of a document: its message and next steps.
pub fn error_of(doc: &Value) -> Option<(String, Vec<String>)> {
    let error = doc.get("error").filter(|e| e.is_object())?;
    let next = list(&error["next"]).iter().map(text).collect();
    Some((text(&error["message"]), next))
}

/// Which sections the settings turn on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tabs {
    pub tunnels: bool,
    pub docker: bool,
    pub podman: bool,
    pub proxmox: bool,
}

/// The tabs of a `settings --json` document; a missing key (or an error
/// document) leaves a tab on, so the app keeps working with any Portico.
pub fn tabs(doc: &Value) -> Tabs {
    let on = |name: &str| doc["tabs"][name] != Value::Bool(false);
    Tabs {
        tunnels: on("tunnels"),
        docker: on("docker"),
        podman: on("podman"),
        proxmox: on("proxmox"),
    }
}

const BACKENDS: [&str; 3] = ["docker", "podman", "proxmox"];

/// The enabled container backends, in Docker, Podman, Proxmox order.
pub fn enabled_backends(t: &Tabs) -> Vec<&'static str> {
    let flags = [t.docker, t.podman, t.proxmox];
    BACKENDS
        .iter()
        .zip(flags)
        .filter(|(_, on)| *on)
        .map(|(name, _)| *name)
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelState {
    Running,
    Stopped,
    Failed(String),
    PortInUse { address: String, by: Option<String> },
    Unknown(String),
}

/// What clicking a tunnel does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Start,
    Stop,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tunnel {
    pub name: String,
    /// The name and what it forwards, as one line.
    pub label: String,
    pub state: TunnelState,
    pub warnings: Vec<String>,
}

impl Tunnel {
    pub fn action(&self) -> Action {
        match self.state {
            TunnelState::Running => Action::Stop,
            TunnelState::Unknown(_) => Action::None,
            _ => Action::Start,
        }
    }
}

fn warning_messages(value: &Value) -> Vec<String> {
    list(&value["warnings"])
        .iter()
        .map(|w| text(&w["message"]))
        .filter(|m| !m.is_empty())
        .collect()
}

pub fn tunnels(doc: &Value) -> Vec<Tunnel> {
    list(&doc["tunnels"])
        .iter()
        .map(|report| {
            let name = text(&report["name"]);
            let description = text(&report["forward"]["description"]);
            let label = if description.is_empty() {
                name.clone()
            } else {
                format!("{name}  {description}")
            };
            let status = &report["status"];
            let state = match text(&status["state"]).as_str() {
                "running" => TunnelState::Running,
                "stopped" => TunnelState::Stopped,
                "failed" => TunnelState::Failed(text(&status["reason"])),
                "port-in-use" => TunnelState::PortInUse {
                    address: text(&status["address"]),
                    by: status["by"].as_str().map(str::to_string),
                },
                other => TunnelState::Unknown(other.to_string()),
            };
            Tunnel {
                name,
                label,
                state,
                warnings: warning_messages(report),
            }
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    pub alias: String,
    pub external: bool,
}

/// The hosts in the document's order, a duplicate alias shown once (the first,
/// as ssh uses the first match), and the document's warnings.
pub fn hosts(doc: &Value) -> (Vec<Host>, Vec<String>) {
    let mut rows: Vec<Host> = Vec::new();
    for host in list(&doc["hosts"]) {
        let alias = text(&host["alias"]);
        if alias.is_empty() || rows.iter().any(|h| h.alias == alias) {
            continue;
        }
        rows.push(Host {
            alias,
            external: host["ownership"] == "external",
        });
    }
    (rows, warning_messages(doc))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Container {
    pub id: String,
    pub backend: String,
    /// `<Backend>: <display_name> · <image> · <status>`.
    pub label: String,
}

fn capitalized(word: &str) -> String {
    let mut chars = word.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

/// The containers of a `containers --json` document, or its error's message and
/// next steps.
pub fn containers(doc: &Value) -> Result<Vec<Container>, (String, Vec<String>)> {
    if let Some(error) = error_of(doc) {
        return Err(error);
    }
    // Docker documents have no backend key.
    let backend = match text(&doc["backend"]) {
        b if b.is_empty() => "docker".to_string(),
        b => b,
    };
    Ok(list(&doc["containers"])
        .iter()
        .map(|c| {
            let id = text(&c["id"]);
            let name = [text(&c["display_name"]), text(&c["name"]), id.clone()]
                .into_iter()
                .find(|n| !n.is_empty())
                .unwrap_or_default();
            let parts: Vec<String> = [name, text(&c["image"]), text(&c["status"])]
                .into_iter()
                .filter(|p| !p.is_empty())
                .collect();
            let label = format!("{}: {}", capitalized(&backend), parts.join(" · "));
            Container {
                id,
                backend: backend.clone(),
                label,
            }
        })
        .collect())
}

fn on_argument(alias: Option<&str>) -> Vec<String> {
    alias
        .filter(|a| !a.is_empty())
        .map(|a| format!("--on={a}"))
        .into_iter()
        .collect()
}

/// `containers --json` for `alias` (this machine when `None`) over the enabled
/// `backends`; all three add no flag, and none is `None`: no flag would mean
/// every backend.
pub fn containers_args(alias: Option<&str>, backends: &[&str]) -> Option<Vec<String>> {
    if backends.is_empty() {
        return None;
    }
    let mut args = vec!["containers".to_string(), "--json".to_string()];
    args.extend(on_argument(alias));
    if backends.len() < BACKENDS.len() {
        args.extend(
            BACKENDS
                .iter()
                .filter(|name| backends.contains(name))
                .map(|name| format!("--backend={name}")),
        );
    }
    Some(args)
}

/// `--` keeps an alias, ID or name that starts with `-` from being read as a flag.
pub fn connect_args(alias: &str) -> Vec<String> {
    ["connect", "--new-terminal", "--", alias]
        .map(String::from)
        .to_vec()
}

pub fn shell_args(alias: Option<&str>, backend: &str, id: &str) -> Vec<String> {
    let backend = if backend.is_empty() {
        "docker"
    } else {
        backend
    };
    let mut args = vec!["shell".to_string(), "--new-terminal".to_string()];
    args.extend(on_argument(alias));
    args.extend([
        format!("--backend={backend}"),
        "--".to_string(),
        id.to_string(),
    ]);
    args
}

/// `tunnel start|stop --json`; `Action::None` has no command and gets `start`'s
/// harmless nothing: callers never ask for it.
pub fn tunnel_args(action: Action, name: &str) -> Vec<String> {
    let verb = if action == Action::Stop {
        "stop"
    } else {
        "start"
    };
    ["tunnel", verb, "--json", "--", name]
        .map(String::from)
        .to_vec()
}

/// The last non-empty line of human text, as one plain line.
fn last_line(output: &str) -> String {
    for line in output.lines().rev() {
        let mut plain = String::new();
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' && chars.peek() == Some(&'[') {
                chars.next();
                for n in chars.by_ref() {
                    if n.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else if !c.is_control() {
                plain.push(c);
            }
        }
        let plain = plain.trim();
        if !plain.is_empty() {
            return if plain.chars().count() > 300 {
                plain.chars().take(300).chain(['…']).collect()
            } else {
                plain.to_string()
            };
        }
    }
    String::new()
}

/// Why `connect` or `shell --new-terminal` failed, from its exit status and stderr.
pub fn launch_failure(exit: Option<i32>, stderr: &str) -> String {
    let line = last_line(stderr);
    if !line.is_empty() {
        return line;
    }
    let status = exit.map_or_else(|| "unknown".to_string(), |n| n.to_string());
    format!("Portico could not open a terminal (exit status {status})")
}

/// The version `portico --version` printed (`portico 0.7.0`).
pub fn version_of(stdout: &str) -> Option<String> {
    let line = last_line(stdout);
    let mut words = line.split_whitespace();
    match (words.next(), words.next(), words.next()) {
        (Some("portico"), Some(version), None) => Some(version.to_string()),
        _ => None,
    }
}

/// A menu title of a readable length: 60 characters at most, ending in `…` when cut.
pub fn title(text: &str) -> String {
    if text.chars().count() <= 60 {
        return text.to_string();
    }
    text.chars().take(59).chain(['…']).collect()
}
