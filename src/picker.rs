//! The inline pickers the CLI opens when a command needs a host or a container and none was
//! given: a single select that ranks rows as the user types, and a multi select with the TUI
//! checklist's keys.

use std::{
    cell::Cell,
    collections::BTreeSet,
    io::{self, Stderr},
};

use crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal, TerminalOptions, Viewport,
    backend::CrosstermBackend,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::{
    fuzzy::{Match, highlight, match_fields, rank},
    tui::TextInput,
};

/// The most lines a picker takes, its prompt line included.
pub const MAX_HEIGHT: u16 = 12;
const PAGE: usize = MAX_HEIGHT as usize - 1;

/// How matched characters are shown, in the pickers and the TUI filters.
pub(crate) const HIT: Style = Style::new().add_modifier(Modifier::BOLD.union(Modifier::UNDERLINED));
const DIM: Style = Style::new().add_modifier(Modifier::DIM);

/// Asks the user to choose among rows.
pub trait Picker {
    /// The chosen row, or `None` when cancelled.
    fn pick_one(&mut self, prompt: &str, rows: Vec<Row>) -> Result<Option<usize>, String>;
    /// The marked rows in input order, or `None` when cancelled.
    fn pick_many(&mut self, title: &str, rows: Vec<Row>) -> Result<Option<Vec<usize>>, String>;
}

/// The pickers drawn inline on the terminal, under the prompt, through stderr.
pub struct TerminalPicker;

impl Picker for TerminalPicker {
    fn pick_one(&mut self, prompt: &str, rows: Vec<Row>) -> Result<Option<usize>, String> {
        Ok(interact(&mut Single::new(prompt, rows))?.map(|picked| picked[0]))
    }

    fn pick_many(&mut self, title: &str, rows: Vec<Row>) -> Result<Option<Vec<usize>>, String> {
        interact(&mut Multi::new(title, rows))
    }
}

/// What the terminal loop needs of a picker.
trait State {
    fn handle_key(&mut self, key: KeyEvent) -> Outcome;
    fn paste(&mut self, text: &str);
    fn height(&self, max: u16) -> u16;
    fn render(&self, frame: &mut Frame, area: Rect);
}

/// Raw mode and bracketed paste for as long as it lives, so an error or a panic restores them.
struct RawMode;

impl RawMode {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let guard = Self;
        execute!(io::stderr(), EnableBracketedPaste)?;
        Ok(guard)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = execute!(io::stderr(), DisableBracketedPaste);
        let _ = disable_raw_mode();
    }
}

/// Runs `state` in an inline viewport until a pick or a cancel, then erases the viewport.
fn interact(state: &mut impl State) -> Result<Option<Vec<usize>>, String> {
    let failed = |error: io::Error| format!("cannot open the picker: {error}");
    let (_, lines) = crossterm::terminal::size().map_err(failed)?;
    let raw = RawMode::enter().map_err(failed)?;
    let mut terminal: Terminal<CrosstermBackend<Stderr>> = Terminal::with_options(
        CrosstermBackend::new(io::stderr()),
        TerminalOptions {
            viewport: Viewport::Inline(state.height(lines)),
        },
    )
    .map_err(failed)?;
    let picker_failed = |error: io::Error| format!("the picker failed: {error}");
    let outcome = loop {
        terminal
            .draw(|frame| state.render(frame, frame.area()))
            .map_err(picker_failed)?;
        match event::read().map_err(picker_failed)? {
            Event::Key(key) if key.kind != KeyEventKind::Release => match state.handle_key(key) {
                Outcome::Continue => {}
                Outcome::Picked(picked) => break Some(picked),
                Outcome::Cancelled => break None,
            },
            Event::Paste(text) => state.paste(&text),
            Event::Resize(..) => terminal.autoresize().map_err(picker_failed)?,
            _ => {}
        }
    };
    terminal.clear().map_err(picker_failed)?;
    drop(terminal);
    drop(raw);
    Ok(outcome)
}

/// One choice: `fields` are searched and the first is shown, `extra` is shown dimmed after it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Row {
    pub fields: Vec<String>,
    pub extra: Vec<String>,
}

impl Row {
    fn matches(&self, query: &str) -> Option<Match> {
        let fields: Vec<&str> = self.fields.iter().map(String::as_str).collect();
        match_fields(query, &fields)
    }

    fn line(&self, prefix: &str, positions: &[usize], selected: bool) -> Line<'static> {
        let mut spans = vec![Span::raw(prefix.to_owned())];
        let label = self.fields.first().map_or("", String::as_str);
        spans.extend(highlight(label, positions, Style::new(), HIT));
        for extra in &self.extra {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(extra.clone(), DIM));
        }
        let line = Line::from(spans);
        if selected {
            line.style(Style::new().add_modifier(Modifier::REVERSED))
        } else {
            line
        }
    }
}

/// What a key did to a picker.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum Outcome {
    Continue,
    /// Indices into the rows the picker was given, in input order.
    Picked(Vec<usize>),
    Cancelled,
}

/// The lines a picker of `rows` rows takes in a terminal with `max` free lines.
fn height(rows: usize, max: u16) -> u16 {
    let wanted = 1 + u16::try_from(rows.max(1)).unwrap_or(u16::MAX);
    wanted.min(MAX_HEIGHT).min(max).max(2.min(max))
}

/// Moves `offset` only as far as keeps `cursor` within `visible` rows.
fn scrolled(offset: &Cell<usize>, cursor: usize, visible: usize) -> usize {
    let mut first = offset.get().min(cursor);
    if visible > 0 && cursor >= first + visible {
        first = cursor + 1 - visible;
    }
    offset.set(first);
    first
}

fn moved(cursor: usize, key: KeyEvent, count: usize) -> Option<usize> {
    let last = count.saturating_sub(1);
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    Some(match key.code {
        KeyCode::Up => cursor.saturating_sub(1),
        KeyCode::Char('p') if control => cursor.saturating_sub(1),
        KeyCode::Down => (cursor + 1).min(last),
        KeyCode::Char('n') if control => (cursor + 1).min(last),
        KeyCode::PageUp => cursor.saturating_sub(PAGE),
        KeyCode::PageDown => (cursor + PAGE).min(last),
        _ => return None,
    })
}

fn is_ctrl_c(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c')
}

/// Picks one row, ranking the rows by the query typed.
pub(crate) struct Single {
    prompt: String,
    rows: Vec<Row>,
    query: TextInput,
    ranked: Vec<(usize, Match)>,
    cursor: usize,
    offset: Cell<usize>,
}

impl Single {
    pub(crate) fn new(prompt: &str, rows: Vec<Row>) -> Self {
        let mut state = Self {
            prompt: prompt.into(),
            rows,
            query: TextInput::default(),
            ranked: Vec::new(),
            cursor: 0,
            offset: Cell::new(0),
        };
        state.rerank();
        state
    }

    fn rerank(&mut self) {
        let fields: Vec<Vec<&str>> = self
            .rows
            .iter()
            .map(|row| row.fields.iter().map(String::as_str).collect())
            .collect();
        self.ranked = rank(self.query.as_str(), fields.iter().map(Vec::as_slice));
        self.cursor = 0;
    }
}

impl State for Single {
    fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        if is_ctrl_c(key) {
            return Outcome::Cancelled;
        }
        if let Some(cursor) = moved(self.cursor, key, self.ranked.len()) {
            self.cursor = cursor;
            return Outcome::Continue;
        }
        match key.code {
            KeyCode::Esc => Outcome::Cancelled,
            KeyCode::Enter => self
                .ranked
                .get(self.cursor)
                .map_or(Outcome::Continue, |(index, _)| {
                    Outcome::Picked(vec![*index])
                }),
            _ => {
                let before = self.query.as_str().to_owned();
                if self.query.handle_key(key) && self.query.as_str() != before {
                    self.rerank();
                }
                Outcome::Continue
            }
        }
    }

    fn paste(&mut self, text: &str) {
        self.query.paste(text);
        self.rerank();
    }

    fn height(&self, max: u16) -> u16 {
        height(self.rows.len(), max)
    }

    fn render(&self, frame: &mut Frame, area: Rect) {
        let prompt = format!("{}> ", self.prompt);
        let mut lines = vec![Line::from(vec![
            Span::raw(prompt.clone()),
            Span::raw(self.query.as_str().to_owned()),
            Span::styled(format!("  {}/{}", self.ranked.len(), self.rows.len()), DIM),
        ])];
        let visible = usize::from(area.height.saturating_sub(1));
        if self.ranked.is_empty() {
            lines.push(Line::raw("No match."));
        } else {
            let first = scrolled(&self.offset, self.cursor, visible);
            for (place, (index, found)) in self.ranked.iter().enumerate().skip(first).take(visible)
            {
                let positions = found.positions.first().map_or(&[][..], Vec::as_slice);
                lines.push(self.rows[*index].line("", positions, place == self.cursor));
            }
        }
        frame.render_widget(Paragraph::new(lines), area);
        let column = prompt.chars().count() + self.query.cursor_offset();
        frame.set_cursor_position((
            area.x
                + u16::try_from(column)
                    .unwrap_or(u16::MAX)
                    .min(area.width.saturating_sub(1)),
            area.y,
        ));
    }
}

/// Marks several rows, with the keys and the filter of the TUI checklist.
pub(crate) struct Multi {
    title: String,
    rows: Vec<Row>,
    marks: BTreeSet<usize>,
    filter: TextInput,
    editing: bool,
    before_edit: String,
    /// Indices of the rows the filter keeps, in input order.
    visible: Vec<usize>,
    /// An index into `visible`.
    cursor: usize,
    offset: Cell<usize>,
}

impl Multi {
    pub(crate) fn new(title: &str, rows: Vec<Row>) -> Self {
        let visible = (0..rows.len()).collect();
        Self {
            title: title.into(),
            rows,
            marks: BTreeSet::new(),
            filter: TextInput::default(),
            editing: false,
            before_edit: String::new(),
            visible,
            cursor: 0,
            offset: Cell::new(0),
        }
    }

    fn set_filter(&mut self, filter: TextInput) {
        let selected = self.visible.get(self.cursor).copied();
        self.filter = filter;
        self.visible = (0..self.rows.len())
            .filter(|&index| self.rows[index].matches(self.filter.as_str()).is_some())
            .collect();
        self.cursor = selected
            .and_then(|selected| self.visible.iter().position(|&index| index == selected))
            .unwrap_or(0);
    }
}

impl State for Multi {
    fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        if is_ctrl_c(key) {
            return Outcome::Cancelled;
        }
        if self.editing {
            match key.code {
                KeyCode::Enter => self.editing = false,
                KeyCode::Esc => {
                    self.editing = false;
                    let before = std::mem::take(&mut self.before_edit);
                    self.set_filter(TextInput::new(&before));
                }
                _ => {
                    let mut filter = self.filter.clone();
                    if filter.handle_key(key) {
                        self.set_filter(filter);
                    }
                }
            }
            return Outcome::Continue;
        }
        if let Some(cursor) = moved(self.cursor, key, self.visible.len()) {
            self.cursor = cursor;
            return Outcome::Continue;
        }
        match key.code {
            KeyCode::Char('k') => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Char('j') => {
                self.cursor = (self.cursor + 1).min(self.visible.len().saturating_sub(1));
            }
            KeyCode::Char(' ') => {
                if let Some(&index) = self.visible.get(self.cursor)
                    && !self.marks.remove(&index)
                {
                    self.marks.insert(index);
                }
            }
            KeyCode::Char('a') => {
                if self.visible.iter().all(|index| self.marks.contains(index)) {
                    for index in &self.visible {
                        self.marks.remove(index);
                    }
                } else {
                    self.marks.extend(self.visible.iter().copied());
                }
            }
            KeyCode::Char('/') => {
                self.before_edit = self.filter.as_str().to_owned();
                self.filter = TextInput::new(&self.before_edit);
                self.editing = true;
            }
            KeyCode::Enter if !self.marks.is_empty() => {
                return Outcome::Picked(self.marks.iter().copied().collect());
            }
            KeyCode::Esc if self.filter.is_empty() => return Outcome::Cancelled,
            KeyCode::Esc => self.set_filter(TextInput::default()),
            _ => {}
        }
        Outcome::Continue
    }

    fn paste(&mut self, text: &str) {
        if self.editing {
            let mut filter = self.filter.clone();
            filter.paste(text);
            self.set_filter(filter);
        }
    }

    fn height(&self, max: u16) -> u16 {
        height(self.rows.len(), max)
    }

    fn render(&self, frame: &mut Frame, area: Rect) {
        let mut title = vec![Span::raw(format!("{}  ", self.title))];
        let filtered = self.editing || !self.filter.is_empty();
        let cursor_column = title[0].content.chars().count() + 1 + self.filter.cursor_offset();
        if filtered {
            title.push(Span::raw(format!("/{}  ", self.filter.as_str())));
        }
        title.push(Span::styled(format!("{} marked", self.marks.len()), DIM));
        let mut lines = vec![Line::from(title)];
        let visible = usize::from(area.height.saturating_sub(1));
        if self.visible.is_empty() {
            lines.push(Line::raw("No match."));
        } else {
            let first = scrolled(&self.offset, self.cursor, visible);
            for (place, &index) in self.visible.iter().enumerate().skip(first).take(visible) {
                let row = &self.rows[index];
                let positions = row
                    .matches(self.filter.as_str())
                    .and_then(|found| found.positions.into_iter().next())
                    .unwrap_or_default();
                let mark = if self.marks.contains(&index) {
                    "[x] "
                } else {
                    "[ ] "
                };
                lines.push(row.line(mark, &positions, place == self.cursor));
            }
        }
        frame.render_widget(Paragraph::new(lines), area);
        if self.editing {
            frame.set_cursor_position((
                area.x
                    + u16::try_from(cursor_column)
                        .unwrap_or(u16::MAX)
                        .min(area.width.saturating_sub(1)),
                area.y,
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend, style::Modifier};

    use super::*;

    fn rows(labels: &[&str]) -> Vec<Row> {
        labels
            .iter()
            .map(|label| Row {
                fields: vec![(*label).into()],
                extra: Vec::new(),
            })
            .collect()
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(character: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL)
    }

    fn type_text(handle: &mut impl FnMut(KeyEvent) -> Outcome, text: &str) {
        for character in text.chars() {
            assert_eq!(handle(key(KeyCode::Char(character))), Outcome::Continue);
        }
    }

    fn draw(
        width: u16,
        height: u16,
        render: impl Fn(&mut ratatui::Frame),
    ) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame)).unwrap();
        terminal
    }

    fn lines(terminal: &Terminal<TestBackend>) -> Vec<String> {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    fn single_lines(state: &Single, height: u16) -> Vec<String> {
        lines(&draw(40, height, |frame| state.render(frame, frame.area())))
    }

    fn multi_lines(state: &Multi, height: u16) -> Vec<String> {
        lines(&draw(40, height, |frame| state.render(frame, frame.area())))
    }

    #[test]
    fn single_ranks_best_first() {
        let mut state = Single::new("host", rows(&["prod-db", "sandbox", "db-main"]));
        type_text(&mut |event| state.handle_key(event), "db");
        assert_eq!(
            state.handle_key(key(KeyCode::Enter)),
            Outcome::Picked(vec![2])
        );
    }

    #[test]
    fn single_cursor_resets_on_query_change() {
        let mut state = Single::new("host", rows(&["alpha", "beta", "delta"]));
        state.handle_key(key(KeyCode::Down));
        state.handle_key(key(KeyCode::Down));
        type_text(&mut |event| state.handle_key(event), "a");
        assert_eq!(
            state.handle_key(key(KeyCode::Enter)),
            Outcome::Picked(vec![0])
        );
    }

    #[test]
    fn single_moves_with_arrows_ctrl_and_pages() {
        let labels: Vec<String> = (0..30).map(|n| format!("host{n}")).collect();
        let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
        let mut state = Single::new("host", rows(&labels));
        state.handle_key(key(KeyCode::Down));
        state.handle_key(ctrl('n'));
        state.handle_key(ctrl('p'));
        assert_eq!(
            state.handle_key(key(KeyCode::Enter)),
            Outcome::Picked(vec![1])
        );
        state.handle_key(key(KeyCode::Up));
        state.handle_key(key(KeyCode::Up));
        state.handle_key(key(KeyCode::PageDown));
        assert_eq!(
            state.handle_key(key(KeyCode::Enter)),
            Outcome::Picked(vec![usize::from(MAX_HEIGHT) - 1])
        );
        for _ in 0..5 {
            state.handle_key(key(KeyCode::PageDown));
        }
        assert_eq!(
            state.handle_key(key(KeyCode::Enter)),
            Outcome::Picked(vec![29])
        );
        state.handle_key(key(KeyCode::PageUp));
        state.handle_key(key(KeyCode::PageUp));
        state.handle_key(key(KeyCode::PageUp));
        assert_eq!(
            state.handle_key(key(KeyCode::Enter)),
            Outcome::Picked(vec![0])
        );
    }

    #[test]
    fn single_enter_with_no_match_does_nothing() {
        let mut state = Single::new("host", rows(&["prod"]));
        type_text(&mut |event| state.handle_key(event), "zzz");
        assert_eq!(state.handle_key(key(KeyCode::Enter)), Outcome::Continue);
        assert_eq!(single_lines(&state, 3)[1], "No match.");
    }

    #[test]
    fn single_cancels_on_esc_and_ctrl_c() {
        let mut state = Single::new("host", rows(&["prod"]));
        assert_eq!(state.handle_key(key(KeyCode::Esc)), Outcome::Cancelled);
        assert_eq!(state.handle_key(ctrl('c')), Outcome::Cancelled);
    }

    #[test]
    fn single_paste_goes_to_the_query() {
        let mut state = Single::new("host", rows(&["prod", "db"]));
        state.paste("db\n");
        assert_eq!(
            state.handle_key(key(KeyCode::Enter)),
            Outcome::Picked(vec![1])
        );
        assert_eq!(single_lines(&state, 3)[0], "host> db  1/2");
    }

    #[test]
    fn single_prompt_line_shows_counts() {
        let mut labels = vec!["pr-1".to_owned(), "pr-2".into(), "pr-3".into()];
        labels.extend((0..39).map(|n| format!("x{n}")));
        let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
        let mut state = Single::new("host", rows(&labels));
        type_text(&mut |event| state.handle_key(event), "pr");
        assert_eq!(single_lines(&state, 5)[0], "host> pr  3/42");
    }

    #[test]
    fn multi_space_marks_and_a_toggles_visible() {
        let mut state = Multi::new("adopt", rows(&["a", "b", "c"]));
        state.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(
            state.handle_key(key(KeyCode::Enter)),
            Outcome::Picked(vec![0])
        );
        state.handle_key(key(KeyCode::Char('a')));
        assert_eq!(
            state.handle_key(key(KeyCode::Enter)),
            Outcome::Picked(vec![0, 1, 2])
        );
        state.handle_key(key(KeyCode::Char('a')));
        assert_eq!(state.handle_key(key(KeyCode::Enter)), Outcome::Continue);
    }

    #[test]
    fn multi_filter_keys_follow_the_hosts_filter() {
        let mut state = Multi::new("adopt", rows(&["web", "prod-a", "prod-b"]));
        state.handle_key(key(KeyCode::Char('/')));
        type_text(&mut |event| state.handle_key(event), "pr");
        state.handle_key(key(KeyCode::Enter));
        assert_eq!(multi_lines(&state, 4)[1..3], ["[ ] prod-a", "[ ] prod-b"]);
        state.handle_key(key(KeyCode::Char('j')));
        state.handle_key(key(KeyCode::Char(' ')));
        state.handle_key(key(KeyCode::Char('/')));
        type_text(&mut |event| state.handle_key(event), "x");
        state.handle_key(key(KeyCode::Esc));
        assert_eq!(multi_lines(&state, 4)[0], "adopt  /pr  1 marked");
        assert_eq!(
            state.handle_key(key(KeyCode::Enter)),
            Outcome::Picked(vec![2])
        );
    }

    #[test]
    fn multi_esc_clears_filter_then_cancels() {
        let mut state = Multi::new("adopt", rows(&["web", "prod"]));
        state.handle_key(key(KeyCode::Char('/')));
        type_text(&mut |event| state.handle_key(event), "pr");
        state.handle_key(key(KeyCode::Enter));
        assert_eq!(state.handle_key(key(KeyCode::Esc)), Outcome::Continue);
        assert_eq!(multi_lines(&state, 4)[0], "adopt  0 marked");
        assert_eq!(state.handle_key(key(KeyCode::Esc)), Outcome::Cancelled);
    }

    #[test]
    fn multi_returns_hidden_marks() {
        let mut state = Multi::new("adopt", rows(&["web", "prod-a", "db"]));
        state.handle_key(key(KeyCode::Char(' ')));
        state.handle_key(key(KeyCode::Char('/')));
        type_text(&mut |event| state.handle_key(event), "db");
        state.handle_key(key(KeyCode::Enter));
        state.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(multi_lines(&state, 4)[0], "adopt  /db  2 marked");
        assert_eq!(
            state.handle_key(key(KeyCode::Enter)),
            Outcome::Picked(vec![0, 2])
        );
    }

    #[test]
    fn multi_enter_without_marks_does_nothing() {
        let mut state = Multi::new("adopt", rows(&["web"]));
        assert_eq!(state.handle_key(key(KeyCode::Enter)), Outcome::Continue);
        assert_eq!(state.handle_key(ctrl('c')), Outcome::Cancelled);
    }

    #[test]
    fn multi_keeps_input_order() {
        let mut state = Multi::new("adopt", rows(&["sandbox", "prod-db", "db-main"]));
        state.handle_key(key(KeyCode::Char('/')));
        type_text(&mut |event| state.handle_key(event), "db");
        assert_eq!(
            multi_lines(&state, 5)[1..4],
            ["[ ] sandbox", "[ ] prod-db", "[ ] db-main"]
        );
    }

    #[test]
    fn renders_highlights_and_dimmed_extra() {
        let mut state = Single::new(
            "host",
            vec![
                Row {
                    fields: vec!["prod".into()],
                    extra: vec!["managed".into()],
                },
                Row {
                    fields: vec!["pub".into()],
                    extra: vec!["external".into()],
                },
            ],
        );
        type_text(&mut |event| state.handle_key(event), "p");
        let terminal = draw(40, 3, |frame| state.render(frame, frame.area()));
        let buffer = terminal.backend().buffer();
        assert_eq!(lines(&terminal)[2], "pub  external");
        assert!(buffer[(0, 2)].modifier.contains(Modifier::BOLD));
        assert!(!buffer[(1, 2)].modifier.contains(Modifier::BOLD));
        assert!(buffer[(5, 2)].modifier.contains(Modifier::DIM));
        assert!(buffer[(1, 1)].modifier.contains(Modifier::REVERSED));
        assert!(!buffer[(1, 2)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn renders_within_a_tiny_height() {
        let labels: Vec<String> = (0..50).map(|n| format!("host{n}")).collect();
        let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
        let mut state = Single::new("host", rows(&labels));
        assert_eq!(state.height(2), 2);
        assert_eq!(state.height(40), MAX_HEIGHT);
        assert_eq!(Single::new("host", rows(&["one"])).height(40), 2);
        for _ in 0..20 {
            state.handle_key(key(KeyCode::Down));
        }
        assert_eq!(single_lines(&state, 2), ["host>   50/50", "host20"]);
    }
}
