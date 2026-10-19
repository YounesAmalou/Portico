//! The tunnels screen: the saved tunnels with their status, their add and edit form, and the
//! worker that starts a tunnel while the interface keeps handling keys.

use std::{
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
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};

use super::TextInput;
use crate::{
    app::{App, AppError, StartOutcome, TunnelChanges, TunnelRefusal},
    fuzzy,
    picker::HIT,
    tunnel::{
        Forward, Kind, Tunnel, TunnelError, TunnelField, TunnelReport, TunnelStatus, collisions,
    },
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
        "The host the tunnel goes through, as you would type it after ssh. Tab completes a known alias.",
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
}

/// What the screen asks the event loop to do.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum TunnelAction {
    None,
    /// Back to the hosts screen.
    Leave,
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
}

#[derive(Clone, Debug, Default)]
pub(super) struct TunnelsScreen {
    reports: Vec<TunnelReport>,
    /// Each report's command line and log tail, in the same order.
    extras: Vec<(String, Vec<String>)>,
    load_error: Option<String>,
    hosts: Vec<String>,
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
}

/// How many log lines the details show unless unfolded.
const LOG_LINES: usize = 3;

impl TunnelsScreen {
    /// Re-reads the tunnels, their statuses and the host aliases, keeping the selection.
    pub(super) fn reload(&mut self, app: &App) {
        let selected = self.selected_name().map(str::to_owned);
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
            self.hosts = inventory.hosts.into_iter().map(|host| host.alias).collect();
        }
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
            Ok(StartOutcome::Started { pid }) => format!("Started {name} (pid {pid})"),
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
                    &[&report.tunnel.name, &report.tunnel.host, &forward],
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

    fn selected_name(&self) -> Option<&str> {
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
                    self.selected_name()
                        .map_or(TunnelAction::None, |name| TunnelAction::Remove(name.into()))
                }
                KeyCode::Esc | KeyCode::Char('n') => {
                    self.screen = Screen::Browse;
                    TunnelAction::None
                }
                _ => TunnelAction::None,
            },
        }
    }

    pub(super) fn paste(&mut self, text: &str) {
        match self.screen {
            Screen::Filter => {
                self.filter.paste(text);
                self.refilter();
            }
            Screen::Form if self.form.active != KIND => {
                let field = FIELDS[self.form.active].0;
                let text: String = text.chars().filter(|c| field.permits(*c)).collect();
                self.form.values[self.form.active].paste(&text);
            }
            _ => {}
        }
    }

    fn handle_browse_key(&mut self, key: KeyEvent) -> TunnelAction {
        let running = self
            .selected()
            .is_some_and(|report| report.status.is_running());
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
            KeyCode::Esc => return TunnelAction::Leave,
            KeyCode::Char('T') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                return TunnelAction::Leave;
            }
            KeyCode::Char('q') => return TunnelAction::Quit,
            KeyCode::Char('r') | KeyCode::F(5) => return TunnelAction::Refresh,
            KeyCode::Char('o') => self.show_log = !self.show_log,
            KeyCode::Char('a') => {
                self.form = TunnelForm::default();
                self.screen = Screen::Form;
            }
            KeyCode::Char('e') | KeyCode::Char('x') if running => {
                self.status = Some(format!(
                    "{} is running; stop it first (s)",
                    self.selected_name().unwrap_or_default()
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
                if running {
                    return TunnelAction::Stop(name);
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
        let form = &mut self.form;
        match key.code {
            KeyCode::Esc => self.screen = Screen::Browse,
            KeyCode::Up | KeyCode::BackTab => form.step(-1),
            KeyCode::Down => form.step(1),
            KeyCode::Tab if form.active == HOST => {
                match host_matches(&self.hosts, form.value(HOST)).first() {
                    Some(alias) if alias != form.value(HOST) => {
                        form.values[HOST] = TextInput::new(alias);
                    }
                    _ => form.step(1),
                }
            }
            KeyCode::Tab => form.step(1),
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
                }
            }
            _ => {}
        }
        TunnelAction::None
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
                format!(
                    "↑↓ move  / filter  {toggle}a add  e edit  x remove  r refresh  {log}T/Esc hosts  q quit"
                )
            }
            Screen::Filter => "type to filter  ←→ move  Enter apply  Esc cancel".into(),
            Screen::Form => {
                let special = match self.form.active {
                    KIND => "  Space/←→ change kind",
                    HOST => "  Tab complete host",
                    _ => "",
                };
                format!("↑↓ field{special}  Enter save  Esc cancel")
            }
            Screen::ConfirmRemove => "Enter remove  Esc cancel".into(),
        }
    }

    fn selected_log(&self) -> &[String] {
        self.selected
            .and_then(|index| self.extras.get(index))
            .map_or(&[], |(_, log)| log.as_slice())
    }

    pub(super) fn render(&self, frame: &mut Frame<'_>) {
        let footer = self.footer();
        let footer_height = (footer.chars().count() as u16)
            .div_ceil(frame.area().width.max(1))
            .max(1);
        let status = self.status.clone().or_else(|| {
            self.starting
                .as_ref()
                .map(|name| format!("Starting {name}\u{2026} (Esc to cancel)"))
        });
        let areas = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(1),
                Constraint::Length(u16::from(status.is_some())),
                Constraint::Length(footer_height),
            ])
            .split(frame.area());
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(areas[0]);

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
                self.form_lines(),
            ),
            Screen::ConfirmRemove => (
                "Remove tunnel",
                vec![Line::raw(format!(
                    "Remove {}?",
                    self.selected_name().unwrap_or("the selected tunnel")
                ))],
            ),
            _ => ("Details", self.detail_lines()),
        };
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(Block::default().borders(Borders::ALL).title(right_title)),
            columns[1],
        );
        frame.render_widget(Paragraph::new(status.unwrap_or_default()), areas[1]);
        frame.render_widget(Paragraph::new(footer).wrap(Wrap { trim: true }), areas[2]);
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
        let rows: Vec<[String; 3]> = self
            .visible
            .iter()
            .map(|&index| {
                let tunnel = &self.reports[index].tunnel;
                [
                    tunnel.name.clone(),
                    tunnel.host.clone(),
                    tunnel.forward.describe(),
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
        self.visible
            .iter()
            .zip(rows)
            .map(|(&index, row)| {
                let report = &self.reports[index];
                let positions = fuzzy::match_fields(self.filter.as_str(), &[&row[0]])
                    .and_then(|found| found.positions.into_iter().next())
                    .unwrap_or_default();
                let mut spans = fuzzy::highlight(&row[0], &positions, Style::default(), HIT);
                spans.push(Span::raw(format!(
                    "{}  {:w1$}  {:w2$}  ",
                    " ".repeat(widths[0] - row[0].chars().count()),
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
                ListItem::new(Line::from(spans))
            })
            .collect()
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
            status.push(Span::raw(format!(" \u{2014} {detail}")));
        }
        let mut lines = vec![
            Line::raw(format!("Name:    {}", tunnel.name)),
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

    fn form_lines(&self) -> Vec<Line<'static>> {
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
            if row == HOST && focused {
                let matches = host_matches(&self.hosts, form.value(HOST));
                if !matches.is_empty() && !matches.iter().any(|alias| alias == form.value(HOST)) {
                    lines.push(Line::styled(format!("    {}", matches.join("  ")), dim));
                }
            }
            if row == 4
                && let Ok(forward) = form.forward()
            {
                let tunnel = Tunnel {
                    name: form.value(0).to_owned(),
                    host: form.value(1).to_owned(),
                    forward,
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

/// The aliases matching what is typed, best first, at most six.
fn host_matches(hosts: &[String], typed: &str) -> Vec<String> {
    if typed.is_empty() {
        return Vec::new();
    }
    let rows: Vec<[&str; 1]> = hosts.iter().map(|alias| [alias.as_str()]).collect();
    fuzzy::rank(typed, rows.iter().map(|row| row.as_slice()))
        .into_iter()
        .take(6)
        .map(|(index, _)| hosts[index].clone())
        .collect()
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

struct Request {
    generation: u64,
    name: String,
    cancelled: Arc<AtomicBool>,
}

struct Delivered {
    generation: u64,
    name: String,
    result: Result<StartOutcome, String>,
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
                if request.cancelled.load(Ordering::Relaxed) {
                    continue;
                }
                let result = app
                    .start_tunnel(&request.name, Some(&request.cancelled), true)
                    .map_err(|error| error.to_string());
                if request.cancelled.load(Ordering::Relaxed) {
                    continue;
                }
                let delivered = Delivered {
                    generation: request.generation,
                    name: request.name,
                    result,
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

    pub(super) fn start(&mut self, name: String) {
        self.cancel();
        self.generation += 1;
        let cancelled = Arc::new(AtomicBool::new(false));
        let request = Request {
            generation: self.generation,
            name: name.clone(),
            cancelled: Arc::clone(&cancelled),
        };
        if self.requests.send(request).is_ok() {
            self.running = Some((name, cancelled));
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

    pub(super) fn poll(&mut self) -> Option<(String, Result<StartOutcome, String>)> {
        let mut current = None;
        while let Ok(delivered) = self.results.try_recv() {
            if self.running.is_some() && delivered.generation == self.generation {
                self.running = None;
                current = Some((delivered.name, delivered.result));
            }
        }
        current
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
    use crate::tunnel::TunnelWarning;

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
        }
    }

    pub(in crate::tui) fn screen_with(reports: Vec<TunnelReport>) -> TunnelsScreen {
        let mut screen = TunnelsScreen {
            extras: reports
                .iter()
                .map(|_| ("ssh -f -N".to_owned(), Vec::new()))
                .collect(),
            reports,
            hosts: vec!["prod".into(), "prod-db".into(), "staging".into()],
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
        screen.handle_key(key(KeyCode::Tab));
        assert_eq!(screen.form.active, KIND, "a completed host moves on");
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
    fn t_and_esc_leave_and_q_quits() {
        let mut screen = screen_with(vec![report("db", TunnelStatus::Stopped)]);
        assert_eq!(
            screen.handle_key(key(KeyCode::Char('T'))),
            TunnelAction::Leave
        );
        assert_eq!(
            screen.handle_key(KeyEvent::new(KeyCode::Char('T'), KeyModifiers::SHIFT)),
            TunnelAction::Leave
        );
        assert_eq!(screen.handle_key(key(KeyCode::Esc)), TunnelAction::Leave);
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
}
