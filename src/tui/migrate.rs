//! The migrate screen: a checklist of hosts to adopt or export, then the exact preview.

use std::{
    cell::Cell,
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

use super::{TextInput, display_width, keep_start};
use crate::{
    adopt::{Candidate, Direction, MigrationPlan},
    fuzzy::{highlight, match_fields},
    picker::HIT,
};

/// Lines PageUp and PageDown scroll the preview by.
const PAGE: usize = 10;

#[derive(Clone, Debug)]
pub(super) struct MigrateState {
    pub plan: MigrationPlan,
    /// Indexes of the checked candidates.
    selected: BTreeSet<usize>,
    cursor: usize,
    /// The first checklist row shown, moved by rendering only as far as keeps the cursor visible.
    offset: Cell<usize>,
    /// The rendered preview of the checked hosts and its first visible line, while it is shown.
    preview: Option<(String, usize)>,
    /// Shown as `~` in file paths.
    home: Option<PathBuf>,
    filter: TextInput,
    editing_filter: bool,
    filter_before_edit: String,
    /// Indexes of the candidates the filter keeps, in checklist order.
    visible: Vec<usize>,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) enum MigrateAction {
    None,
    Close,
    Apply(Vec<String>),
    /// The preview could not be built; the checklist stays open.
    Error(String),
}

impl MigrateState {
    /// The checklist of `plan` with the cursor on `preselect`, checked when it is eligible.
    pub(super) fn new(plan: MigrationPlan, preselect: Option<&str>) -> Self {
        let found = preselect.and_then(|alias| {
            plan.candidates
                .iter()
                .position(|candidate| candidate.alias.eq_ignore_ascii_case(alias))
        });
        let selected = found
            .filter(|&index| plan.candidates[index].reason.is_none())
            .into_iter()
            .collect();
        Self {
            selected,
            cursor: found.unwrap_or(0),
            offset: Cell::new(0),
            preview: None,
            home: None,
            visible: (0..plan.candidates.len()).collect(),
            filter: TextInput::default(),
            editing_filter: false,
            filter_before_edit: String::new(),
            plan,
        }
    }

    /// Applies a new filter, keeping the cursor on its row when it still matches and otherwise
    /// moving it to the first match.
    fn set_filter(&mut self, filter: TextInput) {
        self.filter = filter;
        self.visible = (0..self.plan.candidates.len())
            .filter(|&index| {
                match_fields(self.filter.as_str(), &[&self.plan.candidates[index].alias]).is_some()
            })
            .collect();
        if !self.visible.contains(&self.cursor) {
            self.cursor = self.visible.first().copied().unwrap_or(0);
        }
    }

    /// Pasted text goes into the filter while it is edited.
    pub(super) fn paste(&mut self, text: &str) {
        if self.editing_filter {
            let mut filter = self.filter.clone();
            filter.paste(text);
            self.set_filter(filter);
        }
    }

    pub(super) fn with_home(mut self, home: &Path) -> Self {
        self.home = Some(home.into());
        self
    }

    pub(super) fn handle_key(&mut self, key: KeyEvent) -> MigrateAction {
        if let Some((preview, scroll)) = &mut self.preview {
            let last = preview.lines().count().saturating_sub(1);
            match key.code {
                KeyCode::Up => *scroll = scroll.saturating_sub(1),
                KeyCode::Down => *scroll = (*scroll + 1).min(last),
                KeyCode::PageUp => *scroll = scroll.saturating_sub(PAGE),
                KeyCode::PageDown => *scroll = (*scroll + PAGE).min(last),
                KeyCode::Char('y') => return MigrateAction::Apply(self.selected_aliases()),
                KeyCode::Esc => self.preview = None,
                _ => {}
            }
            return MigrateAction::None;
        }
        if self.editing_filter {
            match key.code {
                KeyCode::Enter => self.editing_filter = false,
                KeyCode::Esc => {
                    self.editing_filter = false;
                    let before = std::mem::take(&mut self.filter_before_edit);
                    self.set_filter(TextInput::new(&before));
                }
                _ => {
                    let mut filter = self.filter.clone();
                    if filter.handle_key(key) {
                        self.set_filter(filter);
                    }
                }
            }
            return MigrateAction::None;
        }
        let place = self.visible.iter().position(|&index| index == self.cursor);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(place) = place {
                    self.cursor = self.visible[place.saturating_sub(1)];
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(place) = place {
                    self.cursor = self.visible[(place + 1).min(self.visible.len() - 1)];
                }
            }
            KeyCode::Char(' ') => {
                if place.is_some()
                    && self.plan.candidates[self.cursor].reason.is_none()
                    && !self.selected.remove(&self.cursor)
                {
                    self.selected.insert(self.cursor);
                }
            }
            KeyCode::Char('a') => {
                let candidates = &self.plan.candidates;
                let eligible: Vec<usize> = self
                    .visible
                    .iter()
                    .copied()
                    .filter(|&index| candidates[index].reason.is_none())
                    .collect();
                // Like the CLI checklist: unmark them when every one is already marked.
                if eligible.iter().all(|index| self.selected.contains(index)) {
                    for index in &eligible {
                        self.selected.remove(index);
                    }
                } else {
                    self.selected.extend(eligible);
                }
            }
            KeyCode::Char('/') => {
                self.filter_before_edit = self.filter.as_str().to_owned();
                self.filter = TextInput::new(&self.filter_before_edit);
                self.editing_filter = true;
            }
            KeyCode::Enter if !self.selected.is_empty() => {
                match self.plan.preview(&self.selected_aliases()) {
                    Ok(preview) => self.preview = Some((preview.render(self.home.as_deref()), 0)),
                    Err(error) => return MigrateAction::Error(error.to_string()),
                }
            }
            KeyCode::Esc if !self.filter.is_empty() => self.set_filter(TextInput::default()),
            KeyCode::Esc => return MigrateAction::Close,
            _ => {}
        }
        MigrateAction::None
    }

    /// The checked aliases in checklist order.
    fn selected_aliases(&self) -> Vec<String> {
        self.selected
            .iter()
            .map(|&index| self.plan.candidates[index].alias.clone())
            .collect()
    }

    /// The pane's lines for `width` columns and `height` rows inside its border: the rows of
    /// the checklist that fit, each cut to the width and the cursor row reversed across the
    /// pane, or the preview from its scroll offset.
    pub(super) fn lines(&self, width: usize, height: usize) -> Vec<Line<'static>> {
        if let Some((preview, scroll)) = &self.preview {
            return preview
                .lines()
                .skip(*scroll)
                .map(|line| Line::raw(line.to_owned()))
                .collect();
        }
        if self.plan.candidates.is_empty() {
            return vec![Line::raw(match self.plan.direction {
                Direction::Adopt => "No host can be adopted.",
                Direction::Export => "No host can be exported.",
            })];
        }
        if self.visible.is_empty() {
            return vec![Line::raw(format!(
                "No hosts match \"{}\".",
                self.filter.as_str()
            ))];
        }
        let alias_width = self
            .plan
            .candidates
            .iter()
            .map(|candidate| candidate.alias.chars().count())
            .max()
            .unwrap_or(0);
        let count = self.visible.len();
        let cursor = self
            .visible
            .iter()
            .position(|&index| index == self.cursor)
            .unwrap_or(0);
        let height = height.max(1);
        let offset = self
            .offset
            .get()
            .min(cursor)
            .max((cursor + 1).saturating_sub(height))
            .min(count.saturating_sub(height));
        self.offset.set(offset);
        self.visible
            .iter()
            .skip(offset)
            .take(height)
            .map(|&index| {
                let candidate = &self.plan.candidates[index];
                let alias = &candidate.alias;
                let (text, mut style) = match &candidate.reason {
                    Some(reason) => (
                        format!("    {alias:<alias_width$}  {reason}"),
                        Style::default().add_modifier(Modifier::DIM),
                    ),
                    None => (
                        format!(
                            "[{}] {alias:<alias_width$}  {}",
                            if self.selected.contains(&index) {
                                'x'
                            } else {
                                ' '
                            },
                            self.source(candidate)
                        ),
                        Style::default(),
                    ),
                };
                // Rows never wrap, so each takes exactly one line of the pane.
                let cut = keep_start(&text, width);
                let mut line = if cut == text {
                    self.highlighted(&text, &candidate.alias)
                } else {
                    Line::raw(cut.clone())
                };
                if index == self.cursor {
                    style = style.add_modifier(Modifier::REVERSED);
                    let padding = width.saturating_sub(display_width(&cut));
                    line.spans.push(Span::raw(" ".repeat(padding)));
                }
                line.style(style)
            })
            .collect()
    }

    /// A checklist row's `text` with the filter's matches highlighted in its `alias`, which
    /// starts at the fifth character.
    fn highlighted(&self, text: &str, alias: &str) -> Line<'static> {
        let positions = match_fields(self.filter.as_str(), &[alias])
            .and_then(|found| found.positions.into_iter().next())
            .unwrap_or_default();
        let prefix: String = text.chars().take(4).collect();
        let rest: String = text.chars().skip(4 + alias.chars().count()).collect();
        let mut spans = vec![Span::raw(prefix)];
        spans.extend(highlight(alias, &positions, Style::default(), HIT));
        spans.push(Span::raw(rest));
        Line::from(spans)
    }

    /// Where an eligible host is declared: its file for adoption, `managed` for export.
    fn source(&self, candidate: &Candidate) -> String {
        match self.plan.direction {
            Direction::Export => "managed".into(),
            Direction::Adopt => match self.home.as_deref() {
                Some(home) => crate::home::tilde(&candidate.source, home),
                None => candidate.source.display().to_string(),
            },
        }
    }

    pub(super) fn title(&self) -> Line<'static> {
        let mut title = vec![Span::raw(match self.plan.direction {
            Direction::Adopt => "Adopt hosts /",
            Direction::Export => "Export hosts /",
        })];
        title.extend(self.filter.spans(self.editing_filter));
        title.push(Span::raw(format!("  {} marked", self.selected.len())));
        Line::from(title)
    }

    pub(super) fn footer(&self) -> String {
        if self.preview.is_some() {
            "↑↓ scroll  y apply  Esc back".into()
        } else if self.editing_filter {
            "type to filter  ←→ move  Enter apply  Esc cancel".into()
        } else if self.filter.is_empty() {
            "↑↓ move  Space toggle  a all  / filter  Enter preview  Esc cancel".into()
        } else {
            "↑↓ move  Space toggle  a all  / filter  Enter preview  Esc clear filter".into()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{
        Terminal,
        backend::TestBackend,
        style::Modifier,
        widgets::{Block, Borders, Paragraph},
    };

    use crate::adopt::{Candidate, Direction, MigrationPlan};

    use super::*;

    fn candidate(alias: &str, reason: Option<&str>) -> Candidate {
        Candidate {
            alias: alias.into(),
            source: PathBuf::from("/home/test/.ssh/config"),
            reason: reason.map(str::to_owned),
        }
    }

    /// `web` and `db` can be adopted, `wild` cannot.
    fn adoption(preselect: Option<&str>) -> MigrateState {
        let plan = MigrationPlan::for_tests(
            Direction::Adopt,
            vec![
                candidate("web", None),
                candidate("wild", Some("its Host line has a wildcard")),
                candidate("db", None),
            ],
        );
        MigrateState::new(plan, preselect).with_home(Path::new("/home/test"))
    }

    fn press(state: &mut MigrateState, code: KeyCode) -> MigrateAction {
        state.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn text(state: &MigrateState) -> Vec<String> {
        state
            .lines(60, usize::MAX)
            .iter()
            .map(|line| line.to_string().trim_end().to_owned())
            .collect()
    }

    /// The right pane as the interface draws it.
    fn pane(state: &MigrateState) -> String {
        let mut terminal = Terminal::new(TestBackend::new(64, 14)).unwrap();
        terminal
            .draw(|frame| {
                let paragraph = Paragraph::new(state.lines(62, 12))
                    .block(Block::default().borders(Borders::ALL).title(state.title()));
                frame.render_widget(paragraph, frame.area());
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn type_text(state: &mut MigrateState, text: &str) {
        for character in text.chars() {
            assert_eq!(press(state, KeyCode::Char(character)), MigrateAction::None);
        }
    }

    /// Applies `filter` with `/`, typing and Enter.
    fn filter(state: &mut MigrateState, filter: &str) {
        press(state, KeyCode::Char('/'));
        type_text(state, filter);
        press(state, KeyCode::Enter);
    }

    /// The aliases of the rows shown, in order.
    fn shown(state: &MigrateState) -> Vec<String> {
        text(state)
            .iter()
            .map(|line| {
                line.trim_start_matches(['[', ']', 'x', ' '])
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect()
    }

    /// The row the cursor is on, among the rows shown.
    fn cursor_row(state: &MigrateState) -> usize {
        state
            .lines(60, usize::MAX)
            .iter()
            .position(|line| line.style.add_modifier.contains(Modifier::REVERSED))
            .unwrap()
    }

    #[test]
    fn slash_filters_the_checklist() {
        let mut state = adoption(None);
        filter(&mut state, "w");
        assert_eq!(shown(&state), ["web", "wild"]);
    }

    #[test]
    fn filter_edit_keys_match_the_hosts_filter() {
        let mut state = adoption(None);
        filter(&mut state, "d");
        assert_eq!(shown(&state), ["wild", "db"]);
        press(&mut state, KeyCode::Char('/'));
        type_text(&mut state, "b");
        assert_eq!(shown(&state), ["db"]);
        press(&mut state, KeyCode::Esc);
        assert_eq!(shown(&state), ["wild", "db"]);
    }

    #[test]
    fn esc_clears_filter_before_closing() {
        let mut state = adoption(None);
        filter(&mut state, "db");
        assert_eq!(press(&mut state, KeyCode::Esc), MigrateAction::None);
        assert_eq!(shown(&state), ["web", "wild", "db"]);
        assert_eq!(press(&mut state, KeyCode::Esc), MigrateAction::Close);
    }

    #[test]
    fn marks_survive_filter_changes() {
        let mut state = adoption(None);
        press(&mut state, KeyCode::Char(' '));
        filter(&mut state, "db");
        press(&mut state, KeyCode::Esc);
        assert!(text(&state)[0].starts_with("[x] web"), "{:?}", text(&state));
    }

    #[test]
    fn a_marks_only_visible_eligible_rows() {
        let mut state = adoption(None);
        filter(&mut state, "d");
        press(&mut state, KeyCode::Char('a'));
        press(&mut state, KeyCode::Enter);
        assert_eq!(
            press(&mut state, KeyCode::Char('y')),
            MigrateAction::Apply(vec!["db".into()])
        );
    }

    #[test]
    fn enter_previews_hidden_marks() {
        let mut state = adoption(None);
        press(&mut state, KeyCode::Char(' '));
        filter(&mut state, "db");
        press(&mut state, KeyCode::Char(' '));
        press(&mut state, KeyCode::Enter);
        assert_eq!(
            press(&mut state, KeyCode::Char('y')),
            MigrateAction::Apply(vec!["web".into(), "db".into()])
        );
    }

    #[test]
    fn cursor_stays_or_moves_to_first_match() {
        let mut state = adoption(None);
        press(&mut state, KeyCode::Down);
        press(&mut state, KeyCode::Down);
        filter(&mut state, "d");
        assert_eq!(shown(&state)[cursor_row(&state)], "db");
        press(&mut state, KeyCode::Esc);
        filter(&mut state, "web");
        assert_eq!(shown(&state)[cursor_row(&state)], "web");
    }

    #[test]
    fn jk_move_through_matches_only() {
        let mut state = adoption(None);
        filter(&mut state, "w");
        press(&mut state, KeyCode::Char('j'));
        press(&mut state, KeyCode::Char('j'));
        assert_eq!(shown(&state)[cursor_row(&state)], "wild");
        press(&mut state, KeyCode::Char('k'));
        assert_eq!(shown(&state)[cursor_row(&state)], "web");
    }

    #[test]
    fn no_match_message() {
        let mut state = adoption(None);
        filter(&mut state, "zzz");
        assert_eq!(text(&state), ["No hosts match \"zzz\"."]);
        assert_eq!(press(&mut state, KeyCode::Char(' ')), MigrateAction::None);
    }

    #[test]
    fn title_and_footer_show_the_filter() {
        let mut state = adoption(Some("web"));
        assert_eq!(state.title().to_string(), "Adopt hosts /  1 marked");
        assert!(state.footer().contains("/ filter"));
        assert!(state.footer().ends_with("Esc cancel"));
        filter(&mut state, "d");
        assert_eq!(state.title().to_string(), "Adopt hosts /d  1 marked");
        assert!(state.footer().ends_with("Esc clear filter"));
        press(&mut state, KeyCode::Char('/'));
        assert!(state.footer().starts_with("type to filter"));
    }

    #[test]
    fn paste_goes_to_the_filter_while_editing() {
        let mut state = adoption(None);
        state.paste("db");
        assert_eq!(shown(&state), ["web", "wild", "db"]);
        press(&mut state, KeyCode::Char('/'));
        state.paste("db\n");
        assert_eq!(shown(&state), ["db"]);
    }

    #[test]
    fn space_toggles_only_eligible_rows() {
        let mut state = adoption(None);
        press(&mut state, KeyCode::Char(' '));
        assert_eq!(text(&state)[0], "[x] web   ~/.ssh/config");
        press(&mut state, KeyCode::Down);
        press(&mut state, KeyCode::Char(' '));
        assert_eq!(text(&state)[1], "    wild  its Host line has a wildcard");
        press(&mut state, KeyCode::Char('k'));
        press(&mut state, KeyCode::Char(' '));
        assert_eq!(text(&state)[0], "[ ] web   ~/.ssh/config");

        let lines = state.lines(60, usize::MAX);
        assert!(lines[1].style.add_modifier.contains(Modifier::DIM));
        assert!(lines[0].style.add_modifier.contains(Modifier::REVERSED));
        assert!(!lines[2].style.add_modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn a_selects_every_eligible_host() {
        let mut state = adoption(None);
        press(&mut state, KeyCode::Char('a'));
        assert_eq!(
            text(&state),
            [
                "[x] web   ~/.ssh/config",
                "    wild  its Host line has a wildcard",
                "[x] db    ~/.ssh/config",
            ]
        );
        press(&mut state, KeyCode::Enter);
        assert_eq!(
            press(&mut state, KeyCode::Char('y')),
            MigrateAction::Apply(vec!["web".into(), "db".into()])
        );
    }

    #[test]
    fn a_again_unmarks_the_visible_hosts() {
        let mut state = adoption(None);
        press(&mut state, KeyCode::Char(' '));
        filter(&mut state, "d");
        press(&mut state, KeyCode::Char('a'));
        press(&mut state, KeyCode::Char('a'));
        press(&mut state, KeyCode::Enter);
        assert_eq!(
            press(&mut state, KeyCode::Char('y')),
            MigrateAction::Apply(vec!["web".into()])
        );
    }

    #[test]
    fn enter_without_selection_stays_on_the_checklist() {
        let mut state = adoption(None);
        assert_eq!(press(&mut state, KeyCode::Enter), MigrateAction::None);
        assert_eq!(
            state.footer(),
            "↑↓ move  Space toggle  a all  / filter  Enter preview  Esc cancel"
        );
        assert_eq!(press(&mut state, KeyCode::Char('y')), MigrateAction::None);
    }

    #[test]
    fn y_in_preview_applies_the_selection() {
        let mut state = adoption(None);
        press(&mut state, KeyCode::Char('j'));
        press(&mut state, KeyCode::Char('j'));
        press(&mut state, KeyCode::Char(' '));
        press(&mut state, KeyCode::Enter);
        assert_eq!(state.footer(), "↑↓ scroll  y apply  Esc back");
        assert!(state.title().to_string().starts_with("Adopt hosts /"));
        let preview = text(&state);
        assert!(preview.contains(&"~/.ssh/config".to_owned()), "{preview:?}");
        assert!(preview.contains(&"  -4: Host db".to_owned()), "{preview:?}");
        assert_eq!(
            press(&mut state, KeyCode::Char('y')),
            MigrateAction::Apply(vec!["db".into()])
        );
    }

    #[test]
    fn preview_scrolls_by_line_and_page() {
        let mut state = adoption(None);
        press(&mut state, KeyCode::Char('a'));
        press(&mut state, KeyCode::Enter);
        let full = text(&state);
        press(&mut state, KeyCode::Down);
        assert_eq!(text(&state), full[1..]);
        press(&mut state, KeyCode::Up);
        press(&mut state, KeyCode::Up);
        assert_eq!(text(&state), full);
        press(&mut state, KeyCode::PageDown);
        assert_eq!(text(&state), full[10.min(full.len() - 1)..]);
        press(&mut state, KeyCode::PageDown);
        press(&mut state, KeyCode::PageDown);
        assert_eq!(text(&state), full[full.len() - 1..]);
        press(&mut state, KeyCode::PageUp);
        press(&mut state, KeyCode::PageUp);
        press(&mut state, KeyCode::PageUp);
        assert_eq!(text(&state), full);
    }

    #[test]
    fn esc_steps_back_then_closes() {
        let mut state = adoption(Some("web"));
        press(&mut state, KeyCode::Enter);
        assert_eq!(state.footer(), "↑↓ scroll  y apply  Esc back");
        assert_eq!(press(&mut state, KeyCode::Esc), MigrateAction::None);
        assert_eq!(text(&state)[0], "[x] web   ~/.ssh/config");
        assert_eq!(press(&mut state, KeyCode::Esc), MigrateAction::Close);
    }

    #[test]
    fn preselected_host_is_checked() {
        let state = adoption(Some("DB"));
        assert_eq!(text(&state)[2], "[x] db    ~/.ssh/config");
        assert!(
            state.lines(60, usize::MAX)[2]
                .style
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        // An ineligible host gets the cursor but stays unchecked.
        let state = adoption(Some("wild"));
        assert!(text(&state).iter().all(|line| !line.starts_with("[x]")));
        assert!(
            state.lines(60, usize::MAX)[1]
                .style
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn export_lists_managed_hosts_and_previews_the_move() {
        let plan = MigrationPlan::for_tests(
            Direction::Export,
            vec![Candidate {
                alias: "dev".into(),
                source: PathBuf::from("/home/test/.ssh/portico/config"),
                reason: None,
            }],
        );
        let mut state = MigrateState::new(plan, Some("dev")).with_home(Path::new("/home/test"));
        assert!(state.title().to_string().starts_with("Export hosts /"));
        assert_eq!(text(&state), ["[x] dev  managed"]);
        press(&mut state, KeyCode::Enter);
        let preview = text(&state);
        assert!(
            preview.contains(&"  +2: Host dev".to_owned()),
            "{preview:?}"
        );
    }

    #[test]
    fn a_preview_error_is_reported_and_keeps_the_checklist() {
        let mut state = adoption(Some("web"));
        state.plan.candidates[0].alias = "gone".into();
        assert!(matches!(
            press(&mut state, KeyCode::Enter),
            MigrateAction::Error(message) if message.contains("gone")
        ));
        assert_eq!(
            state.footer(),
            "↑↓ move  Space toggle  a all  / filter  Enter preview  Esc cancel"
        );
    }

    #[test]
    fn checklist_scrolls_to_keep_the_cursor_visible() {
        let mut candidates = (0..30)
            .map(|index| candidate(&format!("host{index:02}"), None))
            .collect::<Vec<_>>();
        candidates[29].reason = Some("its Host line has a wildcard, so it cannot move".into());
        let plan = MigrationPlan::for_tests(Direction::Adopt, candidates);
        let mut state = MigrateState::new(plan, Some("host27")).with_home(Path::new("/home/test"));
        let window = |state: &MigrateState| {
            let lines = state.lines(30, 10);
            assert_eq!(lines.len(), 10);
            assert!(lines.iter().all(|line| line.width() <= 30), "{lines:?}");
            let cursor = lines
                .iter()
                .position(|line| line.style.add_modifier.contains(Modifier::REVERSED))
                .expect("the cursor row is visible");
            let text = lines
                .iter()
                .map(|line| line.to_string().trim_end().to_owned())
                .collect::<Vec<_>>();
            (text, cursor)
        };

        let (rows, cursor) = window(&state);
        assert_eq!(rows[cursor], "[x] host27  ~/.ssh/config");
        press(&mut state, KeyCode::Down);
        press(&mut state, KeyCode::Down);
        let (rows, cursor) = window(&state);
        assert_eq!(rows[cursor], "    host29  its Host line has…");
        assert_eq!(cursor, 9);
        // Moving up keeps the window until the cursor reaches its top.
        press(&mut state, KeyCode::Up);
        let (rows, cursor) = window(&state);
        assert_eq!((rows[0].as_str(), cursor), ("[ ] host20  ~/.ssh/config", 8));
        for _ in 0..30 {
            press(&mut state, KeyCode::Up);
        }
        let (rows, cursor) = window(&state);
        assert_eq!((rows[0].as_str(), cursor), ("[ ] host00  ~/.ssh/config", 0));
    }

    #[test]
    fn panes_render_the_checklist_and_the_preview() {
        let mut state = adoption(Some("web"));
        let checklist = pane(&state);
        assert!(checklist.contains("Adopt hosts"));
        assert!(checklist.contains("[x] web   ~/.ssh/config"));
        press(&mut state, KeyCode::Enter);
        let preview = pane(&state);
        assert!(preview.contains("  -1: Host web"));
    }
}
