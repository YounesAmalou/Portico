//! The Tunnels tab: the saved tunnels with their status, their add and edit form, and the
//! worker that starts a tunnel while the interface keeps handling keys.

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};

use super::{
    TextInput,
    dropdown::{SUGGESTION_ROWS, Suggestions, suggestion_lines},
};
use crate::{
    app::{
        App, AppError, ForwardTarget, Ownership, SaveOutcome, StartOutcome, TunnelChanges,
        TunnelRefusal,
    },
    backend::Reach,
    domain::fuzzy,
    domain::suggest::{self, Candidate},
    domain::tunnel::{
        Forward, Kind, Tunnel, TunnelError, TunnelField, TunnelReport, TunnelStatus, collisions,
        printable, saved_name_for,
    },
    picker::HIT,
};

/// How often the statuses are re-read while the screen is shown.
pub(super) const REFRESH_EVERY: Duration = Duration::from_secs(2);

/// The form's rows, in order.
const FIELDS: [(TunnelField, &str, &str, &str); 7] = [
    (
        TunnelField::Name,
        "Name",
        "db",
        "The tunnel's name. Letters, digits, '.', '_' and '-'; it must start with a letter or digit.",
    ),
    (
        TunnelField::Host,
        "Host",
        "prod-db",
        "The host the tunnel goes through, as you would type it after ssh. Known aliases are suggested as you type; Ctrl+Space lists them.",
    ),
    (
        TunnelField::Kind,
        "Kind",
        "",
        "Local listens here and connects from the host; remote listens on the host; dynamic is a SOCKS proxy here. Space or ←→ changes it.",
    ),
    (
        TunnelField::BindAddress,
        "Bind address",
        "localhost",
        "Where the forward listens. Leave empty for loopback; * listens on every address.",
    ),
    (
        TunnelField::BindPort,
        "Bind port",
        "5432",
        "The port the forward listens on, from 1 through 65535.",
    ),
    (
        TunnelField::TargetHost,
        "Target host",
        "localhost",
        "Where connections go, as the other side resolves it (localhost there means the server for a local forward).",
    ),
    (
        TunnelField::TargetPort,
        "Target port",
        "5432",
        "The port connections go to, from 1 through 65535.",
    ),
];

const KIND: usize = 2;
const HOST: usize = 1;

#[derive(Clone, Debug, Default)]
struct TunnelForm {
    /// The saved name when editing.
    editing: Option<String>,
    values: [TextInput; 7],
    kind: Option<Kind>,
    active: usize,
    error: Option<(TunnelField, String)>,
    /// The dropdown under Host, while it is open.
    suggestions: Option<Suggestions>,
}

impl TunnelForm {
    fn kind(&self) -> Kind {
        self.kind.unwrap_or(Kind::Local)
    }

    fn for_tunnel(tunnel: &Tunnel) -> Self {
        let forward = &tunnel.forward;
        let (target_host, target_port) = forward
            .destination
            .as_ref()
            .map_or((String::new(), String::new()), |destination| {
                (destination.host.clone(), destination.port.to_string())
            });
        Self {
            editing: Some(tunnel.name.clone()),
            values: [
                TextInput::new(&tunnel.name),
                TextInput::new(&tunnel.host),
                TextInput::new(""),
                TextInput::new(forward.bind_address.as_deref().unwrap_or_default()),
                TextInput::new(&forward.bind_port.to_string()),
                TextInput::new(&target_host),
                TextInput::new(&target_port),
            ],
            kind: Some(forward.kind),
            active: 0,
            error: None,
            suggestions: None,
        }
    }

    /// The rows shown: a dynamic forward has no target.
    fn rows(&self) -> Vec<usize> {
        (0..FIELDS.len())
            .filter(|&row| self.kind() != Kind::Dynamic || row < 5)
            .collect()
    }

    fn step(&mut self, by: isize) {
        let rows = self.rows();
        let at = rows.iter().position(|&row| row == self.active).unwrap_or(0);
        let next = (at as isize + by).clamp(0, rows.len() as isize - 1) as usize;
        self.active = rows[next];
    }

    fn value(&self, row: usize) -> &str {
        self.values[row].as_str()
    }

    fn forward(&self) -> Result<Forward, TunnelError> {
        Forward::from_fields(
            self.kind(),
            self.value(3),
            self.value(4),
            self.value(5),
            self.value(6),
        )
    }

    fn build(&self) -> Result<Tunnel, TunnelError> {
        Tunnel::new(self.value(0), self.value(1), self.forward()?)
    }

    fn cycle_kind(&mut self, by: isize) {
        let at = Kind::ALL
            .iter()
            .position(|kind| *kind == self.kind())
            .unwrap_or(0) as isize;
        self.kind = Some(Kind::ALL[(at + by).rem_euclid(3) as usize]);
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Screen {
    #[default]
    Browse,
    Filter,
    Form,
    ConfirmRemove,
    /// The name prompt of `w`, which saves a temporary forward.
    SaveName,
}

/// What the screen asks the event loop to do.
#[derive(Clone, Debug, Eq, PartialEq)]
#[expect(
    clippy::large_enum_variant,
    reason = "one is made per key and handled at once; boxing the forward's target buys nothing"
)]
pub(super) enum TunnelAction {
    None,
    Quit,
    Refresh,
    Start(String),
    CancelStart,
    Stop(String),
    Add(Tunnel),
    Edit {
        name: String,
        changes: TunnelChanges,
    },
    Remove(String),
    /// Saves temporary forward `temporary` as tunnel `name`.
    Save {
        temporary: String,
        name: String,
    },
    /// Puts a tunnel's local address on the clipboard.
    Copy(String),
    /// Opens a URL in the browser.
    Open(String),
    /// Starts a temporary forward from the forward dialog of the hosts or a container tab.
    Forward {
        target: ForwardTarget,
        /// Where the container is reached, as its suggestions found it.
        reach: Option<Reach>,
        remote: u16,
        local: Option<u16>,
    },
}

#[derive(Clone, Debug, Default)]
pub(super) struct TunnelsScreen {
    reports: Vec<TunnelReport>,
    /// Each report's command line and log tail, in the same order.
    extras: Vec<(String, Vec<String>)>,
    load_error: Option<String>,
    /// The known host aliases the form's Host field suggests.
    hosts: Vec<Candidate>,
    /// The home whose paths the suggestions show as `~`.
    home: PathBuf,
    visible: Vec<usize>,
    selected: Option<usize>,
    filter: TextInput,
    filter_before_edit: String,
    screen: Screen,
    form: TunnelForm,
    /// The tunnel being started, while its ssh connects.
    starting: Option<String>,
    status: Option<String>,
    show_log: bool,
    refreshed: Option<Instant>,
    /// The tunnel to select once the list is next rebuilt, after it was saved.
    pending_select: Option<String>,
    /// The name typed in the save prompt.
    save_name: TextInput,
    /// Why the name typed in the save prompt was refused.
    save_error: Option<String>,
}

/// A local forward's address on this machine, as it is copied and opened: none for a remote or
/// dynamic forward. Without a bind address ssh listens on loopback, reached as `localhost`.
fn local_address(report: &TunnelReport) -> Option<String> {
    let forward = &report.tunnel.forward;
    if forward.kind != Kind::Local || lost_forward(report) {
        return None;
    }
    Some(match forward.bind_address.as_deref() {
        None | Some("" | "*") => format!("localhost:{}", forward.bind_port),
        Some(_) => forward.listen(),
    })
}

/// Whether `report` is a temporary forward whose record was rebuilt without its host and forward.
fn lost_forward(report: &TunnelReport) -> bool {
    report.temporary && report.tunnel.host.is_empty()
}

/// Said when `s` or `w` meets a temporary forward whose record lost its forward.
const LOST: &str = "its record lost the forward; x removes it";

/// How many log lines the details show unless unfolded.
const LOG_LINES: usize = 3;

impl TunnelsScreen {
    /// Re-reads the tunnels, their statuses and the host aliases, keeping the selection.
    pub(super) fn reload(&mut self, app: &App) {
        // A tunnel waiting to be selected wins over the selection it replaces.
        let selected =
            (self.pending_select.take()).or_else(|| self.selected_name().map(str::to_owned));
        match app.tunnels() {
            Ok(reports) => {
                self.extras = reports
                    .iter()
                    .map(|report| {
                        let failed = matches!(report.status, TunnelStatus::Failed { .. });
                        (
                            app.tunnel_command(&report.tunnel).join(" "),
                            if failed {
                                app.tunnel_log(&report.tunnel.name, 20)
                            } else {
                                Vec::new()
                            },
                        )
                    })
                    .collect();
                self.reports = reports;
                self.load_error = None;
            }
            Err(error) => {
                self.reports.clear();
                self.extras.clear();
                self.load_error = Some(error.to_string());
            }
        }
        if let Ok(inventory) = app.hosts() {
            self.hosts = inventory
                .hosts
                .into_iter()
                .map(|host| Candidate {
                    alias: host.alias,
                    origin: match host.ownership {
                        Ownership::Managed => suggest::Origin::Managed,
                        Ownership::External => suggest::Origin::External(host.source),
                    },
                })
                .collect();
        }
        self.home = app.home().to_owned();
        self.refilter();
        if let Some(name) = selected {
            self.select(&name);
        }
        self.refreshed = Some(Instant::now());
    }

    /// Whether the statuses are older than the refresh period.
    pub(super) fn is_stale(&self) -> bool {
        self.refreshed
            .is_none_or(|refreshed| refreshed.elapsed() >= REFRESH_EVERY)
    }

    pub(super) fn set_starting(&mut self, name: Option<String>) {
        self.starting = name;
    }

    pub(super) fn set_status(&mut self, status: impl Into<String>) {
        self.status = Some(status.into());
    }

    /// Shows a start's result; the caller reloads afterwards.
    pub(super) fn finish_start(&mut self, name: &str, result: Result<StartOutcome, String>) {
        self.starting = None;
        self.status = Some(match result {
            Ok(StartOutcome::Started { pid, .. }) => format!("Started {name} (pid {pid})"),
            Ok(StartOutcome::AlreadyRunning { pid }) => {
                format!("{name} is already running (pid {pid})")
            }
            Ok(StartOutcome::Failed {
                reason,
                explanation,
            }) => {
                let next = explanation
                    .and_then(|explanation| explanation.next.first().cloned())
                    .map_or(String::new(), |next| format!("; {next}"));
                format!("{name} failed: {reason}{next}")
            }
            Err(error) => format!("{name}: {error}"),
        });
    }

    /// Applies a save's or removal's outcome: success closes the form, a refusal stays in it.
    pub(super) fn finish_change(&mut self, result: Result<(), AppError>, done: String) {
        match result {
            Ok(()) => {
                if self.screen == Screen::Form {
                    self.pending_select = Some(self.form.value(0).to_owned());
                }
                self.screen = Screen::Browse;
                self.status = Some(done);
            }
            Err(error) if self.screen == Screen::Form => {
                let field = match &error {
                    AppError::InvalidTunnel(error) => error.field(),
                    AppError::Tunnel(TunnelRefusal::Exists(_)) => TunnelField::Name,
                    AppError::Tunnel(
                        TunnelRefusal::UnknownHost { .. } | TunnelRefusal::Rejected { .. },
                    ) => TunnelField::Host,
                    _ => TunnelField::Name,
                };
                self.form.error = Some((field, error.to_string()));
            }
            Err(error) => {
                self.screen = Screen::Browse;
                self.status = Some(error.to_string());
            }
        }
    }

    /// Applies a save of a temporary forward: success selects the saved tunnel and says whether it
    /// was restarted; a refusal stays in the prompt for another name.
    pub(super) fn finish_save(&mut self, result: Result<SaveOutcome, AppError>) {
        match result {
            Ok(outcome) => {
                let name = outcome.tunnel.name;
                let mut status = format!("saved {}", printable(&name));
                match outcome.restarted {
                    Some(StartOutcome::Started { .. } | StartOutcome::AlreadyRunning { .. }) => {
                        status.push_str("; restarted it (connections through it were interrupted)");
                    }
                    Some(StartOutcome::Failed { reason, .. }) => {
                        status.push_str(&format!("; restarting it failed: {}", printable(&reason)));
                    }
                    None if outcome.cancelled => status.push_str("; restart cancelled"),
                    None => {}
                }
                if let Some(note) = outcome.note {
                    status.push_str(&format!("; {}", printable(&note)));
                }
                self.screen = Screen::Browse;
                self.save_error = None;
                self.status = Some(status);
                self.select_new(&name);
            }
            // Asks for another name, unless something else took the screen meanwhile.
            Err(error) if matches!(self.screen, Screen::Browse | Screen::SaveName) => {
                self.screen = Screen::SaveName;
                self.save_error = Some(printable(&error.to_string()));
            }
            Err(error) => self.status = Some(printable(&error.to_string())),
        }
    }

    fn refilter(&mut self) {
        let filter = self.filter.as_str();
        self.visible = self
            .reports
            .iter()
            .enumerate()
            .filter(|(_, report)| {
                let forward = report.tunnel.forward.describe();
                fuzzy::match_fields(
                    filter,
                    &[
                        &report.tunnel.name,
                        &report.tunnel.host,
                        &report.reaches,
                        &forward,
                    ],
                )
                .is_some()
            })
            .map(|(index, _)| index)
            .collect();
        if !self
            .selected
            .is_some_and(|selected| self.visible.contains(&selected))
        {
            self.selected = self.visible.first().copied();
        }
        if let Some(name) = self.pending_select.take() {
            self.select(&name);
        }
    }

    /// Selects tunnel `name`, now if it is listed and otherwise once the list is next rebuilt.
    pub(super) fn select_new(&mut self, name: &str) {
        self.select(name);
        self.pending_select = Some(name.to_owned());
    }

    fn select(&mut self, name: &str) {
        if let Some(index) = self
            .reports
            .iter()
            .position(|report| report.tunnel.name.eq_ignore_ascii_case(name))
            .filter(|index| self.visible.contains(index))
        {
            self.selected = Some(index);
        }
    }

    fn selected(&self) -> Option<&TunnelReport> {
        self.selected.map(|index| &self.reports[index])
    }

    pub(super) fn selected_name(&self) -> Option<&str> {
        self.selected().map(|report| report.tunnel.name.as_str())
    }

    fn move_selection(&mut self, by: isize) {
        let Some(at) = self
            .selected
            .and_then(|selected| self.visible.iter().position(|&index| index == selected))
        else {
            self.selected = self.visible.first().copied();
            return;
        };
        let next = (at as isize + by).clamp(0, self.visible.len() as isize - 1) as usize;
        self.selected = Some(self.visible[next]);
        self.show_log = false;
    }

    /// Whether the table has the keys, rather than the filter, a form or a confirmation.
    pub(super) fn is_browsing(&self) -> bool {
        self.screen == Screen::Browse
    }

    pub(super) fn any_running(&self) -> bool {
        self.reports.iter().any(|report| report.status.is_running())
    }

    pub(super) fn handle_key(&mut self, key: KeyEvent) -> TunnelAction {
        self.status = None;
        if key.code == KeyCode::Esc && self.starting.is_some() && self.screen == Screen::Browse {
            self.status = Some("Start cancelled".into());
            return TunnelAction::CancelStart;
        }
        match self.screen {
            Screen::Browse => self.handle_browse_key(key),
            Screen::Filter => {
                match key.code {
                    KeyCode::Enter => self.screen = Screen::Browse,
                    KeyCode::Esc => {
                        self.filter = TextInput::new(&self.filter_before_edit.clone());
                        self.screen = Screen::Browse;
                    }
                    _ => {
                        self.filter.handle_key(key);
                    }
                }
                self.refilter();
                TunnelAction::None
            }
            Screen::Form => self.handle_form_key(key),
            Screen::ConfirmRemove => match key.code {
                KeyCode::Enter | KeyCode::Char('y') => {
                    self.screen = Screen::Browse;
                    match self.selected() {
                        // Stopping a temporary forward removes it.
                        Some(report) if report.temporary => {
                            TunnelAction::Stop(report.tunnel.name.clone())
                        }
                        Some(report) => TunnelAction::Remove(report.tunnel.name.clone()),
                        None => TunnelAction::None,
                    }
                }
                KeyCode::Esc | KeyCode::Char('n') => {
                    self.screen = Screen::Browse;
                    TunnelAction::None
                }
                _ => TunnelAction::None,
            },
            Screen::SaveName => match key.code {
                // The prompt closes while the save runs, so Esc cancels its restart; a refusal
                // opens it again.
                KeyCode::Enter => match self.selected() {
                    Some(report) => {
                        let action = TunnelAction::Save {
                            temporary: report.tunnel.name.clone(),
                            name: self.save_name.as_str().to_owned(),
                        };
                        self.screen = Screen::Browse;
                        action
                    }
                    None => {
                        self.screen = Screen::Browse;
                        TunnelAction::None
                    }
                },
                KeyCode::Esc => {
                    self.screen = Screen::Browse;
                    self.save_error = None;
                    TunnelAction::None
                }
                _ => {
                    self.save_name.handle_key(key);
                    TunnelAction::None
                }
            },
        }
    }

    /// The question of the confirmation prompt.
    fn confirmation(&self) -> String {
        match self.selected() {
            Some(report) if report.temporary => {
                format!("Stop and remove {}? (y/n)", printable(&report.tunnel.name))
            }
            report => format!(
                "Remove {}?",
                report.map_or("the selected tunnel".into(), |report| printable(
                    &report.tunnel.name
                ))
            ),
        }
    }

    pub(super) fn paste(&mut self, text: &str) {
        match self.screen {
            Screen::Filter => {
                self.filter.paste(text);
                self.refilter();
            }
            Screen::SaveName => self.save_name.paste(text),
            Screen::Form if self.form.active != KIND => {
                let field = FIELDS[self.form.active].0;
                let text: String = text.chars().filter(|c| field.permits(*c)).collect();
                let before = self.form.value(self.form.active).to_owned();
                self.form.values[self.form.active].paste(&text);
                if self.form.value(self.form.active) != before {
                    self.suggest(false);
                }
            }
            _ => {}
        }
    }

    fn handle_browse_key(&mut self, key: KeyEvent) -> TunnelAction {
        let running = self
            .selected()
            .is_some_and(|report| report.status.is_running());
        let temporary = self.selected().is_some_and(|report| report.temporary);
        let lost = self.selected().is_some_and(lost_forward);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Char('/') => {
                self.filter_before_edit = self.filter.as_str().into();
                self.screen = Screen::Filter;
            }
            KeyCode::Esc if !self.filter.is_empty() => {
                self.filter = TextInput::new("");
                self.refilter();
            }
            KeyCode::Char('q') => return TunnelAction::Quit,
            KeyCode::Char('r') | KeyCode::F(5) => return TunnelAction::Refresh,
            KeyCode::Char('o') => self.show_log = !self.show_log,
            KeyCode::Char('a') => {
                self.form = TunnelForm::default();
                self.screen = Screen::Form;
            }
            // A temporary forward is not edited, and a saved tunnel is already saved.
            KeyCode::Char('e') if temporary => {}
            KeyCode::Char('w') if !temporary => {}
            KeyCode::Char('w') if lost => {
                self.status = Some(format!("cannot save it: {LOST}"));
            }
            KeyCode::Char('w') => {
                let name = self.selected_name().map(saved_name_for).unwrap_or_default();
                self.save_name = TextInput::new(&name);
                self.save_error = None;
                self.screen = Screen::SaveName;
            }
            KeyCode::Char('y') | KeyCode::Char('b') => {
                let copy = key.code == KeyCode::Char('y');
                return match self.selected().and_then(local_address) {
                    Some(address) if copy => TunnelAction::Copy(address),
                    Some(address) => TunnelAction::Open(format!("http://{address}")),
                    None => {
                        let what = if copy { "copy" } else { "open" };
                        self.status = Some(format!("no local address to {what}"));
                        TunnelAction::None
                    }
                };
            }
            // Removing a temporary forward stops it.
            KeyCode::Char('x') if temporary => self.screen = Screen::ConfirmRemove,
            KeyCode::Char('e') | KeyCode::Char('x') if running => {
                self.status = Some(format!(
                    "{} is running; stop it first (s)",
                    printable(self.selected_name().unwrap_or_default())
                ));
            }
            KeyCode::Char('e') => {
                if let Some(report) = self.selected() {
                    self.form = TunnelForm::for_tunnel(&report.tunnel);
                    self.screen = Screen::Form;
                }
            }
            KeyCode::Char('x') => {
                if self.selected().is_some() {
                    self.screen = Screen::ConfirmRemove;
                }
            }
            KeyCode::Char('s') => {
                let Some(name) = self.selected_name().map(str::to_owned) else {
                    return TunnelAction::None;
                };
                // Stopping a temporary forward removes it: ask, as x does.
                if running && temporary {
                    self.screen = Screen::ConfirmRemove;
                    return TunnelAction::None;
                }
                if running {
                    return TunnelAction::Stop(name);
                }
                if lost {
                    self.status = Some(format!("cannot start it again: {LOST}"));
                    return TunnelAction::None;
                }
                if let Some(starting) = &self.starting {
                    self.status = Some(format!("{starting} is still starting (Esc to cancel)"));
                    return TunnelAction::None;
                }
                return TunnelAction::Start(name);
            }
            _ => {}
        }
        TunnelAction::None
    }

    fn handle_form_key(&mut self, key: KeyEvent) -> TunnelAction {
        if key.code == KeyCode::Char(' ') && key.modifiers == KeyModifiers::CONTROL {
            if self.form.active == HOST {
                self.suggest(true);
            }
            return TunnelAction::None;
        }
        if let Some(list) = &mut self.form.suggestions {
            match key.code {
                KeyCode::Up => {
                    list.select(list.selected.saturating_sub(1));
                    return TunnelAction::None;
                }
                KeyCode::Down => {
                    list.select(list.selected + 1);
                    return TunnelAction::None;
                }
                KeyCode::Enter if key.modifiers.is_empty() => {
                    self.pick_suggestion();
                    return TunnelAction::None;
                }
                KeyCode::Tab => {
                    self.pick_suggestion();
                    return TunnelAction::None;
                }
                KeyCode::Esc => {
                    self.form.suggestions = None;
                    return TunnelAction::None;
                }
                _ => {}
            }
        }
        let form = &mut self.form;
        match key.code {
            KeyCode::Esc => self.screen = Screen::Browse,
            KeyCode::Up | KeyCode::BackTab => form.step(-1),
            KeyCode::Down | KeyCode::Tab => form.step(1),
            KeyCode::Left if form.active == KIND => form.cycle_kind(-1),
            KeyCode::Right | KeyCode::Char(' ') if form.active == KIND => form.cycle_kind(1),
            KeyCode::Enter => {
                form.error = None;
                return match form.build() {
                    Ok(tunnel) => match &form.editing {
                        Some(name) => TunnelAction::Edit {
                            name: name.clone(),
                            changes: TunnelChanges {
                                name: Some(tunnel.name),
                                host: Some(tunnel.host),
                                forward: Some(tunnel.forward),
                            },
                        },
                        None => TunnelAction::Add(tunnel),
                    },
                    Err(error) => {
                        form.error = Some((error.field(), error.to_string()));
                        if let Some(row) = FIELDS.iter().position(|row| row.0 == error.field()) {
                            form.active = row;
                        }
                        TunnelAction::None
                    }
                };
            }
            _ if form.active != KIND => {
                let field = FIELDS[form.active].0;
                let input = &mut form.values[form.active];
                let before = input.clone();
                input.handle_key(key);
                // Typing never puts a character in a field that the field refuses.
                if !input.as_str().chars().all(|c| field.permits(c)) {
                    *input = before;
                } else if input.as_str() != before.as_str() {
                    self.suggest(false);
                }
            }
            _ => {}
        }
        if self.form.active != HOST || self.screen != Screen::Form {
            self.form.suggestions = None;
        }
        TunnelAction::None
    }

    /// Opens, recomputes or closes the Host dropdown for what is typed. `on_demand` (Ctrl+Space)
    /// opens it even for an empty field or an exact match; afterwards it stays open for an empty
    /// field for as long as it stays open, but an exact match still closes it.
    fn suggest(&mut self, on_demand: bool) {
        let form = &mut self.form;
        let previous = form.suggestions.take();
        let forced = on_demand || previous.as_ref().is_some_and(|list| list.forced);
        if form.active != HOST {
            return;
        }
        let query = form.value(HOST);
        let items = suggest::rank(query, &self.hosts, &[]);
        let exact = items
            .iter()
            .any(|item| item.alias.eq_ignore_ascii_case(query));
        if items.is_empty() || (!forced && query.is_empty()) || (!on_demand && exact) {
            return;
        }
        form.suggestions = Some(Suggestions::reopened(previous, items, forced, HOST));
    }

    /// Fills Host with the selected suggestion and closes the dropdown.
    fn pick_suggestion(&mut self) {
        if let Some(list) = self.form.suggestions.take() {
            self.form.values[HOST] = TextInput::new(&list.items[list.selected].alias);
            self.form.error = None;
        }
    }

    pub(super) fn footer(&self) -> String {
        match self.screen {
            Screen::Browse => {
                let toggle = match self.selected() {
                    Some(report) if report.status.is_running() => "s stop  ",
                    Some(_) => "s start  ",
                    None => "",
                };
                let log = if self.selected_log().is_empty() {
                    ""
                } else {
                    "o log  "
                };
                let edit = match self.selected() {
                    Some(report) if report.temporary => "w save  ",
                    _ => "e edit  ",
                };
                let address = if self.selected().and_then(local_address).is_some() {
                    "y copy  b open  "
                } else {
                    ""
                };
                format!(
                    "↑↓ move  / filter  {toggle}a add  {edit}x remove  {address}r refresh  {log}Tab/1-5 tabs  , settings  q quit"
                )
            }
            Screen::Filter => "type to filter  ←→ move  Enter apply  Esc cancel".into(),
            Screen::Form if self.form.suggestions.is_some() => {
                "↑↓ choose  Enter pick  Esc close".into()
            }
            Screen::Form => {
                let special = match self.form.active {
                    KIND => "  Space/←→ change kind",
                    HOST => "  Ctrl+Space suggest",
                    _ => "",
                };
                format!("↑↓ field{special}  Enter save  Esc cancel")
            }
            Screen::ConfirmRemove if self.selected().is_some_and(|report| report.temporary) => {
                "y stop and remove  n cancel".into()
            }
            Screen::ConfirmRemove => "Enter remove  Esc cancel".into(),
            Screen::SaveName => "type a name  Enter save  Esc cancel".into(),
        }
    }

    fn selected_log(&self) -> &[String] {
        self.selected
            .and_then(|index| self.extras.get(index))
            .map_or(&[], |(_, log)| log.as_slice())
    }

    /// The status line: the last message, otherwise the start in flight.
    pub(super) fn status_line(&self) -> Option<String> {
        self.status.clone().or_else(|| {
            self.starting
                .as_ref()
                .map(|name| format!("Starting {name}\u{2026} (Esc to cancel)"))
        })
    }

    /// Draws the table and the details or form into `area`; the caller draws the status line and
    /// the footer.
    pub(super) fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);

        let mut title = vec![Span::raw("Tunnels /")];
        title.extend(self.filter.spans(self.screen == Screen::Filter));
        let items = self.table_lines();
        let mut list_state = ListState::default().with_selected(
            self.selected
                .and_then(|selected| self.visible.iter().position(|&index| index == selected)),
        );
        let mut block = Block::default()
            .borders(Borders::ALL)
            .title(Line::from(title));
        if self.any_running() {
            block = block.title_bottom(Line::styled(
                " running tunnels keep running after q ",
                Style::default().add_modifier(Modifier::DIM),
            ));
        }
        frame.render_stateful_widget(
            List::new(items)
                .block(block)
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
            columns[0],
            &mut list_state,
        );
        let (right_title, lines) = match self.screen {
            Screen::Form => (
                if self.form.editing.is_some() {
                    "Edit tunnel"
                } else {
                    "Add tunnel"
                },
                self.form_lines(
                    usize::from(columns[1].width.saturating_sub(2)),
                    usize::from(columns[1].height.saturating_sub(2)),
                ),
            ),
            Screen::ConfirmRemove => (
                if self.selected().is_some_and(|report| report.temporary) {
                    "Stop forward"
                } else {
                    "Remove tunnel"
                },
                vec![Line::raw(self.confirmation())],
            ),
            Screen::SaveName => ("Save forward", self.save_lines()),
            _ => ("Details", self.detail_lines()),
        };
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(Block::default().borders(Borders::ALL).title(right_title)),
            columns[1],
        );
    }

    fn table_lines(&self) -> Vec<ListItem<'static>> {
        if let Some(error) = &self.load_error {
            return vec![ListItem::new(Line::styled(
                error.clone(),
                Style::default().fg(Color::Red),
            ))];
        }
        if self.reports.is_empty() {
            return vec![ListItem::new(Line::raw("No tunnels. Press a to add one."))];
        }
        self.row_lines().into_iter().map(ListItem::new).collect()
    }

    /// The table's rows, one line per visible tunnel.
    fn row_lines(&self) -> Vec<Line<'static>> {
        let rows = self.rows();
        let marker_width = |marker: Option<&str>| marker.map_or(0, |marker| marker.len() + 1);
        let width = |column: usize| {
            rows.iter()
                .map(|(marker, row)| {
                    let extra = if column == 0 {
                        marker_width(*marker)
                    } else {
                        0
                    };
                    row[column].chars().count() + extra
                })
                .max()
                .unwrap_or(0)
        };
        let widths = [width(0), width(1), width(2)];
        let dim = Style::default().add_modifier(Modifier::DIM);
        self.visible
            .iter()
            .zip(rows)
            .map(|(&index, (marker, row))| {
                let report = &self.reports[index];
                let positions = fuzzy::match_fields(self.filter.as_str(), &[&row[0]])
                    .and_then(|found| found.positions.into_iter().next())
                    .unwrap_or_default();
                let mut spans = fuzzy::highlight(&row[0], &positions, Style::default(), HIT);
                let name_width = row[0].chars().count() + marker_width(marker);
                if let Some(marker) = marker {
                    spans.push(Span::raw(" "));
                    spans.push(Span::styled(marker, dim));
                }
                spans.push(Span::raw(format!(
                    "{}  {:w1$}  {:w2$}  ",
                    " ".repeat(widths[0] - name_width),
                    row[1],
                    row[2],
                    w1 = widths[1],
                    w2 = widths[2],
                )));
                let starting = self
                    .starting
                    .as_ref()
                    .is_some_and(|name| name.eq_ignore_ascii_case(&report.tunnel.name));
                spans.push(if starting {
                    Span::styled("starting\u{2026}", Style::default().fg(Color::Cyan))
                } else {
                    status_span(&report.status)
                });
                Line::from(spans)
            })
            .collect()
    }

    #[cfg(test)]
    fn row_texts(&self) -> Vec<String> {
        self.row_lines().iter().map(Line::to_string).collect()
    }

    /// The visible rows' columns: the name's marker, then the name, what it reaches and the local
    /// address (a remote or dynamic forward's description), all printable.
    fn rows(&self) -> Vec<(Option<&'static str>, [String; 3])> {
        self.visible
            .iter()
            .map(|&index| {
                let report = &self.reports[index];
                let local = if lost_forward(report) {
                    "unknown".to_owned()
                } else {
                    local_address(report).unwrap_or_else(|| report.tunnel.forward.describe())
                };
                (
                    report.temporary.then_some("temporary"),
                    [
                        printable(&report.tunnel.name),
                        printable(&report.reaches),
                        printable(&local),
                    ],
                )
            })
            .collect()
    }

    /// The save prompt: the name, then why it was refused.
    fn save_lines(&self) -> Vec<Line<'static>> {
        let mut name = vec![Span::raw("> Name: ")];
        name.extend(self.save_name.spans(true));
        let mut lines = vec![
            Line::raw(format!(
                "Save {} as a tunnel.",
                printable(self.selected_name().unwrap_or_default())
            )),
            Line::raw(""),
            Line::from(name),
        ];
        if let Some(error) = &self.save_error {
            lines.push(Line::styled(
                format!("    {error}"),
                Style::default().fg(Color::Red),
            ));
        }
        lines
    }

    fn detail_lines(&self) -> Vec<Line<'static>> {
        let Some(report) = self.selected() else {
            return vec![Line::raw(
                "A tunnel is one forward through a host, started on demand as an ssh process that keeps running after Portico exits.",
            )];
        };
        let tunnel = &report.tunnel;
        let detail = report.status.detail();
        let mut status = vec![Span::raw("Status:  "), status_span(&report.status)];
        if !detail.is_empty() {
            status.push(Span::raw(format!(" \u{2014} {}", printable(&detail))));
        }
        let temporary = if report.temporary { " (temporary)" } else { "" };
        let mut lines = vec![
            Line::raw(format!("Name:    {}{temporary}", printable(&tunnel.name))),
            Line::raw(format!("Reaches: {}", printable(&report.reaches))),
            Line::raw(format!("Host:    {}", tunnel.host)),
            Line::raw(format!(
                "Forward: {} {} {}",
                tunnel.forward.kind.name(),
                tunnel.forward.kind.flag(),
                tunnel.forward.spec()
            )),
            Line::from(status),
        ];
        if let Some((command, _)) = self.selected.and_then(|index| self.extras.get(index)) {
            lines.push(Line::raw(format!("Command: {command}")));
        }
        for warning in &report.warnings {
            lines.push(Line::styled(
                format!("Warning: {warning}"),
                Style::default().fg(Color::Yellow),
            ));
        }
        let log = self.selected_log();
        if !log.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::raw("Log:"));
            let shown = if self.show_log { log.len() } else { LOG_LINES };
            for line in &log[log.len().saturating_sub(shown)..] {
                lines.push(Line::styled(
                    format!("  {line}"),
                    Style::default().add_modifier(Modifier::DIM),
                ));
            }
        }
        lines
    }

    /// The form's lines for a pane of `width` columns and `height` rows inside its border; only
    /// the Host dropdown adapts to them.
    fn form_lines(&self, width: usize, height: usize) -> Vec<Line<'static>> {
        let form = &self.form;
        let dim = Style::default().add_modifier(Modifier::DIM);
        let mut lines = Vec::new();
        for row in form.rows() {
            let (field, label, placeholder, _) = FIELDS[row];
            let focused = row == form.active;
            let mut spans = vec![Span::raw(format!(
                "{} {label}: ",
                if focused { '>' } else { ' ' }
            ))];
            if row == KIND {
                for kind in Kind::ALL {
                    let style = if kind == form.kind() {
                        Style::default().add_modifier(Modifier::REVERSED)
                    } else {
                        dim
                    };
                    spans.push(Span::styled(kind.name(), style));
                    spans.push(Span::raw(" "));
                }
            } else {
                let input = &form.values[row];
                spans.extend(input.spans(focused));
                if input.is_empty() {
                    spans.push(Span::styled(placeholder, dim));
                }
            }
            lines.push(Line::from(spans));
            if let Some((error_field, error)) = &form.error
                && *error_field == field
            {
                lines.push(Line::styled(
                    format!("    {error}"),
                    Style::default().fg(Color::Red),
                ));
            }
            if row == HOST
                && let Some(list) = &form.suggestions
            {
                // Fields, the dropdown's borders, and the blank line and hint under it stay on
                // screen.
                let rows = height
                    .saturating_sub(form.rows().len() + 4)
                    .clamp(1, SUGGESTION_ROWS);
                lines.extend(suggestion_lines(list, label, &self.home, width, rows));
            }
            if row == 4
                && let Ok(forward) = form.forward()
            {
                let tunnel = Tunnel {
                    name: form.value(0).to_owned(),
                    host: form.value(1).to_owned(),
                    forward,
                    container: None,
                };
                let saved: Vec<Tunnel> = self
                    .reports
                    .iter()
                    .map(|report| report.tunnel.clone())
                    .filter(|saved| {
                        form.editing
                            .as_ref()
                            .is_none_or(|editing| !editing.eq_ignore_ascii_case(&saved.name))
                    })
                    .collect();
                for other in collisions(&saved, &tunnel) {
                    lines.push(Line::styled(
                        format!(
                            "    uses the same port as tunnel {other}; only one can run at a time"
                        ),
                        Style::default().fg(Color::Yellow),
                    ));
                }
            }
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled(FIELDS[form.active].3, dim));
        if let Some((field, error)) = &form.error
            && !form.rows().iter().any(|&row| FIELDS[row].0 == *field)
        {
            lines.push(Line::styled(
                format!("Error: {error}"),
                Style::default().fg(Color::Red),
            ));
        }
        lines
    }
}

fn status_span(status: &TunnelStatus) -> Span<'static> {
    let style = match status {
        TunnelStatus::Running { .. } => Style::default().fg(Color::Green),
        TunnelStatus::Failed { .. } => Style::default().fg(Color::Red),
        TunnelStatus::PortInUse { .. } => Style::default().fg(Color::Yellow),
        TunnelStatus::Stopped => Style::default().add_modifier(Modifier::DIM),
    };
    Span::styled(status.name(), style)
}

/// What the starter starts: a tunnel by its name, or a temporary forward.
#[expect(
    clippy::large_enum_variant,
    reason = "one is sent per start; boxing the forward's target buys nothing"
)]
enum Job {
    Tunnel(String),
    /// Saves temporary forward `temporary` as tunnel `name`, restarting it when it runs.
    Save {
        temporary: String,
        name: String,
    },
    Forward {
        target: ForwardTarget,
        reach: Option<Reach>,
        remote: u16,
        local: Option<u16>,
    },
}

struct Request {
    generation: u64,
    job: Job,
    cancelled: Arc<AtomicBool>,
}

struct Delivered {
    generation: u64,
    finished: Finished,
}

/// A start's result.
#[derive(Debug)]
pub(super) enum Finished {
    /// Tunnel `.0`'s start.
    Tunnel(String, Result<StartOutcome, String>),
    /// A temporary forward's start, with the forward it started under its name.
    Forward(Result<(Tunnel, StartOutcome), String>),
    /// A temporary forward's save. It is delivered even when cancelled: the cancel only stops
    /// the restart, and the tunnel is saved.
    Save(Result<SaveOutcome, AppError>),
}

/// Starts one tunnel at a time on a worker thread, with the scheme of the connection tester:
/// a cancelled start kills its ssh, and a stale result is dropped by its generation.
pub(super) struct Starter {
    requests: mpsc::Sender<Request>,
    results: mpsc::Receiver<Delivered>,
    generation: u64,
    running: Option<(String, Arc<AtomicBool>)>,
}

impl Starter {
    pub(super) fn spawn<'scope>(
        scope: &'scope thread::Scope<'scope, '_>,
        app: &'scope App,
    ) -> Self {
        let (requests, receiver) = mpsc::channel::<Request>();
        let (sender, results) = mpsc::channel();
        scope.spawn(move || {
            while let Ok(request) = receiver.recv() {
                let save = matches!(request.job, Job::Save { .. });
                if request.cancelled.load(Ordering::Relaxed) && !save {
                    continue;
                }
                let cancelled = Some(request.cancelled.as_ref());
                let finished = match request.job {
                    Job::Tunnel(name) => {
                        let result = app
                            .start_tunnel(&name, cancelled, true)
                            .map_err(|error| error.to_string());
                        Finished::Tunnel(name, result)
                    }
                    Job::Save { temporary, name } => {
                        Finished::Save(app.save_tunnel(&temporary, &name, cancelled))
                    }
                    Job::Forward {
                        target,
                        reach,
                        remote,
                        local,
                    } => Finished::Forward(
                        app.quick_forward(&target, reach.as_ref(), remote, local, cancelled, true)
                            .map_err(|error| error.to_string()),
                    ),
                };
                if request.cancelled.load(Ordering::Relaxed) && !save {
                    continue;
                }
                let delivered = Delivered {
                    generation: request.generation,
                    finished,
                };
                if sender.send(delivered).is_err() {
                    return;
                }
            }
        });
        Self {
            requests,
            results,
            generation: 0,
            running: None,
        }
    }

    /// Starts tunnel `name`, cancelling the start still running.
    pub(super) fn start(&mut self, name: String) {
        self.send(name.clone(), Job::Tunnel(name));
    }

    /// Saves temporary forward `temporary` as tunnel `name`, shown as the temporary forward
    /// starting while its restart runs; cancels the start still running.
    pub(super) fn save(&mut self, temporary: String, name: String) {
        self.send(temporary.clone(), Job::Save { temporary, name });
    }

    /// Starts a temporary forward to port `remote` of `target`, cancelling the start still
    /// running.
    pub(super) fn forward(
        &mut self,
        target: ForwardTarget,
        reach: Option<Reach>,
        remote: u16,
        local: Option<u16>,
    ) {
        let shown = format!("a forward to {}:{remote}", target.label());
        self.send(
            shown,
            Job::Forward {
                target,
                reach,
                remote,
                local,
            },
        );
    }

    /// Sends `job`, shown as `shown` while it runs.
    fn send(&mut self, shown: String, job: Job) {
        self.cancel();
        self.generation += 1;
        let cancelled = Arc::new(AtomicBool::new(false));
        let request = Request {
            generation: self.generation,
            job,
            cancelled: Arc::clone(&cancelled),
        };
        if self.requests.send(request).is_ok() {
            self.running = Some((shown, cancelled));
        }
    }

    pub(super) fn cancel(&mut self) {
        if let Some((_, cancelled)) = self.running.take() {
            cancelled.store(true, Ordering::Relaxed);
        }
    }

    pub(super) fn running(&self) -> Option<&str> {
        self.running.as_ref().map(|(name, _)| name.as_str())
    }

    /// The results delivered since the last poll: the current job's, and any save's, which
    /// is wanted even once cancelled or replaced.
    pub(super) fn poll(&mut self) -> Vec<Finished> {
        let mut delivered = Vec::new();
        while let Ok(result) = self.results.try_recv() {
            if self.running.is_some() && result.generation == self.generation {
                self.running = None;
                delivered.push(result.finished);
            } else if matches!(result.finished, Finished::Save(_)) {
                delivered.push(result.finished);
            }
        }
        delivered
    }
}

impl Drop for Starter {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::domain::tunnel::TunnelWarning;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn report(name: &str, status: TunnelStatus) -> TunnelReport {
        TunnelReport {
            tunnel: Tunnel::new(
                name,
                "prod",
                Forward::parse(Kind::Local, "5432:localhost:5432").unwrap(),
            )
            .unwrap(),
            status,
            warnings: Vec::new(),
            temporary: false,
            reaches: "prod".into(),
        }
    }

    fn candidate(alias: &str, origin: suggest::Origin) -> Candidate {
        Candidate {
            alias: alias.into(),
            origin,
        }
    }

    pub(in crate::tui) fn screen_with(reports: Vec<TunnelReport>) -> TunnelsScreen {
        let mut screen = TunnelsScreen {
            extras: reports
                .iter()
                .map(|_| ("ssh -f -N".to_owned(), Vec::new()))
                .collect(),
            reports,
            hosts: vec![
                candidate("prod", suggest::Origin::Managed),
                candidate("prod-db", suggest::Origin::Managed),
                candidate(
                    "staging",
                    suggest::Origin::External("/home/u/.ssh/config".into()),
                ),
            ],
            home: "/home/u".into(),
            ..TunnelsScreen::default()
        };
        screen.refilter();
        screen
    }

    fn type_text(screen: &mut TunnelsScreen, text: &str) {
        for character in text.chars() {
            screen.handle_key(key(KeyCode::Char(character)));
        }
    }

    #[test]
    fn s_starts_a_stopped_tunnel_and_stops_a_running_one() {
        let mut screen = screen_with(vec![
            report("db", TunnelStatus::Stopped),
            report("web", TunnelStatus::Running { pid: 7, started: 0 }),
        ]);
        assert_eq!(
            screen.handle_key(key(KeyCode::Char('s'))),
            TunnelAction::Start("db".into())
        );
        screen.handle_key(key(KeyCode::Down));
        assert_eq!(
            screen.handle_key(key(KeyCode::Char('s'))),
            TunnelAction::Stop("web".into())
        );
        assert!(screen.footer().contains("s stop"));
        assert!(screen.any_running());
    }

    #[test]
    fn a_start_in_progress_shows_and_esc_cancels_it() {
        let mut screen = screen_with(vec![report("db", TunnelStatus::Stopped)]);
        screen.set_starting(Some("db".into()));
        assert_eq!(
            screen.handle_key(key(KeyCode::Char('s'))),
            TunnelAction::None
        );
        assert_eq!(
            screen.handle_key(key(KeyCode::Esc)),
            TunnelAction::CancelStart
        );
    }

    #[test]
    fn a_running_tunnel_is_not_edited_or_removed() {
        let mut screen = screen_with(vec![report(
            "db",
            TunnelStatus::Running { pid: 7, started: 0 },
        )]);
        for code in [KeyCode::Char('e'), KeyCode::Char('x')] {
            assert_eq!(screen.handle_key(key(code)), TunnelAction::None);
            assert_eq!(screen.screen, Screen::Browse);
            assert!(screen.status.as_ref().unwrap().contains("stop it first"));
        }
    }

    #[test]
    fn x_asks_before_removing() {
        let mut screen = screen_with(vec![report("db", TunnelStatus::Stopped)]);
        screen.handle_key(key(KeyCode::Char('x')));
        assert_eq!(screen.screen, Screen::ConfirmRemove);
        assert_eq!(screen.handle_key(key(KeyCode::Esc)), TunnelAction::None);
        screen.handle_key(key(KeyCode::Char('x')));
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            TunnelAction::Remove("db".into())
        );
    }

    #[test]
    fn the_form_adds_a_tunnel_and_hides_the_target_of_a_dynamic_one() {
        let mut screen = screen_with(vec![report("db", TunnelStatus::Stopped)]);
        screen.handle_key(key(KeyCode::Char('a')));
        assert_eq!(screen.screen, Screen::Form);
        type_text(&mut screen, "so cks");
        assert_eq!(
            screen.form.value(0),
            "socks",
            "a space is refused in a name"
        );
        screen.handle_key(key(KeyCode::Down));
        type_text(&mut screen, "stag");
        screen.handle_key(key(KeyCode::Tab));
        assert_eq!(screen.form.value(HOST), "staging");
        assert!(
            screen.form.suggestions.is_none(),
            "a pick closes the dropdown"
        );
        screen.handle_key(key(KeyCode::Tab));
        assert_eq!(screen.form.active, KIND, "Tab without a dropdown moves on");
        screen.handle_key(key(KeyCode::Char(' ')));
        screen.handle_key(key(KeyCode::Right));
        assert_eq!(screen.form.kind(), Kind::Dynamic);
        assert_eq!(screen.form.rows(), [0, 1, 2, 3, 4]);
        screen.handle_key(key(KeyCode::Down));
        screen.handle_key(key(KeyCode::Down));
        type_text(&mut screen, "10x80");
        assert_eq!(
            screen.form.value(4),
            "1080",
            "a letter is refused in a port"
        );
        let action = screen.handle_key(key(KeyCode::Enter));
        let TunnelAction::Add(tunnel) = action else {
            panic!("{action:?}");
        };
        assert_eq!(tunnel.name, "socks");
        assert_eq!(tunnel.host, "staging");
        assert_eq!(tunnel.forward.spec(), "1080");
        assert_eq!(tunnel.forward.kind, Kind::Dynamic);
    }

    #[test]
    fn the_form_puts_errors_on_their_field() {
        let mut screen = screen_with(Vec::new());
        screen.handle_key(key(KeyCode::Char('a')));
        type_text(&mut screen, "db");
        screen.handle_key(key(KeyCode::Down));
        type_text(&mut screen, "prod");
        assert_eq!(screen.handle_key(key(KeyCode::Enter)), TunnelAction::None);
        assert_eq!(
            screen.form.error.as_ref().map(|error| error.0),
            Some(TunnelField::BindPort)
        );
        assert_eq!(screen.form.active, 4);
        screen.finish_change(
            Err(AppError::Tunnel(TunnelRefusal::UnknownHost {
                name: "db".into(),
                host: "prod".into(),
            })),
            String::new(),
        );
        assert_eq!(screen.screen, Screen::Form);
        assert_eq!(
            screen.form.error.as_ref().map(|error| error.0),
            Some(TunnelField::Host)
        );
    }

    #[test]
    fn editing_starts_from_the_saved_tunnel() {
        let mut screen = screen_with(vec![report("db", TunnelStatus::Stopped)]);
        screen.handle_key(key(KeyCode::Char('e')));
        assert_eq!(screen.form.editing.as_deref(), Some("db"));
        assert_eq!(screen.form.value(4), "5432");
        let action = screen.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(&action, TunnelAction::Edit { name, changes } if name == "db" && changes.name.as_deref() == Some("db")),
            "{action:?}"
        );
    }

    #[test]
    fn esc_never_leaves_and_q_quits() {
        let mut screen = screen_with(vec![report("db", TunnelStatus::Stopped)]);
        assert!(screen.is_browsing());
        assert_eq!(screen.handle_key(key(KeyCode::Esc)), TunnelAction::None);
        assert_eq!(
            screen.handle_key(key(KeyCode::Char('T'))),
            TunnelAction::None
        );
        screen.handle_key(key(KeyCode::Char('/')));
        assert!(!screen.is_browsing(), "the filter takes the keys");
        screen.handle_key(key(KeyCode::Esc));
        assert_eq!(
            screen.handle_key(key(KeyCode::Char('q'))),
            TunnelAction::Quit
        );
    }

    #[test]
    fn the_filter_matches_names_hosts_and_forwards() {
        let mut screen = screen_with(vec![
            report("db", TunnelStatus::Stopped),
            report("web", TunnelStatus::Stopped),
        ]);
        screen.handle_key(key(KeyCode::Char('/')));
        type_text(&mut screen, "we");
        assert_eq!(screen.visible, [1]);
        screen.handle_key(key(KeyCode::Esc));
        assert_eq!(screen.visible, [0, 1]);
    }

    #[test]
    fn details_show_status_warnings_and_the_log() {
        let mut failed = report(
            "db",
            TunnelStatus::Failed {
                reason: "connection lost".into(),
                finished: None,
            },
        );
        failed.warnings.push(TunnelWarning::CollidesWith {
            tunnel: "web".into(),
        });
        let mut screen = screen_with(vec![failed]);
        screen.extras[0].1 = (1..=5).map(|line| format!("line {line}")).collect();
        let text: Vec<String> = screen
            .detail_lines()
            .iter()
            .map(|line| line.to_string())
            .collect();
        assert!(
            text.contains(&"Status:  failed \u{2014} connection lost".to_owned()),
            "{text:?}"
        );
        assert!(
            text.iter()
                .any(|line| line.contains("same port as tunnel web"))
        );
        assert!(text.contains(&"  line 5".to_owned()));
        assert!(!text.contains(&"  line 1".to_owned()));
        assert!(screen.footer().contains("o log"));
        screen.handle_key(key(KeyCode::Char('o')));
        let text: Vec<String> = screen
            .detail_lines()
            .iter()
            .map(|line| line.to_string())
            .collect();
        assert!(text.contains(&"  line 1".to_owned()));
    }

    fn ctrl(character: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL)
    }

    fn host_form() -> TunnelsScreen {
        let mut screen = screen_with(Vec::new());
        screen.handle_key(key(KeyCode::Char('a')));
        screen.handle_key(key(KeyCode::Down));
        assert_eq!(screen.form.active, HOST);
        screen
    }

    fn suggested(screen: &TunnelsScreen) -> Vec<&str> {
        screen
            .form
            .suggestions
            .as_ref()
            .map_or_else(Vec::new, |list| {
                list.items.iter().map(|item| item.alias.as_str()).collect()
            })
    }

    #[test]
    fn the_host_dropdown_opens_while_typing_and_closes_on_an_exact_match() {
        let mut screen = host_form();
        assert!(
            screen.form.suggestions.is_none(),
            "nothing typed, nothing listed"
        );
        type_text(&mut screen, "pro");
        assert_eq!(suggested(&screen), ["prod", "prod-db"]);
        assert_eq!(screen.footer(), "↑↓ choose  Enter pick  Esc close");
        type_text(&mut screen, "d");
        assert!(
            screen.form.suggestions.is_none(),
            "an exact match needs no list"
        );
        screen.handle_key(key(KeyCode::Backspace));
        assert_eq!(suggested(&screen), ["prod", "prod-db"]);
        screen.handle_key(key(KeyCode::Up));
        assert!(screen.form.suggestions.is_some(), "Up chooses in the list");
        assert_eq!(screen.form.active, HOST);
    }

    #[test]
    fn ctrl_space_opens_the_host_dropdown_on_demand() {
        let mut screen = host_form();
        assert!(screen.footer().contains("Ctrl+Space suggest"));
        screen.handle_key(ctrl(' '));
        assert_eq!(suggested(&screen), ["prod", "prod-db", "staging"]);
        let text = screen
            .form_lines(60, 30)
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("│> prod "), "{text}");
        assert!(text.contains("managed"), "{text}");
        assert!(
            text.contains(&crate::platform::paths::shown("~/.ssh/config")),
            "{text}"
        );
        screen.handle_key(ctrl(' '));
        assert!(screen.form.suggestions.is_some());
        // Ctrl+Space elsewhere does nothing.
        screen.handle_key(key(KeyCode::Esc));
        screen.handle_key(key(KeyCode::Down));
        screen.handle_key(ctrl(' '));
        assert!(screen.form.suggestions.is_none());
    }

    #[test]
    fn enter_picks_esc_closes_and_a_second_esc_cancels() {
        let mut screen = host_form();
        type_text(&mut screen, "pr");
        screen.handle_key(key(KeyCode::Down));
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            TunnelAction::None,
            "Enter picks rather than saves"
        );
        assert_eq!(screen.form.value(HOST), "prod-db");
        assert!(screen.form.suggestions.is_none());
        screen.handle_key(key(KeyCode::Backspace));
        assert!(screen.form.suggestions.is_some());
        screen.handle_key(key(KeyCode::Esc));
        assert!(screen.form.suggestions.is_none());
        assert_eq!(
            screen.screen,
            Screen::Form,
            "the first Esc only closes the list"
        );
        screen.handle_key(key(KeyCode::Esc));
        assert_eq!(screen.screen, Screen::Browse);
    }

    fn temporary(name: &str, status: TunnelStatus) -> TunnelReport {
        TunnelReport {
            tunnel: Tunnel {
                name: name.into(),
                host: "prod".into(),
                forward: Forward::parse(Kind::Local, "127.0.0.1:8080:localhost:80").unwrap(),
                container: None,
            },
            status,
            warnings: Vec::new(),
            temporary: true,
            reaches: "prod".into(),
        }
    }

    /// A temporary record rebuilt without its host and forward, as the app reports it.
    fn lost(name: &str) -> TunnelReport {
        TunnelReport {
            tunnel: Tunnel {
                name: name.into(),
                host: String::new(),
                forward: Forward {
                    kind: Kind::Dynamic,
                    bind_address: None,
                    bind_port: 0,
                    destination: None,
                },
                container: None,
            },
            status: TunnelStatus::Failed {
                reason: "exited".into(),
                finished: None,
            },
            warnings: Vec::new(),
            temporary: true,
            reaches: "unknown".into(),
        }
    }

    fn failed() -> TunnelStatus {
        TunnelStatus::Failed {
            reason: "connection lost".into(),
            finished: None,
        }
    }

    #[test]
    fn rows_show_the_name_what_it_reaches_the_local_address_and_the_status() {
        let mut saved = report("db", TunnelStatus::Stopped);
        saved.reaches = "prod-db".into();
        let screen = screen_with(vec![
            saved,
            temporary("_prod-80", TunnelStatus::Running { pid: 7, started: 0 }),
            lost("_web-80"),
        ]);
        let rows = screen.row_texts();
        assert_eq!(rows.len(), 3, "{rows:?}");
        let words = |row: &str| {
            row.split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            words(&rows[0]),
            ["db", "prod-db", "localhost:5432", "stopped"]
        );
        assert_eq!(
            words(&rows[1]),
            ["_prod-80", "temporary", "prod", "127.0.0.1:8080", "running"]
        );
        assert_eq!(
            words(&rows[2]),
            ["_web-80", "temporary", "unknown", "unknown", "failed"]
        );
    }

    #[test]
    fn row_text_is_made_printable() {
        let mut forward = temporary("_prod-80", TunnelStatus::Stopped);
        forward.reaches = "web\u{1b}[31m on prod".into();
        let screen = screen_with(vec![forward]);
        assert!(
            screen.row_texts()[0].contains("web on prod"),
            "{:?}",
            screen.row_texts()
        );
    }

    #[test]
    fn w_asks_for_a_name_and_saves_a_temporary_forward() {
        let mut screen = screen_with(vec![
            report("db", TunnelStatus::Stopped),
            temporary("_prod-80", TunnelStatus::Running { pid: 7, started: 0 }),
        ]);
        assert_eq!(
            screen.handle_key(key(KeyCode::Char('w'))),
            TunnelAction::None
        );
        assert!(screen.is_browsing(), "w does nothing on a saved tunnel");
        assert_eq!(screen.status, None);
        screen.handle_key(key(KeyCode::Down));
        assert!(screen.footer().contains("w save"), "{}", screen.footer());
        screen.handle_key(key(KeyCode::Char('w')));
        assert_eq!(screen.screen, Screen::SaveName);
        assert_eq!(screen.save_name.as_str(), "prod-80");
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            TunnelAction::Save {
                temporary: "_prod-80".into(),
                name: "prod-80".into()
            }
        );
        assert!(
            screen.is_browsing(),
            "the prompt closes while the save runs, so Esc cancels its restart"
        );
        screen.finish_save(Err(AppError::Tunnel(TunnelRefusal::Exists(
            "prod-80".into(),
        ))));
        assert_eq!(screen.screen, Screen::SaveName, "a refusal asks again");
        assert!(screen.save_error.is_some());
        type_text(&mut screen, "x");
        assert_eq!(
            screen.handle_key(key(KeyCode::Enter)),
            TunnelAction::Save {
                temporary: "_prod-80".into(),
                name: "prod-80x".into()
            }
        );
        screen.handle_key(key(KeyCode::Esc));
        assert!(screen.is_browsing(), "Esc cancels");
    }

    #[test]
    fn a_save_shows_the_restart_and_the_note_and_selects_the_saved_tunnel() {
        let mut screen = screen_with(vec![temporary(
            "_prod-80",
            TunnelStatus::Running { pid: 7, started: 0 },
        )]);
        screen.handle_key(key(KeyCode::Char('w')));
        let saved = Tunnel::new(
            "prod-80",
            "prod",
            Forward::parse(Kind::Local, "127.0.0.1:8080:172.18.0.4:80").unwrap(),
        )
        .unwrap();
        screen.finish_save(Ok(SaveOutcome {
            tunnel: saved.clone(),
            restarted: Some(StartOutcome::Started { pid: 9, warning: None }),
            note: Some("saved with the container's current address 172.18.0.4; it will not follow the container if it is recreated".into()),
            cancelled: false,
        }));
        assert!(screen.is_browsing());
        assert_eq!(
            screen.status_line().as_deref(),
            Some(
                "saved prod-80; restarted it (connections through it were interrupted); saved with the container's current address 172.18.0.4; it will not follow the container if it is recreated"
            )
        );
        assert_eq!(screen.pending_select.as_deref(), Some("prod-80"));

        screen.handle_key(key(KeyCode::Char('w')));
        screen.finish_save(Ok(SaveOutcome {
            tunnel: saved,
            restarted: Some(StartOutcome::Failed {
                reason: "port in use".into(),
                explanation: None,
            }),
            note: None,
            cancelled: false,
        }));
        assert_eq!(
            screen.status_line().as_deref(),
            Some("saved prod-80; restarting it failed: port in use")
        );
    }

    #[test]
    fn a_cancelled_restart_says_the_tunnel_is_saved() {
        let mut screen = screen_with(vec![temporary(
            "_prod-80",
            TunnelStatus::Running { pid: 7, started: 0 },
        )]);
        screen.handle_key(key(KeyCode::Char('w')));
        screen.handle_key(key(KeyCode::Enter));
        screen.finish_save(Ok(SaveOutcome {
            tunnel: temporary("prod-80", TunnelStatus::Stopped).tunnel,
            restarted: None,
            note: None,
            cancelled: true,
        }));
        assert_eq!(
            screen.status_line().as_deref(),
            Some("saved prod-80; restart cancelled")
        );
    }

    #[test]
    fn a_running_tunnel_name_is_printable_in_the_stop_first_message() {
        let mut screen = screen_with(vec![report(
            "db",
            TunnelStatus::Running { pid: 7, started: 0 },
        )]);
        screen.reports[0].tunnel.name = "d\u{1b}[31mb".into();
        screen.handle_key(key(KeyCode::Char('e')));
        assert_eq!(
            screen.status.as_deref(),
            Some("db is running; stop it first (s)")
        );
    }

    #[test]
    fn e_does_not_edit_a_temporary_forward() {
        let mut screen = screen_with(vec![temporary("_prod-80", failed())]);
        assert_eq!(
            screen.handle_key(key(KeyCode::Char('e'))),
            TunnelAction::None
        );
        assert!(screen.is_browsing());
        assert_eq!(screen.status, None);
        assert!(!screen.footer().contains("e edit"), "{}", screen.footer());
    }

    #[test]
    fn x_stops_and_removes_a_temporary_forward_after_asking() {
        let mut screen = screen_with(vec![temporary(
            "_prod-80",
            TunnelStatus::Running { pid: 7, started: 0 },
        )]);
        screen.handle_key(key(KeyCode::Char('x')));
        assert_eq!(screen.screen, Screen::ConfirmRemove);
        assert_eq!(screen.confirmation(), "Stop and remove _prod-80? (y/n)");
        assert_eq!(
            screen.handle_key(key(KeyCode::Char('n'))),
            TunnelAction::None
        );
        screen.handle_key(key(KeyCode::Char('x')));
        assert_eq!(
            screen.handle_key(key(KeyCode::Char('y'))),
            TunnelAction::Stop("_prod-80".into())
        );
    }

    #[test]
    fn s_on_a_running_temporary_forward_asks_before_stopping_it() {
        let mut screen = screen_with(vec![temporary(
            "_prod-80",
            TunnelStatus::Running { pid: 7, started: 0 },
        )]);
        assert_eq!(
            screen.handle_key(key(KeyCode::Char('s'))),
            TunnelAction::None
        );
        assert_eq!(screen.screen, Screen::ConfirmRemove);
        assert_eq!(screen.confirmation(), "Stop and remove _prod-80? (y/n)");
        assert_eq!(
            screen.handle_key(key(KeyCode::Char('n'))),
            TunnelAction::None
        );
        screen.handle_key(key(KeyCode::Char('s')));
        assert_eq!(
            screen.handle_key(key(KeyCode::Char('y'))),
            TunnelAction::Stop("_prod-80".into())
        );
    }

    #[test]
    fn y_copies_and_b_opens_the_local_address() {
        let mut screen = screen_with(vec![
            temporary("_prod-80", TunnelStatus::Running { pid: 7, started: 0 }),
            report("db", TunnelStatus::Stopped),
            lost("_web-80"),
        ]);
        assert!(
            screen.footer().contains("y copy  b open"),
            "{}",
            screen.footer()
        );
        assert_eq!(
            screen.handle_key(key(KeyCode::Char('y'))),
            TunnelAction::Copy("127.0.0.1:8080".into())
        );
        assert_eq!(
            screen.handle_key(key(KeyCode::Char('b'))),
            TunnelAction::Open("http://127.0.0.1:8080".into())
        );
        screen.handle_key(key(KeyCode::Down));
        assert_eq!(
            screen.handle_key(key(KeyCode::Char('y'))),
            TunnelAction::Copy("localhost:5432".into()),
            "a saved local forward has a local address too"
        );
        screen.handle_key(key(KeyCode::Down));
        assert_eq!(
            screen.handle_key(key(KeyCode::Char('y'))),
            TunnelAction::None
        );
        assert_eq!(screen.status.as_deref(), Some("no local address to copy"));
        assert_eq!(
            screen.handle_key(key(KeyCode::Char('b'))),
            TunnelAction::None
        );
        assert_eq!(screen.status.as_deref(), Some("no local address to open"));
    }

    #[test]
    fn s_starts_a_failed_temporary_forward_unless_its_record_lost_the_forward() {
        let mut screen = screen_with(vec![temporary("_prod-80", failed()), lost("_web-80")]);
        assert_eq!(
            screen.handle_key(key(KeyCode::Char('s'))),
            TunnelAction::Start("_prod-80".into())
        );
        screen.handle_key(key(KeyCode::Down));
        assert_eq!(
            screen.handle_key(key(KeyCode::Char('s'))),
            TunnelAction::None
        );
        assert_eq!(
            screen.status.as_deref(),
            Some("cannot start it again: its record lost the forward; x removes it")
        );
    }

    #[test]
    fn pasting_into_host_opens_the_dropdown_and_leaving_the_field_closes_it() {
        let mut screen = host_form();
        screen.paste("sta");
        assert_eq!(suggested(&screen), ["staging"]);
        screen.handle_key(key(KeyCode::BackTab));
        assert!(screen.form.suggestions.is_none());
        assert_eq!(screen.form.active, 0);
    }
}
