//! The forward dialog of the hosts and container tabs: the remote port of a host or container
//! with its suggestions, the local port, and the worker that finds the suggestions while the
//! interface keeps handling keys. On "this machine" it shows where each port is already
//! reachable instead.

use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};

use super::{
    Tab, TextInput, UiAction, UiState,
    containers::wrap,
    dropdown::{SUGGESTION_ROWS, Suggestions as Dropdown, suggestion_lines},
    tunnels::TunnelAction,
};
use crate::{
    app::{App, ForwardTarget, PortSuggestion, StartOutcome, Suggestions},
    backend::{Reach, Target},
    domain::suggest::{Candidate, Origin},
    domain::tunnel::{
        Tunnel,
        runtime::{Bind, default_local_port, try_bind},
    },
};

/// The widest the dialog grows.
const MAX_WIDTH: u16 = 64;
const REMOTE_LABEL: &str = "Remote port";
const LOCAL_LABEL: &str = "Local port";

/// The dialog's text fields.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Field {
    Remote,
    Local,
}

/// What the forward worker found for the dialog.
#[derive(Debug, Default)]
pub(super) struct Loaded {
    pub(super) suggestions: Suggestions,
    /// Where each port of a container on this machine is reachable, `(port, address)`.
    pub(super) addresses: Vec<(u16, String)>,
}

/// What a key in the dialog asks for.
#[derive(Debug, Eq, PartialEq)]
enum DialogAction {
    None,
    Close,
    Submit {
        reach: Option<Reach>,
        remote: u16,
        local: Option<u16>,
    },
    CancelStart,
    Copy(String),
    Open(String),
}

/// The open forward dialog.
#[derive(Debug)]
pub(super) struct ForwardDialog {
    target: ForwardTarget,
    /// Set until the suggestions arrive.
    loading: bool,
    ports: Vec<PortSuggestion>,
    reach: Option<Reach>,
    unavailable: Option<String>,
    /// On this machine, where each port is reachable, `(port, address)`.
    addresses: Vec<(u16, String)>,
    selected_address: usize,
    remote: TextInput,
    local: TextInput,
    /// Set once the local port is typed, so it no longer follows the remote one.
    local_edited: bool,
    field: Field,
    dropdown: Option<Dropdown>,
    /// Set while the forward starts.
    starting: bool,
    error: Option<String>,
    status: Option<String>,
    /// Tells whether `127.0.0.1` binds a port, for the local port's default.
    probe: fn(u16) -> Bind,
}

/// Whether `127.0.0.1:<port>` can be bound here.
fn loopback_probe(port: u16) -> Bind {
    try_bind("127.0.0.1", port)
}

impl ForwardDialog {
    /// The dialog for `target`, looking for its ports.
    pub(super) fn open(target: ForwardTarget) -> Self {
        Self {
            target,
            loading: true,
            ports: Vec::new(),
            reach: None,
            unavailable: None,
            addresses: Vec::new(),
            selected_address: 0,
            remote: TextInput::default(),
            local: TextInput::default(),
            local_edited: false,
            field: Field::Remote,
            dropdown: None,
            starting: false,
            error: None,
            status: None,
            probe: loopback_probe,
        }
    }

    /// Whether the target is a container on this machine, which needs no forward.
    fn here(&self) -> bool {
        matches!(
            self.target,
            ForwardTarget::Container {
                target: Target::Local,
                ..
            }
        )
    }

    fn title(&self) -> String {
        format!("Forward a port of {}", self.target.label())
    }

    /// Takes the suggestions the worker found.
    pub(super) fn finish_loading(&mut self, loaded: Loaded) {
        self.loading = false;
        self.ports = loaded.suggestions.ports;
        self.reach = loaded.suggestions.reach;
        self.unavailable = loaded.suggestions.unavailable;
        self.addresses = loaded.addresses;
        self.selected_address = 0;
        self.suggest(false);
    }

    /// Shows why the forward did not start; the dialog stays open.
    pub(super) fn start_failed(&mut self, reason: String) {
        self.starting = false;
        self.error = Some(reason);
    }

    pub(super) fn set_status(&mut self, status: String) {
        self.status = Some(status);
    }

    fn handle_key(&mut self, key: KeyEvent) -> DialogAction {
        self.status = None;
        if self.starting {
            if key.code == KeyCode::Esc {
                self.starting = false;
                self.status = Some("Start cancelled".into());
                return DialogAction::CancelStart;
            }
            return DialogAction::None;
        }
        if self.here() {
            return self.handle_address_key(key);
        }
        if self.dropdown.is_some() {
            match key.code {
                KeyCode::Up => return self.move_dropdown(-1),
                KeyCode::Down => return self.move_dropdown(1),
                KeyCode::Enter => {
                    self.pick();
                    return DialogAction::None;
                }
                KeyCode::Esc => {
                    self.dropdown = None;
                    return DialogAction::None;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Esc => DialogAction::Close,
            KeyCode::Enter => self.submit(),
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Up | KeyCode::Down => {
                self.field = match self.field {
                    Field::Remote => Field::Local,
                    Field::Local => Field::Remote,
                };
                self.dropdown = None;
                DialogAction::None
            }
            KeyCode::Char(' ') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.suggest(true);
                DialogAction::None
            }
            KeyCode::Char(character) if !character.is_ascii_digit() => DialogAction::None,
            _ => {
                let input = match self.field {
                    Field::Remote => &mut self.remote,
                    Field::Local => &mut self.local,
                };
                let before = input.as_str().to_owned();
                input.handle_key(key);
                if input.as_str() != before {
                    self.edited();
                }
                DialogAction::None
            }
        }
    }

    /// The keys of the list of addresses on this machine.
    fn handle_address_key(&mut self, key: KeyEvent) -> DialogAction {
        let selected = self.addresses.get(self.selected_address);
        match key.code {
            KeyCode::Esc => DialogAction::Close,
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected_address = self.selected_address.saturating_sub(1);
                DialogAction::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected_address + 1 < self.addresses.len() {
                    self.selected_address += 1;
                }
                DialogAction::None
            }
            KeyCode::Char('y') => selected.map_or(DialogAction::None, |(_, address)| {
                DialogAction::Copy(address.clone())
            }),
            KeyCode::Char('b') => selected.map_or(DialogAction::None, |(_, address)| {
                DialogAction::Open(format!("http://{address}"))
            }),
            _ => DialogAction::None,
        }
    }

    /// Pastes the digits of `text` into the field being typed.
    fn paste(&mut self, text: &str) {
        if self.here() || self.starting {
            return;
        }
        let digits: String = text.chars().filter(char::is_ascii_digit).collect();
        match self.field {
            Field::Remote => self.remote.paste(&digits),
            Field::Local => self.local.paste(&digits),
        }
        self.edited();
    }

    /// Follows an edit of the field being typed: the local port follows the remote one until
    /// it is typed itself, and the dropdown follows the remote port.
    fn edited(&mut self) {
        self.error = None;
        match self.field {
            Field::Remote => {
                self.follow_remote();
                self.suggest(false);
            }
            Field::Local => self.local_edited = true,
        }
    }

    /// Prefills the local port for the remote one, unless it was typed.
    fn follow_remote(&mut self) {
        if self.local_edited {
            return;
        }
        let local = match self.remote.as_str().parse::<u16>() {
            Ok(remote) if remote > 0 => default_local_port(remote, self.probe),
            _ => None,
        };
        self.local = TextInput::new(&local.map_or_else(String::new, |port| port.to_string()));
    }

    /// Opens, narrows or closes the dropdown for the remote port typed. `on_demand`
    /// (Ctrl+Space) opens it for an exact match too.
    fn suggest(&mut self, on_demand: bool) {
        let previous = self.dropdown.take();
        if self.field != Field::Remote {
            return;
        }
        let query = self.remote.as_str();
        let items: Vec<Candidate> = (self.ports.iter())
            .filter(|suggestion| suggestion.port.to_string().starts_with(query))
            .map(|suggestion| Candidate {
                alias: match &suggestion.note {
                    Some(note) => format!("{}  {note}", suggestion.port),
                    None => suggestion.port.to_string(),
                },
                origin: Origin::Note(String::new()),
            })
            .collect();
        let exact = (self.ports.iter()).any(|suggestion| suggestion.port.to_string() == query);
        if items.is_empty() || (exact && !on_demand) {
            return;
        }
        self.dropdown = Some(Dropdown::reopened(previous, items, on_demand, 0));
    }

    fn move_dropdown(&mut self, by: isize) -> DialogAction {
        if let Some(list) = &mut self.dropdown {
            let selected = list.selected.saturating_add_signed(by);
            list.select(selected);
        }
        DialogAction::None
    }

    /// Fills the remote port with the selected suggestion and closes the dropdown.
    fn pick(&mut self) {
        let Some(list) = self.dropdown.take() else {
            return;
        };
        let alias = &list.items[list.selected].alias;
        let port = alias.split(' ').next().unwrap_or_default();
        self.remote = TextInput::new(port);
        self.error = None;
        self.follow_remote();
    }

    /// The start of the forward typed, or why it cannot start.
    fn submit(&mut self) -> DialogAction {
        let remote = match self.remote.as_str().parse::<u16>() {
            Ok(remote) if remote > 0 => remote,
            _ => {
                self.field = Field::Remote;
                self.error = Some("Type the remote port, from 1 through 65535".into());
                return DialogAction::None;
            }
        };
        let local = match self.local.as_str() {
            "" => None,
            typed => match typed.parse::<u16>() {
                Ok(local) if local > 0 => Some(local),
                _ => {
                    self.field = Field::Local;
                    self.error = Some("The local port is from 1 through 65535".into());
                    return DialogAction::None;
                }
            },
        };
        self.error = None;
        self.starting = true;
        DialogAction::Submit {
            reach: self.reach.clone(),
            remote,
            local,
        }
    }

    fn footer(&self) -> &'static str {
        if self.here() {
            "↑↓ choose  y copy  b open  Esc close"
        } else if self.starting {
            "Esc cancel the start"
        } else if self.dropdown.is_some() {
            "↑↓ choose  Enter pick  Esc close"
        } else {
            "Tab field  Ctrl+Space suggest  Enter forward  Esc cancel"
        }
    }

    /// The dialog's lines at `width` columns.
    fn lines(&self, home: &Path, width: usize) -> Vec<Line<'static>> {
        let dim = Style::default().add_modifier(Modifier::DIM);
        let mut lines = Vec::new();
        if self.here() {
            if self.loading {
                lines.push(Line::styled(
                    "Looking for the container's ports\u{2026}",
                    dim,
                ));
            } else if let Some(unavailable) = &self.unavailable {
                lines.extend(wrapped(unavailable, width, Style::default()));
            } else if self.addresses.is_empty() {
                let none = format!(
                    "no port of {} is reachable from this machine; publish one",
                    self.target.short()
                );
                lines.extend(wrapped(&none, width, Style::default()));
            } else {
                lines.push(Line::raw("Reachable from this machine without a forward:"));
                for (index, (port, address)) in self.addresses.iter().enumerate() {
                    let text = format!("{address}  (port {port})");
                    lines.push(if index == self.selected_address {
                        Line::from(vec![
                            Span::raw("> "),
                            Span::styled(text, Style::default().add_modifier(Modifier::REVERSED)),
                        ])
                    } else {
                        Line::raw(format!("  {text}"))
                    });
                }
            }
        } else {
            for (field, label, input) in [
                (Field::Remote, REMOTE_LABEL, &self.remote),
                (Field::Local, LOCAL_LABEL, &self.local),
            ] {
                let focused = field == self.field && !self.starting;
                let mut spans = vec![Span::raw(format!(
                    "{} {label}: ",
                    if focused { '>' } else { ' ' }
                ))];
                spans.extend(input.spans(focused));
                lines.push(Line::from(spans));
                if field == Field::Remote
                    && let Some(list) = &self.dropdown
                {
                    lines.extend(suggestion_lines(list, label, home, width, SUGGESTION_ROWS));
                }
            }
            lines.push(Line::raw(""));
            if self.loading {
                lines.push(Line::styled(
                    "Looking for listening ports\u{2026} (type a port meanwhile)",
                    dim,
                ));
            } else if let Some(unavailable) = &self.unavailable {
                lines.extend(wrapped(unavailable, width, Style::default()));
            } else if self.ports.is_empty() {
                lines.push(Line::raw("No listening port found; type one"));
            }
            if self.starting {
                lines.push(Line::raw("Starting the forward\u{2026} (Esc to cancel)"));
            }
        }
        if let Some(error) = &self.error {
            lines.extend(wrapped(error, width, Style::default().fg(Color::Red)));
        }
        if let Some(status) = &self.status {
            lines.extend(wrapped(status, width, Style::default()));
        }
        lines.push(Line::raw(""));
        lines.extend(wrapped(self.footer(), width, dim));
        lines
    }

    /// Draws the dialog over the middle of `area`.
    pub(super) fn render(&self, frame: &mut Frame<'_>, area: Rect, home: &Path) {
        let width = area.width.min(MAX_WIDTH);
        let lines = self.lines(home, usize::from(width.saturating_sub(2)));
        let height = u16::try_from(lines.len() + 2)
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
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(self.title())),
            overlay,
        );
    }
}

/// `text` wrapped to `width` columns, each line in `style`.
fn wrapped(text: &str, width: usize, style: Style) -> Vec<Line<'static>> {
    wrap(text, width)
        .into_iter()
        .map(|line| Line::styled(line, style))
        .collect()
}

impl UiState {
    /// Opens the forward dialog for `target` and asks for its suggestions.
    pub(super) fn open_forward(&mut self, target: ForwardTarget) -> UiAction {
        self.forward = Some(ForwardDialog::open(target.clone()));
        UiAction::OpenForward(target)
    }

    /// The open forward dialog's keys.
    pub(super) fn handle_forward_key(&mut self, key: KeyEvent) -> UiAction {
        let Some(dialog) = &mut self.forward else {
            return UiAction::None;
        };
        match dialog.handle_key(key) {
            DialogAction::None => UiAction::None,
            DialogAction::Close => {
                self.forward = None;
                UiAction::CloseForward
            }
            DialogAction::Submit {
                reach,
                remote,
                local,
            } => UiAction::Tunnels(TunnelAction::Forward {
                target: dialog.target.clone(),
                reach,
                remote,
                local,
            }),
            DialogAction::CancelStart => UiAction::Tunnels(TunnelAction::CancelStart),
            DialogAction::Copy(address) => UiAction::Copy(address),
            DialogAction::Open(url) => UiAction::OpenUrl(url),
        }
    }

    /// Pastes into the open forward dialog.
    pub(super) fn paste_forward(&mut self, text: &str) {
        if let Some(dialog) = &mut self.forward {
            dialog.paste(text);
        }
    }

    /// Applies a forward's start: once it runs, the dialog closes and the tunnels tab shows it
    /// selected (or, with that tab hidden, the status line says where it listens); otherwise
    /// the dialog stays open with the reason.
    pub(super) fn finish_forward(&mut self, result: Result<(Tunnel, StartOutcome), String>) {
        let tunnel = match result {
            Ok((tunnel, StartOutcome::Started { .. } | StartOutcome::AlreadyRunning { .. })) => {
                tunnel
            }
            Ok((
                _,
                StartOutcome::Failed {
                    reason,
                    explanation,
                },
            )) => {
                let next = explanation
                    .and_then(|explanation| explanation.next.first().cloned())
                    .map_or(String::new(), |next| format!("; {next}"));
                if let Some(dialog) = &mut self.forward {
                    dialog.start_failed(format!("{reason}{next}"));
                }
                return;
            }
            Err(error) => {
                if let Some(dialog) = &mut self.forward {
                    dialog.start_failed(error);
                }
                return;
            }
        };
        let Some(dialog) = self.forward.take() else {
            return;
        };
        // The port of the target, as typed: a published port's destination is the host's.
        let remote = dialog.remote.as_str().parse::<u16>().ok().or_else(|| {
            tunnel
                .forward
                .destination
                .as_ref()
                .map(|destination| destination.port)
        });
        let message = format!(
            "Forwarding 127.0.0.1:{} \u{2192} {}:{} (temporary forward {})",
            tunnel.forward.bind_port,
            dialog.target.short(),
            remote.unwrap_or(0),
            tunnel.name,
        );
        if self.tabs.tunnels {
            self.tab = Tab::Tunnels;
            self.tunnels.select_new(&tunnel.name);
            self.tunnels.set_status(message);
        } else {
            match self.tab {
                Tab::Containers(backend) => {
                    self.containers.panel_mut(backend).set_status(Some(message));
                }
                _ => self.status = Some(message),
            }
        }
    }
}

struct Request {
    generation: u64,
    target: ForwardTarget,
    cancelled: Arc<AtomicBool>,
}

/// Finds a forward target's suggestions on a worker thread, one at a time, with the scheme of
/// the lister: asking again or cancelling kills the running `ssh`, and a stale result is
/// dropped by its generation.
pub(super) struct Suggester {
    requests: mpsc::Sender<Request>,
    results: mpsc::Receiver<(u64, Loaded)>,
    generation: u64,
    running: Option<Arc<AtomicBool>>,
}

impl Suggester {
    pub(super) fn spawn<'scope>(
        scope: &'scope thread::Scope<'scope, '_>,
        app: &'scope App,
    ) -> Self {
        let (requests, receiver) = mpsc::channel::<Request>();
        let (sender, results) = mpsc::channel();
        scope.spawn(move || {
            while let Ok(request) = receiver.recv() {
                if request.cancelled.load(Ordering::Relaxed) {
                    continue;
                }
                let loaded = load(app, &request.target, &request.cancelled);
                if request.cancelled.load(Ordering::Relaxed) {
                    continue;
                }
                if sender.send((request.generation, loaded)).is_err() {
                    return;
                }
            }
        });
        Self {
            requests,
            results,
            generation: 0,
            running: None,
        }
    }

    /// Asks for `target`'s suggestions, cancelling the request still running.
    pub(super) fn request(&mut self, target: ForwardTarget) {
        self.cancel();
        self.generation += 1;
        let cancelled = Arc::new(AtomicBool::new(false));
        let request = Request {
            generation: self.generation,
            target,
            cancelled: Arc::clone(&cancelled),
        };
        if self.requests.send(request).is_ok() {
            self.running = Some(cancelled);
        }
    }

    pub(super) fn cancel(&mut self) {
        if let Some(cancelled) = self.running.take() {
            cancelled.store(true, Ordering::Relaxed);
        }
    }

    pub(super) fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// The current request's result, if it has arrived.
    pub(super) fn try_finished(&mut self) -> Option<Loaded> {
        let mut current = None;
        while let Ok((generation, loaded)) = self.results.try_recv() {
            if self.running.is_some() && generation == self.generation {
                self.running = None;
                current = Some(loaded);
            }
        }
        current
    }
}

impl Drop for Suggester {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// `target`'s suggestions and, for a container on this machine, where its ports are reachable.
fn load(app: &App, target: &ForwardTarget, cancelled: &AtomicBool) -> Loaded {
    let suggestions = app.forward_suggestions(target, Some(cancelled));
    let addresses = match (target, &suggestions.reach) {
        (
            ForwardTarget::Container {
                target: Target::Local,
                container,
                ..
            },
            Some(reach),
        ) => app.local_addresses(container, reach),
        _ => Vec::new(),
    };
    Loaded {
        suggestions,
        addresses,
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyCode;

    use super::*;
    use crate::{
        app::{Ownership, PortSuggestion, StartOutcome, Suggestions as PortSuggestions},
        backend::{BackendId, Container, ContainerList, Listing, Reach, Target},
        domain::tunnel::{Forward, Kind, Tunnel, TunnelReport, TunnelStatus, runtime::Bind},
        tui::{
            Tab, UiAction, UiState,
            tests::{press, rendered, summary, test_state, type_text},
            tunnels::{TunnelAction, tests::screen_with},
        },
    };

    /// 80 needs root here; every other port is free.
    fn probe(port: u16) -> Bind {
        if port < 1024 {
            Bind::Denied
        } else {
            Bind::Free
        }
    }

    fn prod() -> ForwardTarget {
        ForwardTarget::Host("prod".into())
    }

    /// The hosts tab with `prod` selected and its forward dialog open.
    fn dialog_state() -> UiState {
        let mut state = test_state(vec![summary("prod", Ownership::Managed)]);
        assert_eq!(
            press(&mut state, KeyCode::Char('f')),
            UiAction::OpenForward(prod())
        );
        dialog(&mut state).probe = probe;
        state
    }

    fn dialog(state: &mut UiState) -> &mut ForwardDialog {
        state.forward.as_mut().expect("the forward dialog is open")
    }

    fn suggestion(port: u16, note: Option<&str>) -> PortSuggestion {
        PortSuggestion {
            port,
            note: note.map(String::from),
        }
    }

    fn loaded(ports: Vec<PortSuggestion>) -> Loaded {
        Loaded {
            suggestions: PortSuggestions {
                ports,
                reach: None,
                unavailable: None,
            },
            addresses: Vec::new(),
        }
    }

    fn three_ports() -> Loaded {
        loaded(vec![
            suggestion(80, None),
            suggestion(5432, Some("loopback only")),
            suggestion(8080, Some("published")),
        ])
    }

    fn dropdown_rows(state: &mut UiState) -> Vec<String> {
        dialog(state)
            .dropdown
            .as_ref()
            .map(|list| list.items.iter().map(|item| item.alias.clone()).collect())
            .unwrap_or_default()
    }

    fn temporary(name: &str, local: u16, status: TunnelStatus) -> TunnelReport {
        TunnelReport {
            tunnel: Tunnel {
                name: name.into(),
                host: "prod".into(),
                forward: Forward::parse(Kind::Local, &format!("127.0.0.1:{local}:localhost:80"))
                    .unwrap(),
                container: None,
            },
            status,
            warnings: Vec::new(),
            temporary: true,
            reaches: "prod".into(),
        }
    }

    #[test]
    fn f_on_a_host_opens_the_dialog_titled_after_it() {
        let state = dialog_state();
        let text = rendered(&state);
        assert!(text.contains("Forward a port of prod"), "{text}");
        assert!(text.contains("Looking for listening ports"), "{text}");
    }

    #[test]
    fn digits_fill_the_remote_port_while_suggestions_load() {
        let mut state = dialog_state();
        type_text(&mut state, "8x0");
        let open = dialog(&mut state);
        assert_eq!(open.remote.as_str(), "80");
        assert!(open.loading);
        // 80 cannot be bound without root, so the local port starts 8000 above it.
        assert_eq!(open.local.as_str(), "8080");
        // An edited local port no longer follows the remote one.
        press(&mut state, KeyCode::Tab);
        press(&mut state, KeyCode::Backspace);
        press(&mut state, KeyCode::BackTab);
        press(&mut state, KeyCode::Backspace);
        type_text(&mut state, "1");
        assert_eq!(dialog(&mut state).local.as_str(), "808");
    }

    #[test]
    fn the_dropdown_lists_the_ports_with_their_notes() {
        let mut state = dialog_state();
        dialog(&mut state).finish_loading(three_ports());
        assert_eq!(
            dropdown_rows(&mut state),
            ["80", "5432  loopback only", "8080  published"]
        );
        let text = rendered(&state);
        assert!(text.contains("5432  loopback only"), "{text}");
        // Typing narrows it; an exact match closes it.
        type_text(&mut state, "8");
        assert_eq!(dropdown_rows(&mut state), ["80", "8080  published"]);
        type_text(&mut state, "0");
        assert!(dialog(&mut state).dropdown.is_none());
    }

    #[test]
    fn enter_picks_a_suggestion_then_starts_the_forward() {
        let mut state = dialog_state();
        dialog(&mut state).finish_loading(three_ports());
        press(&mut state, KeyCode::Down);
        assert_eq!(press(&mut state, KeyCode::Enter), UiAction::None);
        assert_eq!(dialog(&mut state).remote.as_str(), "5432");
        assert_eq!(dialog(&mut state).local.as_str(), "5432");
        assert_eq!(
            press(&mut state, KeyCode::Enter),
            UiAction::Tunnels(TunnelAction::Forward {
                target: prod(),
                reach: None,
                remote: 5432,
                local: Some(5432),
            })
        );
        assert!(dialog(&mut state).starting);
        // Esc cancels the start and keeps the dialog; a second Esc closes it.
        assert_eq!(
            press(&mut state, KeyCode::Esc),
            UiAction::Tunnels(TunnelAction::CancelStart)
        );
        assert!(!dialog(&mut state).starting);
        assert_eq!(press(&mut state, KeyCode::Esc), UiAction::CloseForward);
        assert!(state.forward.is_none());
    }

    #[test]
    fn a_started_forward_shows_selected_in_the_tunnels_tab() {
        let mut state = dialog_state();
        state.tunnels = screen_with(vec![
            temporary("_web-80", 8081, TunnelStatus::Stopped),
            temporary(
                "_prod-80",
                8080,
                TunnelStatus::Running { pid: 9, started: 0 },
            ),
        ]);
        let tunnel = temporary("_prod-80", 8080, TunnelStatus::Stopped).tunnel;
        state.finish_forward(Ok((tunnel, StartOutcome::Started { pid: 9 })));
        assert!(state.forward.is_none());
        assert_eq!(state.tab, Tab::Tunnels);
        assert_eq!(state.tunnels.selected_name(), Some("_prod-80"));
        assert_eq!(
            state.tunnels.status_line().as_deref(),
            Some("Forwarding 127.0.0.1:8080 \u{2192} prod:80 (temporary forward _prod-80)")
        );
    }

    #[test]
    fn a_published_port_is_named_by_the_port_typed_not_the_host_one() {
        let mut state = dialog_state();
        type_text(&mut state, "80");
        press(&mut state, KeyCode::Enter);
        let mut tunnel = temporary("_prod-80", 9000, TunnelStatus::Stopped).tunnel;
        tunnel.forward = Forward::parse(Kind::Local, "127.0.0.1:9000:localhost:8080").unwrap();
        state.finish_forward(Ok((tunnel, StartOutcome::Started { pid: 9 })));
        assert_eq!(
            state.tunnels.status_line().as_deref(),
            Some("Forwarding 127.0.0.1:9000 \u{2192} prod:80 (temporary forward _prod-80)")
        );
    }

    #[test]
    fn with_the_tunnels_tab_hidden_the_address_goes_to_the_status_line() {
        let mut state = dialog_state();
        state.tabs.tunnels = false;
        let tunnel = temporary("_prod-80", 8080, TunnelStatus::Stopped).tunnel;
        state.finish_forward(Ok((tunnel, StartOutcome::AlreadyRunning { pid: 9 })));
        assert_eq!(state.tab, Tab::Hosts);
        assert!(state.forward.is_none());
        assert!(
            state
                .status
                .as_deref()
                .is_some_and(|status| status.starts_with("Forwarding 127.0.0.1:8080")),
            "{:?}",
            state.status
        );
    }

    #[test]
    fn a_failed_forward_keeps_the_dialog_open_with_the_reason() {
        let mut state = dialog_state();
        type_text(&mut state, "80");
        press(&mut state, KeyCode::Enter);
        let tunnel = temporary("_prod-80", 8080, TunnelStatus::Stopped).tunnel;
        state.finish_forward(Ok((
            tunnel,
            StartOutcome::Failed {
                reason: "Connection refused".into(),
                explanation: None,
            },
        )));
        assert_eq!(state.tab, Tab::Hosts);
        assert!(!dialog(&mut state).starting);
        let text = rendered(&state);
        assert!(text.contains("Connection refused"), "{text}");
        state.finish_forward(Err("the local port 8080 is in use".into()));
        assert!(rendered(&state).contains("the local port 8080 is in use"));
    }

    #[test]
    fn unavailable_suggestions_say_why_and_keep_free_entry() {
        let mut state = dialog_state();
        dialog(&mut state).finish_loading(Loaded {
            suggestions: PortSuggestions {
                unavailable: Some("could not list the ports of prod: timed out".into()),
                ..PortSuggestions::default()
            },
            addresses: Vec::new(),
        });
        assert!(rendered(&state).contains("could not list the ports of prod: timed out"));
        type_text(&mut state, "3000");
        assert_eq!(
            press(&mut state, KeyCode::Enter),
            UiAction::Tunnels(TunnelAction::Forward {
                target: prod(),
                reach: None,
                remote: 3000,
                local: Some(3000),
            })
        );
    }

    #[test]
    fn enter_without_a_port_says_so() {
        let mut state = dialog_state();
        assert_eq!(press(&mut state, KeyCode::Enter), UiAction::None);
        assert!(rendered(&state).contains("Type the remote port"));
    }

    fn web() -> Container {
        Container {
            id: "web-id".into(),
            name: "web".into(),
            image: "nginx".into(),
            command: "nginx".into(),
            running_for: "1 hour".into(),
            status: "Up 1 hour".into(),
            ports: "0.0.0.0:8080->80/tcp".into(),
            networks: "bridge".into(),
            mounts: String::new(),
            project: None,
            service: None,
            number: None,
            working_dir: None,
        }
    }

    #[test]
    fn f_on_this_machine_shows_where_each_port_is_reachable() {
        let mut state = test_state(vec![summary("prod", Ownership::Managed)]);
        state.tab = Tab::Containers(BackendId::Docker);
        let panel = state.containers.panel_mut(BackendId::Docker);
        panel.show(Target::Local);
        panel.finish(
            Target::Local,
            Ok(ContainerList {
                backend: BackendId::Docker,
                target: Target::Local,
                listing: Listing {
                    containers: vec![web()],
                    skipped: 0,
                },
                via: None,
                note: None,
            }),
        );
        let target = ForwardTarget::Container {
            target: Target::Local,
            backend: BackendId::Docker,
            container: web(),
            via: None,
        };
        assert_eq!(
            press(&mut state, KeyCode::Char('f')),
            UiAction::OpenForward(target)
        );
        dialog(&mut state).finish_loading(Loaded {
            suggestions: PortSuggestions {
                ports: vec![suggestion(80, Some("published 8080"))],
                reach: Some(Reach {
                    address: Some("172.17.0.2".into()),
                    host_network: false,
                    ports: Vec::new(),
                }),
                unavailable: None,
            },
            addresses: vec![(80, "127.0.0.1:8080".into()), (80, "172.17.0.2:80".into())],
        });
        let text = rendered(&state);
        assert!(
            text.contains("Forward a port of web on this machine"),
            "{text}"
        );
        assert!(text.contains("127.0.0.1:8080"), "{text}");
        assert!(text.contains("y copy"), "{text}");
        assert!(text.contains("b open"), "{text}");
        assert_eq!(
            press(&mut state, KeyCode::Char('y')),
            UiAction::Copy("127.0.0.1:8080".into())
        );
        press(&mut state, KeyCode::Down);
        assert_eq!(
            press(&mut state, KeyCode::Char('b')),
            UiAction::OpenUrl("http://172.17.0.2:80".into())
        );
        // Nothing is forwarded from this machine to itself.
        assert_eq!(press(&mut state, KeyCode::Enter), UiAction::None);
        type_text(&mut state, "80");
        assert_eq!(press(&mut state, KeyCode::Enter), UiAction::None);
        assert_eq!(press(&mut state, KeyCode::Esc), UiAction::CloseForward);
    }

    #[test]
    fn a_container_without_a_reachable_port_says_to_publish_one() {
        let mut state = test_state(Vec::new());
        state.open_forward(ForwardTarget::Container {
            target: Target::Local,
            backend: BackendId::Docker,
            container: web(),
            via: None,
        });
        dialog(&mut state).finish_loading(loaded(Vec::new()));
        assert!(rendered(&state).contains("no port of web is reachable from this machine"));
    }

    #[test]
    fn the_footers_name_f() {
        let state = test_state(vec![summary("prod", Ownership::Managed)]);
        let footer = crate::tui::footer_text(&state);
        assert!(footer.contains("n new terminal  f forward"), "{footer}");
    }
}
