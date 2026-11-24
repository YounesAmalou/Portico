//! The hosts tab: the list, its filter and keys, the details pane and the trust prompt.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use std::path::Path;

use crate::app::{HostDetails, HostInventory, HostSummary, Ownership, TestOutcome, TestTarget};
use crate::domain::discovery::DiscoveryWarning;
use crate::domain::explain::Explanation;
use crate::domain::host::Host;
use crate::domain::identity::IdentityInventory;
use crate::domain::openssh::{ConnectionOutcome, PresentedKey, ResolvedConfig};
use crate::domain::{adopt, fuzzy};
use crate::picker::HIT;

use super::form::{FormState, FormTestResult, IDENTITY_FILE, form_lines};
use super::tester::{Finished, Origin};
use super::{
    Loaded, Mode, Tab, TextInput, TuiError, UiAction, UiState, footer_text, identities,
    init::ownership, local_time,
};

/// Resolved details of hosts already looked up, so revisiting a host costs nothing.
///
/// Entries stay valid until the inventory changes. Lookup is linear: it is bounded by the
/// number of hosts the user scrolls through.
#[derive(Clone, Debug, Default)]
pub(super) struct DetailsCache {
    entries: Vec<(HostSummary, Loaded)>,
}

impl DetailsCache {
    pub(super) fn get(&self, host: &HostSummary) -> Option<&Loaded> {
        self.entries
            .iter()
            .find(|(cached, _)| cached == host)
            .map(|(_, loaded)| loaded)
    }

    pub(super) fn insert(&mut self, host: HostSummary, loaded: Loaded) {
        match self.entries.iter_mut().find(|(cached, _)| *cached == host) {
            Some(entry) => entry.1 = loaded,
            None => self.entries.push((host, loaded)),
        }
    }

    pub(super) fn clear(&mut self) {
        self.entries.clear();
    }
}

/// The outcome of a host's last connection test.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct TestMark {
    pub(super) passed: bool,
    line: String,
    finished_at: String,
    explanation: Option<Explanation>,
}

/// Something remembered per host, such as test results. Like `DetailsCache`, lookup is linear.
#[derive(Clone, Debug)]
pub(super) struct PerHost<T> {
    entries: Vec<(HostSummary, T)>,
}

impl<T> Default for PerHost<T> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
}

impl<T> PerHost<T> {
    pub(super) fn get(&self, host: &HostSummary) -> Option<&T> {
        self.entries
            .iter()
            .find(|(tested, _)| tested == host)
            .map(|(_, mark)| mark)
    }

    fn insert(&mut self, host: HostSummary, mark: T) {
        self.forget_host(&host);
        self.entries.push((host, mark));
    }

    fn forget_host(&mut self, host: &HostSummary) {
        self.entries.retain(|(tested, _)| tested != host);
    }

    /// Drops every result for `alias`, whichever file defines it.
    pub(super) fn forget(&mut self, alias: &str) {
        self.entries
            .retain(|(tested, _)| !tested.alias.eq_ignore_ascii_case(alias));
    }

    pub(super) fn clear(&mut self) {
        self.entries.clear();
    }
}

/// A host key the server presented that the user is deciding whether to trust.
#[derive(Clone, Debug)]
pub(super) struct PendingTrust {
    pub(super) origin: Origin,
    pub(super) target: TestTarget,
    pub(super) key: PresentedKey,
    /// The mode to return to once the prompt is answered.
    previous: Mode,
}

/// One host field and every value OpenSSH may use for it (IdentityFile can repeat).
struct DetailRow {
    label: &'static str,
    values: Vec<String>,
}

impl DetailRow {
    fn new(label: &'static str, values: impl IntoIterator<Item = String>) -> Self {
        let mut unique: Vec<String> = Vec::new();
        for value in values {
            if !unique.contains(&value) {
                unique.push(value);
            }
        }
        Self {
            label,
            values: unique,
        }
    }
}

impl UiState {
    pub(super) fn set_filter(&mut self, filter: &str) {
        self.filter = TextInput::new(filter);
        self.refilter();
    }

    /// Recomputes the visible hosts from the filter, keeping the selection when it still matches.
    fn refilter(&mut self) {
        let filter = self.filter.as_str();
        self.visible = self
            .hosts
            .iter()
            .enumerate()
            .filter(|(_, host)| fuzzy::match_fields(filter, &[&host.alias]).is_some())
            .map(|(index, _)| index)
            .collect();
        if !self
            .selected
            .is_some_and(|selected| self.visible.contains(&selected))
        {
            self.selected = self.visible.first().copied();
        }
    }

    pub(super) fn select_alias(&mut self, alias: &str) {
        if let Some(index) = self
            .hosts
            .iter()
            .position(|host| host.alias.eq_ignore_ascii_case(alias))
            .filter(|index| self.visible.contains(index))
        {
            self.selected = Some(index);
        }
    }

    pub(super) fn selected_alias(&self) -> Option<&str> {
        self.selected.map(|index| self.hosts[index].alias.as_str())
    }

    pub(super) fn selected_summary(&self) -> Option<&HostSummary> {
        self.selected.map(|index| &self.hosts[index])
    }

    pub(super) fn selected_details(&self) -> Option<&HostDetails> {
        self.details.get(self.selected_summary()?)?.as_ref().ok()
    }

    pub(super) fn replace_hosts(&mut self, inventory: HostInventory) {
        let selected = self.selected_summary().cloned();
        self.show_raw = false;
        self.hosts = inventory.hosts;
        self.warnings = inventory.warnings;
        self.inputs = inventory.inputs;
        self.selected = None;
        self.refilter();
        // The same row if it survived, otherwise the host of that name wherever it is now defined.
        if let Some(selected) = selected
            && let Some(index) = self
                .hosts
                .iter()
                .position(|host| host == &selected)
                .or_else(|| {
                    self.hosts
                        .iter()
                        .position(|host| host.alias.eq_ignore_ascii_case(&selected.alias))
                })
                .filter(|index| self.visible.contains(index))
        {
            self.selected = Some(index);
        }
        if self.form.suggestions.is_some() {
            self.suggest(false);
        }
    }

    pub(super) fn handle_hosts_key(&mut self, key: KeyEvent) -> UiAction {
        self.status = None;
        if key.code == KeyCode::Esc && self.testing.is_some() {
            self.status = Some("Test cancelled".into());
            return UiAction::CancelTest;
        }
        let (selected, in_form) = (self.selected, self.in_form());
        let action = match self.mode {
            Mode::Browse => self.handle_browse_key(key),
            Mode::Filter => self.handle_filter_key(key),
            Mode::AddForm | Mode::EditForm => self.handle_form_key(key),
            Mode::ConfirmRemove => self.handle_remove_key(key),
            Mode::ConfirmTrust => self.handle_trust_key(key),
            Mode::Migrate => self.handle_migrate_key(key),
            // The overlay takes its keys before the tab sees them.
            Mode::Settings => UiAction::None,
        };
        // Unfolded output belongs to one selection, or to one open form.
        if (selected, in_form) != (self.selected, self.in_form()) {
            self.show_raw = false;
        }
        action
    }

    pub(super) fn handle_paste(&mut self, text: &str) -> UiAction {
        if self.mode == Mode::Settings {
            return UiAction::None;
        }
        match self.tab {
            Tab::Hosts => {}
            Tab::Tunnels => {
                self.tunnels.paste(text);
                return UiAction::None;
            }
            Tab::Containers(backend) => {
                self.containers.panel_mut(backend).paste(text);
                return UiAction::None;
            }
        }
        self.status = None;
        match self.mode {
            Mode::Filter => {
                self.filter.paste(text);
                self.refilter();
            }
            Mode::AddForm | Mode::EditForm => {
                let input = &mut self.form.values[self.form.active];
                let (before, cursor) = (input.as_str().to_owned(), input.cursor);
                input.paste(text);
                return self.form_input_changed(&before, cursor);
            }
            Mode::Migrate => {
                if let Some(migrate) = &mut self.migrate {
                    migrate.paste(text);
                }
            }
            Mode::Browse | Mode::ConfirmRemove | Mode::ConfirmTrust | Mode::Settings => {}
        }
        UiAction::None
    }

    fn handle_browse_key(&mut self, key: KeyEvent) -> UiAction {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Char('/') => {
                self.filter_before_edit = self.filter.as_str().into();
                self.filter.cursor = self.filter.as_str().len();
                self.mode = Mode::Filter;
            }
            KeyCode::Esc => self.set_filter(""),
            KeyCode::Char('a') => {
                self.form = FormState::default();
                self.mode = Mode::AddForm;
            }
            KeyCode::Char('e') => self.begin_edit(),
            KeyCode::Char('m') => {
                if let Some(host) = self.selected_summary() {
                    return UiAction::OpenMigrate {
                        direction: match host.ownership {
                            Ownership::External => adopt::Direction::Adopt,
                            Ownership::Managed => adopt::Direction::Export,
                        },
                        preselect: host.alias.clone(),
                    };
                }
            }
            KeyCode::Char('x') => {
                if self.selected_is_managed() {
                    self.mode = Mode::ConfirmRemove;
                }
            }
            KeyCode::Enter if key.modifiers.is_empty() => return self.connect_action(false),
            KeyCode::Enter if key.modifiers == KeyModifiers::CONTROL => {
                return self.connect_action(true);
            }
            KeyCode::Char('n') => return self.connect_action(true),
            KeyCode::Char('r') | KeyCode::F(5) => return UiAction::Refresh,
            KeyCode::Char('t') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                return self.test_selected();
            }
            KeyCode::Char('o') if key.modifiers.is_empty() && self.selected_has_foldable_raw() => {
                self.show_raw = !self.show_raw;
            }
            KeyCode::Char('q') => return UiAction::Quit,
            _ => {}
        }
        UiAction::None
    }

    /// Opens the edit form for the selected managed host once its declaration is known.
    fn begin_edit(&mut self) {
        if !self.selected_is_managed() {
            return;
        }
        match self
            .selected_summary()
            .and_then(|host| self.details.get(host))
        {
            // The host is still being looked up: finish the request when the result arrives.
            None => self.edit_pending = true,
            Some(Ok(HostDetails {
                declared: Some(host),
                ..
            })) => {
                self.form = FormState::from_host(host);
                self.mode = Mode::EditForm;
            }
            Some(_) => {}
        }
    }

    /// Opens the edit form `e` asked for, unless the Hosts tab has been left since.
    pub(super) fn open_pending_edit(&mut self) {
        if std::mem::take(&mut self.edit_pending) && self.tab == Tab::Hosts {
            self.begin_edit();
        }
    }

    fn handle_filter_key(&mut self, key: KeyEvent) -> UiAction {
        match key.code {
            KeyCode::Esc => {
                let before = std::mem::take(&mut self.filter_before_edit);
                self.set_filter(&before);
                self.mode = Mode::Browse;
            }
            KeyCode::Enter if key.modifiers.is_empty() => self.mode = Mode::Browse,
            _ => {
                if self.filter.handle_key(key) {
                    self.refilter();
                }
            }
        }
        UiAction::None
    }

    fn test_selected(&self) -> UiAction {
        self.selected_summary()
            .map_or(UiAction::None, |host| UiAction::Test {
                origin: Origin::Host(host.clone()),
                target: TestTarget::Saved(host.alias.clone()),
            })
    }

    /// Whether the selected host has an explanation whose ssh output `o` can unfold.
    pub(super) fn selected_has_foldable_raw(&self) -> bool {
        self.selected_summary().is_some_and(|host| {
            self.results
                .get(host)
                .and_then(|mark| mark.explanation.as_ref())
                .into_iter()
                .chain(self.connections.get(host))
                .any(has_foldable_raw)
        })
    }

    /// Shows how a connection attempt ended, and remembers a failure for the details pane.
    pub(super) fn finish_connect(
        &mut self,
        alias: &str,
        result: Result<ConnectionOutcome, TuiError>,
    ) {
        self.status = Some(match result {
            Ok(ConnectionOutcome::LaunchRequested) => format!("Launch requested for {alias}"),
            Ok(ConnectionOutcome::LaunchFailed(status)) => {
                format!("terminal launcher exited with {status}")
            }
            Ok(ConnectionOutcome::Exited {
                alias: spelled,
                status,
                explanation,
            }) => {
                let host = self
                    .selected_summary()
                    .filter(|host| host.alias.eq_ignore_ascii_case(&spelled))
                    .or_else(|| {
                        self.hosts
                            .iter()
                            .find(|host| host.alias.eq_ignore_ascii_case(&spelled))
                    })
                    .cloned();
                match explanation {
                    Some(explanation) => {
                        let line = format!("{alias}: failed \u{2014} {}", explanation.headline);
                        if let Some(host) = host {
                            self.connections.insert(host, explanation);
                        }
                        line
                    }
                    None => {
                        // A connection that got through supersedes an earlier failure.
                        if let Some(host) = host {
                            self.connections.forget_host(&host);
                        }
                        if status.success() {
                            format!("Disconnected from {alias}")
                        } else {
                            format!("ssh exited with {status}")
                        }
                    }
                }
            }
            Err(error) => error.to_string(),
        });
    }

    /// Shows a finished test: a saved host's result marks it in the list and the details pane
    /// and goes to the status line; a form's result goes to the form, if it is still open.
    pub(super) fn record_test(&mut self, finished: Finished, finished_at: String) {
        let Finished {
            origin,
            target,
            outcome,
        } = finished;
        let (passed, line, explanation, status_hint) = match &outcome {
            Ok(TestOutcome::UnknownHostKey { key, .. }) if self.can_prompt(&origin) => {
                self.trust = Some(PendingTrust {
                    origin,
                    target,
                    key: key.clone(),
                    previous: self.mode.clone(),
                });
                self.mode = Mode::ConfirmTrust;
                return;
            }
            // The user has moved on; a prompt now would take the keys they are typing.
            Ok(TestOutcome::UnknownHostKey { report, .. }) => (
                false,
                report.summary(),
                report.explanation.clone(),
                Some("press t to review the host key"),
            ),
            Ok(TestOutcome::Done(report)) => (
                report.passed(),
                report.summary(),
                report.explanation.clone(),
                None,
            ),
            Err(error) => (false, format!("failed \u{2014} {error}"), None, None),
        };
        let reason = match &explanation {
            Some(explanation) => format!("failed \u{2014} {}", explanation.headline),
            None => line.clone(),
        };
        let status = match (&origin, status_hint) {
            (Origin::Host(_), Some(hint)) => format!("{}: {reason} ({hint})", target.alias()),
            (Origin::Host(_), None) => format!("{}: {reason}", target.alias()),
            (Origin::Form, _) => String::new(),
        };
        self.show_result(origin, passed, line, explanation, finished_at);
        if !status.is_empty() {
            self.status = Some(status);
        }
    }

    /// Whether the user is still where a test from `origin` started, so its trust prompt may
    /// open: the host list for a saved host, the same open form for a form. Leaving a form
    /// cancels its test, so a form result that arrives finds its own form.
    fn can_prompt(&self, origin: &Origin) -> bool {
        match origin {
            Origin::Host(_) => self.tab == Tab::Hosts && self.mode == Mode::Browse,
            Origin::Form => self.in_form(),
        }
    }

    /// Records a result: a saved host's marks it in the list and the details pane; a form's goes
    /// to the form with its `explanation`, if the form is still open.
    fn show_result(
        &mut self,
        origin: Origin,
        passed: bool,
        line: String,
        explanation: Option<Explanation>,
        finished_at: String,
    ) {
        match origin {
            Origin::Host(host) => {
                // Output unfolded for the old result is not a request to see the new one's.
                if self.selected_summary() == Some(&host) {
                    self.show_raw = false;
                }
                self.results.insert(
                    host,
                    TestMark {
                        passed,
                        line,
                        finished_at,
                        explanation,
                    },
                );
            }
            Origin::Form => {
                if self.in_form() {
                    self.show_raw = false;
                    self.form.test_result = Some(FormTestResult { line, explanation });
                }
            }
        }
    }

    fn handle_trust_key(&mut self, key: KeyEvent) -> UiAction {
        let trusted = match key.code {
            KeyCode::Char('y') => true,
            KeyCode::Char('n') | KeyCode::Esc => false,
            _ => return UiAction::None,
        };
        let Some(pending) = &self.trust else {
            self.mode = Mode::Browse;
            return UiAction::None;
        };
        self.mode = pending.previous.clone();
        if !trusted && let Some(pending) = self.trust.take() {
            self.show_result(
                pending.origin,
                false,
                "failed \u{2014} host key not trusted".into(),
                None,
                local_time(),
            );
            self.status = Some("Host key not trusted".into());
        }
        UiAction::Trust(trusted)
    }

    fn handle_remove_key(&mut self, key: KeyEvent) -> UiAction {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Browse;
                UiAction::None
            }
            KeyCode::Enter if key.modifiers.is_empty() => self
                .selected_alias()
                .map_or(UiAction::None, |alias| UiAction::Remove(alias.into())),
            _ => UiAction::None,
        }
    }

    /// Keeps the built identity inventory and, while IdentityFile has the focus, recomputes its
    /// dropdown: what was typed, or an earlier Ctrl+Space, can now find candidates. A failed
    /// build leaves the suggestions to the key files in `~/.ssh`.
    pub(super) fn finish_identities(&mut self, built: Result<IdentityInventory, String>) {
        match built {
            Ok(inventory) => self.identity_inventory = Some(inventory),
            Err(error) => self.status = Some(format!("Could not list identities: {error}")),
        }
        if self.in_form() && self.form.active == IDENTITY_FILE {
            let pending = std::mem::take(&mut self.form.identity_pending);
            self.suggest(pending);
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

/// Re-labels details looked up by alias as the selected row: an external row shares its alias
/// with a managed host it shadows, but must neither show nor edit that host's declaration.
pub(super) fn details_for_selected(
    selected: &HostSummary,
    mut details: HostDetails,
) -> HostDetails {
    details.summary = selected.clone();
    if selected.ownership == Ownership::External {
        details.declared = None;
    }
    details
}

/// The Hosts tab: the hosts on the left, the details, a form or a prompt on the right.
pub(super) fn render_hosts(frame: &mut Frame<'_>, area: Rect, state: &UiState) {
    let footer = footer_text(state);
    let status = state.status_line();
    let footer_height = (footer.chars().count() as u16)
        .div_ceil(area.width.max(1))
        .max(1);
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(state.warnings.len().min(usize::from(area.height / 3)) as u16),
            Constraint::Length(u16::from(status.is_some())),
            Constraint::Length(footer_height),
        ])
        .split(area);
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(areas[0]);

    let hosts = state
        .visible
        .iter()
        .map(|&index| {
            let host = &state.hosts[index];
            let mark = match state.results.get(host) {
                Some(mark) if mark.passed => "\u{2713} ",
                Some(_) => "\u{2717} ",
                None => "  ",
            };
            let positions = fuzzy::match_fields(state.filter.as_str(), &[&host.alias])
                .and_then(|found| found.positions.into_iter().next())
                .unwrap_or_default();
            let mut spans = vec![Span::raw(mark)];
            spans.extend(fuzzy::highlight(
                &host.alias,
                &positions,
                Style::default(),
                HIT,
            ));
            spans.push(Span::raw(format!("  {}", ownership(host.ownership))));
            ListItem::new(Line::from(spans))
        })
        .collect::<Vec<_>>();
    let mut list_state = ListState::default().with_selected(
        state
            .selected
            .and_then(|selected| state.visible.iter().position(|&index| index == selected)),
    );
    let mut title = vec![Span::raw("Hosts /")];
    title.extend(state.filter.spans(state.mode == Mode::Filter));
    frame.render_stateful_widget(
        List::new(hosts)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(Line::from(title)),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        columns[0],
        &mut list_state,
    );
    let (right_title, detail_lines) = match state.mode {
        Mode::AddForm | Mode::EditForm => (
            Line::from(if state.mode == Mode::AddForm {
                "Add host"
            } else {
                "Edit host"
            }),
            form_lines(
                &state.form,
                state.show_raw,
                &state.home,
                usize::from(columns[1].width.saturating_sub(2)),
                usize::from(columns[1].height.saturating_sub(2)),
            ),
        ),
        Mode::ConfirmRemove => (
            Line::from("Remove host"),
            vec![Line::raw(format!(
                "Remove {}?",
                state.selected_alias().unwrap_or("selected host")
            ))],
        ),
        Mode::ConfirmTrust => (
            Line::from("Trust host key"),
            trust_lines(state.trust.as_ref()),
        ),
        Mode::Migrate => state.migrate.as_ref().map_or(
            (
                Line::from("Migrate"),
                vec![Line::raw("Checking each host with OpenSSH…")],
            ),
            |migrate| {
                (
                    migrate.title(),
                    migrate.lines(
                        usize::from(columns[1].width.saturating_sub(2)),
                        usize::from(columns[1].height.saturating_sub(2)),
                    ),
                )
            },
        ),
        Mode::Browse | Mode::Filter | Mode::Settings => {
            let text = details_text(state);
            (
                Line::from("Details"),
                text.lines()
                    .map(|line| Line::raw(line.to_owned()))
                    .collect(),
            )
        }
    };
    frame.render_widget(
        Paragraph::new(detail_lines)
            .wrap(Wrap { trim: false })
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
    frame.render_widget(Paragraph::new(status.unwrap_or_default()), areas[2]);
    frame.render_widget(Paragraph::new(footer).wrap(Wrap { trim: true }), areas[3]);
}

fn trust_lines(pending: Option<&PendingTrust>) -> Vec<Line<'static>> {
    let Some(pending) = pending else {
        return Vec::new();
    };
    let alias = pending.target.alias();
    let mut lines = pending
        .key
        .fingerprints
        .iter()
        .map(|fingerprint| Line::raw(fingerprint.describe(alias)))
        .collect::<Vec<_>>();
    lines.push(Line::raw(
        "Compare it with the fingerprint from the server's administrator or console.",
    ));
    lines.push(Line::raw(""));
    lines.push(Line::raw("Trust it and test again? y/N"));
    lines
}

pub(super) fn details_text(state: &UiState) -> String {
    let Some(selected) = state.selected_summary() else {
        return "No host selected".into();
    };
    let text = match state.details.get(selected) {
        Some(Ok(details)) => detail_text(details, &state.home),
        Some(Err(error)) => format!("Could not load details: {error}"),
        None => "Resolving…".into(),
    };
    let mut text = text.trim_end().to_owned();
    if let Some(mark) = state.results.get(selected) {
        match &mark.explanation {
            Some(explanation) => {
                text.push_str(&format!(
                    "\n\nConnection test: failed \u{2014} {} at {}",
                    explanation.headline, mark.finished_at
                ));
                for line in explanation_lines(explanation, state.show_raw) {
                    text.push('\n');
                    text.push_str(&line);
                }
            }
            None => text.push_str(&format!(
                "\n\nConnection test: {} at {}",
                mark.line, mark.finished_at
            )),
        }
    }
    if let Some(explanation) = state.connections.get(selected) {
        text.push_str(&format!(
            "\n\nLast connection: failed \u{2014} {}",
            explanation.headline
        ));
        for line in explanation_lines(explanation, state.show_raw) {
            text.push('\n');
            text.push_str(&line);
        }
    }
    text
}

/// The explanation's lines below its first, which the caller words itself.
pub(super) fn explanation_lines(
    explanation: &Explanation,
    show_raw: bool,
) -> impl Iterator<Item = String> {
    explanation.render("", show_raw).into_iter().skip(1)
}

/// Whether the explanation has ssh output that is folded away unless asked for. Output that
/// `always_raw` shows anyway (the facts alone do not explain the failure) has nothing to unfold.
pub(super) fn has_foldable_raw(explanation: &Explanation) -> bool {
    !explanation.raw.is_empty() && !explanation.always_raw
}

fn resolved_rows(resolved: &ResolvedConfig) -> [DetailRow; 5] {
    [
        DetailRow::new("HostName", [resolved.hostname.clone()]),
        DetailRow::new("User", resolved.user.clone()),
        DetailRow::new("Port", resolved.port.map(|port| port.to_string())),
        DetailRow::new("IdentityFile", resolved.identity_files.iter().cloned()),
        DetailRow::new("ProxyJump", resolved.proxy_jump.clone()),
    ]
}

fn declared_rows(host: &Host) -> [DetailRow; 5] {
    [
        DetailRow::new("HostName", [host.hostname().to_owned()]),
        DetailRow::new("User", host.user().map(String::from)),
        DetailRow::new("Port", host.port().map(|port| port.to_string())),
        DetailRow::new("IdentityFile", host.identity_file().map(String::from)),
        DetailRow::new("ProxyJump", host.proxy_jump().map(String::from)),
    ]
}

/// Describes a host with each field shown once, as OpenSSH resolves it. Portico's own
/// declaration only appears where it does not take effect, or while resolution is unavailable.
fn detail_text(details: &HostDetails, home: &Path) -> String {
    let mut text = format!(
        "Ownership: {}\nSource: {}\n\n",
        ownership(details.summary.ownership),
        details.summary.source.display(),
    );
    let declared = details.declared.as_ref().map(declared_rows);
    match &details.resolution {
        Ok(resolved) => {
            let rows = resolved_rows(resolved);
            // The identities section shows the IdentityFile paths with their availability.
            let shown = rows
                .iter()
                .filter(|row| details.identities.is_none() || row.label != "IdentityFile");
            for row in shown {
                push_rows(&mut text, std::slice::from_ref(row));
            }
            if let Some(identities) = &details.identities {
                text.push('\n');
                text.push_str(&identities::detail_lines(identities, home));
            }
            let overridden = declared
                .iter()
                .flatten()
                .zip(&rows)
                .filter(|(declared, effective)| !declared_in_effect(declared, effective, home))
                .map(|(declared, _)| format!("  {}: {}\n", declared.label, declared.values[0]))
                .collect::<String>();
            if !overridden.is_empty() {
                text.push_str(&format!(
                    "\nDeclared by Portico but overridden by OpenSSH:\n{overridden}"
                ));
            }
        }
        Err(error) => {
            if let Some(rows) = declared {
                push_rows(&mut text, &rows);
                text.push('\n');
            }
            text.push_str(&format!("Could not resolve with ssh -G: {error}"));
        }
    }
    text
}

fn push_rows(text: &mut String, rows: &[DetailRow]) {
    for row in rows {
        let mut values = row.values.iter();
        text.push_str(&format!(
            "{:<13}{}\n",
            row.label,
            values.next().map_or("—", String::as_str)
        ));
        for value in values {
            text.push_str(&format!("{:<13}{value}\n", ""));
        }
    }
}

/// Whether the value Portico declared is among those OpenSSH uses. An undeclared field never
/// conflicts, and `ssh -G` reports `~` paths already expanded against `home`.
fn declared_in_effect(declared: &DetailRow, effective: &DetailRow, home: &Path) -> bool {
    declared.values.iter().all(|value| {
        effective
            .values
            .iter()
            .any(|used| used == value || *used == crate::platform::paths::expand(value, home))
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::init::init_decision;
    use crate::tui::{load_details, render};

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Modifier;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicBool;

    use crate::app::{App, HostInventory, Ownership, TestOutcome, TestTarget};
    use crate::domain::discovery::DiscoveryWarning;
    use crate::domain::host::{Change, Host};
    use crate::domain::probe::ProbeResult;
    use crate::tui::tester::{Finished, Origin};
    use crate::tui::tests::{
        authenticated, buffer_text, ctrl, details, explained, fill_form, host_finished, key, press,
        render_to_buffer, rendered, report, resolve, summary, test_state, type_text, unknown_key,
    };
    use crate::tui::{Mode, TextInput, UiAction, UiState, footer_text};

    #[test]
    fn renders_master_detail_and_hides_mutation_actions_for_external_host() {
        let hosts = vec![
            summary("dev", Ownership::Managed),
            summary("legacy", Ownership::External),
        ];
        let mut state = test_state(hosts);
        state.select_alias("legacy");
        resolve(&mut state, details("legacy", Ownership::External));
        let text = rendered(&state);

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
    fn unresolved_selection_says_so_and_keeps_navigation_actions() {
        let state = test_state(vec![summary("dev", Ownership::Managed)]);

        let text = rendered(&state);

        assert!(text.contains("Resolving"), "{text}");
        assert!(text.contains("Enter connect"), "{text}");
        assert!(!text.contains("e edit"), "{text}");
    }

    #[test]
    fn inventory_warnings_remain_visible_without_resolved_details() {
        for external in ["", "Host legacy\n"] {
            let dir = tempfile::tempdir().unwrap();
            let paths = crate::domain::managed::ConfigPaths::from_home(dir.path());
            let store = crate::domain::managed::ManagedStore::new(paths.clone());
            store.apply_init(true, |_, _| Ok(())).unwrap();
            std::fs::write(
                &paths.entry,
                format!("Include ~/.ssh/portico/config\nInclude missing.conf\n{external}"),
            )
            .unwrap();
            let app = App::new(
                store,
                crate::domain::openssh::OpenSsh::new(
                    "/usr/bin/false".into(),
                    "/usr/bin/false".into(),
                ),
            );
            let mut state = UiState::new(app.hosts().unwrap(), false);

            if let Some(host) = state.selected_summary().cloned() {
                let loaded = load_details(&app, &host, &AtomicBool::new(false));
                assert!(loaded.as_ref().unwrap().resolution.is_err());
                state.details.insert(host, loaded);
            }
            let text = rendered(&state);

            assert!(text.contains("Warning"), "{text}");
            assert!(text.contains("missing.conf"), "{text}");
            assert!(text.contains("q quit"), "{text}");
            if !external.is_empty() {
                assert!(text.contains("exited with"), "{text}");
            }
        }
    }

    fn visible_aliases(state: &UiState) -> Vec<String> {
        state
            .visible
            .iter()
            .map(|&index| state.hosts[index].alias.clone())
            .collect()
    }

    #[test]
    fn hosts_filter_is_fuzzy_and_keeps_order() {
        let mut state = test_state(vec![
            summary("web-prod", Ownership::Managed),
            summary("db-prod", Ownership::Managed),
            summary("prod-db", Ownership::Managed),
        ]);
        let order = visible_aliases(&state);
        state.set_filter("pdb");
        assert_eq!(visible_aliases(&state), ["prod-db"]);
        state.set_filter("PROD");
        assert_eq!(visible_aliases(&state), order);
    }

    #[test]
    fn hosts_filter_highlights_matches() {
        let mut state = test_state(vec![
            summary("dev", Ownership::Managed),
            summary("prod-db", Ownership::Managed),
        ]);
        state.set_filter("pd");
        let buffer = render_to_buffer(&state);
        let (x, y) = (0..buffer.area.height)
            .find_map(|y| {
                let line: Vec<&str> = (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect();
                line.windows(7)
                    .position(|cells| cells.concat() == "prod-db")
                    .map(|x| (x as u16, y))
            })
            .unwrap();
        let bold = |offset: u16| buffer[(x + offset, y)].modifier.contains(Modifier::BOLD);
        assert!(bold(0) && bold(5), "p and the d after the dash");
        assert!(!bold(1) && !bold(3));
    }

    #[test]
    fn browse_keys_move_connect_and_protect_external_hosts() {
        let mut state = test_state(vec![
            summary("dev", Ownership::Managed),
            summary("legacy", Ownership::External),
        ]);
        press(&mut state, KeyCode::Down);
        assert_eq!(state.selected_alias(), Some("legacy"));

        assert_eq!(
            press(&mut state, KeyCode::Enter),
            UiAction::Connect {
                alias: "legacy".into(),
                new_terminal: false,
            }
        );
        assert_eq!(
            press(&mut state, KeyCode::Char('n')),
            UiAction::Connect {
                alias: "legacy".into(),
                new_terminal: true,
            }
        );
        assert_eq!(press(&mut state, KeyCode::Char('e')), UiAction::None);
        assert_eq!(press(&mut state, KeyCode::Char('x')), UiAction::None);
        assert_eq!(state.mode, Mode::Browse);
    }

    #[test]
    fn ctrl_enter_opens_a_new_terminal_like_n() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        let ctrl_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL);

        assert_eq!(
            state.handle_key(ctrl_enter),
            UiAction::Connect {
                alias: "dev".into(),
                new_terminal: true,
            }
        );
        assert_eq!(
            state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)),
            UiAction::None
        );
    }

    #[test]
    fn ctrl_enter_does_nothing_in_forms_filter_and_confirmation() {
        let ctrl_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL);
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        resolve(&mut state, details("dev", Ownership::Managed));

        press(&mut state, KeyCode::Char('/'));
        assert_eq!(state.handle_key(ctrl_enter), UiAction::None);
        assert_eq!(state.mode, Mode::Filter);
        press(&mut state, KeyCode::Enter);

        press(&mut state, KeyCode::Char('a'));
        fill_form(&mut state, ["new", "new.example", "", "", "", ""]);
        assert_eq!(state.handle_key(ctrl_enter), UiAction::None);
        assert_eq!(state.mode, Mode::AddForm);
        assert!(matches!(
            press(&mut state, KeyCode::Enter),
            UiAction::Add(_)
        ));
        press(&mut state, KeyCode::Esc);

        press(&mut state, KeyCode::Char('x'));
        assert_eq!(state.handle_key(ctrl_enter), UiAction::None);
        assert_eq!(state.mode, Mode::ConfirmRemove);
        assert_eq!(
            press(&mut state, KeyCode::Enter),
            UiAction::Remove("dev".into())
        );
    }

    #[test]
    fn duplicate_external_row_keeps_its_identity_and_cannot_edit_managed_details() {
        let managed = summary("shared", Ownership::Managed);
        let mut external = summary("shared", Ownership::External);
        external.source = PathBuf::from("/home/test/.ssh/external.conf");
        let mut state = test_state(vec![managed.clone(), external.clone()]);
        resolve(&mut state, details("shared", Ownership::Managed));
        assert!(state.selected_details().is_some());

        press(&mut state, KeyCode::Down);
        assert!(
            state.selected_details().is_none(),
            "details belong to the row they were resolved for"
        );
        let relabelled = details_for_selected(
            state.selected_summary().unwrap(),
            details("shared", Ownership::Managed),
        );
        assert_eq!(relabelled.summary, external);
        assert!(relabelled.declared.is_none());
        resolve(&mut state, relabelled);

        press(&mut state, KeyCode::Char('e'));
        assert_eq!(state.mode, Mode::Browse);

        state.replace_hosts(HostInventory {
            hosts: vec![managed, external.clone()],
            warnings: Vec::new(),
            inputs: Vec::new(),
        });
        assert_eq!(state.selected_summary(), Some(&external));
    }

    #[test]
    fn managed_remove_requires_confirmation_and_q_quits() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);

        assert_eq!(press(&mut state, KeyCode::Char('x')), UiAction::None);
        assert_eq!(state.mode, Mode::ConfirmRemove);
        assert_eq!(press(&mut state, KeyCode::Esc), UiAction::None);
        assert_eq!(state.mode, Mode::Browse);
        assert_eq!(press(&mut state, KeyCode::Char('q')), UiAction::Quit);
    }

    #[test]
    fn details_render_each_field_once_with_the_effective_value() {
        let mut host_details = details("dev", Ownership::Managed);
        host_details.resolution.as_mut().unwrap().identity_files = vec![
            "~/.ssh/id_ed25519".into(),
            "~/.ssh/id_rsa".into(),
            "~/.ssh/id_ed25519".into(),
        ];
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        resolve(&mut state, host_details);

        let text = rendered(&state);

        for label in ["HostName", "User", "Port", "IdentityFile", "ProxyJump"] {
            assert_eq!(text.matches(label).count(), 1, "{label}\n{text}");
        }
        assert_eq!(text.matches("id_ed25519").count(), 1, "{text}");
        assert!(text.contains("id_rsa"), "{text}");
        assert!(!text.contains("overridden"), "{text}");
    }

    #[test]
    fn details_flag_declared_values_that_openssh_overrides() {
        let mut host_details = details("dev", Ownership::Managed);
        host_details.resolution.as_mut().unwrap().hostname = "effective.example".into();
        host_details.warnings = vec![DiscoveryWarning::Include {
            path: PathBuf::from("/home/test/.ssh/missing.conf"),
            message: "no matching files".into(),
        }];
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        state.warnings = host_details.warnings.clone();
        resolve(&mut state, host_details);

        let text = rendered(&state);

        assert!(text.contains("effective.example"));
        assert!(text.contains("overridden by OpenSSH"), "{text}");
        assert!(text.contains("HostName: dev.example"), "{text}");
        assert!(text.contains("Warning:"));
        assert!(text.contains("missing.conf: no matching files"));
    }

    #[test]
    fn declared_home_relative_paths_match_the_expanded_effective_path() {
        let declared = DetailRow::new("IdentityFile", ["~/.ssh/id".to_owned()]);
        let home = Path::new("/home/test");
        let effective = DetailRow::new("IdentityFile", ["/home/test/.ssh/id".to_owned()]);

        assert!(declared_in_effect(&declared, &effective, home));
        assert!(declared_in_effect(
            &DetailRow::new("IdentityFile", []),
            &effective,
            home
        ));
        assert!(!declared_in_effect(
            &DetailRow::new("IdentityFile", ["~/.ssh/other".to_owned()]),
            &effective,
            home
        ));
    }

    #[test]
    fn resolution_failure_keeps_managed_host_editable_and_shows_declared_values() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        let mut host_details = details("dev", Ownership::Managed);
        host_details.resolution = Err("ssh -G failed".into());
        resolve(&mut state, host_details);

        let text = rendered(&state);
        assert!(text.contains("ssh -G failed"));
        assert!(text.contains("dev.example"));

        press(&mut state, KeyCode::Char('e'));

        assert_eq!(state.mode, Mode::EditForm);
        assert_eq!(state.form.values[0].as_str(), "dev");
    }

    #[test]
    fn edit_requested_before_details_load_opens_when_they_arrive() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);

        press(&mut state, KeyCode::Char('e'));
        assert_eq!(state.mode, Mode::Browse);
        resolve(&mut state, details("dev", Ownership::Managed));
        state.open_pending_edit();

        assert_eq!(state.mode, Mode::EditForm);
        assert_eq!(state.form.values[0].as_str(), "dev");
    }

    #[test]
    fn pending_edit_is_dropped_by_any_other_key() {
        let mut state = test_state(vec![
            summary("dev", Ownership::Managed),
            summary("prod", Ownership::Managed),
        ]);

        press(&mut state, KeyCode::Char('e'));
        press(&mut state, KeyCode::Down);
        resolve(&mut state, details("prod", Ownership::Managed));
        state.open_pending_edit();

        assert_eq!(state.mode, Mode::Browse);
    }

    #[test]
    fn edit_form_prefills_and_clears_optional_fields() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        resolve(&mut state, details("dev", Ownership::Managed));
        press(&mut state, KeyCode::Char('e'));
        assert_eq!(
            state.form.values.each_ref().map(TextInput::as_str),
            [
                "dev",
                "dev.example",
                "deploy",
                "22",
                "~/.ssh/id_ed25519",
                "bastion",
            ]
        );
        for value in &mut state.form.values[2..6] {
            *value = TextInput::default();
        }

        let UiAction::Edit { alias, changes } = press(&mut state, KeyCode::Enter) else {
            panic!("expected edit action")
        };
        assert_eq!(alias, "dev");
        assert_eq!(changes.user, Change::Clear);
        assert_eq!(changes.port, Change::Clear);
        assert_eq!(changes.identity_file, Change::Clear);
        assert_eq!(changes.proxy_jump, Change::Clear);
    }

    #[test]
    fn filter_mode_updates_selection_and_enter_keeps_the_filter() {
        let mut state = test_state(vec![
            summary("dev", Ownership::Managed),
            summary("prod", Ownership::Managed),
        ]);
        press(&mut state, KeyCode::Char('/'));
        type_text(&mut state, "pro");
        assert_eq!(state.selected_alias(), Some("prod"));
        press(&mut state, KeyCode::Enter);
        assert_eq!(state.mode, Mode::Browse);
        assert_eq!(state.filter.as_str(), "pro");

        press(&mut state, KeyCode::Esc);
        assert_eq!(state.filter.as_str(), "");
        assert_eq!(state.visible.len(), 2);
    }

    #[test]
    fn escape_cancels_a_filter_edit_back_to_the_previous_filter() {
        let mut state = test_state(vec![
            summary("dev", Ownership::Managed),
            summary("prod", Ownership::Managed),
        ]);
        press(&mut state, KeyCode::Char('/'));
        type_text(&mut state, "de");
        press(&mut state, KeyCode::Enter);

        press(&mut state, KeyCode::Char('/'));
        type_text(&mut state, "v");
        press(&mut state, KeyCode::Backspace);
        press(&mut state, KeyCode::Backspace);
        press(&mut state, KeyCode::Backspace);
        assert_eq!(state.filter.as_str(), "");
        assert_eq!(state.visible.len(), 2);

        press(&mut state, KeyCode::Esc);

        assert_eq!(state.mode, Mode::Browse);
        assert_eq!(state.filter.as_str(), "de");
        assert_eq!(state.selected_alias(), Some("dev"));
    }

    #[test]
    fn backspace_erases_text_and_only_escape_cancels() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        resolve(&mut state, details("dev", Ownership::Managed));

        press(&mut state, KeyCode::Char('/'));
        type_text(&mut state, "dex");
        press(&mut state, KeyCode::Backspace);
        assert_eq!(state.filter.as_str(), "de");
        assert_eq!(state.mode, Mode::Filter);
        press(&mut state, KeyCode::Esc);
        assert_eq!(state.mode, Mode::Browse);

        press(&mut state, KeyCode::Char('a'));
        fill_form(&mut state, ["dev", "", "", "", "", ""]);
        press(&mut state, KeyCode::Backspace);
        assert_eq!(state.form.values[0].as_str(), "de");
        assert_eq!(state.mode, Mode::AddForm);
        press(&mut state, KeyCode::Esc);
        assert_eq!(state.mode, Mode::Browse);

        press(&mut state, KeyCode::Char('x'));
        assert_eq!(state.mode, Mode::ConfirmRemove);
        assert_eq!(press(&mut state, KeyCode::Backspace), UiAction::None);
        assert_eq!(state.mode, Mode::ConfirmRemove);
        press(&mut state, KeyCode::Esc);
        assert_eq!(state.mode, Mode::Browse);
        assert_eq!(init_decision(key(KeyCode::Backspace)), None);
        assert_eq!(init_decision(key(KeyCode::Esc)), Some(false));
    }

    #[test]
    fn paste_inserts_at_the_cursor_without_line_breaks() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        press(&mut state, KeyCode::Char('a'));
        type_text(&mut state, "ad");
        press(&mut state, KeyCode::Left);

        state.handle_paste("bc\r\n");

        assert_eq!(state.form.values[0].as_str(), "abcd");
        press(&mut state, KeyCode::Char('x'));
        assert_eq!(state.form.values[0].as_str(), "abcxd");

        state.mode = Mode::Filter;
        state.handle_paste("de\nv");
        assert_eq!(state.filter.as_str(), "dev");
        assert_eq!(state.visible.len(), 1);
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
            inputs: Vec::new(),
        });

        assert_eq!(state.selected_alias(), Some("prod"));
    }

    #[test]
    fn t_starts_a_test_of_the_selected_host_in_browse_mode_only() {
        let dev = summary("dev", Ownership::Managed);
        let mut state = test_state(vec![dev.clone()]);

        assert_eq!(
            press(&mut state, KeyCode::Char('t')),
            UiAction::Test {
                origin: Origin::Host(dev),
                target: TestTarget::Saved("dev".into()),
            }
        );

        press(&mut state, KeyCode::Char('/'));
        assert_eq!(press(&mut state, KeyCode::Char('t')), UiAction::None);
        assert_eq!(state.filter.as_str(), "t");
        press(&mut state, KeyCode::Esc);

        press(&mut state, KeyCode::Char('a'));
        assert_eq!(press(&mut state, KeyCode::Char('t')), UiAction::FormEdited);
        assert_eq!(state.form.values[0].as_str(), "t");

        let mut empty = test_state(Vec::new());
        assert_eq!(press(&mut empty, KeyCode::Char('t')), UiAction::None);
    }

    #[test]
    fn ctrl_t_tests_the_typed_values_in_forms() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        assert_eq!(state.handle_key(ctrl('t')), UiAction::None, "browse");

        press(&mut state, KeyCode::Char('a'));
        fill_form(&mut state, ["new", "h", "", "", "", ""]);
        assert_eq!(
            state.handle_key(ctrl('t')),
            UiAction::Test {
                origin: Origin::Form,
                target: TestTarget::Add(Host::new("new", "h", None, None, None, None).unwrap()),
            }
        );
        assert_eq!(state.mode, Mode::AddForm);

        press(&mut state, KeyCode::Esc);
        resolve(&mut state, details("dev", Ownership::Managed));
        press(&mut state, KeyCode::Char('e'));
        state.form.values[0] = TextInput::new("renamed");
        let UiAction::Test {
            origin: Origin::Form,
            target: TestTarget::Edit { alias, changes },
        } = state.handle_key(ctrl('t'))
        else {
            panic!("expected a test of the edit");
        };
        assert_eq!(alias, "dev");
        assert_eq!(changes.alias, Change::Set("renamed".into()));
        assert_eq!(state.mode, Mode::EditForm);
    }

    #[test]
    fn ctrl_t_with_an_invalid_form_reports_the_field_error() {
        let mut state = test_state(Vec::new());
        press(&mut state, KeyCode::Char('a'));
        fill_form(&mut state, ["new", "", "", "", "", ""]);

        assert_eq!(state.handle_key(ctrl('t')), UiAction::None);

        assert_eq!(state.form.active, 1);
        assert!(state.form.error.is_some());
        assert_eq!(state.mode, Mode::AddForm);
    }

    #[test]
    fn esc_cancels_a_running_test_before_leaving_a_form() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        resolve(&mut state, details("dev", Ownership::Managed));
        press(&mut state, KeyCode::Char('e'));
        state.testing = Some("dev".into());

        assert_eq!(press(&mut state, KeyCode::Esc), UiAction::CancelTest);
        assert_eq!(state.mode, Mode::EditForm);
        assert_eq!(state.status.as_deref(), Some("Test cancelled"));

        state.testing = None;
        assert_eq!(press(&mut state, KeyCode::Esc), UiAction::None);
        assert_eq!(state.mode, Mode::Browse);

        // In browse mode too, Esc cancels before it clears the filter.
        state.set_filter("de");
        state.testing = Some("dev".into());
        assert_eq!(press(&mut state, KeyCode::Esc), UiAction::CancelTest);
        assert_eq!(state.filter.as_str(), "de");
    }

    #[test]
    fn results_mark_the_host_and_appear_in_details_and_status() {
        let dev = summary("dev", Ownership::Managed);
        let prod = summary("prod", Ownership::Managed);
        let web = summary("web", Ownership::External);
        let mut state = test_state(vec![dev.clone(), prod.clone(), web]);
        resolve(&mut state, details("dev", Ownership::Managed));

        state.record_test(
            host_finished(dev.clone(), Ok(authenticated("dev"))),
            "12:34".into(),
        );

        let text = rendered(&state);
        assert!(text.contains("✓ dev  managed"), "{text}");
        assert!(text.contains("  web  external"), "{text}");
        assert!(
            text.contains("dev: authenticated as u@10.0.0.5:22 (publickey)"),
            "{text}"
        );
        assert!(
            details_text(&state)
                .contains("Connection test: authenticated as u@10.0.0.5:22 (publickey) at 12:34"),
            "{}",
            details_text(&state)
        );

        let unknown_key = TestOutcome::UnknownHostKey {
            report: report(
                "prod",
                ProbeResult::Failed(crate::domain::probe::Failure::HostKeyUnknown),
            ),
            key: crate::domain::openssh::PresentedKey {
                lines: Vec::new(),
                fingerprints: Vec::new(),
            },
        };
        state.record_test(host_finished(prod.clone(), Ok(unknown_key)), "12:35".into());
        assert_eq!(state.mode, Mode::ConfirmTrust);
        assert_eq!(
            press(&mut state, KeyCode::Char('n')),
            UiAction::Trust(false)
        );
        let text = rendered(&state);
        assert!(text.contains("✗ prod  managed"), "{text}");
        assert!(text.contains("Host key not trusted"), "{text}");

        state.record_test(
            host_finished(prod, Err("unknown host: prod".into())),
            "12:36".into(),
        );
        assert_eq!(
            state.status.as_deref(),
            Some("prod: failed — unknown host: prod")
        );
        assert!(state.form.test_result.is_none());
    }

    #[test]
    fn a_result_for_a_closed_form_is_dropped() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        let form_finished = || Finished {
            origin: Origin::Form,
            target: TestTarget::Add(Host::new("new", "h", None, None, None, None).unwrap()),
            outcome: Ok(authenticated("new")),
        };

        state.record_test(form_finished(), "12:34".into());
        assert!(state.form.test_result.is_none());
        assert!(state.status.is_none());
        assert!(!rendered(&state).contains('✓'));

        press(&mut state, KeyCode::Char('a'));
        state.record_test(form_finished(), "12:34".into());
        assert_eq!(
            state
                .form
                .test_result
                .as_ref()
                .map(|result| result.line.as_str()),
            Some("authenticated as u@10.0.0.5:22 (publickey)")
        );
        let text = rendered(&state);
        assert!(text.contains("Connection test: authenticated"), "{text}");
        assert!(state.status.is_none());

        // Editing the form clears the result, as it clears an error.
        press(&mut state, KeyCode::Char('x'));
        assert!(state.form.test_result.is_none());
    }

    #[test]
    fn an_unknown_key_opens_the_trust_prompt_with_the_fingerprint() {
        let dev = summary("dev", Ownership::Managed);
        let mut state = test_state(vec![dev.clone()]);

        state.record_test(
            host_finished(dev.clone(), Ok(unknown_key("dev", "|1|salt|hash"))),
            "12:34".into(),
        );

        assert_eq!(state.mode, Mode::ConfirmTrust);
        let backend = TestBackend::new(140, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &state)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("Trust host key"), "{text}");
        assert!(
            text.contains("dev presents an ED25519 key with fingerprint SHA256:abc"),
            "{text}"
        );
        assert!(
            text.contains(
                "Compare it with the fingerprint from the server's administrator or console."
            ),
            "{text}"
        );
        assert!(text.contains("Trust it and test again? y/N"), "{text}");
        assert!(
            text.contains("y trust and test again  n/Esc keep it untrusted"),
            "{text}"
        );
        assert!(state.results.get(&dev).is_none(), "nothing recorded yet");
    }

    #[test]
    fn y_trusts_and_retests_and_returns_to_the_previous_mode() {
        let dev = summary("dev", Ownership::Managed);
        let mut state = test_state(vec![dev.clone()]);
        press(&mut state, KeyCode::Char('a'));
        state.mode = Mode::EditForm;
        let finished = Finished {
            origin: Origin::Form,
            target: TestTarget::Saved("dev".into()),
            outcome: Ok(unknown_key("dev", "dev.example")),
        };
        state.record_test(finished, "12:34".into());
        assert_eq!(state.mode, Mode::ConfirmTrust);

        assert_eq!(press(&mut state, KeyCode::Char('x')), UiAction::None);
        assert_eq!(state.mode, Mode::ConfirmTrust);
        assert_eq!(press(&mut state, KeyCode::Char('y')), UiAction::Trust(true));

        assert_eq!(state.mode, Mode::EditForm);
        let pending = state.trust.take().expect("the key to trust");
        assert_eq!(pending.origin, Origin::Form);
        assert_eq!(pending.target, TestTarget::Saved("dev".into()));
        assert_eq!(pending.key.lines, b"h ssh-ed25519 AAAA\n");
        assert!(state.form.test_result.is_none(), "nothing recorded on y");
    }

    #[test]
    fn n_and_esc_decline_and_mark_the_host_untrusted() {
        for code in [KeyCode::Char('n'), KeyCode::Esc] {
            let dev = summary("dev", Ownership::Managed);
            let mut state = test_state(vec![dev.clone()]);
            state.record_test(
                host_finished(dev.clone(), Ok(unknown_key("dev", "dev.example"))),
                "12:34".into(),
            );

            assert_eq!(press(&mut state, code), UiAction::Trust(false));

            assert_eq!(state.mode, Mode::Browse);
            assert!(state.trust.is_none());
            assert_eq!(state.status.as_deref(), Some("Host key not trusted"));
            let mark = state.results.get(&dev).expect("a recorded result");
            assert!(!mark.passed);
            assert_eq!(mark.line, "failed \u{2014} host key not trusted");
        }
    }

    #[test]
    fn a_changed_key_shows_its_warning_without_a_prompt() {
        let dev = summary("dev", Ownership::Managed);
        let mut state = test_state(vec![dev.clone()]);
        resolve(&mut state, details("dev", Ownership::Managed));
        let home = std::path::PathBuf::from("/home/me");
        let mut changed = report(
            "dev",
            ProbeResult::Failed(crate::domain::probe::Failure::HostKeyChanged {
                known_hosts: None,
            }),
        );
        changed.explanation = Some(crate::domain::explain::explain(
            &crate::domain::probe::Diagnosis {
                failure: crate::domain::probe::Failure::HostKeyChanged { known_hosts: None },
                raw: Vec::new(),
            },
            &crate::domain::explain::Context {
                alias: "dev",
                operation: crate::domain::explain::Operation::Test,
                user: None,
                hostname: Some("dev.example"),
                port: None,
                identity_files: &[],
                jump_is_known: false,
                home: &home,
                managed_file: None,
                adding: false,
                in_tui: true,
            },
        ));
        let changed = TestOutcome::Done(changed);

        state.record_test(host_finished(dev.clone(), Ok(changed)), "12:34".into());

        assert_eq!(state.mode, Mode::Browse);
        assert!(state.trust.is_none());
        assert!(!state.results.get(&dev).unwrap().passed);
        assert_eq!(
            state.status.as_deref(),
            Some("dev: failed \u{2014} the host key changed, which can mean an attack")
        );
        let text = details_text(&state);
        assert!(text.contains("`ssh-keygen -R dev.example`"), "{text}");
    }

    #[test]
    fn a_form_result_for_a_closed_form_opens_no_prompt() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        let finished = Finished {
            origin: Origin::Form,
            target: TestTarget::Saved("dev".into()),
            outcome: Ok(unknown_key("dev", "dev.example")),
        };
        state.record_test(finished, "12:34".into());
        assert_eq!(state.mode, Mode::Browse);
        assert!(state.trust.is_none());
    }

    #[test]
    fn a_host_test_opens_the_prompt_only_from_browse_mode() {
        let dev = summary("dev", Ownership::Managed);
        let mut state = test_state(vec![dev.clone()]);
        press(&mut state, KeyCode::Char('/'));

        state.record_test(
            host_finished(dev.clone(), Ok(unknown_key("dev", "dev.example"))),
            "12:34".into(),
        );

        assert_eq!(state.mode, Mode::Filter);
        assert!(state.trust.is_none());
        assert_eq!(
            state.status.as_deref(),
            Some("dev: failed \u{2014} host key not trusted (press t to review the host key)")
        );
        type_text(&mut state, "yd");
        assert_eq!(
            state.filter.as_str(),
            "yd",
            "typed-ahead keys were swallowed"
        );
        let mark = state.results.get(&dev).expect("a recorded result");
        assert!(!mark.passed);
        assert_eq!(mark.line, "failed \u{2014} host key not trusted");

        // From an open form, too, the host's result is recorded without a prompt.
        let mut state = test_state(vec![dev.clone()]);
        press(&mut state, KeyCode::Char('a'));
        state.record_test(
            host_finished(dev.clone(), Ok(unknown_key("dev", "dev.example"))),
            "12:34".into(),
        );
        assert_eq!(state.mode, Mode::AddForm);
        assert!(state.trust.is_none());
        press(&mut state, KeyCode::Char('y'));
        assert_eq!(state.form.values[0].as_str(), "y");
        assert!(state.results.get(&dev).is_some());
    }

    #[test]
    fn a_form_test_opens_the_prompt_only_in_its_open_form() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        let finished = || Finished {
            origin: Origin::Form,
            target: TestTarget::Add(Host::new("new", "h", None, None, None, None).unwrap()),
            outcome: Ok(unknown_key("new", "h")),
        };
        press(&mut state, KeyCode::Char('/'));
        state.record_test(finished(), "12:34".into());
        assert_eq!(state.mode, Mode::Filter);
        assert!(state.trust.is_none());

        press(&mut state, KeyCode::Esc);
        press(&mut state, KeyCode::Char('a'));
        state.record_test(finished(), "12:34".into());
        assert_eq!(state.mode, Mode::ConfirmTrust);
    }

    #[test]
    fn editing_or_pasting_into_a_form_cancels_its_test() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        press(&mut state, KeyCode::Char('a'));
        state.testing = Some("new".into());

        assert_eq!(press(&mut state, KeyCode::Char('x')), UiAction::FormEdited);
        assert_eq!(state.handle_paste("y"), UiAction::FormEdited);
        assert_eq!(press(&mut state, KeyCode::Left), UiAction::None, "no edit");
        assert_eq!(press(&mut state, KeyCode::Tab), UiAction::None);
        assert_eq!(state.form.values[0].as_str(), "xy");
    }

    #[test]
    fn a_failed_test_shows_its_explanation_in_the_details() {
        let dev = summary("dev", Ownership::Managed);
        let mut state = test_state(vec![dev.clone()]);
        resolve(&mut state, details("dev", Ownership::Managed));

        state.record_test(
            host_finished(
                dev,
                Ok(explained(
                    "dev",
                    "the server refused the login",
                    &["denied"],
                    false,
                )),
            ),
            "12:34".into(),
        );

        let text = details_text(&state);
        let expected = "Connection test: failed \u{2014} the server refused the login at 12:34\n  user: u\n  next: try this";
        assert!(text.ends_with(expected), "{text}");
        assert!(!text.contains("dev: failed"), "{text}");
        assert!(!text.contains("ssh said"), "{text}");
    }

    #[test]
    fn o_unfolds_the_ssh_output_and_the_footer_offers_it_only_when_there_is_some() {
        let dev = summary("dev", Ownership::Managed);
        let prod = summary("prod", Ownership::Managed);
        let mut state = test_state(vec![dev.clone(), prod.clone()]);
        resolve(&mut state, details("dev", Ownership::Managed));
        assert!(!footer_text(&state).contains("o ssh output"));

        state.record_test(
            host_finished(dev.clone(), Ok(explained("dev", "refused", &[], false))),
            "12:34".into(),
        );
        assert!(
            !footer_text(&state).contains("o ssh output"),
            "no raw lines"
        );

        state.record_test(
            host_finished(
                dev,
                Ok(explained("dev", "refused", &["Permission denied"], false)),
            ),
            "12:35".into(),
        );
        assert!(footer_text(&state).contains("o ssh output"));
        assert!(!details_text(&state).contains("ssh said:"));

        press(&mut state, KeyCode::Char('o'));
        let text = details_text(&state);
        assert!(
            text.contains("  ssh said:\n    Permission denied"),
            "{text}"
        );
        press(&mut state, KeyCode::Char('o'));
        assert!(!details_text(&state).contains("ssh said:"));

        press(&mut state, KeyCode::Char('o'));
        assert!(state.show_raw);
        press(&mut state, KeyCode::Char('j'));
        assert!(!state.show_raw, "moving the selection hides it");
        assert!(
            !footer_text(&state).contains("o ssh output"),
            "prod has none"
        );

        // `o` does nothing where there is no output to show.
        press(&mut state, KeyCode::Char('o'));
        assert!(!state.show_raw);

        // Opening or closing a form hides it too.
        press(&mut state, KeyCode::Char('k'));
        press(&mut state, KeyCode::Char('o'));
        press(&mut state, KeyCode::Char('a'));
        assert!(!state.show_raw);
        press(&mut state, KeyCode::Esc);
        assert!(!state.show_raw);
    }

    #[test]
    fn a_new_result_or_a_form_edit_folds_the_output_again() {
        let dev = summary("dev", Ownership::Managed);
        let mut state = test_state(vec![dev.clone()]);
        resolve(&mut state, details("dev", Ownership::Managed));
        let failed = || Ok(explained("dev", "refused", &["Permission denied"], false));

        state.record_test(host_finished(dev.clone(), failed()), "12:34".into());
        press(&mut state, KeyCode::Char('o'));
        assert!(state.show_raw);
        state.record_test(host_finished(dev, failed()), "12:35".into());
        assert!(!state.show_raw, "a new result of the selected host");

        press(&mut state, KeyCode::Char('a'));
        let form_finished = || Finished {
            origin: Origin::Form,
            target: TestTarget::Add(Host::new("new", "h", None, None, None, None).unwrap()),
            outcome: failed(),
        };
        state.record_test(form_finished(), "12:36".into());
        state.handle_key(ctrl('o'));
        assert!(state.show_raw);
        state.record_test(form_finished(), "12:37".into());
        assert!(!state.show_raw, "a new result of the form");

        state.handle_key(ctrl('o'));
        assert!(state.show_raw);
        press(&mut state, KeyCode::Char('x'));
        assert!(!state.show_raw, "typing in a field");

        state.record_test(form_finished(), "12:38".into());
        state.handle_key(ctrl('o'));
        state.handle_paste("y");
        assert!(!state.show_raw, "pasting into a field");
    }

    #[test]
    fn output_that_is_always_shown_is_not_offered_as_a_fold() {
        let dev = summary("dev", Ownership::Managed);
        let mut state = test_state(vec![dev.clone()]);
        resolve(&mut state, details("dev", Ownership::Managed));

        state.record_test(
            host_finished(
                dev,
                Ok(explained("dev", "unexplained", &["odd output"], true)),
            ),
            "12:34".into(),
        );

        assert!(details_text(&state).contains("  ssh said:\n    odd output"));
        assert!(!footer_text(&state).contains("o ssh output"));
    }

    #[test]
    fn identities_replace_the_identity_file_row_in_the_details() {
        let mut details = details("web", Ownership::Managed);
        let plain = detail_text(&details, Path::new("/home/test"));
        assert!(plain.contains("IdentityFile ~/.ssh/id_ed25519"), "{plain}");
        details.identities = Some(crate::domain::identity::HostIdentities {
            agent: crate::domain::identity::Agent::Disabled,
            defaults: false,
            identities: vec![crate::domain::identity::Identity {
                configured: "~/.ssh/id_ed25519".into(),
                path: None,
                public_key: None,
                availability: crate::domain::identity::Availability::KeyFile {
                    public_key_found: false,
                },
            }],
            identities_only: false,
            also_offered: Vec::new(),
            pkcs11_provider: None,
        });
        let text = detail_text(&details, Path::new("/home/test"));
        assert_eq!(text.matches("~/.ssh/id_ed25519").count(), 1, "{text}");
        assert!(
            text.contains(
                "Agent        none (IdentityAgent none)\n\
                 IdentityFile ~/.ssh/id_ed25519  ✓ key file (agent not checked: no public key file)\n"
            ),
            "{text}"
        );
        assert!(text.contains("ProxyJump"), "{text}");
    }
}
