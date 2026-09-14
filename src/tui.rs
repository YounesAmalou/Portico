use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use crossterm::{
    cursor::{Hide, Show},
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout},
    style::{Modifier, Style},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};

use crate::{
    app::{App, AppError, HostDetails, HostInventory, HostSummary, Ownership},
    discovery::DiscoveryWarning,
    host::{Change, ClearField, Host, HostChanges},
    managed::InitPlan,
};
use thiserror::Error;

const FORM_LABELS: [&str; 6] = [
    "Alias",
    "HostName",
    "User",
    "Port",
    "IdentityFile",
    "ProxyJump",
];

#[derive(Debug, Error)]
pub enum TuiError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    App(#[from] AppError),
    #[error("child process failed: {0}")]
    Child(String),
}

pub fn run(app: &App) -> Result<(), TuiError> {
    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let terminated = termination_flag()?;
    let plan = app.init_plan()?;
    if plan.requires_confirmation() && !confirm_initialization(&mut terminal, &plan, &terminated)? {
        return Ok(());
    }
    if plan.requires_confirmation() {
        app.initialize_cancellable(true, &terminated)?;
    }

    browse(app, &mut terminal, &terminated)
}

fn confirm_initialization<B: Backend>(
    terminal: &mut Terminal<B>,
    plan: &InitPlan,
    terminated: &AtomicBool,
) -> Result<bool, TuiError> {
    terminal.draw(|frame| render_init(frame, plan))?;
    loop {
        if terminated.load(Ordering::Relaxed) {
            return Ok(false);
        }
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                    return Ok(false);
                }
                if let Some(approved) = init_decision(key) {
                    return Ok(approved);
                }
            }
            Event::Resize(_, _) => {
                terminal.draw(|frame| render_init(frame, plan))?;
            }
            _ => {}
        }
    }
}

fn browse<B: Backend>(
    app: &App,
    terminal: &mut Terminal<B>,
    terminated: &AtomicBool,
) -> Result<(), TuiError> {
    let mut state = UiState::new(app.hosts()?);
    let mut details = resolve_selected(app, &mut state, terminated);
    let mut redraw = true;

    loop {
        if terminated.load(Ordering::Relaxed) {
            return Ok(());
        }
        if redraw {
            terminal.draw(|frame| render(frame, &state, details.as_ref()))?;
            redraw = false;
        }
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let event = event::read()?;
        if matches!(event, Event::Resize(_, _)) {
            redraw = true;
            continue;
        }
        if let Event::Paste(text) = event {
            let before = state.selected_summary().cloned();
            state.handle_paste(&text);
            if details_stale(before.as_ref(), state.selected_summary(), false) {
                details = resolve_selected(app, &mut state, terminated);
            }
            redraw = true;
            continue;
        }
        let Event::Key(key) = event else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Ok(());
        }

        let before = state.selected_summary().cloned();
        let action = match state.handle_key(key, details.as_ref()) {
            Ok(action) => action,
            Err(error) => {
                state.status = Some(error);
                redraw = true;
                continue;
            }
        };

        let mut inventory_changed = false;
        match action {
            UiAction::None => {}
            UiAction::Quit => return Ok(()),
            UiAction::Connect {
                alias,
                new_terminal,
            } => {
                let result = if new_terminal {
                    app.connect(&alias, true).map_err(TuiError::from)
                } else {
                    let result = suspend_for_child(
                        leave_terminal,
                        || app.connect(&alias, false).map_err(TuiError::from),
                        enter_terminal,
                    );
                    match result {
                        Err(TuiError::Io(error)) => return Err(TuiError::Io(error)),
                        result => {
                            terminal.clear()?;
                            result
                        }
                    }
                };
                state.status = Some(match result {
                    Ok(status) if status.success() && new_terminal => {
                        format!("Opened {alias} in a new terminal")
                    }
                    Ok(status) if status.success() => format!("Disconnected from {alias}"),
                    Ok(status) => format!("ssh exited with {status}"),
                    Err(error) => error.to_string(),
                });
            }
            UiAction::Add(host) => {
                let alias = host.alias().to_owned();
                match app.add_cancellable(host, terminated) {
                    Ok(()) => {
                        inventory_changed = complete_mutation(
                            &mut state,
                            Some(&alias),
                            format!("Saved {alias}"),
                            || app.hosts(),
                        )
                    }
                    Err(error) => state.status = Some(error.to_string()),
                }
            }
            UiAction::Edit { alias, changes } => {
                let selected = match &changes.alias {
                    Change::Set(changed) => changed.clone(),
                    Change::Keep | Change::Clear => alias.clone(),
                };
                match app.edit_cancellable(&alias, changes, terminated) {
                    Ok(()) => {
                        inventory_changed = complete_mutation(
                            &mut state,
                            Some(&selected),
                            format!("Saved {selected}"),
                            || app.hosts(),
                        )
                    }
                    Err(error) => state.status = Some(error.to_string()),
                }
            }
            UiAction::Remove(alias) => match app.remove_cancellable(&alias, terminated) {
                Ok(()) => {
                    inventory_changed =
                        complete_mutation(&mut state, None, format!("Removed {alias}"), || {
                            app.hosts()
                        })
                }
                Err(error) => state.status = Some(error.to_string()),
            },
        }

        if details_stale(before.as_ref(), state.selected_summary(), inventory_changed) {
            details = resolve_selected(app, &mut state, terminated);
        }
        redraw = true;
    }
}

fn complete_mutation<E, F>(
    state: &mut UiState,
    preferred: Option<&str>,
    status: String,
    refresh: F,
) -> bool
where
    E: std::fmt::Display,
    F: FnOnce() -> Result<HostInventory, E>,
{
    state.mode = Mode::Browse;
    state.form = FormState::default();
    match refresh() {
        Ok(hosts) => {
            state.replace_hosts(hosts);
            if let Some(alias) = preferred {
                state.select_alias(alias);
            }
            state.status = Some(status);
            true
        }
        Err(error) => {
            state.status = Some(format!("{status}, refresh failed: {error}"));
            false
        }
    }
}

fn resolve_selected(
    app: &App,
    state: &mut UiState,
    terminated: &AtomicBool,
) -> Option<HostDetails> {
    let selected = state.selected_summary()?.clone();
    match app.show_cancellable(&selected.alias, terminated) {
        Ok(details) => Some(details_for_selected(&selected, details)),
        Err(error) => {
            state.status = Some(error.to_string());
            None
        }
    }
}

fn termination_flag() -> Result<Arc<AtomicBool>, TuiError> {
    let terminated = Arc::new(AtomicBool::new(false));
    for signal in [
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
    ] {
        signal_hook::flag::register(signal, Arc::clone(&terminated))?;
    }
    Ok(terminated)
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self, TuiError> {
        enter_terminal()?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = leave_terminal();
    }
}

fn enter_terminal() -> Result<(), TuiError> {
    enable_raw_mode()?;
    if let Err(error) = execute!(
        io::stdout(),
        EnterAlternateScreen,
        EnableBracketedPaste,
        Hide
    ) {
        let _ = leave_terminal();
        return Err(error.into());
    }
    Ok(())
}

fn leave_terminal() -> Result<(), TuiError> {
    let raw = disable_raw_mode();
    let screen = execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen);
    let cursor = execute!(io::stdout(), Show);
    raw?;
    screen?;
    cursor?;
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Mode {
    Browse,
    Filter,
    AddForm,
    EditForm,
    ConfirmRemove,
}

#[derive(Clone, Debug)]
struct UiState {
    hosts: Vec<HostSummary>,
    warnings: Vec<DiscoveryWarning>,
    visible: Vec<usize>,
    selected: Option<usize>,
    filter: String,
    mode: Mode,
    status: Option<String>,
    form: FormState,
}

#[derive(Clone, Debug, Default)]
struct FormState {
    values: [String; 6],
    active: usize,
    original: Option<Host>,
}

#[derive(Debug, Eq, PartialEq)]
enum UiAction {
    None,
    Quit,
    Connect { alias: String, new_terminal: bool },
    Add(Host),
    Edit { alias: String, changes: HostChanges },
    Remove(String),
}

impl UiState {
    fn new(inventory: HostInventory) -> Self {
        let HostInventory { hosts, warnings } = inventory;
        let visible = (0..hosts.len()).collect::<Vec<_>>();
        let selected = visible.first().copied();
        Self {
            hosts,
            warnings,
            visible,
            selected,
            filter: String::new(),
            mode: Mode::Browse,
            status: None,
            form: FormState::default(),
        }
    }

    fn set_filter(&mut self, filter: &str) {
        self.filter = filter.into();
        let filter = filter.to_ascii_lowercase();
        self.visible = self
            .hosts
            .iter()
            .enumerate()
            .filter(|(_, host)| host.alias.to_ascii_lowercase().contains(&filter))
            .map(|(index, _)| index)
            .collect();
        if !self
            .selected
            .is_some_and(|selected| self.visible.contains(&selected))
        {
            self.selected = self.visible.first().copied();
        }
    }

    fn select_alias(&mut self, alias: &str) {
        if let Some(index) = self
            .hosts
            .iter()
            .position(|host| host.alias.eq_ignore_ascii_case(alias))
            .filter(|index| self.visible.contains(index))
        {
            self.selected = Some(index);
        }
    }

    fn selected_alias(&self) -> Option<&str> {
        self.selected.map(|index| self.hosts[index].alias.as_str())
    }

    fn selected_summary(&self) -> Option<&HostSummary> {
        self.selected.map(|index| &self.hosts[index])
    }

    fn replace_hosts(&mut self, inventory: HostInventory) {
        let selected = self.selected_summary().cloned();
        self.hosts = inventory.hosts;
        self.warnings = inventory.warnings;
        let filter = self.filter.clone();
        self.selected = None;
        self.set_filter(&filter);
        if let Some(selected) = selected
            && let Some(index) = self.hosts.iter().position(|host| host == &selected)
        {
            self.selected = Some(index);
        }
    }

    fn handle_key(
        &mut self,
        key: KeyEvent,
        details: Option<&HostDetails>,
    ) -> Result<UiAction, String> {
        self.status = None;
        match self.mode {
            Mode::Browse => self.handle_browse_key(key, details),
            Mode::Filter => self.handle_filter_key(key),
            Mode::AddForm | Mode::EditForm => self.handle_form_key(key),
            Mode::ConfirmRemove => Ok(self.handle_remove_key(key)),
        }
    }

    fn handle_paste(&mut self, text: &str) {
        self.status = None;
        match self.mode {
            Mode::Filter => {
                let mut filter = self.filter.clone();
                filter.push_str(text);
                self.set_filter(&filter);
            }
            Mode::AddForm | Mode::EditForm => {
                self.form.values[self.form.active].push_str(text);
            }
            Mode::Browse | Mode::ConfirmRemove => {}
        }
    }

    fn handle_browse_key(
        &mut self,
        key: KeyEvent,
        details: Option<&HostDetails>,
    ) -> Result<UiAction, String> {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Char('/') => self.mode = Mode::Filter,
            KeyCode::Char('a') => {
                self.form = FormState::default();
                self.mode = Mode::AddForm;
            }
            KeyCode::Char('e') => {
                if self.selected_is_managed()
                    && let Some(host) = details.and_then(|details| details.declared.as_ref())
                {
                    self.form = FormState::from_host(host);
                    self.mode = Mode::EditForm;
                }
            }
            KeyCode::Char('x') => {
                if self.selected_is_managed() {
                    self.mode = Mode::ConfirmRemove;
                }
            }
            KeyCode::Enter => return Ok(self.connect_action(false)),
            KeyCode::Char('n') => return Ok(self.connect_action(true)),
            KeyCode::Char('q') => return Ok(UiAction::Quit),
            _ => {}
        }
        Ok(UiAction::None)
    }

    fn handle_filter_key(&mut self, key: KeyEvent) -> Result<UiAction, String> {
        match key.code {
            KeyCode::Esc | KeyCode::Backspace | KeyCode::Enter => self.mode = Mode::Browse,
            KeyCode::Delete => {
                let mut filter = self.filter.clone();
                filter.pop();
                self.set_filter(&filter);
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                let mut filter = self.filter.clone();
                filter.push(character);
                self.set_filter(&filter);
            }
            _ => {}
        }
        Ok(UiAction::None)
    }

    fn handle_form_key(&mut self, key: KeyEvent) -> Result<UiAction, String> {
        match key.code {
            KeyCode::Esc | KeyCode::Backspace => self.mode = Mode::Browse,
            KeyCode::Up | KeyCode::BackTab => self.form.active = self.form.active.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab => {
                self.form.active = (self.form.active + 1).min(FORM_LABELS.len() - 1)
            }
            KeyCode::Delete => {
                self.form.values[self.form.active].pop();
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.form.values[self.form.active].push(character);
            }
            KeyCode::Enter => return self.form.submission(&self.mode),
            _ => {}
        }
        Ok(UiAction::None)
    }

    fn handle_remove_key(&mut self, key: KeyEvent) -> UiAction {
        match key.code {
            KeyCode::Esc | KeyCode::Backspace => {
                self.mode = Mode::Browse;
                UiAction::None
            }
            KeyCode::Enter => self
                .selected_alias()
                .map_or(UiAction::None, |alias| UiAction::Remove(alias.into())),
            _ => UiAction::None,
        }
    }

    fn move_selection(&mut self, offset: isize) {
        let Some(selected) = self.selected else {
            return;
        };
        let Some(position) = self.visible.iter().position(|&index| index == selected) else {
            return;
        };
        let next = position
            .saturating_add_signed(offset)
            .min(self.visible.len().saturating_sub(1));
        self.selected = self.visible.get(next).copied();
    }

    fn selected_is_managed(&self) -> bool {
        self.selected
            .is_some_and(|index| self.hosts[index].ownership == Ownership::Managed)
    }

    fn connect_action(&self, new_terminal: bool) -> UiAction {
        self.selected_alias()
            .map_or(UiAction::None, |alias| UiAction::Connect {
                alias: alias.into(),
                new_terminal,
            })
    }
}

impl FormState {
    fn from_host(host: &Host) -> Self {
        Self {
            values: [
                host.alias().into(),
                host.hostname().into(),
                host.user().unwrap_or_default().into(),
                host.port()
                    .map_or_else(String::new, |port| port.to_string()),
                host.identity_file().unwrap_or_default().into(),
                host.proxy_jump().unwrap_or_default().into(),
            ],
            active: 0,
            original: Some(host.clone()),
        }
    }

    fn submission(&self, mode: &Mode) -> Result<UiAction, String> {
        let port = if self.values[3].is_empty() {
            None
        } else {
            Some(
                self.values[3]
                    .parse::<u16>()
                    .map_err(|_| format!("invalid port: {}", self.values[3]))?,
            )
        };
        let host = Host::new(
            self.values[0].clone(),
            self.values[1].clone(),
            optional(&self.values[2]),
            port,
            optional(&self.values[4]),
            optional(&self.values[5]),
        )
        .map_err(|error| error.to_string())?;

        if *mode == Mode::AddForm {
            return Ok(UiAction::Add(host));
        }

        let original = self
            .original
            .as_ref()
            .ok_or_else(|| "missing original host".to_string())?;
        let mut clear = Vec::new();
        if original.user().is_some() && host.user().is_none() {
            clear.push(ClearField::User);
        }
        if original.port().is_some() && host.port().is_none() {
            clear.push(ClearField::Port);
        }
        if original.identity_file().is_some() && host.identity_file().is_none() {
            clear.push(ClearField::Identity);
        }
        if original.proxy_jump().is_some() && host.proxy_jump().is_none() {
            clear.push(ClearField::Jump);
        }
        let mut changes = HostChanges::from_clear_fields(clear);
        set_changed(&mut changes.alias, original.alias(), host.alias());
        set_changed(&mut changes.hostname, original.hostname(), host.hostname());
        set_optional_changed(&mut changes.user, original.user(), host.user());
        if original.port() != host.port() && host.port().is_some() {
            changes.port = Change::Set(host.port().expect("checked above"));
        }
        set_optional_changed(
            &mut changes.identity_file,
            original.identity_file(),
            host.identity_file(),
        );
        set_optional_changed(
            &mut changes.proxy_jump,
            original.proxy_jump(),
            host.proxy_jump(),
        );
        Ok(UiAction::Edit {
            alias: original.alias().into(),
            changes,
        })
    }
}

fn optional(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.into())
}

fn set_changed(change: &mut Change<String>, before: &str, after: &str) {
    if before != after {
        *change = Change::Set(after.into());
    }
}

fn set_optional_changed(change: &mut Change<String>, before: Option<&str>, after: Option<&str>) {
    if before != after
        && let Some(after) = after
    {
        *change = Change::Set(after.into());
    }
}

fn details_for_selected(selected: &HostSummary, mut details: HostDetails) -> HostDetails {
    details.summary = selected.clone();
    if selected.ownership == Ownership::External {
        details.declared = None;
    }
    details
}

fn details_stale(
    cached_for: Option<&HostSummary>,
    selected: Option<&HostSummary>,
    inventory_changed: bool,
) -> bool {
    inventory_changed || cached_for != selected
}

fn render(frame: &mut Frame<'_>, state: &UiState, details: Option<&HostDetails>) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(
                state
                    .warnings
                    .len()
                    .min(usize::from(frame.area().height / 3)) as u16,
            ),
            Constraint::Length(u16::from(state.status.is_some())),
            Constraint::Length(1),
        ])
        .split(frame.area());
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(areas[0]);

    let hosts = state
        .visible
        .iter()
        .map(|&index| {
            let host = &state.hosts[index];
            ListItem::new(format!("{}  {}", host.alias, ownership(host.ownership)))
        })
        .collect::<Vec<_>>();
    let mut list_state = ListState::default().with_selected(
        state
            .selected
            .and_then(|selected| state.visible.iter().position(|&index| index == selected)),
    );
    frame.render_stateful_widget(
        List::new(hosts)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!("Hosts /{}", state.filter)),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        columns[0],
        &mut list_state,
    );

    let (right_title, detail_text) = match state.mode {
        Mode::AddForm | Mode::EditForm => {
            let rows = FORM_LABELS
                .iter()
                .enumerate()
                .map(|(index, label)| {
                    format!(
                        "{} {label}: {}",
                        if index == state.form.active { '>' } else { ' ' },
                        state.form.values[index]
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            (
                if state.mode == Mode::AddForm {
                    "Add host"
                } else {
                    "Edit host"
                },
                rows,
            )
        }
        Mode::ConfirmRemove => (
            "Remove host",
            format!(
                "Remove {}?",
                state.selected_alias().unwrap_or("selected host")
            ),
        ),
        _ => (
            "Details",
            details.map_or_else(|| "No host selected".into(), detail_text),
        ),
    };
    frame.render_widget(
        Paragraph::new(detail_text)
            .block(Block::default().borders(Borders::ALL).title(right_title)),
        columns[1],
    );

    let warnings = state
        .warnings
        .iter()
        .map(|warning| format!("Warning: {}", format_warning(warning)))
        .collect::<Vec<_>>()
        .join("\n");
    frame.render_widget(Paragraph::new(warnings), areas[1]);
    frame.render_widget(
        Paragraph::new(state.status.as_deref().unwrap_or_default()),
        areas[2],
    );
    let footer = match state.mode {
        Mode::Browse => match state.selected.map(|index| state.hosts[index].ownership) {
            Some(Ownership::Managed)
                if details.is_some_and(|details| details.declared.is_some()) =>
            {
                "↑↓ move  / filter  a add  e edit  x remove  Enter connect  n new terminal  q quit"
            }
            Some(Ownership::Managed) => {
                "↑↓ move  / filter  a add  x remove  Enter connect  n new terminal  q quit"
            }
            Some(Ownership::External) => {
                "↑↓ move  / filter  a add  Enter connect  n new terminal  q quit"
            }
            None => "/ filter  a add  q quit",
        },
        Mode::Filter => "type to filter  Delete erase  Enter apply  Esc/Backspace cancel",
        Mode::AddForm | Mode::EditForm => {
            "↑↓ field  Delete erase  Enter save  Esc/Backspace cancel"
        }
        Mode::ConfirmRemove => "Enter remove  Esc/Backspace cancel",
    };
    frame.render_widget(Paragraph::new(footer), areas[3]);
}

fn detail_text(details: &HostDetails) -> String {
    let mut text = format!(
        "Ownership: {}\nSource: {}\n",
        ownership(details.summary.ownership),
        details.summary.source.display(),
    );
    if let Some(host) = &details.declared {
        text.push_str(&format!(
            "Portico-declared\nAlias: {}\nHostName: {}\nUser: {}\nPort: {}\nIdentityFile: {}\nProxyJump: {}\n",
            host.alias(),
            host.hostname(),
            host.user().unwrap_or("—"),
            host.port().map_or_else(|| "—".into(), |port| port.to_string()),
            host.identity_file().unwrap_or("—"),
            host.proxy_jump().unwrap_or("—"),
        ));
    } else {
        text.push_str("Portico-declared: —\n");
    }
    match &details.resolution {
        Ok(resolved) => text.push_str(&format!(
            "Resolved\nHostName: {}\nUser: {}\nPort: {}\nIdentityFile: {}\nProxyJump: {}",
            resolved.hostname,
            resolved.user.as_deref().unwrap_or("—"),
            resolved
                .port
                .map_or_else(|| "—".into(), |port| port.to_string()),
            if resolved.identity_files.is_empty() {
                "—".into()
            } else {
                resolved.identity_files.join(", ")
            },
            resolved.proxy_jump.as_deref().unwrap_or("—"),
        )),
        Err(error) => text.push_str(&format!("Resolved\nError: {error}")),
    }
    text
}

fn format_warning(warning: &DiscoveryWarning) -> String {
    match warning {
        DiscoveryWarning::Include { path, message } => {
            format!("{}: {message}", path.display())
        }
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

fn render_init(frame: &mut Frame<'_>, plan: &InitPlan) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(frame.area());
    let changes = [
        (plan.create_entry, "Create ~/.ssh/config"),
        (plan.create_managed, "Create managed config"),
        (plan.insert_include, "Add Portico Include"),
        (plan.reposition_include, "Reposition Portico Include"),
        (plan.backup_entry, "Back up ~/.ssh/config"),
        (plan.fix_portico_permissions, "Fix Portico permissions"),
        (plan.fix_managed_permissions, "Fix managed permissions"),
    ]
    .into_iter()
    .filter_map(|(needed, label)| needed.then_some(label))
    .collect::<Vec<_>>()
    .join("\n");
    frame.render_widget(
        Paragraph::new(changes).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Initialize Portico"),
        ),
        areas[0],
    );
    frame.render_widget(
        Paragraph::new("y initialize  n/q/Esc/Backspace cancel"),
        areas[1],
    );
}

fn ownership(ownership: Ownership) -> &'static str {
    match ownership {
        Ownership::Managed => "managed",
        Ownership::External => "external",
    }
}

fn init_decision(key: KeyEvent) -> Option<bool> {
    match key.code {
        KeyCode::Char('y') => Some(true),
        KeyCode::Char('q') | KeyCode::Esc | KeyCode::Backspace | KeyCode::Char('n') => Some(false),
        _ => None,
    }
}

fn suspend_for_child<T, L, C, E>(mut leave: L, child: C, mut enter: E) -> Result<T, TuiError>
where
    L: FnMut() -> Result<(), TuiError>,
    C: FnOnce() -> Result<T, TuiError>,
    E: FnMut() -> Result<(), TuiError>,
{
    leave()?;
    let child_result = child();
    enter()?;
    child_result
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

    use crate::{
        app::{HostDetails, HostSummary, Ownership},
        discovery::DiscoveryWarning,
        host::{Change, Host},
        managed::InitPlan,
        openssh::ResolvedConfig,
    };

    use super::*;

    fn test_state(hosts: Vec<HostSummary>) -> UiState {
        UiState::new(HostInventory {
            hosts,
            warnings: Vec::new(),
        })
    }

    fn summary(alias: &str, ownership: Ownership) -> HostSummary {
        HostSummary {
            alias: alias.into(),
            ownership,
            source: PathBuf::from("/home/test/.ssh/config"),
        }
    }

    fn details(alias: &str, ownership: Ownership) -> HostDetails {
        HostDetails {
            summary: summary(alias, ownership),
            declared: (ownership == Ownership::Managed).then(|| {
                Host::new(
                    alias,
                    format!("{alias}.example"),
                    Some("deploy".into()),
                    Some(22),
                    Some("~/.ssh/id_ed25519".into()),
                    Some("bastion".into()),
                )
                .unwrap()
            }),
            resolution: Ok(ResolvedConfig {
                hostname: format!("{alias}.example"),
                user: Some("deploy".into()),
                port: Some(22),
                identity_files: vec!["~/.ssh/id_ed25519".into()],
                proxy_jump: None,
            }),
            warnings: Vec::new(),
        }
    }

    fn render_to_buffer(state: &UiState, details: HostDetails) -> Buffer {
        render_optional_to_buffer(state, Some(&details))
    }

    fn render_optional_to_buffer(state: &UiState, details: Option<&HostDetails>) -> Buffer {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, state, details))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn buffer_text(buffer: &Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn render_init_to_buffer(plan: &InitPlan) -> Buffer {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render_init(frame, plan)).unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn renders_master_detail_and_hides_mutation_actions_for_external_host() {
        let hosts = vec![
            summary("dev", Ownership::Managed),
            summary("legacy", Ownership::External),
        ];
        let mut state = test_state(hosts);
        state.select_alias("legacy");
        let buffer = render_to_buffer(&state, details("legacy", Ownership::External));
        let text = buffer_text(&buffer);

        assert!(text.contains("legacy"));
        assert!(text.contains("external"));
        assert!(text.contains("Enter connect"));
        assert!(!text.contains("e edit"));
        assert!(!text.contains("x remove"));
    }

    #[test]
    fn filtering_preserves_a_valid_selection() {
        let mut state = test_state(vec![
            summary("dev", Ownership::Managed),
            summary("prod", Ownership::Managed),
        ]);
        state.set_filter("pro");
        assert_eq!(state.selected_alias(), Some("prod"));
    }

    #[test]
    fn inventory_warnings_remain_visible_without_resolved_details() {
        for external in ["", "Host legacy\n"] {
            let dir = tempfile::tempdir().unwrap();
            let paths = crate::managed::ConfigPaths::from_home(dir.path());
            let store = crate::managed::ManagedStore::new(paths.clone());
            store.apply_init(true, |_, _| Ok(())).unwrap();
            std::fs::write(
                &paths.entry,
                format!("Include ~/.ssh/portico/config\nInclude missing.conf\n{external}"),
            )
            .unwrap();
            let app = App::new(
                store,
                crate::openssh::OpenSsh::new("/usr/bin/false".into(), "/usr/bin/false".into()),
            );
            let mut state = UiState::new(app.hosts().unwrap());

            let details =
                resolve_selected(&app, &mut state, &std::sync::atomic::AtomicBool::new(false));
            if external.is_empty() {
                assert!(details.is_none());
            } else {
                assert!(details.as_ref().unwrap().resolution.is_err());
            }
            let text = buffer_text(&render_optional_to_buffer(&state, details.as_ref()));

            assert!(text.contains("Warning"), "{text}");
            assert!(text.contains("missing.conf"), "{text}");
            assert!(text.contains("q quit"), "{text}");
            if !external.is_empty() {
                assert!(text.contains("exited with"), "{text}");
            }
        }
    }

    #[test]
    fn footer_without_selection_omits_host_actions() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        state.set_filter("missing");

        let text = buffer_text(&render_optional_to_buffer(&state, None));

        assert!(text.contains("a add"));
        assert!(text.contains("q quit"));
        for action in [
            "Enter connect",
            "n new terminal",
            "e edit",
            "x remove",
            "↑↓ move",
        ] {
            assert!(!text.contains(action), "invalid action: {action}\n{text}");
        }
    }

    #[test]
    fn status_preserves_context_valid_footer_actions() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        state.status = Some("Saved dev".into());
        let text = buffer_text(&render_to_buffer(
            &state,
            details("dev", Ownership::Managed),
        ));
        assert!(text.contains("Saved dev"));
        assert!(text.contains("e edit"));
        assert!(text.contains("q quit"));

        state.mode = Mode::AddForm;
        state.status = Some("invalid alias".into());
        let text = buffer_text(&render_optional_to_buffer(&state, None));
        assert!(text.contains("invalid alias"));
        assert!(text.contains("Enter save"));
        assert!(text.contains("Esc/Backspace cancel"));
        assert!(!text.contains("e edit"));
    }

    #[test]
    fn browse_keys_move_connect_and_protect_external_hosts() {
        let mut state = test_state(vec![
            summary("dev", Ownership::Managed),
            summary("legacy", Ownership::External),
        ]);
        state.handle_key(key(KeyCode::Down), None).unwrap();
        assert_eq!(state.selected_alias(), Some("legacy"));

        assert_eq!(
            state.handle_key(key(KeyCode::Enter), None).unwrap(),
            UiAction::Connect {
                alias: "legacy".into(),
                new_terminal: false,
            }
        );
        assert_eq!(
            state.handle_key(key(KeyCode::Char('n')), None).unwrap(),
            UiAction::Connect {
                alias: "legacy".into(),
                new_terminal: true,
            }
        );
        assert_eq!(
            state.handle_key(key(KeyCode::Char('e')), None).unwrap(),
            UiAction::None
        );
        assert_eq!(
            state.handle_key(key(KeyCode::Char('x')), None).unwrap(),
            UiAction::None
        );
        assert_eq!(state.mode, Mode::Browse);
    }

    #[test]
    fn duplicate_external_row_keeps_its_identity_and_cannot_edit_managed_details() {
        let managed = summary("shared", Ownership::Managed);
        let mut external = summary("shared", Ownership::External);
        external.source = PathBuf::from("/home/test/.ssh/external.conf");
        let mut state = test_state(vec![managed.clone(), external.clone()]);
        let cached_for = state.selected_summary().cloned();
        let mut cached_details = details("shared", Ownership::Managed);

        state.handle_key(key(KeyCode::Down), None).unwrap();
        assert!(details_stale(
            cached_for.as_ref(),
            state.selected_summary(),
            false,
        ));
        cached_details = details_for_selected(state.selected_summary().unwrap(), cached_details);

        state
            .handle_key(key(KeyCode::Char('e')), Some(&cached_details))
            .unwrap();
        assert_eq!(state.mode, Mode::Browse);
        assert_eq!(cached_details.summary, external);
        assert!(cached_details.declared.is_none());

        state.replace_hosts(HostInventory {
            hosts: vec![managed, external.clone()],
            warnings: Vec::new(),
        });
        assert_eq!(state.selected_summary(), Some(&external));
    }

    #[test]
    fn managed_remove_requires_confirmation_and_q_quits() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);

        assert_eq!(
            state.handle_key(key(KeyCode::Char('x')), None).unwrap(),
            UiAction::None
        );
        assert_eq!(state.mode, Mode::ConfirmRemove);
        assert_eq!(
            state.handle_key(key(KeyCode::Esc), None).unwrap(),
            UiAction::None
        );
        assert_eq!(state.mode, Mode::Browse);
        assert_eq!(
            state.handle_key(key(KeyCode::Char('q')), None).unwrap(),
            UiAction::Quit
        );
    }

    #[test]
    fn form_renders_exactly_the_six_v1_fields() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        state.handle_key(key(KeyCode::Char('a')), None).unwrap();
        let text = buffer_text(&render_to_buffer(
            &state,
            details("dev", Ownership::Managed),
        ));

        for label in [
            "Alias:",
            "HostName:",
            "User:",
            "Port:",
            "IdentityFile:",
            "ProxyJump:",
        ] {
            assert!(text.contains(label), "missing {label}");
        }
        assert_eq!(text.matches(": ").count(), 6);
    }

    #[test]
    fn details_render_declared_resolved_and_discovery_warnings() {
        let mut host_details = details("dev", Ownership::Managed);
        host_details.declared = Some(
            Host::new(
                "dev",
                "declared.example",
                Some("declared-user".into()),
                Some(2200),
                Some("~/.ssh/id_declared".into()),
                Some("declared-jump".into()),
            )
            .unwrap(),
        );
        host_details.resolution.as_mut().unwrap().hostname = "effective.example".into();
        host_details.warnings = vec![DiscoveryWarning::Include {
            path: PathBuf::from("/home/test/.ssh/missing.conf"),
            message: "no matching files".into(),
        }];
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        state.warnings = host_details.warnings.clone();

        let text = buffer_text(&render_to_buffer(&state, host_details));

        assert!(text.contains("Portico-declared"));
        assert!(text.contains("declared.example"));
        assert!(text.contains("Resolved"));
        assert!(text.contains("effective.example"));
        assert!(text.contains("Warning:"));
        assert!(text.contains("missing.conf: no matching files"));
    }

    #[test]
    fn resolution_failure_keeps_managed_host_editable() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        let mut host_details = details("dev", Ownership::Managed);
        host_details.resolution = Err("ssh -G failed".into());
        let text = buffer_text(&render_to_buffer(&state, host_details.clone()));

        assert!(text.contains("ssh -G failed"));

        state
            .handle_key(key(KeyCode::Char('e')), Some(&host_details))
            .unwrap();

        assert_eq!(state.mode, Mode::EditForm);
        assert_eq!(state.form.values[0], "dev");
    }

    #[test]
    fn add_form_reuses_host_validation() {
        let mut state = test_state(Vec::new());
        state.handle_key(key(KeyCode::Char('a')), None).unwrap();
        state.form.values[0] = "-bad".into();
        state.form.values[1] = "dev.example".into();

        let error = state.handle_key(key(KeyCode::Enter), None).unwrap_err();

        assert!(error.contains("invalid alias"));
        assert_eq!(state.mode, Mode::AddForm);
    }

    #[test]
    fn edit_form_prefills_and_clears_optional_fields() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        let details = details("dev", Ownership::Managed);
        state
            .handle_key(key(KeyCode::Char('e')), Some(&details))
            .unwrap();
        assert_eq!(
            state.form.values,
            [
                "dev",
                "dev.example",
                "deploy",
                "22",
                "~/.ssh/id_ed25519",
                "bastion",
            ]
        );
        for index in 2..6 {
            state.form.values[index].clear();
        }

        let UiAction::Edit { alias, changes } =
            state.handle_key(key(KeyCode::Enter), None).unwrap()
        else {
            panic!("expected edit action")
        };
        assert_eq!(alias, "dev");
        assert_eq!(changes.user, Change::Clear);
        assert_eq!(changes.port, Change::Clear);
        assert_eq!(changes.identity_file, Change::Clear);
        assert_eq!(changes.proxy_jump, Change::Clear);
    }

    #[test]
    fn filter_mode_updates_selection_and_escape_returns_to_browse() {
        let mut state = test_state(vec![
            summary("dev", Ownership::Managed),
            summary("prod", Ownership::Managed),
        ]);
        state.handle_key(key(KeyCode::Char('/')), None).unwrap();
        for character in "pro".chars() {
            state
                .handle_key(key(KeyCode::Char(character)), None)
                .unwrap();
        }
        assert_eq!(state.selected_alias(), Some("prod"));
        state.handle_key(key(KeyCode::Esc), None).unwrap();
        assert_eq!(state.mode, Mode::Browse);
    }

    #[test]
    fn backspace_cancels_while_delete_erases_text() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        state.handle_key(key(KeyCode::Char('/')), None).unwrap();
        state.handle_key(key(KeyCode::Char('d')), None).unwrap();
        state.handle_key(key(KeyCode::Delete), None).unwrap();
        assert_eq!(state.filter, "");
        assert_eq!(state.mode, Mode::Filter);
        state.handle_key(key(KeyCode::Backspace), None).unwrap();
        assert_eq!(state.mode, Mode::Browse);

        state.handle_key(key(KeyCode::Char('a')), None).unwrap();
        state.form.values[0] = "dev".into();
        state.handle_key(key(KeyCode::Delete), None).unwrap();
        assert_eq!(state.form.values[0], "de");
        assert_eq!(state.mode, Mode::AddForm);
        state.handle_key(key(KeyCode::Backspace), None).unwrap();
        assert_eq!(state.mode, Mode::Browse);

        state.handle_key(key(KeyCode::Char('x')), None).unwrap();
        assert_eq!(state.mode, Mode::ConfirmRemove);
        state.handle_key(key(KeyCode::Backspace), None).unwrap();
        assert_eq!(state.mode, Mode::Browse);
        assert_eq!(init_decision(key(KeyCode::Backspace)), Some(false));
    }

    #[test]
    fn refreshing_hosts_retains_the_selected_alias() {
        let mut state = test_state(vec![
            summary("dev", Ownership::Managed),
            summary("prod", Ownership::Managed),
        ]);
        state.select_alias("prod");

        state.replace_hosts(HostInventory {
            hosts: vec![
                summary("new", Ownership::Managed),
                summary("prod", Ownership::Managed),
            ],
            warnings: Vec::new(),
        });

        assert_eq!(state.selected_alias(), Some("prod"));
    }

    #[test]
    fn completed_mutation_exits_form_even_when_refresh_fails() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        let details = details("dev", Ownership::Managed);
        state
            .handle_key(key(KeyCode::Char('e')), Some(&details))
            .unwrap();
        assert_eq!(state.mode, Mode::EditForm);

        let refreshed = complete_mutation(&mut state, Some("dev"), "Saved dev".into(), || {
            Err::<HostInventory, _>("inventory unavailable")
        });

        assert!(!refreshed);
        assert_eq!(state.mode, Mode::Browse);
        assert!(state.form.original.is_none());
        assert_eq!(
            state.status.as_deref(),
            Some("Saved dev, refresh failed: inventory unavailable")
        );
        assert!(matches!(
            state.handle_key(key(KeyCode::Enter), None).unwrap(),
            UiAction::Connect { .. }
        ));
    }

    #[test]
    fn initialization_requires_an_explicit_yes_key() {
        assert_eq!(init_decision(key(KeyCode::Enter)), None);
        assert_eq!(init_decision(key(KeyCode::Char('y'))), Some(true));
        assert_eq!(init_decision(key(KeyCode::Char('q'))), Some(false));
    }

    #[test]
    fn initialization_screen_shows_the_plan_and_confirmation_key() {
        let plan = InitPlan {
            create_entry: true,
            create_managed: true,
            insert_include: true,
            reposition_include: false,
            backup_entry: false,
            fix_portico_permissions: false,
            fix_managed_permissions: false,
        };

        let text = buffer_text(&render_init_to_buffer(&plan));

        assert!(text.contains("Create ~/.ssh/config"));
        assert!(text.contains("Create managed config"));
        assert!(text.contains("Add Portico Include"));
        assert!(text.contains("y initialize"));
    }

    #[test]
    fn restores_terminal_around_child_even_when_child_fails() {
        let events = std::cell::RefCell::new(Vec::new());
        let result: Result<(), TuiError> = suspend_for_child(
            || {
                events.borrow_mut().push("leave");
                Ok(())
            },
            || {
                events.borrow_mut().push("child");
                Err(TuiError::Child("failed".into()))
            },
            || {
                events.borrow_mut().push("enter");
                Ok(())
            },
        );

        assert!(result.is_err());
        assert_eq!(*events.borrow(), ["leave", "child", "enter"]);
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
}
