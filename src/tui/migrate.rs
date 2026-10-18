//! The migrate screen: a checklist of hosts to adopt or export, then the exact preview.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    style::{Modifier, Style},
    text::Line,
};

use crate::adopt::{Candidate, Direction, MigrationPlan};

/// Lines PageUp and PageDown scroll the preview by.
const PAGE: usize = 10;

#[derive(Clone, Debug)]
pub(super) struct MigrateState {
    pub plan: MigrationPlan,
    /// Indexes of the checked candidates.
    selected: BTreeSet<usize>,
    cursor: usize,
    /// The rendered preview of the checked hosts and its first visible line, while it is shown.
    preview: Option<(String, usize)>,
    /// Shown as `~` in file paths.
    home: Option<PathBuf>,
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
            plan,
            selected,
            cursor: found.unwrap_or(0),
            preview: None,
            home: None,
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
        let count = self.plan.candidates.len();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.cursor = (self.cursor + 1).min(count.saturating_sub(1));
            }
            KeyCode::Char(' ') => {
                if self
                    .plan
                    .candidates
                    .get(self.cursor)
                    .is_some_and(|candidate| candidate.reason.is_none())
                    && !self.selected.remove(&self.cursor)
                {
                    self.selected.insert(self.cursor);
                }
            }
            KeyCode::Char('a') => {
                self.selected = (0..count)
                    .filter(|&index| self.plan.candidates[index].reason.is_none())
                    .collect();
            }
            KeyCode::Enter if !self.selected.is_empty() => {
                match self.plan.preview(&self.selected_aliases()) {
                    Ok(preview) => self.preview = Some((preview.render(self.home.as_deref()), 0)),
                    Err(error) => return MigrateAction::Error(error.to_string()),
                }
            }
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

    /// The pane's lines for `width` columns inside its border: the checklist with the cursor
    /// row reversed across the pane, or the preview from its scroll offset.
    pub(super) fn lines(&self, width: usize) -> Vec<Line<'static>> {
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
        let alias_width = self
            .plan
            .candidates
            .iter()
            .map(|candidate| candidate.alias.chars().count())
            .max()
            .unwrap_or(0);
        self.plan
            .candidates
            .iter()
            .enumerate()
            .map(|(index, candidate)| {
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
                if index == self.cursor {
                    style = style.add_modifier(Modifier::REVERSED);
                    let padding = width.saturating_sub(text.chars().count());
                    return Line::styled(format!("{text}{}", " ".repeat(padding)), style);
                }
                Line::styled(text, style)
            })
            .collect()
    }

    /// Where an eligible host is declared: its file for adoption, `managed` for export.
    fn source(&self, candidate: &Candidate) -> String {
        match self.plan.direction {
            Direction::Export => "managed".into(),
            Direction::Adopt => match self
                .home
                .as_deref()
                .and_then(|home| candidate.source.strip_prefix(home).ok())
            {
                Some(rest) => format!("~/{}", rest.display()),
                None => candidate.source.display().to_string(),
            },
        }
    }

    pub(super) fn title(&self) -> &'static str {
        match self.plan.direction {
            Direction::Adopt => "Adopt hosts",
            Direction::Export => "Export hosts",
        }
    }

    pub(super) fn footer(&self) -> &'static str {
        if self.preview.is_some() {
            "↑↓ scroll  y apply  Esc back"
        } else {
            "↑↓ move  Space toggle  a all  Enter preview  Esc cancel"
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
            .lines(60)
            .iter()
            .map(|line| line.to_string().trim_end().to_owned())
            .collect()
    }

    /// The right pane as the interface draws it.
    fn pane(state: &MigrateState) -> String {
        let mut terminal = Terminal::new(TestBackend::new(64, 14)).unwrap();
        terminal
            .draw(|frame| {
                let paragraph = Paragraph::new(state.lines(62))
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

        let lines = state.lines(60);
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
    fn enter_without_selection_stays_on_the_checklist() {
        let mut state = adoption(None);
        assert_eq!(press(&mut state, KeyCode::Enter), MigrateAction::None);
        assert_eq!(
            state.footer(),
            "↑↓ move  Space toggle  a all  Enter preview  Esc cancel"
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
        assert_eq!(state.title(), "Adopt hosts");
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
            state.lines(60)[2]
                .style
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        // An ineligible host gets the cursor but stays unchecked.
        let state = adoption(Some("wild"));
        assert!(text(&state).iter().all(|line| !line.starts_with("[x]")));
        assert!(
            state.lines(60)[1]
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
        assert_eq!(state.title(), "Export hosts");
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
            "↑↓ move  Space toggle  a all  Enter preview  Esc cancel"
        );
    }

    #[test]
    fn panes_render_the_checklist_and_the_preview() {
        let mut state = adoption(Some("web"));
        let checklist = pane(&state);
        println!("{checklist}");
        assert!(checklist.contains("Adopt hosts"));
        assert!(checklist.contains("[x] web   ~/.ssh/config"));
        press(&mut state, KeyCode::Enter);
        let preview = pane(&state);
        println!("{preview}");
        assert!(preview.contains("  -1: Host web"));
    }
}
