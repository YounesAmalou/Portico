//! The single-line text field shared by the host form, the filter and the tunnel forms.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    style::{Modifier, Style},
    text::Span,
};

use super::display_width;
use crate::domain::host::HostField;

/// A single-line text field edited at a cursor.
///
/// The cursor is a byte offset that always sits on a character boundary. An input bound to a
/// host field applies an edit only when the result is something that field accepts, so it never
/// holds text the host model would reject for its characters.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct TextInput {
    pub(super) text: String,
    pub(super) cursor: usize,
    field: Option<HostField>,
}

impl TextInput {
    pub(crate) fn new(text: &str) -> Self {
        Self {
            text: text.into(),
            cursor: text.len(),
            field: None,
        }
    }

    pub(super) fn for_field(field: HostField, text: &str) -> Self {
        Self {
            field: Some(field),
            ..Self::new(text)
        }
    }

    fn accepts(&self, text: &str) -> bool {
        self.field.is_none_or(|field| field.accepts(text))
    }

    /// The columns the text before the cursor takes, so a terminal cursor can be placed after it.
    pub(crate) fn cursor_offset(&self) -> usize {
        display_width(&self.text[..self.cursor])
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.text
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Applies an editing key and reports whether the key was an editing key.
    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.insert(&character.to_string());
            }
            KeyCode::Backspace => {
                if let Some(previous) = self.previous_boundary()
                    && self.remove(previous)
                {
                    self.cursor = previous;
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.text.len() {
                    self.remove(self.cursor);
                }
            }
            KeyCode::Left => self.cursor = self.previous_boundary().unwrap_or(0),
            KeyCode::Right => {
                if let Some(character) = self.text[self.cursor..].chars().next() {
                    self.cursor += character.len_utf8();
                }
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.text.len(),
            _ => return false,
        }
        true
    }

    /// Inserts pasted text at the cursor. Fields are single-line, so control characters (such as
    /// the newline that ends a copied line) are dropped, and so is whatever the field never
    /// permits. Text that would leave the field in a state it does not accept is refused whole.
    pub(crate) fn paste(&mut self, text: &str) {
        self.insert(text);
    }

    fn insert(&mut self, text: &str) {
        let text: String = text
            .chars()
            .filter(|&c| !c.is_control() && self.field.is_none_or(|field| field.permits(c)))
            .collect();
        let mut edited = self.text.clone();
        edited.insert_str(self.cursor, &text);
        if self.accepts(&edited) {
            self.text = edited;
            self.cursor += text.len();
        }
    }

    /// Removes the character at `index` unless the remainder would not be accepted.
    fn remove(&mut self, index: usize) -> bool {
        let mut edited = self.text.clone();
        edited.remove(index);
        let accepted = self.accepts(&edited);
        if accepted {
            self.text = edited;
        }
        accepted
    }

    fn previous_boundary(&self) -> Option<usize> {
        self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
    }

    /// The text as styled spans, with the character under the cursor reversed when `focused`.
    pub(crate) fn spans(&self, focused: bool) -> Vec<Span<'static>> {
        if !focused {
            return vec![Span::raw(self.text.clone())];
        }
        let (before, rest) = self.text.split_at(self.cursor);
        let mut rest = rest.chars();
        let under = rest.next().map_or(" ".to_owned(), String::from);
        vec![
            Span::raw(before.to_owned()),
            Span::styled(under, Style::default().add_modifier(Modifier::REVERSED)),
            Span::raw(rest.as_str().to_owned()),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::tests::key;

    #[test]
    fn text_input_edits_at_the_cursor_on_character_boundaries() {
        let mut input = TextInput::new("aéz");
        assert!(input.handle_key(key(KeyCode::Left)));
        assert!(input.handle_key(key(KeyCode::Left)));
        input.handle_key(key(KeyCode::Char('x')));
        assert_eq!(input.as_str(), "axéz");
        input.handle_key(key(KeyCode::Delete));
        assert_eq!(input.as_str(), "axz");
        input.handle_key(key(KeyCode::Backspace));
        input.handle_key(key(KeyCode::Backspace));
        assert_eq!(input.as_str(), "z");
        input.handle_key(key(KeyCode::Backspace));
        input.handle_key(key(KeyCode::Left));
        assert_eq!(input.as_str(), "z");
        input.handle_key(key(KeyCode::End));
        input.handle_key(key(KeyCode::Delete));
        input.handle_key(key(KeyCode::Right));
        input.handle_key(key(KeyCode::Char('é')));
        assert_eq!(input.as_str(), "zé");
        input.handle_key(key(KeyCode::Home));
        input.handle_key(key(KeyCode::Char('>')));
        assert_eq!(input.as_str(), ">zé");
        assert!(!input.handle_key(key(KeyCode::Enter)));
        assert!(!input.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL)));
    }
}
