//! The settings overlay, opened with `,`: the tab toggles and the desktop integration.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};

use super::{display_width, keep_start};
use crate::{
    backend::BackendId,
    desktop::status::Status,
    domain::settings::{Key, Settings, Tabs},
};

/// The widest the overlay is drawn.
const MAX_WIDTH: u16 = 60;

const FOOTER: &str = "\u{2191}\u{2193} move \u{b7} Space/Enter toggle or run \u{b7} Esc close";

/// A desktop integration action, run as `portico desktop install` or `uninstall`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DesktopRun {
    Install,
    Uninstall,
}

impl DesktopRun {
    /// The `portico desktop` subcommand.
    pub(super) fn command(self) -> &'static str {
        match self {
            Self::Install => "install",
            Self::Uninstall => "uninstall",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Install => "Install or update",
            Self::Uninstall => "Uninstall",
        }
    }
}

/// What a key asks of the overlay's owner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum OverlayAction {
    None,
    Close,
    Toggle(Key),
    Run(DesktopRun),
}

/// A row the cursor moves over.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Row {
    Toggle(Key),
    Run(DesktopRun),
}

#[derive(Clone, Debug)]
pub(super) struct SettingsOverlay {
    cursor: usize,
    /// The tabs as written, applied when the overlay closes.
    pub(super) tabs: Tabs,
    /// The desktop integration's status, or `None` where it is not offered (outside Linux).
    pub(super) desktop: Option<Status>,
    /// The last toggle's or action's failure, as one line.
    pub(super) error: Option<String>,
}

impl SettingsOverlay {
    pub(super) fn open(tabs: Tabs, desktop: Option<Status>) -> Self {
        Self {
            cursor: 0,
            tabs,
            desktop,
            error: None,
        }
    }

    /// The toggles in `Key::ALL` order, then the desktop actions on offer.
    pub(super) fn rows(&self) -> Vec<Row> {
        let mut rows = Key::ALL.into_iter().map(Row::Toggle).collect::<Vec<_>>();
        if let Some(desktop) = &self.desktop {
            rows.push(Row::Run(DesktopRun::Install));
            if desktop.installed {
                rows.push(Row::Run(DesktopRun::Uninstall));
            }
        }
        rows
    }

    pub(super) fn handle_key(&mut self, key: KeyEvent) -> OverlayAction {
        let rows = self.rows();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.cursor = (self.cursor + 1).min(rows.len() - 1);
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                return match rows[self.cursor.min(rows.len() - 1)] {
                    Row::Toggle(key) => OverlayAction::Toggle(key),
                    Row::Run(run) => OverlayAction::Run(run),
                };
            }
            KeyCode::Esc | KeyCode::Char(',') => return OverlayAction::Close,
            _ => {}
        }
        OverlayAction::None
    }

    /// Records a toggle's outcome: on success `key` holds `value`, otherwise it keeps its value
    /// and the error is shown.
    pub(super) fn finish_toggle(&mut self, key: Key, value: bool, result: Result<(), String>) {
        match result {
            Ok(()) => {
                let mut settings = Settings { tabs: self.tabs };
                key.set(&mut settings, value);
                self.tabs = settings.tabs;
                self.error = None;
            }
            Err(error) => self.error = Some(error),
        }
    }

    /// The value `key` has now.
    pub(super) fn value(&self, key: Key) -> bool {
        key.get(&Settings { tabs: self.tabs })
    }

    /// Records a desktop action's outcome and the status read after it.
    pub(super) fn finish_run(&mut self, result: Result<(), String>, desktop: Status) {
        self.desktop = Some(desktop);
        self.error = result.err();
        self.cursor = self.cursor.min(self.rows().len() - 1);
    }

    /// Draws the overlay centred over `area`, with the settings file's `path`. Every line is cut
    /// or, for the error, wrapped here to the overlay's width, so its height is its line count.
    pub(super) fn render(&self, frame: &mut Frame<'_>, area: Rect, path: &str) {
        let width = area.width.min(MAX_WIDTH);
        let inner = usize::from(width.saturating_sub(2));
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let selected = Style::default().add_modifier(Modifier::REVERSED);
        let rows = self.rows();
        let cursor = rows.get(self.cursor).copied();
        let mut lines = vec![Line::styled("Tabs", bold)];
        let mut cursor_line = 0;
        for key in Key::ALL {
            let on = self.value(key);
            let marker = if cursor == Some(Row::Toggle(key)) {
                cursor_line = lines.len();
                ">"
            } else {
                " "
            };
            let check = if on { "x" } else { " " };
            lines.push(Line::raw(format!("{marker} [{check}] {}", label(key))));
        }
        if let Some(desktop) = &self.desktop {
            lines.push(Line::raw(""));
            lines.push(Line::styled("Desktop integration", bold));
            for line in &desktop.lines {
                lines.push(Line::raw(keep_start(
                    &format!("  {}", line.render()),
                    inner,
                )));
            }
            let on_action = matches!(cursor, Some(Row::Run(_)));
            if on_action {
                cursor_line = lines.len();
            }
            let mut actions = vec![Span::raw(if on_action { "> " } else { "  " })];
            for row in &rows {
                let Row::Run(run) = *row else { continue };
                if actions.len() > 1 {
                    actions.push(Span::raw("  "));
                }
                let text = format!("[{}]", run.label());
                actions.push(if cursor == Some(*row) {
                    Span::styled(text, selected)
                } else {
                    Span::raw(text)
                });
            }
            lines.push(Line::from(actions));
        }
        lines.push(Line::raw(""));
        lines.push(Line::raw(keep_start(path, inner)));
        if let Some(error) = &self.error {
            lines.extend(wrap(error, inner).into_iter().map(Line::raw));
        }
        lines.push(Line::raw(keep_start(FOOTER, inner)));
        let height = u16::try_from(lines.len() + 2)
            .unwrap_or(u16::MAX)
            .min(area.height);
        let overlay = Rect {
            x: area.x + (area.width - width) / 2,
            y: area.y + (area.height - height) / 2,
            width,
            height,
        };
        // On a terminal too short for every line, the lines scroll to keep the cursor's in view.
        let visible = usize::from(height.saturating_sub(2)).max(1);
        let scroll = (cursor_line + 1)
            .saturating_sub(visible)
            .min(lines.len().saturating_sub(visible));
        frame.render_widget(Clear, overlay);
        frame.render_widget(
            Paragraph::new(lines)
                .block(Block::default().borders(Borders::ALL).title("Settings"))
                .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0)),
            overlay,
        );
    }
}

/// `text` in lines of at most `columns` columns, broken between words where it can be.
fn wrap(text: &str, columns: usize) -> Vec<String> {
    let columns = columns.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split(' ') {
        let joined = if line.is_empty() {
            word.to_owned()
        } else {
            format!("{line} {word}")
        };
        if display_width(&joined) <= columns {
            line = joined;
            continue;
        }
        if !line.is_empty() {
            lines.push(std::mem::take(&mut line));
        }
        // A word wider than the overlay is broken where it reaches the edge.
        for character in word.chars() {
            if display_width(&line) + display_width(&character.to_string()) > columns {
                lines.push(std::mem::take(&mut line));
            }
            line.push(character);
        }
    }
    lines.push(line);
    lines
}

/// The name a toggle shows: its tab's.
fn label(key: Key) -> &'static str {
    match key {
        Key::TabsTunnels => "Tunnels",
        Key::TabsDocker => BackendId::Docker.title(),
        Key::TabsPodman => BackendId::Podman.title(),
        Key::TabsProxmox => BackendId::Proxmox.title(),
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;
    use crate::desktop::status::{Line as StatusLine, Status};

    fn status(installed: bool) -> Status {
        Status {
            lines: vec![StatusLine {
                label: "Panel",
                text: "not installed".into(),
            }],
            installed,
        }
    }

    fn press(overlay: &mut SettingsOverlay, code: KeyCode) -> OverlayAction {
        overlay.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn without_the_desktop_integration_only_the_toggles_are_rows() {
        let overlay = SettingsOverlay::open(Tabs::default(), None);
        assert_eq!(
            overlay.rows(),
            Key::ALL.into_iter().map(Row::Toggle).collect::<Vec<_>>()
        );
    }

    #[test]
    fn rows_without_an_installation_have_no_uninstall() {
        let overlay = SettingsOverlay::open(Tabs::default(), Some(status(false)));
        assert_eq!(
            overlay.rows(),
            vec![
                Row::Toggle(Key::TabsTunnels),
                Row::Toggle(Key::TabsDocker),
                Row::Toggle(Key::TabsPodman),
                Row::Toggle(Key::TabsProxmox),
                Row::Run(DesktopRun::Install),
            ]
        );
        let overlay = SettingsOverlay::open(Tabs::default(), Some(status(true)));
        assert_eq!(
            overlay.rows().last(),
            Some(&Row::Run(DesktopRun::Uninstall))
        );
    }

    #[test]
    fn enter_on_a_toggle_asks_to_toggle_it() {
        let mut overlay = SettingsOverlay::open(Tabs::default(), Some(status(true)));
        press(&mut overlay, KeyCode::Down);
        press(&mut overlay, KeyCode::Char('j'));
        assert_eq!(
            press(&mut overlay, KeyCode::Enter),
            OverlayAction::Toggle(Key::TabsPodman)
        );
        assert_eq!(
            press(&mut overlay, KeyCode::Char(' ')),
            OverlayAction::Toggle(Key::TabsPodman)
        );
        press(&mut overlay, KeyCode::Char('k'));
        press(&mut overlay, KeyCode::Up);
        press(&mut overlay, KeyCode::Up);
        assert_eq!(
            press(&mut overlay, KeyCode::Enter),
            OverlayAction::Toggle(Key::TabsTunnels)
        );
        for _ in 0..10 {
            press(&mut overlay, KeyCode::Down);
        }
        assert_eq!(
            press(&mut overlay, KeyCode::Enter),
            OverlayAction::Run(DesktopRun::Uninstall)
        );
        press(&mut overlay, KeyCode::Up);
        assert_eq!(
            press(&mut overlay, KeyCode::Char(' ')),
            OverlayAction::Run(DesktopRun::Install)
        );
    }

    #[test]
    fn esc_and_comma_close() {
        let mut overlay = SettingsOverlay::open(Tabs::default(), Some(status(false)));
        assert_eq!(press(&mut overlay, KeyCode::Esc), OverlayAction::Close);
        assert_eq!(
            press(&mut overlay, KeyCode::Char(',')),
            OverlayAction::Close
        );
        assert_eq!(press(&mut overlay, KeyCode::Char('x')), OverlayAction::None);
    }

    #[test]
    fn the_overlay_is_as_tall_as_its_lines_on_a_narrow_terminal() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut overlay = SettingsOverlay::open(Tabs::default(), Some(status(true)));
        overlay.error = Some(
            "invalid settings file ~/.config/portico/settings.toml: line 1: expected `]`".into(),
        );
        let mut terminal = Terminal::new(TestBackend::new(30, 30)).unwrap();
        terminal
            .draw(|frame| overlay.render(frame, frame.area(), "~/.config/portico/settings.toml"))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rows = (0..30)
            .map(|y| (0..30).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>();
        let text = rows.join("\n");
        // Nothing is lost to wrapping: the whole error and the footer's last word are shown.
        assert!(text.contains("expected `]`"), "{text}");
        assert!(text.contains("oml: line 1: expected `]`"), "{text}");
        let bottom = rows.iter().position(|row| row.starts_with('└')).unwrap();
        assert!(rows[bottom - 1].contains('…'), "the footer is cut: {text}");
        for row in &rows {
            assert!(row.chars().count() <= 30);
        }
    }

    #[test]
    fn a_short_terminal_scrolls_to_the_cursor() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut overlay = SettingsOverlay::open(Tabs::default(), Some(status(true)));
        let draw = |overlay: &SettingsOverlay| {
            let mut terminal = Terminal::new(TestBackend::new(60, 6)).unwrap();
            terminal
                .draw(|frame| overlay.render(frame, frame.area(), "settings.toml"))
                .unwrap();
            let buffer = terminal.backend().buffer();
            (0..6)
                .map(|y| (0..60).map(|x| buffer[(x, y)].symbol()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let top = draw(&overlay);
        assert!(top.contains("[x] Tunnels"), "{top}");
        for _ in 0..10 {
            press(&mut overlay, KeyCode::Down);
        }
        let text = draw(&overlay);
        assert!(text.contains("> [Install or update]"), "{text}");
        assert!(!text.contains("[x] Tunnels"), "{text}");
    }

    #[test]
    fn wrap_breaks_between_words_and_inside_long_ones() {
        assert_eq!(wrap("ab cd ef", 5), vec!["ab cd", "ef"]);
        assert_eq!(wrap("abcdefg", 3), vec!["abc", "def", "g"]);
        assert_eq!(wrap("", 3), vec![""]);
    }

    #[test]
    fn a_refreshed_status_keeps_the_cursor_on_a_row() {
        let mut overlay = SettingsOverlay::open(Tabs::default(), Some(status(true)));
        for _ in 0..10 {
            press(&mut overlay, KeyCode::Down);
        }
        overlay.finish_run(Ok(()), status(false));
        assert_eq!(
            press(&mut overlay, KeyCode::Enter),
            OverlayAction::Run(DesktopRun::Install)
        );
    }
}
