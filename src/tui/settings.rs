//! The settings overlay, opened with `,`: the tab toggles and the desktop integration.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

use super::keep_start;
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
    pub(super) desktop: Status,
    /// The last toggle's or action's failure, as one line.
    pub(super) error: Option<String>,
}

impl SettingsOverlay {
    pub(super) fn open(tabs: Tabs, desktop: Status) -> Self {
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
        rows.push(Row::Run(DesktopRun::Install));
        if self.desktop.installed {
            rows.push(Row::Run(DesktopRun::Uninstall));
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
        self.desktop = desktop;
        self.error = result.err();
        self.cursor = self.cursor.min(self.rows().len() - 1);
    }

    /// Draws the overlay centred over `area`, with the settings file's `path`.
    pub(super) fn render(&self, frame: &mut Frame<'_>, area: Rect, path: &str) {
        let width = area.width.min(MAX_WIDTH);
        let inner = usize::from(width.saturating_sub(2));
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let selected = Style::default().add_modifier(Modifier::REVERSED);
        let rows = self.rows();
        let cursor = rows.get(self.cursor).copied();
        let mut lines = vec![Line::styled("Tabs", bold)];
        for key in Key::ALL {
            let on = self.value(key);
            let marker = if cursor == Some(Row::Toggle(key)) {
                ">"
            } else {
                " "
            };
            let check = if on { "x" } else { " " };
            lines.push(Line::raw(format!("{marker} [{check}] {}", label(key))));
        }
        lines.push(Line::raw(""));
        lines.push(Line::styled("Desktop integration", bold));
        for line in &self.desktop.lines {
            lines.push(Line::raw(keep_start(
                &format!("  {}", line.render()),
                inner,
            )));
        }
        let on_action = matches!(cursor, Some(Row::Run(_)));
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
        lines.push(Line::raw(""));
        lines.push(Line::raw(keep_start(path, inner)));
        let mut height = lines.len() + 1;
        if let Some(error) = &self.error {
            lines.push(Line::raw(error.clone()));
            height += super::display_width(error).div_ceil(inner.max(1)).max(1);
        }
        lines.push(Line::raw(FOOTER));
        let height = u16::try_from(height + 2)
            .unwrap_or(u16::MAX)
            .min(area.height);
        let overlay = Rect {
            x: area.x + (area.width - width) / 2,
            y: area.y + (area.height - height) / 2,
            width,
            height,
        };
        frame.render_widget(Clear, overlay);
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(Block::default().borders(Borders::ALL).title("Settings")),
            overlay,
        );
    }
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
    fn rows_without_an_installation_have_no_uninstall() {
        let overlay = SettingsOverlay::open(Tabs::default(), status(false));
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
        let overlay = SettingsOverlay::open(Tabs::default(), status(true));
        assert_eq!(
            overlay.rows().last(),
            Some(&Row::Run(DesktopRun::Uninstall))
        );
    }

    #[test]
    fn enter_on_a_toggle_asks_to_toggle_it() {
        let mut overlay = SettingsOverlay::open(Tabs::default(), status(true));
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
        let mut overlay = SettingsOverlay::open(Tabs::default(), status(false));
        assert_eq!(press(&mut overlay, KeyCode::Esc), OverlayAction::Close);
        assert_eq!(
            press(&mut overlay, KeyCode::Char(',')),
            OverlayAction::Close
        );
        assert_eq!(press(&mut overlay, KeyCode::Char('x')), OverlayAction::None);
    }

    #[test]
    fn a_refreshed_status_keeps_the_cursor_on_a_row() {
        let mut overlay = SettingsOverlay::open(Tabs::default(), status(true));
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
