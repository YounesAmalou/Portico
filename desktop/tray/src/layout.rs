//! What the tray menu shows, as plain data: the app's state, how Portico's
//! answers and the user's clicks change it, and the menu it describes. No GUI
//! types, so it is tested on every platform; `menu.rs` turns the description
//! into a native menu on macOS and Windows.

use crate::model::{self, Action, Container, Host, Parsed, Tabs, Tunnel, TunnelState};
use crate::portico::{Output, Request};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::io;

/// What clicking an item does.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Click {
    Tunnel(Action, String),
    Connect(String),
    /// Lists the containers of a host (`None`: this machine).
    ShowContainers(Option<String>),
    /// Target (`None`: this machine), backend, container ID.
    Shell(Option<String>, String, String),
    Refresh,
    Update,
    Quit,
}

/// One entry of the menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// A disabled line of text.
    Line(String),
    /// A clickable item; `check` is `Some` for an item with a checkmark column.
    Item {
        text: String,
        click: Click,
        check: Option<bool>,
    },
    Submenu(String, Vec<Entry>),
    Separator,
}

/// A part of the menu filled by one `portico` run.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Listing<T> {
    #[default]
    Loading,
    Loaded(T),
    /// The lines to show instead: a message and what to do next.
    Failed(Vec<String>),
}

/// A row that can show the error of its last action under it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Row {
    Tunnel(String),
    Host(String),
    /// Target (`None`: this machine) and container ID.
    Container(Option<String>, String),
}

/// Everything the menu is built from: the last answer to each request.
#[derive(Debug, Clone)]
pub struct State {
    /// This app's version, `dev` for a build without one.
    pub app_version: String,
    /// The `portico` the app runs, as shown in "not found".
    pub program: Option<String>,
    /// `portico` could not be run: the menu shows only that.
    pub missing: bool,
    /// What `portico --version` printed.
    pub portico_version: Option<String>,
    /// A document this app cannot read (an old Portico, another schema).
    pub problem: Option<String>,
    /// Why the last self-update failed.
    pub update_failure: Option<String>,
    pub tabs: Tabs,
    /// The hosts and the document's warnings.
    pub hosts: Listing<(Vec<Host>, Vec<String>)>,
    pub tunnels: Listing<Vec<Tunnel>>,
    /// Container listings by target (`None`: this machine), kept until Refresh.
    pub containers: HashMap<Option<String>, Listing<Vec<Container>>>,
    /// Tunnels being started or stopped.
    pub busy: HashMap<String, Action>,
    /// The error of a row's last action, until Refresh.
    pub row_errors: HashMap<Row, String>,
    /// Start and stop runs answered so far.
    actions_finished: u64,
    /// For each `tunnels` listing sent and not answered yet, `actions_finished`
    /// when it was sent: a listing that crossed a start or stop is stale.
    tunnel_listings: VecDeque<u64>,
}

const NOT_RUN: &str = "Could not run Portico";

fn failure(error: &io::Error) -> String {
    format!("{NOT_RUN}: {error}")
}

impl State {
    pub fn new(app_version: &str, program: Option<String>) -> State {
        State {
            app_version: app_version.to_string(),
            missing: program.is_none(),
            program,
            portico_version: None,
            problem: None,
            update_failure: None,
            tabs: model::tabs(&Value::Null),
            hosts: Listing::Loading,
            tunnels: Listing::Loading,
            containers: HashMap::new(),
            busy: HashMap::new(),
            row_errors: HashMap::new(),
            actions_finished: 0,
            tunnel_listings: VecDeque::new(),
        }
    }

    fn backends(&self) -> Vec<String> {
        model::enabled_backends(&self.tabs)
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    fn list_tunnels(&mut self) -> Request {
        self.tunnel_listings.push_back(self.actions_finished);
        Request::Tunnels
    }

    /// Forgets the requests in flight (their worker was replaced): their
    /// answers are then ignored.
    pub fn forget_pending(&mut self) {
        self.tunnel_listings.clear();
    }

    /// Start, and Refresh: everything is read again, and the errors and
    /// container listings of the last refresh are dropped.
    pub fn refresh(&mut self) -> Vec<Request> {
        self.problem = None;
        self.containers.clear();
        self.row_errors.clear();
        if self.missing {
            return Vec::new();
        }
        let tunnels = self.list_tunnels();
        vec![Request::Version, Request::Settings, Request::Hosts, tunnels]
    }

    /// The tray icon was clicked to open the menu: the tunnels are listed
    /// again, for the next time it opens.
    pub fn menu_opened(&mut self) -> Vec<Request> {
        if self.missing || self.problem.is_some() || !self.tabs.tunnels {
            return Vec::new();
        }
        vec![self.list_tunnels()]
    }

    /// What a click runs, if anything; Refresh, Update and Quit are the app's.
    pub fn click(&mut self, click: &Click) -> Option<Request> {
        if self.missing {
            return None;
        }
        match click {
            Click::Tunnel(Action::None, _) => None,
            Click::Tunnel(action, name) => {
                if self.busy.contains_key(name) {
                    return None;
                }
                self.busy.insert(name.clone(), *action);
                self.row_errors.remove(&Row::Tunnel(name.clone()));
                Some(Request::Tunnel(*action, name.clone()))
            }
            Click::Connect(alias) => {
                self.row_errors.remove(&Row::Host(alias.clone()));
                Some(Request::Connect(alias.clone()))
            }
            Click::ShowContainers(target) => {
                let backends = self.backends();
                if backends.is_empty() {
                    return None;
                }
                self.containers.insert(target.clone(), Listing::Loading);
                Some(Request::Containers(target.clone(), backends))
            }
            Click::Shell(target, backend, id) => {
                self.row_errors
                    .remove(&Row::Container(target.clone(), id.clone()));
                Some(Request::Shell(target.clone(), backend.clone(), id.clone()))
            }
            Click::Refresh | Click::Update | Click::Quit => None,
        }
    }

    /// Records what a run answered.
    pub fn apply(&mut self, request: Request, result: io::Result<Output>) {
        match request {
            Request::Version => match result {
                Ok(output) => self.portico_version = model::version_of(&output.stdout),
                Err(_) => self.missing = true,
            },
            // An older Portico has no `settings`: any failure leaves every tab on.
            Request::Settings => {
                let tabs = match result.as_ref().map(|o| model::parse(&o.stdout, o.exit)) {
                    Ok(Parsed::Doc(doc)) => model::tabs(&doc),
                    Ok(Parsed::Problem(_)) => model::tabs(&Value::Null),
                    Err(_) => {
                        self.missing = true;
                        return;
                    }
                };
                if model::enabled_backends(&tabs) != model::enabled_backends(&self.tabs) {
                    self.containers.clear();
                }
                if !tabs.tunnels {
                    self.tunnels = Listing::Loading;
                }
                self.tabs = tabs;
            }
            Request::Hosts => {
                if let Some(doc) = self.document(result) {
                    self.hosts = match model::error_of(&doc) {
                        Some(error) => Listing::Failed(error_lines(error)),
                        None => Listing::Loaded(model::hosts(&doc)),
                    };
                }
            }
            Request::Tunnels => {
                let sent = self.tunnel_listings.pop_front();
                if sent != Some(self.actions_finished) {
                    return;
                }
                if let Some(doc) = self.document(result) {
                    self.tunnels = match model::error_of(&doc) {
                        Some(error) => Listing::Failed(error_lines(error)),
                        None => Listing::Loaded(model::tunnels(&doc)),
                    };
                }
            }
            Request::Containers(target, backends) => {
                if backends != self.backends()
                    || self.containers.get(&target) != Some(&Listing::Loading)
                {
                    return;
                }
                let listing = match result.map(|o| model::parse(&o.stdout, o.exit)) {
                    Err(error) => Listing::Failed(vec![failure(&error)]),
                    Ok(Parsed::Problem(problem)) => Listing::Failed(vec![problem]),
                    Ok(Parsed::Doc(doc)) => match model::containers(&doc) {
                        Ok(rows) => Listing::Loaded(rows),
                        Err((message, next)) => {
                            Listing::Failed([message].into_iter().chain(next).take(2).collect())
                        }
                    },
                };
                self.containers.insert(target, listing);
            }
            Request::Tunnel(_, name) => {
                self.busy.remove(&name);
                self.actions_finished += 1;
                if let Some(error) = self.tunnel_outcome(&name, result) {
                    self.row_errors.insert(Row::Tunnel(name), error);
                }
            }
            Request::Connect(alias) => {
                if let Some(error) = launch_error(result) {
                    self.row_errors.insert(Row::Host(alias), error);
                }
            }
            Request::Shell(target, _, id) => {
                if let Some(error) = launch_error(result) {
                    self.row_errors.insert(Row::Container(target, id), error);
                }
            }
        }
    }

    /// A schema 1 document, or `None` with the problem recorded.
    fn document(&mut self, result: io::Result<Output>) -> Option<Value> {
        match result.map(|o| model::parse(&o.stdout, o.exit)) {
            Ok(Parsed::Doc(doc)) => Some(doc),
            Ok(Parsed::Problem(problem)) => {
                self.problem = Some(problem);
                None
            }
            Err(_) => {
                self.missing = true;
                None
            }
        }
    }

    /// Updates the tunnel's row from a start or stop; its error, if any.
    fn tunnel_outcome(&mut self, name: &str, result: io::Result<Output>) -> Option<String> {
        let doc = match result.map(|o| model::parse(&o.stdout, o.exit)) {
            Err(error) => return Some(failure(&error)),
            Ok(Parsed::Problem(problem)) => return Some(problem),
            Ok(Parsed::Doc(doc)) => doc,
        };
        let reported = || "Portico reported a failure.".to_string();
        let results = doc["results"].as_array().map_or(&[][..], Vec::as_slice);
        let Some(outcome) = results.iter().find(|r| r["name"] == name) else {
            return Some(model::error_of(&doc).map_or_else(reported, |(message, _)| message));
        };
        if outcome["tunnel"].is_object()
            && let Listing::Loaded(rows) = &mut self.tunnels
            && let Some(row) = model::tunnels(&json!({ "tunnels": [outcome["tunnel"]] })).pop()
        {
            match rows.iter_mut().find(|r| r.name == name) {
                Some(old) => *old = row,
                None => rows.push(row),
            }
        }
        (outcome["outcome"] == "failed")
            .then(|| model::error_of(outcome).map_or_else(reported, |(message, _)| message))
    }
}

fn error_lines((message, next): (String, Vec<String>)) -> Vec<String> {
    [message].into_iter().chain(next).collect()
}

fn launch_error(result: io::Result<Output>) -> Option<String> {
    match result {
        Ok(output) if output.exit == Some(0) => None,
        Ok(output) => Some(model::launch_failure(output.exit, &output.stderr)),
        Err(error) => Some(failure(&error)),
    }
}

/// Longest line of a wrapped message; `model::title` cuts longer words.
const WIDTH: usize = 60;
/// Lines under a row (its detail, warnings and errors) start with this.
const INDENT: &str = "    ";

/// `text` as disabled lines of at most `WIDTH` characters, broken between words.
fn wrapped(text: &str, indent: &str) -> Vec<Entry> {
    let room = WIDTH.saturating_sub(indent.chars().count()).max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let fits = line.chars().count() + 1 + word.chars().count() <= room;
        if !line.is_empty() && !fits {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
        .into_iter()
        .map(|l| Entry::Line(model::title(&format!("{indent}{l}"))))
        .collect()
}

fn line(text: &str) -> Entry {
    Entry::Line(model::title(text))
}

fn item(text: &str, click: Click) -> Entry {
    Entry::Item {
        text: model::title(text),
        click,
        check: None,
    }
}

fn state_text(state: &TunnelState) -> (String, Option<String>) {
    match state {
        TunnelState::Running => ("Running".into(), None),
        TunnelState::Stopped => ("Stopped".into(), None),
        TunnelState::Failed(reason) => (
            "Failed".into(),
            Some(reason.clone()).filter(|r| !r.is_empty()),
        ),
        TunnelState::PortInUse { address, by } => (
            "Port in use".into(),
            Some(match by {
                Some(tunnel) => format!("held by tunnel {tunnel}"),
                None => format!("another program listens on {address}"),
            }),
        ),
        TunnelState::Unknown(raw) if raw.is_empty() => ("Unknown".into(), None),
        TunnelState::Unknown(raw) => (raw.clone(), None),
    }
}

fn listing_lines<T>(listing: &Listing<T>) -> Option<Vec<Entry>> {
    match listing {
        Listing::Loading => Some(vec![line("Loading…")]),
        Listing::Failed(lines) => Some(lines.iter().flat_map(|l| wrapped(l, "")).collect()),
        Listing::Loaded(_) => None,
    }
}

impl State {
    fn row_error(&self, row: &Row, entries: &mut Vec<Entry>) {
        if let Some(error) = self.row_errors.get(row) {
            entries.extend(wrapped(error, INDENT));
        }
    }

    fn tunnel_entries(&self) -> Vec<Entry> {
        if let Some(lines) = listing_lines(&self.tunnels) {
            return lines;
        }
        let Listing::Loaded(tunnels) = &self.tunnels else {
            return Vec::new();
        };
        if tunnels.is_empty() {
            return vec![line("No tunnels")];
        }
        let mut entries = Vec::new();
        for tunnel in tunnels {
            let (word, detail) = state_text(&tunnel.state);
            let action = tunnel.action();
            match self.busy.get(&tunnel.name) {
                Some(busy) => {
                    let doing = if *busy == Action::Stop {
                        "Stopping…"
                    } else {
                        "Starting…"
                    };
                    entries.push(line(&format!("{}  {doing}", tunnel.label)));
                }
                None if action == Action::None => {
                    entries.push(line(&format!("{}  {word}", tunnel.label)));
                }
                None => entries.push(Entry::Item {
                    text: model::title(&format!("{}  {word}", tunnel.label)),
                    click: Click::Tunnel(action, tunnel.name.clone()),
                    check: Some(tunnel.state == TunnelState::Running),
                }),
            }
            if let Some(detail) = detail {
                entries.extend(wrapped(&detail, INDENT));
            }
            for warning in &tunnel.warnings {
                entries.extend(wrapped(warning, INDENT));
            }
            self.row_error(&Row::Tunnel(tunnel.name.clone()), &mut entries);
        }
        entries
    }

    /// "Show containers" and the last listing of `target`.
    fn container_entries(&self, target: &Option<String>) -> Vec<Entry> {
        let show = |text: &str| item(text, Click::ShowContainers(target.clone()));
        match self.containers.get(target) {
            None => vec![show("Show containers")],
            Some(Listing::Loading) => vec![line("Listing containers…")],
            Some(Listing::Failed(lines)) => std::iter::once(show("Refresh containers"))
                .chain(lines.iter().flat_map(|l| wrapped(l, "")))
                .collect(),
            Some(Listing::Loaded(rows)) if rows.is_empty() => {
                vec![show("Refresh containers"), line("No containers")]
            }
            Some(Listing::Loaded(rows)) => {
                let mut entries = vec![show("Refresh containers")];
                for container in rows {
                    entries.push(item(
                        &container.label,
                        Click::Shell(
                            target.clone(),
                            container.backend.clone(),
                            container.id.clone(),
                        ),
                    ));
                    let row = Row::Container(target.clone(), container.id.clone());
                    self.row_error(&row, &mut entries);
                }
                entries
            }
        }
    }

    fn host_entries(&self) -> Vec<Entry> {
        if let Some(lines) = listing_lines(&self.hosts) {
            return lines;
        }
        let Listing::Loaded((hosts, warnings)) = &self.hosts else {
            return Vec::new();
        };
        let containers = !model::enabled_backends(&self.tabs).is_empty();
        let mut entries = Vec::new();
        if hosts.is_empty() {
            entries.push(line("No hosts"));
        }
        for host in hosts {
            let mut inner = vec![item("Open shell", Click::Connect(host.alias.clone()))];
            self.row_error(&Row::Host(host.alias.clone()), &mut inner);
            if containers {
                inner.push(Entry::Separator);
                inner.extend(self.container_entries(&Some(host.alias.clone())));
            }
            let label = if host.external {
                format!("{} (external)", host.alias)
            } else {
                host.alias.clone()
            };
            entries.push(Entry::Submenu(model::title(&label), inner));
        }
        for warning in warnings {
            entries.extend(wrapped(warning, INDENT));
        }
        entries
    }

    fn version_entries(&self) -> Vec<Entry> {
        let mut entries = Vec::new();
        if self.app_version == "dev" {
            entries.push(line("Development build"));
        } else if let Some(portico) = &self.portico_version
            && *portico != self.app_version
        {
            entries.push(item(
                &format!(
                    "Portico is {portico}, this app is {}: Update",
                    self.app_version
                ),
                Click::Update,
            ));
        }
        if let Some(failure) = &self.update_failure {
            entries.extend(wrapped(&format!("Update failed: {failure}"), ""));
        }
        entries
    }
}

/// The menu for `state`, top to bottom.
pub fn describe(state: &State) -> Vec<Entry> {
    let mut entries = Vec::new();
    if state.missing {
        let at = state.program.as_deref().map_or_else(
            || "Portico was not found".to_string(),
            |p| format!("Portico was not found at {p}"),
        );
        entries.extend(wrapped(&at, ""));
        entries.extend(wrapped(
            "Run portico desktop install again, then Refresh.",
            "",
        ));
        entries.push(Entry::Separator);
    } else if let Some(problem) = &state.problem {
        entries.extend(wrapped(problem, ""));
        entries.push(Entry::Separator);
        entries.extend(state.version_entries());
    } else {
        if state.tabs.tunnels {
            entries.push(line("Tunnels"));
            entries.extend(state.tunnel_entries());
            entries.push(Entry::Separator);
        }
        entries.push(line("Hosts"));
        entries.extend(state.host_entries());
        if !model::enabled_backends(&state.tabs).is_empty() {
            entries.push(Entry::Submenu(
                "This machine".into(),
                state.container_entries(&None),
            ));
        }
        entries.push(Entry::Separator);
        entries.extend(state.version_entries());
    }
    entries.push(item("Refresh", Click::Refresh));
    entries.push(item("Quit", Click::Quit));
    entries
}
