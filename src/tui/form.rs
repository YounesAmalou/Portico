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
    hosts::{explanation_lines, has_foldable_raw},
    identities,
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
    label: &'static str,
    placeholder: &'static str,
    hint: &'static str,
    optional: bool,
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

    use crossterm::event::KeyCode;

    use crate::app::{AppError, HostInventory, Ownership, TestTarget};
    use crate::domain::config_error::ConfigRejection;
    use crate::domain::host::{Host, HostField};
    use crate::domain::identity::IdentityInventory;
    use crate::domain::managed::TransactionError;
    use crate::tui::dropdown::Suggestions;
    use crate::tui::form::{FORM_FIELDS, IDENTITY_FILE, PROXY_JUMP, form_lines};
    use crate::tui::tester::{Finished, Origin};
    use crate::tui::tests::{
        details, explained, fill_form, form_with_active_field, jump_hosts, rendered, resolve,
        test_state,
    };
    use crate::tui::{Mode, TextInput, UiAction, UiState, footer_text};

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

    #[test]
    fn form_renders_exactly_the_six_v1_fields() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        press(&mut state, KeyCode::Char('a'));
        let text = rendered(&state);

        for label in [
            "Alias:",
            "HostName:",
            "User:",
            "Port:",
            "IdentityFile:",
            "ProxyJump:",
        ] {
            assert_eq!(text.matches(label).count(), 1, "{label}\n{text}");
        }
    }

    #[test]
    fn focused_proxy_jump_explains_its_format() {
        let mut state = test_state(Vec::new());
        press(&mut state, KeyCode::Char('a'));
        assert!(!rendered(&state).contains("[user@]host[:port]"));

        for _ in 0..5 {
            press(&mut state, KeyCode::Tab);
        }
        let text = rendered(&state);

        assert!(text.contains("[user@]host[:port]"), "{text}");
        assert!(text.contains("existing alias"), "{text}");
        assert!(text.contains("commas"), "{text}");
        assert!(text.contains("bastion"), "{text}");
    }

    #[test]
    fn add_form_reports_host_validation_errors_inline_on_the_field() {
        let mut state = test_state(Vec::new());
        press(&mut state, KeyCode::Char('a'));
        fill_form(&mut state, ["-bad", "dev.example", "", "", "", ""]);

        assert_eq!(press(&mut state, KeyCode::Enter), UiAction::None);

        assert_eq!(state.mode, Mode::AddForm);
        assert_eq!(state.form.active, 0);
        let text = rendered(&state);
        assert!(text.contains("Error: invalid alias"), "{text}");

        type_text(&mut state, "x");
        assert!(state.form.error.is_none());
    }

    #[test]
    fn padded_or_blank_proxy_jump_is_rejected_not_trimmed() {
        for jump in ["   ", " bastion", "bastion ", "a b"] {
            let mut state = test_state(Vec::new());
            press(&mut state, KeyCode::Char('a'));
            fill_form(&mut state, ["dev", "dev.example", "", "", "", jump]);

            assert_eq!(
                press(&mut state, KeyCode::Enter),
                UiAction::None,
                "{jump:?}"
            );

            assert_eq!(state.form.active, 5, "{jump:?}");
            let error = state.form.error.as_deref().unwrap();
            assert!(error.contains("proxy jump"), "{jump:?}: {error}");
        }
    }

    #[test]
    fn form_fields_refuse_characters_their_rules_never_permit() {
        for (index, typed, kept) in [
            (0, "de v/1\t", "dev1"),
            (1, "exa mple", "example"),
            (2, "dep loy", "deploy"),
            (3, "2 2a2", "222"),
            (5, "a b,c d", "ab,cd"),
        ] {
            let mut state = form_with_active_field(index);
            type_text(&mut state, typed);
            assert_eq!(state.form.values[index].as_str(), kept, "field {index}");
        }
    }

    #[test]
    fn identity_file_takes_inner_spaces_but_never_at_either_edge() {
        let mut state = form_with_active_field(4);
        type_text(&mut state, " ");
        assert_eq!(state.form.values[4].as_str(), "");
        type_text(&mut state, "my");
        type_text(&mut state, " ");
        assert_eq!(
            state.form.values[4].as_str(),
            "my",
            "trailing space refused"
        );
        press(&mut state, KeyCode::Left);
        type_text(&mut state, " ");
        assert_eq!(state.form.values[4].as_str(), "m y");
        press(&mut state, KeyCode::End);
        type_text(&mut state, "key");
        assert_eq!(state.form.values[4].as_str(), "m ykey");
    }

    #[test]
    fn identity_file_edits_cannot_leave_whitespace_at_an_edge() {
        let mut state = form_with_active_field(4);
        state.form.values[4] = TextInput::for_field(HostField::IdentityFile, "a b");
        press(&mut state, KeyCode::Backspace);
        assert_eq!(state.form.values[4].as_str(), "a b", "would leave 'a '");
        press(&mut state, KeyCode::Home);
        press(&mut state, KeyCode::Delete);
        assert_eq!(state.form.values[4].as_str(), "a b", "would leave ' b'");
    }

    #[test]
    fn pasted_text_is_filtered_per_field_and_never_leaves_an_invalid_edge() {
        let mut state = form_with_active_field(5);
        state.handle_paste("bastion, jump:22\n");
        assert_eq!(state.form.values[5].as_str(), "bastion,jump:22");

        let mut state = form_with_active_field(3);
        state.handle_paste("22 22\n");
        assert_eq!(state.form.values[3].as_str(), "2222");

        let mut state = form_with_active_field(4);
        state.handle_paste("/keys/my key\n");
        assert_eq!(state.form.values[4].as_str(), "/keys/my key");
        state.handle_paste(" ");
        assert_eq!(state.form.values[4].as_str(), "/keys/my key");
        state.handle_paste(" extra ");
        assert_eq!(state.form.values[4].as_str(), "/keys/my key");
        state.handle_paste(" extra");
        assert_eq!(state.form.values[4].as_str(), "/keys/my key extra");
    }

    #[test]
    fn editing_a_host_keeps_each_field_bound_to_its_rules() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        resolve(&mut state, details("dev", Ownership::Managed));
        press(&mut state, KeyCode::Char('e'));
        press(&mut state, KeyCode::Tab);
        type_text(&mut state, " x");
        assert!(!state.form.values[1].as_str().contains(' '));
    }

    #[test]
    fn invalid_port_is_reported_on_the_port_field() {
        for port in ["0", "70000", " 22", "ssh"] {
            let mut state = test_state(Vec::new());
            press(&mut state, KeyCode::Char('a'));
            fill_form(&mut state, ["dev", "dev.example", "", port, "", ""]);

            assert_eq!(
                press(&mut state, KeyCode::Enter),
                UiAction::None,
                "{port:?}"
            );

            assert_eq!(state.form.active, 3, "{port:?}");
            assert!(state.form.error.is_some(), "{port:?}");
        }
    }

    #[test]
    fn application_rejections_land_on_the_alias_field() {
        let mut state = test_state(Vec::new());
        press(&mut state, KeyCode::Char('a'));
        press(&mut state, KeyCode::Tab);

        state.report_error(&AppError::AliasCollision("dev".into()));

        assert_eq!(state.form.active, 0);
        assert_eq!(
            state.form.error.as_deref(),
            Some("alias already exists: dev")
        );
        assert!(state.status.is_none());

        state.mode = Mode::Browse;
        state.report_error(&AppError::UnknownHost("dev".into()));
        assert_eq!(state.status.as_deref(), Some("unknown host: dev"));
    }

    #[test]
    fn a_rejected_field_is_focused_in_the_form() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        state.mode = Mode::EditForm;

        state.report_error(&AppError::Transaction(TransactionError::Validation(
            ConfigRejection::Managed {
                alias: "dev".into(),
                field: Some(HostField::Port),
                directive: "Port".into(),
                message: "Bad port '99999'.".into(),
            },
        )));

        let port = FORM_FIELDS
            .iter()
            .position(|row| row.field == HostField::Port)
            .unwrap();
        assert_eq!(state.form.active, port);
        assert_eq!(
            state.form.error.as_deref(),
            Some("Port for dev: Bad port '99999'.")
        );
    }

    #[test]
    fn a_form_test_failure_shows_its_explanation() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        press(&mut state, KeyCode::Char('a'));
        let form_finished = |outcome| Finished {
            origin: Origin::Form,
            target: TestTarget::Add(Host::new("new", "h", None, None, None, None).unwrap()),
            outcome: Ok(outcome),
        };
        let lines = |state: &UiState| -> Vec<String> {
            form_lines(
                &state.form,
                state.show_raw,
                &state.home,
                usize::MAX,
                usize::MAX,
            )
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
        };

        state.record_test(
            form_finished(explained(
                "new",
                "the host key has changed",
                &["Offending key"],
                false,
            )),
            "12:34".into(),
        );

        let shown = lines(&state);
        let at = shown
            .iter()
            .position(|line| line == "Connection test: failed \u{2014} the host key has changed")
            .unwrap_or_else(|| panic!("{shown:#?}"));
        assert_eq!(shown[at + 1], "  user: u", "{shown:#?}");
        assert_eq!(shown[at + 2], "  next: try this", "{shown:#?}");
        assert!(!shown.join("\n").contains("ssh said"), "{shown:#?}");
        assert!(state.status.is_none());

        // `o` is text in a form; Ctrl+O unfolds the output, and the footer offers it.
        assert!(footer_text(&state).contains("Ctrl+O ssh output"));
        press(&mut state, KeyCode::Char('o'));
        assert_eq!(state.form.values[0].as_str(), "o");
        assert!(state.form.test_result.is_none(), "editing clears it");
        assert!(!footer_text(&state).contains("Ctrl+O"));

        state.record_test(
            form_finished(explained(
                "new",
                "the host key has changed",
                &["Offending key"],
                false,
            )),
            "12:34".into(),
        );
        state.handle_key(ctrl('o'));
        let shown = lines(&state).join("\n");
        assert!(shown.contains("ssh said:"), "{shown}");
        assert!(shown.contains("    Offending key"), "{shown}");
        state.handle_key(ctrl('o'));
        assert!(!lines(&state).join("\n").contains("ssh said"));

        // Without raw output there is nothing to offer.
        state.record_test(
            form_finished(explained("new", "the host key has changed", &[], false)),
            "12:34".into(),
        );
        assert!(!footer_text(&state).contains("Ctrl+O"));
    }

    fn listed(state: &UiState) -> Vec<&str> {
        state.form.suggestions.as_ref().map_or(Vec::new(), |list| {
            list.items.iter().map(|item| item.alias.as_str()).collect()
        })
    }

    fn suggestion(state: &UiState) -> &Suggestions {
        state.form.suggestions.as_ref().expect("suggestions open")
    }

    fn identity_form(inventory: Option<IdentityInventory>) -> UiState {
        let mut state = test_state(Vec::new());
        state.identity_inventory = inventory;
        press(&mut state, KeyCode::Char('a'));
        for _ in 0..IDENTITY_FILE {
            press(&mut state, KeyCode::Tab);
        }
        state
    }

    fn identity_inventory() -> IdentityInventory {
        use crate::domain::identity::{Availability, InventoryFile, MissingReason};
        let file = |path: &str, availability| InventoryFile {
            configured: path.into(),
            path: None,
            public_key: None,
            availability,
            used_by: Vec::new(),
        };
        IdentityInventory {
            agents: Vec::new(),
            files: vec![
                file("~/.ssh/github.pub", Availability::InAgent),
                file(
                    "~/.ssh/gitlab",
                    Availability::Missing(MissingReason::NoSuchFile),
                ),
                file(
                    "~/.ssh/git_work",
                    Availability::KeyFile {
                        public_key_found: true,
                    },
                ),
            ],
            warnings: Vec::new(),
        }
    }

    #[test]
    fn identity_file_suggests_known_identities_with_missing_last() {
        let mut state = identity_form(Some(identity_inventory()));
        assert_eq!(state.form.active, IDENTITY_FILE);
        assert!(state.form.suggestions.is_none());
        assert!(footer_text(&state).contains("Ctrl+Space suggest"));
        type_text(&mut state, "git");
        assert_eq!(
            listed(&state),
            ["~/.ssh/git_work", "~/.ssh/github.pub", "~/.ssh/gitlab"]
        );
        press(&mut state, KeyCode::Down);
        assert_eq!(press(&mut state, KeyCode::Enter), UiAction::FormEdited);
        assert_eq!(
            state.form.values[IDENTITY_FILE].as_str(),
            "~/.ssh/github.pub"
        );
        assert_eq!(state.form.active, IDENTITY_FILE);
        assert!(state.form.suggestions.is_none());
        assert_eq!(state.mode, Mode::AddForm);
    }

    #[test]
    fn identity_suggestions_open_when_the_inventory_arrives_for_an_empty_home() {
        let home = tempfile::tempdir().unwrap();
        // Ctrl+Space before the inventory, with no key files to scan, opens it once it arrives.
        let mut state = identity_form(None);
        state.home = home.path().to_owned();
        state.handle_key(ctrl(' '));
        assert!(state.form.suggestions.is_none());
        state.finish_identities(Ok(identity_inventory()));
        assert_eq!(listed(&state).len(), 3);

        // So does what was typed meanwhile.
        let mut state = identity_form(None);
        state.home = home.path().to_owned();
        type_text(&mut state, "hub");
        assert!(state.form.suggestions.is_none());
        state.finish_identities(Ok(identity_inventory()));
        assert_eq!(listed(&state), ["~/.ssh/github.pub"]);

        // A Ctrl+Space whose field lost the focus is forgotten.
        let mut state = identity_form(None);
        state.home = home.path().to_owned();
        state.handle_key(ctrl(' '));
        press(&mut state, KeyCode::Tab);
        press(&mut state, KeyCode::BackTab);
        state.finish_identities(Ok(identity_inventory()));
        assert!(state.form.suggestions.is_none());

        // Nothing opens outside a form.
        let mut state = test_state(Vec::new());
        state.finish_identities(Ok(identity_inventory()));
        assert!(state.form.suggestions.is_none());
    }

    #[test]
    fn identity_suggestions_never_scan_without_a_home() {
        let mut state = identity_form(None);
        assert_eq!(state.home, PathBuf::new());
        state.handle_key(ctrl(' '));
        assert!(state.form.suggestions.is_none());
    }

    #[test]
    fn identity_file_ctrl_space_lists_everything_and_moving_away_closes_it() {
        let mut state = identity_form(Some(identity_inventory()));
        state.handle_key(ctrl(' '));
        assert_eq!(listed(&state).len(), 3);
        assert_eq!(suggestion(&state).field, IDENTITY_FILE);
        press(&mut state, KeyCode::Esc);
        assert!(state.form.suggestions.is_none());
        assert_eq!(state.mode, Mode::AddForm);
        press(&mut state, KeyCode::Tab);
        assert_eq!(state.form.active, PROXY_JUMP);
        assert!(state.form.suggestions.is_none());
    }

    #[test]
    fn identity_suggestions_follow_the_inventory_when_it_arrives() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(".ssh")).unwrap();
        std::fs::write(home.path().join(".ssh/scanned.pub"), "").unwrap();
        let mut state = identity_form(None);
        state.home = home.path().to_owned();
        state.handle_key(ctrl(' '));
        assert_eq!(listed(&state), ["~/.ssh/scanned.pub"]);
        state.finish_identities(Ok(identity_inventory()));
        assert_eq!(listed(&state).len(), 3);
        state.finish_identities(Err("boom".into()));
        assert_eq!(
            state.status.as_deref(),
            Some("Could not list identities: boom")
        );
    }

    #[test]
    fn jump_suggestions_do_not_open_on_focus() {
        let state = proxy_jump_form(jump_hosts());
        assert_eq!(state.form.active, PROXY_JUMP);
        assert_eq!(state.form.suggestions, None);
    }

    #[test]
    fn typing_a_jump_host_opens_matching_suggestions() {
        let mut state = proxy_jump_form(jump_hosts());
        type_text(&mut state, "bas");
        assert_eq!(listed(&state), ["bastion", "bastion-eu"]);
        assert_eq!(suggestion(&state).selected, 0);
    }

    #[test]
    fn up_and_down_move_through_suggestions_without_wrapping() {
        let mut state = proxy_jump_form(jump_hosts());
        type_text(&mut state, "bas");
        press(&mut state, KeyCode::Down);
        assert_eq!(suggestion(&state).selected, 1);
        press(&mut state, KeyCode::Down);
        assert_eq!(suggestion(&state).selected, 1);
        press(&mut state, KeyCode::Up);
        press(&mut state, KeyCode::Up);
        assert_eq!(suggestion(&state).selected, 0);
        assert_eq!(state.form.active, PROXY_JUMP);
    }

    #[test]
    fn enter_and_tab_pick_a_suggestion_without_saving() {
        let mut state = proxy_jump_form(jump_hosts());
        type_text(&mut state, "bas");
        press(&mut state, KeyCode::Down);
        assert_eq!(press(&mut state, KeyCode::Enter), UiAction::FormEdited);
        let input = &state.form.values[PROXY_JUMP];
        assert_eq!(input.as_str(), "bastion-eu");
        assert_eq!(input.cursor, "bastion-eu".len());
        assert_eq!(state.form.suggestions, None);
        assert_eq!(state.mode, Mode::AddForm);
        assert_eq!(state.form.active, PROXY_JUMP);

        let mut state = proxy_jump_form(jump_hosts());
        type_text(&mut state, "bas");
        press(&mut state, KeyCode::Tab);
        assert_eq!(state.form.values[PROXY_JUMP].as_str(), "bastion");
        assert_eq!(state.form.active, PROXY_JUMP);
        assert_eq!(state.form.suggestions, None);
    }

    #[test]
    fn esc_closes_suggestions_then_cancels_the_form() {
        let mut state = proxy_jump_form(jump_hosts());
        type_text(&mut state, "bas");
        press(&mut state, KeyCode::Esc);
        assert_eq!(state.form.suggestions, None);
        assert_eq!(state.mode, Mode::AddForm);
        press(&mut state, KeyCode::Esc);
        assert_eq!(state.mode, Mode::Browse);
    }

    #[test]
    fn enter_saves_when_suggestions_are_closed() {
        let mut state = proxy_jump_form(jump_hosts());
        fill_form(&mut state, ["app", "app.example", "", "", "", ""]);
        state.form.values[PROXY_JUMP] = TextInput::for_field(HostField::ProxyJump, "");
        type_text(&mut state, "bastion");
        assert_eq!(state.form.suggestions, None);
        let UiAction::Add(host) = press(&mut state, KeyCode::Enter) else {
            panic!("expected add action")
        };
        assert_eq!(host.proxy_jump(), Some("bastion"));
    }

    #[test]
    fn ctrl_space_lists_every_candidate_for_an_empty_hop() {
        let mut state = proxy_jump_form(vec![
            summary("b", Ownership::Managed),
            summary("a", Ownership::Managed),
        ]);
        state.handle_key(ctrl(' '));
        assert_eq!(listed(&state), ["a", "b"]);
        assert!(suggestion(&state).forced);
        press(&mut state, KeyCode::Backspace);
        assert_eq!(listed(&state), ["a", "b"]);
    }

    #[test]
    fn exact_alias_closes_suggestions() {
        let mut state = proxy_jump_form(vec![summary("bastion", Ownership::Managed)]);
        type_text(&mut state, "bastio");
        assert_eq!(listed(&state), ["bastion"]);
        type_text(&mut state, "n");
        assert_eq!(state.form.suggestions, None);
    }

    #[test]
    fn no_other_hosts_means_no_suggestions() {
        let mut state = proxy_jump_form(Vec::new());
        type_text(&mut state, "b");
        assert_eq!(state.form.suggestions, None);
        state.handle_key(ctrl(' '));
        assert_eq!(state.form.suggestions, None);
    }

    #[test]
    fn edit_form_does_not_suggest_its_own_host() {
        let mut state = test_state(vec![
            summary("dev", Ownership::Managed),
            summary("devbox", Ownership::Managed),
        ]);
        resolve(&mut state, details("dev", Ownership::Managed));
        press(&mut state, KeyCode::Char('e'));
        assert_eq!(state.mode, Mode::EditForm);
        state.form.active = PROXY_JUMP;
        state.form.values[PROXY_JUMP] = TextInput::for_field(HostField::ProxyJump, "");
        type_text(&mut state, "dev");
        assert_eq!(listed(&state), ["devbox"]);
    }

    #[test]
    fn picking_in_a_chain_replaces_only_the_current_hop() {
        let mut state = proxy_jump_form(vec![
            summary("bastion", Ownership::Managed),
            summary("prod-jump", Ownership::Managed),
            summary("web", Ownership::Managed),
        ]);
        type_text(&mut state, "bastion,pro");
        assert_eq!(listed(&state), ["prod-jump"]);
        press(&mut state, KeyCode::Enter);
        assert_eq!(state.form.values[PROXY_JUMP].as_str(), "bastion,prod-jump");
        type_text(&mut state, ",");
        assert_eq!(state.form.suggestions, None);
        state.handle_key(ctrl(' '));
        assert_eq!(listed(&state), ["web"]);
    }

    #[test]
    fn paste_into_proxy_jump_opens_suggestions() {
        let mut state = proxy_jump_form(vec![summary("bastion", Ownership::Managed)]);
        assert_eq!(state.handle_paste("bas"), UiAction::FormEdited);
        assert_eq!(listed(&state), ["bastion"]);
    }

    #[test]
    fn leaving_proxy_jump_closes_suggestions() {
        let mut state = proxy_jump_form(jump_hosts());
        type_text(&mut state, "bas");
        press(&mut state, KeyCode::BackTab);
        assert_eq!(state.form.suggestions, None);
        assert_eq!(state.form.active, PROXY_JUMP - 1);

        let mut state = proxy_jump_form(jump_hosts());
        type_text(&mut state, "bas");
        state.handle_key(ctrl('t'));
        assert_eq!(state.form.suggestions, None);
    }

    #[test]
    fn replacing_hosts_recomputes_an_open_dropdown() {
        let mut state = proxy_jump_form(jump_hosts());
        type_text(&mut state, "bas");
        press(&mut state, KeyCode::Down);
        state.replace_hosts(HostInventory {
            hosts: vec![summary("bastion", Ownership::Managed)],
            warnings: Vec::new(),
            inputs: Vec::new(),
        });
        assert_eq!(listed(&state), ["bastion"]);
        assert_eq!(suggestion(&state).selected, 0);
        state.replace_hosts(HostInventory {
            hosts: vec![summary("web", Ownership::Managed)],
            warnings: Vec::new(),
            inputs: Vec::new(),
        });
        assert_eq!(state.form.suggestions, None);
    }

    #[test]
    fn dropdown_renders_aliases_and_origins_under_proxy_jump() {
        let mut state = proxy_jump_form(jump_hosts());
        type_text(&mut state, "bas");
        let text = rendered(&state);
        assert!(text.contains("│> bastion    │  managed"), "{text}");
        assert!(text.contains("│  bastion-eu │"), "{text}");
        let lines = text.lines().collect::<Vec<_>>();
        let row = lines
            .iter()
            .position(|line| line.contains("> ProxyJump:"))
            .unwrap();
        assert!(lines[row + 1].contains('┌'), "{text}");
        assert!(lines[row + 4].contains('└'), "{text}");
    }

    #[test]
    fn dropdown_scrolls_to_keep_the_selection_visible() {
        let hosts = (1..=7)
            .map(|n| summary(&format!("h{n}"), Ownership::Managed))
            .collect();
        let mut state = proxy_jump_form(hosts);
        state.handle_key(ctrl(' '));
        for _ in 0..6 {
            press(&mut state, KeyCode::Down);
        }
        assert_eq!(
            (suggestion(&state).selected, suggestion(&state).offset),
            (6, 2)
        );
        let text = rendered(&state);
        assert!(text.contains("│> h7 │"), "{text}");
        assert!(!text.contains("│  h1 │"), "{text}");
        assert!(text.contains("│  h3 │"), "{text}");
        press(&mut state, KeyCode::Down);
        assert_eq!(suggestion(&state).selected, 6);
        for _ in 0..6 {
            press(&mut state, KeyCode::Up);
        }
        assert_eq!(
            (suggestion(&state).selected, suggestion(&state).offset),
            (0, 0)
        );
    }

    #[test]
    fn typing_an_exact_alias_after_ctrl_space_closes_suggestions() {
        let mut state = proxy_jump_form(jump_hosts());
        state.handle_key(ctrl(' '));
        type_text(&mut state, "bastion");
        assert_eq!(state.form.suggestions, None);
    }

    #[test]
    fn picking_the_alias_already_typed_is_not_an_edit() {
        let mut state = proxy_jump_form(jump_hosts());
        type_text(&mut state, "bastion");
        state.handle_key(ctrl(' '));
        assert_eq!(listed(&state), ["bastion", "bastion-eu"]);
        assert_eq!(press(&mut state, KeyCode::Enter), UiAction::None);
        assert_eq!(state.form.values[PROXY_JUMP].as_str(), "bastion");
        assert_eq!(state.form.suggestions, None);
    }

    #[test]
    fn picking_after_multibyte_text_keeps_earlier_hops() {
        let mut state = proxy_jump_form(vec![summary("bastion", Ownership::Managed)]);
        type_text(&mut state, "café,bas");
        assert_eq!(listed(&state), ["bastion"]);
        press(&mut state, KeyCode::Enter);
        let input = &state.form.values[PROXY_JUMP];
        assert_eq!(input.as_str(), "café,bastion");
        assert_eq!(input.cursor, input.as_str().len());
    }
}
