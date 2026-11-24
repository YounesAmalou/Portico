//! The Add/Edit host form: its fields, state, rendering and key handling.

use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use super::{
    Mode, TextInput, UiAction, UiState,
    dropdown::{SUGGESTION_ROWS, Suggestions, suggestion_lines},
    explanation_lines, has_foldable_raw, identities,
    tester::Origin,
};
use crate::{
    app::{Ownership, TestTarget},
    domain::{
        explain::Explanation,
        host::{Change, ClearField, Host, HostChanges, HostError, HostField, parse_port},
        suggest::{self, Candidate},
    },
};

/// One row of the Add/Edit form.
pub(super) struct FormField {
    pub(super) field: HostField,
    pub(super) label: &'static str,
    pub(super) placeholder: &'static str,
    pub(super) hint: &'static str,
    pub(super) optional: bool,
}

pub(super) const FORM_FIELDS: [FormField; 6] = [
    FormField {
        field: HostField::Alias,
        label: "Alias",
        placeholder: "dev-vps",
        hint: "The name you type after ssh. Letters, digits, '.', '_' and '-'; it must start with a letter or digit.",
        optional: false,
    },
    FormField {
        field: HostField::Hostname,
        label: "HostName",
        placeholder: "dev.example.com",
        hint: "DNS name or IP address to connect to.",
        optional: false,
    },
    FormField {
        field: HostField::User,
        label: "User",
        placeholder: "deploy",
        hint: "Login user. Leave empty to let OpenSSH choose.",
        optional: true,
    },
    FormField {
        field: HostField::Port,
        label: "Port",
        placeholder: "22",
        hint: "A number from 1 through 65535. Leave empty for the OpenSSH default.",
        optional: true,
    },
    FormField {
        field: HostField::IdentityFile,
        label: "IdentityFile",
        placeholder: "~/.ssh/id_ed25519",
        hint: "Path to a private key. Portico stores the path and never reads the file.",
        optional: true,
    },
    FormField {
        field: HostField::ProxyJump,
        label: "ProxyJump",
        placeholder: "bastion",
        hint: "Jump host as [user@]host[:port] or an existing alias. Separate several hops with commas, without spaces.",
        optional: true,
    },
];

#[derive(Clone, Debug)]
pub(super) struct FormState {
    pub(super) values: [TextInput; 6],
    pub(super) active: usize,
    pub(super) original: Option<Host>,
    /// The reason the last save was rejected, shown until the form is edited.
    pub(super) error: Option<String>,
    /// The last connection test of the typed values, shown until the form is edited.
    pub(super) test_result: Option<FormTestResult>,
    /// The dropdown under ProxyJump or IdentityFile, while it is open.
    pub(super) suggestions: Option<Suggestions>,
    /// Ctrl+Space asked for IdentityFile suggestions before there were any: the dropdown opens
    /// when the identity inventory arrives, unless the focus left the field meanwhile.
    pub(super) identity_pending: bool,
}

/// The index of IdentityFile in `FORM_FIELDS`.
pub(super) const IDENTITY_FILE: usize = 4;
/// The index of ProxyJump in `FORM_FIELDS`.
pub(super) const PROXY_JUMP: usize = 5;
/// A form's connection test result, with the explanation shown under it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct FormTestResult {
    pub(super) line: String,
    pub(super) explanation: Option<Explanation>,
}

impl Default for FormState {
    fn default() -> Self {
        Self {
            values: FORM_FIELDS.map(|row| TextInput::for_field(row.field, "")),
            active: 0,
            original: None,
            error: None,
            test_result: None,
            suggestions: None,
            identity_pending: false,
        }
    }
}

impl FormState {
    pub(super) fn from_host(host: &Host) -> Self {
        let port = host
            .port()
            .map_or_else(String::new, |port| port.to_string());
        let texts = [
            host.alias(),
            host.hostname(),
            host.user().unwrap_or_default(),
            &port,
            host.identity_file().unwrap_or_default(),
            host.proxy_jump().unwrap_or_default(),
        ];
        Self {
            values: std::array::from_fn(|index| {
                TextInput::for_field(FORM_FIELDS[index].field, texts[index])
            }),
            active: 0,
            original: Some(host.clone()),
            error: None,
            test_result: None,
            suggestions: None,
            identity_pending: false,
        }
    }

    /// Builds the host the form describes. Every rule comes from the host model, so the form
    /// cannot accept what `Host::new` rejects; whitespace is never trimmed away silently.
    fn host(&self) -> Result<Host, HostError> {
        let value = |index: usize| self.values[index].as_str();
        let port = optional(value(3))
            .map(|port| parse_port(&port))
            .transpose()?;
        Host::new(
            value(0),
            value(1),
            optional(value(2)),
            port,
            optional(value(4)),
            optional(value(5)),
        )
    }

    fn submission(&self, mode: &Mode) -> Result<UiAction, HostError> {
        let host = self.host()?;

        if *mode == Mode::AddForm {
            return Ok(UiAction::Add(host));
        }

        let original = self
            .original
            .as_ref()
            .expect("an edit form is opened from a host");
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
        if original.port() != host.port()
            && let Some(port) = host.port()
        {
            changes.port = Change::Set(port);
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

/// The form's lines for a pane of `width` columns and `height` rows inside its border; only the
/// jump host dropdown adapts to them. Suggestions from files under `home` show it as `~`.
pub(super) fn form_lines(
    form: &FormState,
    show_raw: bool,
    home: &Path,
    width: usize,
    height: usize,
) -> Vec<Line<'static>> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let mut lines = FORM_FIELDS
        .iter()
        .zip(&form.values)
        .enumerate()
        .map(|(index, (row, input))| {
            let focused = index == form.active;
            let mut spans = vec![Span::raw(format!(
                "{} {}: ",
                if focused { '>' } else { ' ' },
                row.label
            ))];
            spans.extend(input.spans(focused));
            if input.is_empty() {
                spans.push(Span::styled(row.placeholder, dim));
            }
            Line::from(spans)
        })
        .collect::<Vec<_>>();
    if let Some(list) = &form.suggestions {
        // Fields, the dropdown's borders, and the blank line and hint under it stay on screen.
        let rows = height
            .saturating_sub(FORM_FIELDS.len() + 4)
            .clamp(1, SUGGESTION_ROWS);
        let rows = suggestion_lines(list, FORM_FIELDS[list.field].label, home, width, rows);
        lines.splice(list.field + 1..list.field + 1, rows);
    }
    lines.push(Line::raw(""));
    let active = &FORM_FIELDS[form.active];
    lines.push(Line::styled(
        format!(
            "{}{}",
            active.hint,
            if active.optional { "" } else { " Required." }
        ),
        dim,
    ));
    if let Some(error) = &form.error {
        lines.push(Line::styled(
            format!("Error: {error}"),
            Style::default().fg(Color::Red),
        ));
    }
    if let Some(result) = &form.test_result {
        match &result.explanation {
            Some(explanation) => {
                lines.push(Line::raw(format!(
                    "Connection test: failed \u{2014} {}",
                    explanation.headline
                )));
                lines.extend(explanation_lines(explanation, show_raw).map(Line::raw));
            }
            None => lines.push(Line::raw(format!("Connection test: {}", result.line))),
        }
    }
    lines
}

impl UiState {
    pub(super) fn handle_form_key(&mut self, key: KeyEvent) -> UiAction {
        if key.code == KeyCode::Char(' ') && key.modifiers == KeyModifiers::CONTROL {
            if matches!(self.form.active, PROXY_JUMP | IDENTITY_FILE) {
                self.suggest(true);
            }
            return UiAction::None;
        }
        if let Some(list) = &mut self.form.suggestions {
            match key.code {
                KeyCode::Up => {
                    list.select(list.selected.saturating_sub(1));
                    return UiAction::None;
                }
                KeyCode::Down => {
                    list.select(list.selected + 1);
                    return UiAction::None;
                }
                KeyCode::Enter if key.modifiers.is_empty() => return self.pick_suggestion(),
                KeyCode::Tab => return self.pick_suggestion(),
                KeyCode::Esc => {
                    self.form.suggestions = None;
                    return UiAction::None;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Esc => self.mode = Mode::Browse,
            KeyCode::Up | KeyCode::BackTab => self.form.active = self.form.active.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab => {
                self.form.active = (self.form.active + 1).min(FORM_FIELDS.len() - 1)
            }
            KeyCode::Enter if key.modifiers.is_empty() => return self.submit_form(),
            KeyCode::Char('t') if key.modifiers == KeyModifiers::CONTROL => {
                self.form.suggestions = None;
                return self.test_form();
            }
            KeyCode::Char('o') if key.modifiers == KeyModifiers::CONTROL => {
                if self.form_has_foldable_raw() {
                    self.show_raw = !self.show_raw;
                }
            }
            _ => {
                let input = &mut self.form.values[self.form.active];
                let (before, cursor) = (input.as_str().to_owned(), input.cursor);
                if input.handle_key(key) {
                    return self.form_input_changed(&before, cursor);
                }
            }
        }
        if self
            .form
            .suggestions
            .as_ref()
            .is_some_and(|list| list.field != self.form.active)
        {
            self.form.suggestions = None;
        }
        if self.form.active != IDENTITY_FILE || key.code == KeyCode::Esc {
            self.form.identity_pending = false;
        }
        UiAction::None
    }

    /// Follows up a key or paste that went to the focused input: a change clears the form's last
    /// error and test result and recomputes the jump host dropdown, which also follows the
    /// cursor while it is open.
    pub(super) fn form_input_changed(&mut self, before: &str, cursor: usize) -> UiAction {
        self.form.error = None;
        self.form.test_result = None;
        self.show_raw = false;
        let input = &self.form.values[self.form.active];
        let edited = input.as_str() != before;
        if edited || (input.cursor != cursor && self.form.suggestions.is_some()) {
            self.suggest(false);
        }
        if edited {
            UiAction::FormEdited
        } else {
            UiAction::None
        }
    }

    /// Opens, recomputes or closes the jump host dropdown for the hop under the ProxyJump cursor.
    /// `on_demand` (Ctrl+Space) opens it even for an empty hop or an exact match; afterwards it
    /// stays open for an empty hop for as long as it stays open, but an exact match still closes it.
    pub(super) fn suggest(&mut self, on_demand: bool) {
        let form = &mut self.form;
        let previous = form.suggestions.take();
        let forced = on_demand || previous.as_ref().is_some_and(|list| list.forced);
        if form.active == IDENTITY_FILE {
            let candidates = identities::candidates(self.identity_inventory.as_ref(), &self.home);
            let query = form.values[IDENTITY_FILE].as_str();
            let items = identities::missing_last(suggest::rank(query, &candidates, &[]));
            let exact = items.iter().any(|item| item.alias == query);
            if items.is_empty() && on_demand && self.identity_inventory.is_none() {
                form.identity_pending = true;
            }
            if items.is_empty() || (!forced && query.is_empty()) || (!on_demand && exact) {
                return;
            }
            form.identity_pending = false;
            form.suggestions = Some(Suggestions::reopened(
                previous,
                items,
                forced,
                IDENTITY_FILE,
            ));
            return;
        }
        if form.active != PROXY_JUMP {
            return;
        }
        let input = &form.values[PROXY_JUMP];
        let value = input.as_str();
        let Some(hop) = suggest::current_hop(value, input.cursor) else {
            return;
        };
        let hop_start = value[..hop.range.start]
            .rfind(',')
            .map_or(0, |comma| comma + 1);
        let mut exclude = vec![form.values[0].as_str()];
        if let Some(original) = &form.original {
            exclude.push(original.alias());
        }
        let mut start = 0;
        for segment in value.split(',') {
            if start != hop_start {
                let alias = segment.rsplit('@').next().unwrap_or(segment);
                exclude.push(alias.split(':').next().unwrap_or(alias));
            }
            start += segment.len() + 1;
        }
        let candidates = self
            .hosts
            .iter()
            .map(|host| Candidate {
                alias: host.alias.clone(),
                origin: match host.ownership {
                    Ownership::Managed => suggest::Origin::Managed,
                    Ownership::External => suggest::Origin::External(host.source.clone()),
                },
            })
            .collect::<Vec<_>>();
        let items = suggest::rank(&hop.query, &candidates, &exclude);
        let exact = items
            .iter()
            .any(|item| item.alias.eq_ignore_ascii_case(&hop.query));
        if items.is_empty() || (!forced && hop.query.is_empty()) || (!on_demand && exact) {
            return;
        }
        form.suggestions = Some(Suggestions::reopened(previous, items, forced, PROXY_JUMP));
    }

    /// Replaces the hop under the ProxyJump cursor with the selected suggestion.
    fn pick_suggestion(&mut self) -> UiAction {
        let Some(list) = self.form.suggestions.take() else {
            return UiAction::None;
        };
        if list.field == IDENTITY_FILE {
            let input = &mut self.form.values[IDENTITY_FILE];
            let (before, cursor) = (input.as_str().to_owned(), input.cursor);
            input.text = list.items[list.selected].alias.clone();
            input.cursor = input.text.len();
            let action = self.form_input_changed(&before, cursor);
            self.form.suggestions = None;
            return action;
        }
        let input = &mut self.form.values[PROXY_JUMP];
        let Some(hop) = suggest::current_hop(input.as_str(), input.cursor) else {
            return UiAction::None;
        };
        let (before, cursor) = (input.as_str().to_owned(), input.cursor);
        (input.text, input.cursor) =
            suggest::apply(input.as_str(), &hop, &list.items[list.selected].alias);
        let action = self.form_input_changed(&before, cursor);
        // A pick always closes the dropdown, whatever recomputing it found.
        self.form.suggestions = None;
        action
    }

    fn submit_form(&mut self) -> UiAction {
        match self.form.submission(&self.mode) {
            Ok(action) => action,
            Err(error) => {
                self.report_form_error(&error);
                UiAction::None
            }
        }
    }

    /// Tests the values typed into the form, without saving them.
    fn test_form(&mut self) -> UiAction {
        let target = match self.form.submission(&self.mode) {
            Ok(UiAction::Add(host)) => TestTarget::Add(host),
            Ok(UiAction::Edit { alias, changes }) => TestTarget::Edit { alias, changes },
            Ok(other) => unreachable!("a form submits an add or an edit, not {other:?}"),
            Err(error) => {
                self.report_form_error(&error);
                return UiAction::None;
            }
        };
        UiAction::Test {
            origin: Origin::Form,
            target,
        }
    }

    pub(super) fn form_has_foldable_raw(&self) -> bool {
        self.form
            .test_result
            .as_ref()
            .and_then(|result| result.explanation.as_ref())
            .is_some_and(has_foldable_raw)
    }

    pub(super) fn report_form_error(&mut self, error: &HostError) {
        if let HostError::Invalid { field, .. } = error
            && let Some(index) = FORM_FIELDS.iter().position(|row| row.field == *field)
        {
            self.form.active = index;
        }
        self.form.error = Some(error.to_string());
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::tui::tests::{ctrl, press, proxy_jump_form, summary, type_text};

    fn form_text(state: &UiState, width: usize, height: usize) -> Vec<String> {
        form_lines(&state.form, state.show_raw, &state.home, width, height)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }

    fn columns_before(line: &str, needle: char, nth: usize) -> usize {
        let index = line.match_indices(needle).nth(nth).unwrap().0;
        Line::raw(&line[..index]).width()
    }

    #[test]
    fn dropdown_fits_a_narrow_pane() {
        let mut deep = summary("bastion-eu", Ownership::External);
        deep.source = PathBuf::from("/srv/ssh/config.d/clients/work/eu-west.conf");
        let mut state = proxy_jump_form(vec![summary("bastion", Ownership::Managed), deep]);
        type_text(&mut state, "bas");
        let lines = form_text(&state, 40, 30);
        let rows = lines
            .iter()
            .filter(|line| line.contains('│'))
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 2, "{lines:#?}");
        for line in &lines[PROXY_JUMP + 1..PROXY_JUMP + 5] {
            assert!(Line::raw(line.as_str()).width() <= 40, "{line:?}");
        }
        assert!(rows[1].contains("bastion-eu"), "{rows:?}");
        assert!(rows[1].ends_with("│  …west.conf"), "{rows:?}");
    }

    #[test]
    fn dropdown_aligns_wide_aliases() {
        let mut state = proxy_jump_form(vec![
            summary("東京-jump", Ownership::Managed),
            summary("tokyo-jump", Ownership::Managed),
        ]);
        state.handle_key(ctrl(' '));
        let lines = form_text(&state, 100, 30);
        let rows = &lines[PROXY_JUMP + 2..PROXY_JUMP + 4];
        assert_eq!(
            columns_before(&rows[0], '│', 1),
            columns_before(&rows[1], '│', 1),
            "{rows:#?}"
        );
    }

    #[test]
    fn dropdown_shrinks_to_keep_the_hint_visible() {
        let hosts = (1..=7)
            .map(|n| summary(&format!("h{n}"), Ownership::Managed))
            .collect();
        let mut state = proxy_jump_form(hosts);
        state.handle_key(ctrl(' '));
        for _ in 0..6 {
            press(&mut state, KeyCode::Down);
        }
        let lines = form_text(&state, 100, 13);
        assert!(lines.len() <= 13, "{lines:#?}");
        assert!(
            lines.iter().any(|line| line.contains("│> h7 │")),
            "{lines:#?}"
        );
        assert!(
            lines.iter().any(|line| line.starts_with("Jump host")),
            "{lines:#?}"
        );
    }
}
