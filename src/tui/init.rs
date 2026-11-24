//! The first-run initialization prompt.

use std::{
    io,
    sync::atomic::{AtomicBool, Ordering},
};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::backend::Backend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Frame, Terminal};

use crate::app::Ownership;
use crate::domain::managed::InitPlan;

use super::{
    IDLE_TICK, TuiError,
    events::{Step, TerminalEvents},
};

pub(super) fn confirm_initialization<B: Backend<Error = io::Error>>(
    terminal: &mut Terminal<B>,
    events: &mut TerminalEvents,
    plan: &InitPlan,
    terminated: &AtomicBool,
) -> Result<bool, TuiError> {
    terminal.draw(|frame| render_init(frame, plan))?;
    loop {
        if terminated.load(Ordering::Relaxed) {
            return Ok(false);
        }
        let event = match events.next(IDLE_TICK)? {
            Step::Input(event) => event,
            Step::Tick(_) => continue,
            Step::Closed => return Ok(false),
        };
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                    return Ok(false);
                }
                if let Some(approved) = init_decision(key) {
                    return Ok(approved);
                }
            }
            Event::Resize(_, _) => {
                terminal.draw(|frame| render_init(frame, plan))?;
            }
            _ => {}
        }
    }
}

fn render_init(frame: &mut Frame<'_>, plan: &InitPlan) {
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(frame.area());
    let changes = [
        (plan.create_entry, "Create ~/.ssh/config"),
        (plan.create_managed, "Create managed config"),
        (plan.insert_include, "Add Portico Include"),
        (plan.reposition_include, "Reposition Portico Include"),
        (plan.backup_entry, "Back up ~/.ssh/config"),
        (plan.fix_portico_permissions, "Fix Portico permissions"),
        (plan.fix_managed_permissions, "Fix managed permissions"),
    ]
    .into_iter()
    .filter_map(|(needed, label)| needed.then_some(label))
    .collect::<Vec<_>>()
    .join("\n");
    frame.render_widget(
        Paragraph::new(changes).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Initialize Portico"),
        ),
        areas[0],
    );
    frame.render_widget(Paragraph::new("y initialize  n/q/Esc cancel"), areas[1]);
}

pub(super) fn ownership(ownership: Ownership) -> &'static str {
    match ownership {
        Ownership::Managed => "managed",
        Ownership::External => "external",
    }
}

pub(super) fn init_decision(key: KeyEvent) -> Option<bool> {
    match key.code {
        KeyCode::Char('y') => Some(true),
        KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('n') => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crossterm::event::KeyCode;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    use crate::domain::managed::InitPlan;
    use crate::tui::tests::{buffer_text, key};

    fn render_init_to_buffer(plan: &InitPlan) -> Buffer {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render_init(frame, plan)).unwrap();
        terminal.backend().buffer().clone()
    }

    #[test]
    fn initialization_requires_an_explicit_yes_key() {
        assert_eq!(init_decision(key(KeyCode::Enter)), None);
        assert_eq!(init_decision(key(KeyCode::Char('y'))), Some(true));
        assert_eq!(init_decision(key(KeyCode::Char('q'))), Some(false));
    }

    #[test]
    fn initialization_screen_shows_the_plan_and_confirmation_key() {
        let plan = InitPlan {
            create_entry: true,
            create_managed: true,
            insert_include: true,
            reposition_include: false,
            backup_entry: false,
            fix_portico_permissions: false,
            fix_managed_permissions: false,
        };

        let text = buffer_text(&render_init_to_buffer(&plan));

        assert!(text.contains("Create ~/.ssh/config"));
        assert!(text.contains("Create managed config"));
        assert!(text.contains("Add Portico Include"));
        assert!(text.contains("y initialize"));
    }
}
