//! The suggestion dropdown the host and tunnel forms open under a field: its state and how it
//! is drawn. Ranking is `suggest::rank`'s.

use std::path::Path;

use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

use super::{display_width, keep_end, keep_start};
use crate::suggest::{self, Candidate};

/// How many suggestions the dropdown shows at once.
pub(super) const SUGGESTION_ROWS: usize = 5;

/// An open suggestion dropdown.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Suggestions {
    /// The matching aliases, never empty while the dropdown is open.
    pub(super) items: Vec<Candidate>,
    pub(super) selected: usize,
    /// The first visible row; the selection always stays within the visible rows.
    pub(super) offset: usize,
    /// Whether Ctrl+Space opened it, which keeps it open for an empty hop.
    pub(super) forced: bool,
    /// The index of the form field it suggests for.
    pub(super) field: usize,
}

impl Suggestions {
    /// A dropdown of `items`, keeping the selection of the `previous` one when it still lists it.
    pub(super) fn reopened(
        previous: Option<Self>,
        items: Vec<Candidate>,
        forced: bool,
        field: usize,
    ) -> Self {
        let selected = previous
            .and_then(|list| {
                let alias = &list.items[list.selected].alias;
                items.iter().position(|item| &item.alias == alias)
            })
            .unwrap_or(0);
        let mut list = Self {
            items,
            selected: 0,
            offset: 0,
            forced,
            field,
        };
        list.select(selected);
        list
    }

    pub(super) fn select(&mut self, selected: usize) {
        self.selected = selected.min(self.items.len() - 1);
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + SUGGESTION_ROWS {
            self.offset = self.selected + 1 - SUGGESTION_ROWS;
        }
    }
}

/// The jump host dropdown as boxed lines under the value of `label`'s row: at most `rows` rows
/// around the selection, fitted to `width` columns by shortening the indent, then the origins
/// (keeping their end), then the aliases.
pub(super) fn suggestion_lines(
    list: &Suggestions,
    label: &str,
    home: &Path,
    width: usize,
    rows: usize,
) -> Vec<Line<'static>> {
    let first = list
        .offset
        .max((list.selected + 1).saturating_sub(rows))
        .min(list.selected);
    let visible = &list.items[first..(first + rows).min(list.items.len())];
    let longest = visible
        .iter()
        .map(|item| display_width(&item.alias))
        .max()
        .unwrap_or(0);
    // A row is `│> alias │`: the alias column plus five columns of border, marker and padding.
    let column = longest.min(width.saturating_sub(5)).max(1);
    let boxed = column + 5;
    let indent =
        " ".repeat(display_width(&format!("> {label}: ")).min(width.saturating_sub(boxed)));
    let room = width.saturating_sub(indent.len() + boxed + 2);
    let border = "─".repeat(column + 3);
    let mut lines = vec![Line::raw(format!("{indent}┌{border}┐"))];
    lines.extend(visible.iter().enumerate().map(|(row, item)| {
        let selected = first + row == list.selected;
        let alias = keep_start(&item.alias, column);
        let padding = " ".repeat(column - display_width(&alias));
        let cell = format!("{} {alias}{padding} ", if selected { '>' } else { ' ' });
        let origin = suggest::origin_label(&item.origin, Some(home));
        let origin = if room >= 4 {
            keep_end(&origin, room)
        } else {
            String::new()
        };
        Line::from(vec![
            Span::raw(format!("{indent}│")),
            if selected {
                Span::styled(cell, Style::default().add_modifier(Modifier::REVERSED))
            } else {
                Span::raw(cell)
            },
            Span::raw(if origin.is_empty() {
                "│".to_owned()
            } else {
                format!("│  {origin}")
            }),
        ])
    }));
    lines.push(Line::raw(format!("{indent}└{border}┘")));
    lines
}
