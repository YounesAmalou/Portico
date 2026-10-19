use std::{
    io, panic,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError},
    },
    thread,
    time::Duration,
};

use crossterm::{
    cursor::{Hide, Show},
    event::{
        DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
        supports_keyboard_enhancement,
    },
};
use ratatui::{
    Frame, Terminal,
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};

mod containers;
mod dropdown;
mod hangup;
mod identities;
mod lister;
mod migrate;
mod planner;
mod tester;
mod tunnels;

use crate::{
    adopt,
    app::{
        App, AppError, HostDetails, HostInventory, HostSummary, Ownership, Target, TestOutcome,
        TestTarget,
    },
    config_error::ConfigRejection,
    discovery::DiscoveryWarning,
    explain::Explanation,
    fuzzy,
    host::{Change, ClearField, Host, HostChanges, HostError, HostField, parse_port},
    identity::IdentityInventory,
    managed::{InitPlan, TransactionError},
    openssh::{ConnectionOutcome, PresentedKey, ResolvedConfig},
    picker::HIT,
    suggest::{self, Candidate},
};
use containers::{Panel, PanelAction, fix_status, shell_status, show_fix};
use dropdown::{SUGGESTION_ROWS, Suggestions, suggestion_lines};
use hangup::{HangupWatch, Input};
use identities::Indexer;
use lister::Lister;
use migrate::{MigrateAction, MigrateState};
use planner::Planner;
use tester::{Finished, Job, Origin, Tester};
use thiserror::Error;
use tunnels::{Starter, TunnelAction, TunnelsScreen};

/// One row of the Add/Edit form.
struct FormField {
    field: HostField,
    label: &'static str,
    placeholder: &'static str,
    hint: &'static str,
    optional: bool,
}

const FORM_FIELDS: [FormField; 6] = [
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

/// Longest quiet period after a selection change before the host is resolved.
const RESOLVE_DEBOUNCE: Duration = Duration::from_millis(60);

#[derive(Debug, Error)]
pub enum TuiError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    App(#[from] AppError),
    #[error("child process failed: {0}")]
    Child(String),
}

pub fn run(app: &App) -> Result<(), TuiError> {
    let signals = Signals::install()?;
    let terminated = signals.flag();
    // A closed terminal ends the TUI like SIGHUP, even when crossterm is reading it.
    let _hangup = HangupWatch::start(signals.shared_flag())?;
    let session = Arc::new(TerminalSession::detect());
    // Declared before the guard, so the terminal is restored before the hook is removed.
    let _panic_hook = PanicHook::install({
        let session = Arc::clone(&session);
        move || {
            let _ = session.leave();
        }
    });
    let _guard = TerminalGuard(Arc::clone(&session));
    session.enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let plan = app.init_plan()?;
    if plan.requires_confirmation() && !confirm_initialization(&mut terminal, &plan, terminated)? {
        return Ok(());
    }
    if plan.requires_confirmation()
        && let Err(error) = app.initialize_cancellable(true, terminated)
    {
        if terminated.load(Ordering::Relaxed) {
            return Ok(());
        }
        return Err(error.into());
    }

    browse(app, &mut terminal, &signals, &session)
}

/// Clears the alternate screen and forgets what was drawn, so the next frame repaints everything.
///
/// Unlike `Terminal::clear`, this never asks the terminal for the cursor position: the
/// cursor is hidden here, and a terminal that is slow to answer must not abort the interface.
fn redraw_from_scratch<B: Backend>(terminal: &mut Terminal<B>) -> Result<(), B::Error> {
    let size = terminal.size()?;
    terminal.resize(Rect::new(0, 0, size.width, size.height))
}

fn confirm_initialization<B: Backend<Error = io::Error>>(
    terminal: &mut Terminal<B>,
    plan: &InitPlan,
    terminated: &AtomicBool,
) -> Result<bool, TuiError> {
    terminal.draw(|frame| render_init(frame, plan))?;
    loop {
        if terminated.load(Ordering::Relaxed) {
            return Ok(false);
        }
        let event = match hangup::next_event(Duration::from_millis(100))? {
            Input::Event(event) => event,
            Input::Idle => continue,
            Input::Closed => return Ok(false),
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

fn browse<B: Backend<Error = io::Error>>(
    app: &App,
    terminal: &mut Terminal<B>,
    signals: &Signals,
    session: &TerminalSession,
) -> Result<(), TuiError> {
    // Host details come from `ssh -G` and connection tests run `ssh`, so workers do both while
    // the UI keeps handling keys. The resolver and the tester are dropped (cancelling any running
    // `ssh`) before the scope waits for the workers.
    thread::scope(|scope| {
        let resolver = Resolver::spawn(scope, app);
        let tester = Tester::spawn(scope, app);
        let lister = Lister::spawn(scope, app);
        let planner = Planner::spawn(scope, app);
        let indexer = Indexer::spawn(scope, app);
        let starter = Starter::spawn(scope, app);
        browse_hosts(
            app, terminal, signals, session, resolver, tester, lister, planner, indexer, starter,
        )
    })
}

#[expect(
    clippy::too_many_arguments,
    reason = "each worker is spawned in the caller's scope"
)]
fn browse_hosts<B: Backend<Error = io::Error>>(
    app: &App,
    terminal: &mut Terminal<B>,
    signals: &Signals,
    session: &TerminalSession,
    mut resolver: Resolver,
    mut tester: Tester,
    mut lister: Lister,
    mut planner: Planner,
    mut indexer: Indexer,
    mut starter: Starter,
) -> Result<(), TuiError> {
    let terminated = signals.flag();
    let mut state = UiState::new(app.hosts()?, session.keyboard_enhancement());
    state.home = app.home().to_owned();
    sync_details(&mut state, &mut resolver, false);
    let mut redraw = true;

    loop {
        if terminated.load(Ordering::Relaxed) {
            return Ok(());
        }
        if let Some((host, loaded)) = resolver.poll() {
            state.details.insert(host, loaded);
            state.open_pending_edit();
            redraw = true;
        }
        if let Some(finished) = tester.poll() {
            state.record_test(finished, local_time());
            redraw = true;
        }
        if let Some((target, result)) = lister.try_finished() {
            state.containers.finish(target, result);
            redraw = true;
        }
        if let Some((planned, preselect)) = planner.try_finished() {
            state.finish_plan(planned, &preselect, app.home());
            redraw = true;
        }
        if let Some(built) = indexer.try_finished() {
            state.finish_identities(built);
            redraw = true;
        }
        // The IdentityFile suggestions draw on every host's identities, built once a form opens.
        if state.in_form() {
            indexer.request();
        }
        if let Some((name, result)) = starter.poll() {
            state.tunnels.finish_start(&name, result);
            state.tunnels.reload(app);
            redraw = true;
        }
        if state.tab == Tab::Tunnels && state.tunnels.is_stale() {
            state.tunnels.reload(app);
            redraw = true;
        }
        let starting = starter.running().map(str::to_owned);
        state.tunnels.set_starting(starting);
        let testing = tester.running().map(str::to_owned);
        if state.testing != testing {
            state.testing = testing;
            redraw = true;
        }
        if redraw {
            terminal.draw(|frame| render(frame, &state))?;
            redraw = false;
        }
        // Poll quickly while a resolution or a test is running so its result appears promptly.
        let wait = if resolver.is_pending()
            || tester.running().is_some()
            || lister.is_running()
            || planner.is_running()
            || indexer.is_running()
            || starter.running().is_some()
        {
            Duration::from_millis(25)
        } else {
            Duration::from_millis(100)
        };
        let event = match hangup::next_event(wait)? {
            Input::Event(event) => event,
            Input::Idle => continue,
            Input::Closed => return Ok(()),
        };
        if matches!(event, Event::Resize(_, _)) {
            redraw = true;
            continue;
        }
        if let Event::Paste(text) = event {
            if state.handle_paste(&text) == UiAction::FormEdited {
                tester.cancel_if(|origin, _| *origin == Origin::Form);
            }
            sync_details(&mut state, &mut resolver, false);
            redraw = true;
            continue;
        }
        let Event::Key(key) = event else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Ok(());
        }

        let mut inventory_changed = false;
        let tab = state.tab;
        let action = state.handle_key(key);
        if state.tab != tab && state.tab == Tab::Tunnels {
            state.tunnels.reload(app);
        }
        match action {
            UiAction::None => {}
            UiAction::Quit => return Ok(()),
            UiAction::Tunnels(action) => run_tunnel_action(app, &mut state, &mut starter, action),
            UiAction::Connect {
                alias,
                new_terminal,
            } => {
                let result = if new_terminal {
                    app.connect(&alias, true).map_err(TuiError::from)
                } else {
                    suspended(terminal, session, signals, || {
                        app.connect_cancellable(&alias, terminated)
                            .map_err(TuiError::from)
                    })?
                };
                state.finish_connect(&alias, result);
            }
            UiAction::Containers(PanelAction::List(target)) => {
                lister.list(&mut state.containers, target);
            }
            UiAction::Containers(PanelAction::CancelList) => {
                lister.cancel();
            }
            UiAction::Containers(PanelAction::Shell {
                target,
                id,
                user,
                via,
                new_terminal,
            }) => {
                let shell = |cancelled| {
                    let user = user.as_deref();
                    app.container_shell(&target, &id, user, via, new_terminal, cancelled, true)
                };
                let result = if new_terminal {
                    Ok(shell(None))
                } else {
                    suspended(terminal, session, signals, || Ok(shell(Some(terminated))))?
                };
                state
                    .containers
                    .set_status(Some(shell_status(&target, result)));
            }
            UiAction::Containers(PanelAction::Fix { target, offer }) => {
                let result = suspended(terminal, session, signals, || {
                    show_fix(&mut io::stdout(), &target, &offer)?;
                    Ok(app.docker_fix(&target, &offer))
                })?;
                state.containers.set_status(fix_status(&offer, result));
                lister.list(&mut state.containers, target);
            }
            UiAction::Containers(_) => {}
            UiAction::Refresh => {
                // Agents and key files may have changed too: ask them again on the next look.
                app.forget_identities();
                indexer.reset();
                state.identity_inventory = None;
                inventory_changed = apply_refresh(&mut state, || app.hosts());
                if inventory_changed {
                    tester.cancel_if(|origin, _| matches!(origin, Origin::Host(_)));
                }
            }
            UiAction::Add(host) => {
                let alias = host.alias().to_owned();
                let result = app.add_cancellable(host, terminated);
                inventory_changed = finish_mutation(
                    app,
                    &mut state,
                    result,
                    Some(&alias),
                    &[],
                    format!("Saved {alias}"),
                );
            }
            UiAction::Edit { alias, changes } => {
                let selected = match &changes.alias {
                    Change::Set(changed) => changed.clone(),
                    Change::Keep | Change::Clear => alias.clone(),
                };
                let result = app.edit_cancellable(&alias, changes, terminated);
                let edited = [alias.as_str(), selected.as_str()];
                if result.is_ok() {
                    cancel_host_test(&mut tester, &edited);
                }
                inventory_changed = finish_mutation(
                    app,
                    &mut state,
                    result,
                    Some(&selected),
                    &edited,
                    format!("Saved {selected}"),
                );
            }
            UiAction::Remove(alias) => {
                let result = app.remove_cancellable(&alias, terminated);
                if result.is_ok() {
                    cancel_host_test(&mut tester, &[&alias]);
                }
                inventory_changed = finish_mutation(
                    app,
                    &mut state,
                    result,
                    None,
                    &[&alias],
                    format!("Removed {alias}"),
                );
            }
            UiAction::OpenMigrate {
                direction,
                preselect,
            } => {
                // The screen shows that the plan is being checked until it arrives.
                state.migrate = None;
                state.mode = Mode::Migrate;
                if !planner.start(direction, preselect) {
                    state.mode = Mode::Browse;
                    state.status = Some("the adoption worker stopped".into());
                }
            }
            UiAction::CancelPlan => planner.cancel(),
            UiAction::Migrate(aliases) => {
                if let Some(migrate) = state.migrate.take() {
                    let result = app.migrate_cancellable(&migrate.plan, &aliases, terminated);
                    let moved = result.as_ref().map_or_else(|_| Vec::new(), Clone::clone);
                    let changed = moved.iter().map(String::as_str).collect::<Vec<_>>();
                    if result.is_ok() {
                        cancel_host_test(&mut tester, &changed);
                    }
                    let verb = match migrate.plan.direction {
                        adopt::Direction::Adopt => "Adopted",
                        adopt::Direction::Export => "Exported",
                    };
                    inventory_changed = finish_mutation(
                        app,
                        &mut state,
                        result.map(drop),
                        None,
                        &changed,
                        format!("{verb}: {}", moved.join(", ")),
                    );
                }
                // A failure ends the screen too, as its plan may no longer match the files.
                state.mode = Mode::Browse;
            }
            UiAction::Test { origin, target } => tester.start(origin, Job::Test(target)),
            UiAction::CancelTest => tester.cancel(),
            UiAction::FormEdited => tester.cancel_if(|origin, _| *origin == Origin::Form),
            UiAction::Trust(trusted) => {
                if let Some(pending) = state.trust.take()
                    && trusted
                {
                    let job = Job::Trust {
                        target: pending.target,
                        key: pending.key,
                    };
                    tester.start(pending.origin, job);
                }
            }
        }
        // A form's result is only shown in that form.
        if !state.in_form() {
            tester.cancel_if(|origin, _| *origin == Origin::Form);
        }

        sync_details(&mut state, &mut resolver, inventory_changed);
        redraw = true;
    }
}

/// Carries out what the Tunnels tab asked for.
fn run_tunnel_action(app: &App, state: &mut UiState, starter: &mut Starter, action: TunnelAction) {
    let screen = &mut state.tunnels;
    match action {
        TunnelAction::None | TunnelAction::Quit => return,
        TunnelAction::Refresh => screen.set_status("Refreshed"),
        TunnelAction::Start(name) => {
            starter.start(name);
            screen.set_starting(starter.running().map(str::to_owned));
            return;
        }
        TunnelAction::CancelStart => {
            starter.cancel();
            screen.set_starting(None);
        }
        TunnelAction::Stop(name) => screen.set_status(match app.stop_tunnel(&name) {
            Ok(crate::app::StopOutcome::Stopped) => format!("Stopped {name}"),
            Ok(crate::app::StopOutcome::NotRunning) => format!("{name} was not running"),
            Err(error) => error.to_string(),
        }),
        TunnelAction::Add(tunnel) => {
            let done = format!("Saved {}", tunnel.name);
            screen.finish_change(app.add_tunnel(tunnel), done);
        }
        TunnelAction::Edit { name, changes } => {
            let done = format!("Saved {}", changes.name.as_deref().unwrap_or(&name));
            screen.finish_change(app.edit_tunnel(&name, changes), done);
        }
        TunnelAction::Remove(name) => {
            let done = format!("Removed {name}");
            screen.finish_change(app.remove_tunnel(&name), done);
        }
    }
    screen.reload(app);
}

/// Runs `child` in the terminal with the interface suspended and the terminal's Ctrl+C left to
/// the child (Portico waits for it), then redraws. Failing to leave or re-enter the screen is
/// the outer error; the child's own result is passed through.
fn suspended<B: Backend<Error = io::Error>, T>(
    terminal: &mut Terminal<B>,
    session: &TerminalSession,
    signals: &Signals,
    child: impl FnOnce() -> Result<T, TuiError>,
) -> Result<Result<T, TuiError>, TuiError> {
    let result = suspend_for_child(
        || session.leave(),
        || {
            let _foreground = signals.foreground_child();
            child()
        },
        || session.enter(),
    );
    if let Err(TuiError::Io(error)) = result {
        return Err(TuiError::Io(error));
    }
    redraw_from_scratch(terminal)?;
    Ok(result)
}

/// Cancels a running test of one of `aliases`, whose configuration just changed.
fn cancel_host_test(tester: &mut Tester, aliases: &[&str]) {
    tester.cancel_if(|origin, tested| {
        matches!(origin, Origin::Host(_))
            && aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(tested))
    });
}

/// Applies a mutation's outcome to the UI and reports whether the inventory may have changed.
/// On success, the test results of `changed` aliases are dropped.
fn finish_mutation(
    app: &App,
    state: &mut UiState,
    result: Result<(), AppError>,
    preferred: Option<&str>,
    changed: &[&str],
    status: String,
) -> bool {
    match result {
        Ok(()) => {
            complete_mutation(state, preferred, changed, status, || app.hosts());
            true
        }
        Err(error) => fail_mutation(state, &error, || app.hosts()),
    }
}

/// Reports a failed mutation and, when it may have altered the managed file, reloads the
/// inventory so the list reflects what is actually on disk. Returns whether it may have changed.
fn fail_mutation<E, F>(state: &mut UiState, error: &AppError, refresh: F) -> bool
where
    F: FnOnce() -> Result<HostInventory, E>,
{
    state.report_error(error);
    let changed = may_have_changed_inventory(error);
    if changed && let Ok(hosts) = refresh() {
        state.replace_hosts(hosts);
    }
    changed
}

/// Whether a failed mutation may have left the managed file different from what was loaded.
///
/// Rejections before activation (validation, collisions, unknown hosts) leave it untouched. A
/// filesystem failure can strike after the file was replaced or while rolling it back, and an
/// unparsable managed file means it changed behind Portico's back, so those reload.
fn may_have_changed_inventory(error: &AppError) -> bool {
    match error {
        AppError::Transaction(error) => match error {
            TransactionError::NotInitialized
            | TransactionError::Operation(_)
            | TransactionError::Validation(_)
            | TransactionError::Entry(_) => false,
            TransactionError::Managed { .. }
            | TransactionError::Io { .. }
            | TransactionError::Rollback { .. } => true,
        },
        AppError::Init(_) => true,
        AppError::Adopt(error) => matches!(
            error,
            crate::adopt::AdoptError::Io { .. } | crate::adopt::AdoptError::Rollback { .. }
        ),
        AppError::Discovery(_)
        | AppError::OpenSsh(_)
        | AppError::InvalidHost(_)
        | AppError::ReadOnlyExternal(_)
        | AppError::UnknownHost(_)
        | AppError::AliasCollision(_)
        | AppError::IncompleteDiscovery(_)
        | AppError::KnownHosts { .. }
        | AppError::TunnelStore(_)
        | AppError::InvalidTunnel(_)
        | AppError::Tunnel(_) => false,
    }
}

/// Re-reads the inventory on request, so changes made outside Portico appear. Returns whether
/// the inventory was replaced, which makes the caller discard the cached details. A failed read
/// leaves everything as it was and only reports why.
fn apply_refresh<E, F>(state: &mut UiState, read: F) -> bool
where
    E: std::fmt::Display,
    F: FnOnce() -> Result<HostInventory, E>,
{
    match read() {
        Ok(inventory) => {
            let count = inventory.hosts.len();
            state.replace_hosts(inventory);
            state.results.clear();
            state.connections.clear();
            let noun = if count == 1 { "host" } else { "hosts" };
            state.status = Some(format!("Refreshed: {count} {noun}"));
            true
        }
        Err(error) => {
            state.status = Some(format!("Refresh failed: {error}"));
            false
        }
    }
}

fn complete_mutation<E, F>(
    state: &mut UiState,
    preferred: Option<&str>,
    changed: &[&str],
    status: String,
    refresh: F,
) where
    E: std::fmt::Display,
    F: FnOnce() -> Result<HostInventory, E>,
{
    state.mode = Mode::Browse;
    state.form = FormState::default();
    for alias in changed {
        state.results.forget(alias);
        state.connections.forget(alias);
    }
    state.show_raw = false;
    match refresh() {
        Ok(hosts) => {
            state.replace_hosts(hosts);
            if let Some(alias) = preferred {
                state.select_alias(alias);
            }
            state.status = Some(status);
        }
        Err(error) => state.status = Some(format!("{status}, refresh failed: {error}")),
    }
}

type Loaded = Result<HostDetails, String>;

fn load_details(app: &App, host: &HostSummary, cancelled: &AtomicBool) -> Loaded {
    app.show_cancellable(&host.alias, cancelled)
        .map(|details| details_for_selected(host, details))
        .map_err(|error| error.to_string())
}

/// Makes the resolver follow the selection: cached hosts need no work, any other is requested.
fn sync_details(state: &mut UiState, resolver: &mut Resolver, inventory_changed: bool) {
    if inventory_changed {
        resolver.cancel();
        state.details.clear();
    }
    match state.selected_summary() {
        Some(host) if state.details.get(host).is_none() => resolver.request(host),
        _ => resolver.cancel(),
    }
}

struct ResolveRequest {
    generation: u64,
    host: HostSummary,
    cancelled: Arc<AtomicBool>,
}

struct Resolved {
    generation: u64,
    host: HostSummary,
    details: Loaded,
}

struct InFlight {
    host: HostSummary,
    cancelled: Arc<AtomicBool>,
}

/// Resolves one host at a time on a worker thread.
///
/// Every request carries a generation. Superseding a request cancels its `ssh -G` and bumps the
/// generation, so a result that was already on its way back can never be mistaken for the
/// current selection's.
struct Resolver {
    requests: mpsc::Sender<ResolveRequest>,
    results: mpsc::Receiver<Resolved>,
    generation: u64,
    in_flight: Option<InFlight>,
}

impl Resolver {
    fn spawn<'scope>(scope: &'scope thread::Scope<'scope, '_>, app: &'scope App) -> Self {
        let (requests, receiver) = mpsc::channel();
        let (sender, results) = mpsc::channel();
        scope.spawn(move || resolve_worker(app, &receiver, &sender));
        Self {
            requests,
            results,
            generation: 0,
            in_flight: None,
        }
    }

    fn is_pending(&self) -> bool {
        self.in_flight.is_some()
    }

    /// Starts resolving `host`, replacing any other pending request.
    fn request(&mut self, host: &HostSummary) {
        if self
            .in_flight
            .as_ref()
            .is_some_and(|in_flight| in_flight.host == *host)
        {
            return;
        }
        self.cancel();
        self.generation += 1;
        let cancelled = Arc::new(AtomicBool::new(false));
        let request = ResolveRequest {
            generation: self.generation,
            host: host.clone(),
            cancelled: Arc::clone(&cancelled),
        };
        if self.requests.send(request).is_ok() {
            self.in_flight = Some(InFlight {
                host: host.clone(),
                cancelled,
            });
        }
    }

    /// Abandons the pending request, interrupting its `ssh -G` if it is running.
    fn cancel(&mut self) {
        if let Some(in_flight) = self.in_flight.take() {
            in_flight.cancelled.store(true, Ordering::Relaxed);
        }
    }

    /// Returns the result of the current request, if it has arrived.
    fn poll(&mut self) -> Option<(HostSummary, Loaded)> {
        let mut current = None;
        while let Ok(resolved) = self.results.try_recv() {
            if self.in_flight.is_some() && resolved.generation == self.generation {
                self.in_flight = None;
                current = Some((resolved.host, resolved.details));
            }
        }
        current
    }
}

impl Drop for Resolver {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn resolve_worker(
    app: &App,
    requests: &mpsc::Receiver<ResolveRequest>,
    results: &mpsc::Sender<Resolved>,
) {
    while let Ok(mut request) = requests.recv() {
        // Navigation requests a host per keypress; only the last one of a burst is worth an `ssh -G`.
        loop {
            match requests.recv_timeout(RESOLVE_DEBOUNCE) {
                Ok(newer) => request = newer,
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        if request.cancelled.load(Ordering::Relaxed) {
            continue;
        }
        let details = load_details(app, &request.host, &request.cancelled);
        if request.cancelled.load(Ordering::Relaxed) {
            continue;
        }
        let resolved = Resolved {
            generation: request.generation,
            host: request.host,
            details,
        };
        if results.send(resolved).is_err() {
            return;
        }
    }
}

/// The signals that end the TUI.
///
/// SIGTERM and SIGHUP always do. SIGINT does too, except while an in-place `ssh` owns the
/// terminal: with raw mode off, the terminal's Ctrl+C reaches the whole foreground process
/// group, so `ssh` and Portico both receive it, and it is `ssh`'s to act on.
struct Signals {
    terminated: Arc<AtomicBool>,
    child_in_foreground: Arc<AtomicBool>,
}

impl Signals {
    fn install() -> io::Result<Self> {
        let signals = Self {
            terminated: Arc::new(AtomicBool::new(false)),
            child_in_foreground: Arc::new(AtomicBool::new(false)),
        };
        for signal in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGHUP] {
            signal_hook::flag::register(signal, Arc::clone(&signals.terminated))?;
        }
        let terminated = Arc::clone(&signals.terminated);
        let child_in_foreground = Arc::clone(&signals.child_in_foreground);
        // SAFETY: the handler only performs atomic loads and stores, which are async-signal-safe.
        unsafe {
            signal_hook::low_level::register(signal_hook::consts::SIGINT, move || {
                if !child_in_foreground.load(Ordering::Relaxed) {
                    terminated.store(true, Ordering::Relaxed);
                }
            })?;
        }
        Ok(signals)
    }

    /// Set once a signal asks the TUI to end.
    fn flag(&self) -> &AtomicBool {
        &self.terminated
    }

    /// The same flag, for whatever else may end the TUI.
    fn shared_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.terminated)
    }

    /// Marks an in-place child as owning the terminal until the returned scope is dropped.
    fn foreground_child(&self) -> ForegroundChild<'_> {
        self.child_in_foreground.store(true, Ordering::Relaxed);
        ForegroundChild(&self.child_in_foreground)
    }
}

struct ForegroundChild<'a>(&'a AtomicBool);

impl Drop for ForegroundChild<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

/// The terminal operations a session performs, separated so the session's bookkeeping can be
/// tested without a terminal.
trait TerminalControl: Send + Sync {
    fn enable_raw_mode(&self) -> io::Result<()>;
    fn disable_raw_mode(&self) -> io::Result<()>;
    fn enter_screen(&self) -> io::Result<()>;
    fn leave_screen(&self) -> io::Result<()>;
    fn push_keyboard_flags(&self) -> io::Result<()>;
    fn pop_keyboard_flags(&self) -> io::Result<()>;
}

struct CrosstermControl;

impl TerminalControl for CrosstermControl {
    fn enable_raw_mode(&self) -> io::Result<()> {
        enable_raw_mode()
    }

    fn disable_raw_mode(&self) -> io::Result<()> {
        disable_raw_mode()
    }

    fn enter_screen(&self) -> io::Result<()> {
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste,
            Hide
        )
    }

    fn leave_screen(&self) -> io::Result<()> {
        let screen = execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen);
        let cursor = execute!(io::stdout(), Show);
        screen.and(cursor)
    }

    fn push_keyboard_flags(&self) -> io::Result<()> {
        execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )
    }

    fn pop_keyboard_flags(&self) -> io::Result<()> {
        execute!(io::stdout(), PopKeyboardEnhancementFlags)
    }
}

/// The terminal modes Portico has switched on, so each is switched off exactly once.
///
/// `enter` and `leave` are idempotent and each undoes only what is recorded as done. That keeps
/// an error halfway through `enter`, a `leave` before the child runs and the final restore from
/// ever popping keyboard flags that were not pushed (the kitty protocol keeps a stack).
struct TerminalSession {
    control: Box<dyn TerminalControl>,
    /// Whether the terminal reports modifiers on keys such as Enter (the kitty keyboard protocol).
    keyboard_enhancement: bool,
    raw: AtomicBool,
    screen: AtomicBool,
    keyboard_flags: AtomicBool,
}

impl TerminalSession {
    fn detect() -> Self {
        // A terminal that cannot answer the query simply keeps its legacy key reporting.
        let keyboard_enhancement = supports_keyboard_enhancement().unwrap_or(false);
        Self::new(Box::new(CrosstermControl), keyboard_enhancement)
    }

    fn new(control: Box<dyn TerminalControl>, keyboard_enhancement: bool) -> Self {
        Self {
            control,
            keyboard_enhancement,
            raw: AtomicBool::new(false),
            screen: AtomicBool::new(false),
            keyboard_flags: AtomicBool::new(false),
        }
    }

    fn keyboard_enhancement(&self) -> bool {
        self.keyboard_enhancement
    }

    /// Switches on every mode not yet on; a failure leaves the terminal restored.
    fn enter(&self) -> Result<(), TuiError> {
        self.enter_modes().map_err(|error| {
            let _ = self.leave();
            error.into()
        })
    }

    fn enter_modes(&self) -> io::Result<()> {
        if !self.raw.load(Ordering::Relaxed) {
            self.control.enable_raw_mode()?;
            self.raw.store(true, Ordering::Relaxed);
        }
        if !self.screen.load(Ordering::Relaxed) {
            // Recorded first: a write that failed partway may still have switched modes on.
            self.screen.store(true, Ordering::Relaxed);
            self.control.enter_screen()?;
        }
        // The kitty keyboard protocol keeps separate flags per screen, so push them after switching.
        if self.keyboard_enhancement && !self.keyboard_flags.load(Ordering::Relaxed) {
            self.control.push_keyboard_flags()?;
            self.keyboard_flags.store(true, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Switches off every mode that is on, reporting the first failure after trying them all.
    fn leave(&self) -> Result<(), TuiError> {
        let keys = if self.keyboard_flags.swap(false, Ordering::Relaxed) {
            self.control.pop_keyboard_flags()
        } else {
            Ok(())
        };
        let raw = if self.raw.swap(false, Ordering::Relaxed) {
            self.control.disable_raw_mode()
        } else {
            Ok(())
        };
        let screen = if self.screen.swap(false, Ordering::Relaxed) {
            self.control.leave_screen()
        } else {
            Ok(())
        };
        keys.and(raw).and(screen)?;
        Ok(())
    }
}

/// Restores the terminal when dropped, including while unwinding from a panic.
struct TerminalGuard(Arc<TerminalSession>);

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.0.leave();
    }
}

type PanicHandler = dyn Fn(&panic::PanicHookInfo<'_>) + Send + Sync + 'static;

/// Runs `restore` before the previous panic hook reports, so the message lands on the normal
/// screen instead of vanishing with the alternate one. Dropping it removes the extra step.
struct PanicHook {
    previous: Arc<PanicHandler>,
}

impl PanicHook {
    fn install(restore: impl Fn() + Send + Sync + 'static) -> Self {
        let previous: Arc<PanicHandler> = Arc::from(panic::take_hook());
        let delegate = Arc::clone(&previous);
        panic::set_hook(Box::new(move |info| {
            restore();
            delegate(info);
        }));
        Self { previous }
    }
}

impl Drop for PanicHook {
    fn drop(&mut self) {
        // The hook cannot be replaced while unwinding; the process is on its way out then.
        if !thread::panicking() {
            let previous = Arc::clone(&self.previous);
            panic::set_hook(Box::new(move |info| previous(info)));
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Mode {
    Browse,
    Filter,
    AddForm,
    EditForm,
    ConfirmRemove,
    ConfirmTrust,
    Migrate,
}

/// The tabs, in the order the tab bar shows them and Tab cycles them.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Tab {
    #[default]
    Hosts,
    Tunnels,
    Containers,
}

impl Tab {
    const ALL: [Self; 3] = [Self::Hosts, Self::Tunnels, Self::Containers];

    fn label(self) -> &'static str {
        match self {
            Self::Hosts => "Hosts",
            Self::Tunnels => "Tunnels",
            Self::Containers => "Containers",
        }
    }

    fn number(self) -> usize {
        Self::ALL.iter().position(|&tab| tab == self).unwrap_or(0) + 1
    }

    /// The tab `by` places after this one, wrapping around.
    fn cycled(self, by: isize) -> Self {
        let count = Self::ALL.len() as isize;
        Self::ALL[(self.number() as isize - 1 + by).rem_euclid(count) as usize]
    }
}

/// A single-line text field edited at a cursor.
///
/// The cursor is a byte offset that always sits on a character boundary. An input bound to a
/// host field applies an edit only when the result is something that field accepts, so it never
/// holds text the host model would reject for its characters.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct TextInput {
    text: String,
    cursor: usize,
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

    fn for_field(field: HostField, text: &str) -> Self {
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

/// Resolved details of hosts already looked up, so revisiting a host costs nothing.
///
/// Entries stay valid until the inventory changes. Lookup is linear: it is bounded by the
/// number of hosts the user scrolls through.
#[derive(Clone, Debug, Default)]
struct DetailsCache {
    entries: Vec<(HostSummary, Loaded)>,
}

impl DetailsCache {
    fn get(&self, host: &HostSummary) -> Option<&Loaded> {
        self.entries
            .iter()
            .find(|(cached, _)| cached == host)
            .map(|(_, loaded)| loaded)
    }

    fn insert(&mut self, host: HostSummary, loaded: Loaded) {
        match self.entries.iter_mut().find(|(cached, _)| *cached == host) {
            Some(entry) => entry.1 = loaded,
            None => self.entries.push((host, loaded)),
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
    }
}

#[derive(Debug)]
struct UiState {
    hosts: Vec<HostSummary>,
    warnings: Vec<DiscoveryWarning>,
    visible: Vec<usize>,
    selected: Option<usize>,
    filter: TextInput,
    /// The filter as it was when filter mode began, which Esc restores.
    filter_before_edit: String,
    mode: Mode,
    status: Option<String>,
    form: FormState,
    details: DetailsCache,
    /// Set when `e` was pressed before the selected host's details had loaded.
    edit_pending: bool,
    /// Whether the terminal reports Ctrl+Enter, so the footer may advertise it.
    ctrl_enter: bool,
    /// The alias under test while a connection test runs.
    testing: Option<String>,
    /// Connection test results of saved hosts, kept for the session.
    results: TestResults,
    /// The explanation of each saved host's last failed connection, kept for the session.
    connections: PerHost<Explanation>,
    /// Whether the selected host's (or the form's) ssh output is unfolded.
    show_raw: bool,
    /// The host key awaiting the user's decision while in `Mode::ConfirmTrust`.
    trust: Option<PendingTrust>,
    containers: Panel,
    /// The tab shown and receiving keys.
    tab: Tab,
    /// The migrate screen while in `Mode::Migrate`.
    migrate: Option<MigrateState>,
    /// Every identity Portico knows of, for the IdentityFile suggestions; built once a form
    /// opens and dropped on refresh.
    identity_inventory: Option<IdentityInventory>,
    /// The home whose `~/.ssh` the IdentityFile suggestions scan and whose paths show as `~`;
    /// empty (nothing scanned) until the session sets it from the app.
    home: PathBuf,
    /// The Tunnels tab.
    tunnels: TunnelsScreen,
}

/// The outcome of a host's last connection test.
#[derive(Clone, Debug, Eq, PartialEq)]
struct TestMark {
    passed: bool,
    line: String,
    finished_at: String,
    explanation: Option<Explanation>,
}

type TestResults = PerHost<TestMark>;

/// Something remembered per host, such as test results. Like `DetailsCache`, lookup is linear.
#[derive(Clone, Debug)]
struct PerHost<T> {
    entries: Vec<(HostSummary, T)>,
}

impl<T> Default for PerHost<T> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
}

impl<T> PerHost<T> {
    fn get(&self, host: &HostSummary) -> Option<&T> {
        self.entries
            .iter()
            .find(|(tested, _)| tested == host)
            .map(|(_, mark)| mark)
    }

    fn insert(&mut self, host: HostSummary, mark: T) {
        self.forget_host(&host);
        self.entries.push((host, mark));
    }

    fn forget_host(&mut self, host: &HostSummary) {
        self.entries.retain(|(tested, _)| tested != host);
    }

    /// Drops every result for `alias`, whichever file defines it.
    fn forget(&mut self, alias: &str) {
        self.entries
            .retain(|(tested, _)| !tested.alias.eq_ignore_ascii_case(alias));
    }

    fn clear(&mut self) {
        self.entries.clear();
    }
}

#[derive(Clone, Debug)]
struct FormState {
    values: [TextInput; 6],
    active: usize,
    original: Option<Host>,
    /// The reason the last save was rejected, shown until the form is edited.
    error: Option<String>,
    /// The last connection test of the typed values, shown until the form is edited.
    test_result: Option<FormTestResult>,
    /// The dropdown under ProxyJump or IdentityFile, while it is open.
    suggestions: Option<Suggestions>,
    /// Ctrl+Space asked for IdentityFile suggestions before there were any: the dropdown opens
    /// when the identity inventory arrives, unless the focus left the field meanwhile.
    identity_pending: bool,
}

/// The index of IdentityFile in `FORM_FIELDS`.
const IDENTITY_FILE: usize = 4;
/// The index of ProxyJump in `FORM_FIELDS`.
const PROXY_JUMP: usize = 5;
/// A form's connection test result, with the explanation shown under it.
#[derive(Clone, Debug, Eq, PartialEq)]
struct FormTestResult {
    line: String,
    explanation: Option<Explanation>,
}

/// A host key the server presented that the user is deciding whether to trust.
#[derive(Clone, Debug)]
struct PendingTrust {
    origin: Origin,
    target: TestTarget,
    key: PresentedKey,
    /// The mode to return to once the prompt is answered.
    previous: Mode,
}

#[derive(Debug, Eq, PartialEq)]
enum UiAction {
    None,
    Quit,
    Tunnels(TunnelAction),
    Connect {
        alias: String,
        new_terminal: bool,
    },
    Add(Host),
    Edit {
        alias: String,
        changes: HostChanges,
    },
    Remove(String),
    Refresh,
    Test {
        origin: Origin,
        target: TestTarget,
    },
    CancelTest,
    /// The open form's values changed, so a test of the old values no longer applies.
    FormEdited,
    /// The answer to the trust prompt; on `true` the key to trust is in `UiState::trust`.
    Trust(bool),
    Containers(PanelAction),
    /// Opens the migrate screen of every host owned like the selected one.
    OpenMigrate {
        direction: adopt::Direction,
        preselect: String,
    },
    /// Moves the checked hosts of the open migrate screen.
    Migrate(Vec<String>),
    /// Stops building the plan of the migrate screen, which closed before it arrived.
    CancelPlan,
}

impl UiState {
    fn new(inventory: HostInventory, ctrl_enter: bool) -> Self {
        let HostInventory { hosts, warnings } = inventory;
        let visible = (0..hosts.len()).collect::<Vec<_>>();
        let selected = visible.first().copied();
        Self {
            hosts,
            warnings,
            visible,
            selected,
            filter: TextInput::default(),
            filter_before_edit: String::new(),
            mode: Mode::Browse,
            status: None,
            form: FormState::default(),
            details: DetailsCache::default(),
            edit_pending: false,
            ctrl_enter,
            testing: None,
            results: TestResults::default(),
            connections: PerHost::default(),
            show_raw: false,
            trust: None,
            containers: Panel::default(),
            tab: Tab::Hosts,
            migrate: None,
            identity_inventory: None,
            home: PathBuf::new(),
            tunnels: TunnelsScreen::default(),
        }
    }

    fn set_filter(&mut self, filter: &str) {
        self.filter = TextInput::new(filter);
        self.refilter();
    }

    /// Recomputes the visible hosts from the filter, keeping the selection when it still matches.
    fn refilter(&mut self) {
        let filter = self.filter.as_str();
        self.visible = self
            .hosts
            .iter()
            .enumerate()
            .filter(|(_, host)| fuzzy::match_fields(filter, &[&host.alias]).is_some())
            .map(|(index, _)| index)
            .collect();
        if !self
            .selected
            .is_some_and(|selected| self.visible.contains(&selected))
        {
            self.selected = self.visible.first().copied();
        }
    }

    fn select_alias(&mut self, alias: &str) {
        if let Some(index) = self
            .hosts
            .iter()
            .position(|host| host.alias.eq_ignore_ascii_case(alias))
            .filter(|index| self.visible.contains(index))
        {
            self.selected = Some(index);
        }
    }

    fn selected_alias(&self) -> Option<&str> {
        self.selected.map(|index| self.hosts[index].alias.as_str())
    }

    fn selected_summary(&self) -> Option<&HostSummary> {
        self.selected.map(|index| &self.hosts[index])
    }

    fn selected_details(&self) -> Option<&HostDetails> {
        self.details.get(self.selected_summary()?)?.as_ref().ok()
    }

    fn replace_hosts(&mut self, inventory: HostInventory) {
        let selected = self.selected_summary().cloned();
        self.show_raw = false;
        self.hosts = inventory.hosts;
        self.warnings = inventory.warnings;
        self.selected = None;
        self.refilter();
        // The same row if it survived, otherwise the host of that name wherever it is now defined.
        if let Some(selected) = selected
            && let Some(index) = self
                .hosts
                .iter()
                .position(|host| host == &selected)
                .or_else(|| {
                    self.hosts
                        .iter()
                        .position(|host| host.alias.eq_ignore_ascii_case(&selected.alias))
                })
                .filter(|index| self.visible.contains(index))
        {
            self.selected = Some(index);
        }
        if self.form.suggestions.is_some() {
            self.suggest(false);
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> UiAction {
        if let Some(action) = self.handle_tab_key(key) {
            return action;
        }
        match self.tab {
            Tab::Hosts => self.handle_hosts_key(key),
            Tab::Tunnels => match self.tunnels.handle_key(key) {
                TunnelAction::Quit => UiAction::Quit,
                action => UiAction::Tunnels(action),
            },
            Tab::Containers => match self.containers.handle_key(key) {
                PanelAction::Quit => UiAction::Quit,
                PanelAction::ChooseTarget => {
                    let hosts = self
                        .hosts
                        .iter()
                        .map(|host| {
                            let origin = match host.ownership {
                                Ownership::Managed => suggest::Origin::Managed,
                                Ownership::External => {
                                    suggest::Origin::External(host.source.clone())
                                }
                            };
                            (
                                host.alias.clone(),
                                suggest::origin_label(&origin, Some(&self.home)),
                            )
                        })
                        .collect();
                    self.containers.choose_target(hosts);
                    UiAction::None
                }
                action => UiAction::Containers(action),
            },
        }
    }

    /// Whether the active tab is browsing, so the keys shared by every tab apply: no text input,
    /// form, confirmation or checklist has them.
    fn browsing(&self) -> bool {
        match self.tab {
            Tab::Hosts => self.mode == Mode::Browse,
            Tab::Tunnels => self.tunnels.is_browsing(),
            Tab::Containers => self.containers.is_browsing(),
        }
    }

    /// The keys shared by every browsing tab: `1` `2` `3`, Tab and Shift+Tab switch tabs, `T`
    /// shows the tunnels and `c` the containers, of the selected host when on the hosts.
    fn handle_tab_key(&mut self, key: KeyEvent) -> Option<UiAction> {
        if !self.browsing()
            || key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        let tab = match key.code {
            KeyCode::Char('1') => Tab::Hosts,
            KeyCode::Char('2') => Tab::Tunnels,
            KeyCode::Char('3') => Tab::Containers,
            KeyCode::Tab => self.tab.cycled(1),
            KeyCode::BackTab => self.tab.cycled(-1),
            // Terminals report Shift+T as `T`, with or without the Shift modifier.
            KeyCode::Char('T') => Tab::Tunnels,
            KeyCode::Char('c') => {
                if self.tab == Tab::Hosts
                    && let Some(alias) = self.selected_alias()
                {
                    let target = Target::Host(alias.to_owned());
                    self.tab = Tab::Containers;
                    return Some(UiAction::Containers(self.containers.show(target)));
                }
                Tab::Containers
            }
            _ => return None,
        };
        let entered = tab != self.tab;
        self.tab = tab;
        // A target is listed once its tab is shown, never before.
        if entered && tab == Tab::Containers {
            let target = self.containers.target();
            return Some(UiAction::Containers(self.containers.show(target)));
        }
        Some(UiAction::None)
    }

    fn handle_hosts_key(&mut self, key: KeyEvent) -> UiAction {
        self.status = None;
        self.edit_pending = false;
        if key.code == KeyCode::Esc && self.testing.is_some() {
            self.status = Some("Test cancelled".into());
            return UiAction::CancelTest;
        }
        let (selected, in_form) = (self.selected, self.in_form());
        let action = match self.mode {
            Mode::Browse => self.handle_browse_key(key),
            Mode::Filter => self.handle_filter_key(key),
            Mode::AddForm | Mode::EditForm => self.handle_form_key(key),
            Mode::ConfirmRemove => self.handle_remove_key(key),
            Mode::ConfirmTrust => self.handle_trust_key(key),
            Mode::Migrate => self.handle_migrate_key(key),
        };
        // Unfolded output belongs to one selection, or to one open form.
        if (selected, in_form) != (self.selected, self.in_form()) {
            self.show_raw = false;
        }
        action
    }

    fn handle_paste(&mut self, text: &str) -> UiAction {
        match self.tab {
            Tab::Hosts => {}
            Tab::Tunnels => {
                self.tunnels.paste(text);
                return UiAction::None;
            }
            Tab::Containers => {
                self.containers.paste(text);
                return UiAction::None;
            }
        }
        self.status = None;
        match self.mode {
            Mode::Filter => {
                self.filter.paste(text);
                self.refilter();
            }
            Mode::AddForm | Mode::EditForm => {
                let input = &mut self.form.values[self.form.active];
                let (before, cursor) = (input.as_str().to_owned(), input.cursor);
                input.paste(text);
                return self.form_input_changed(&before, cursor);
            }
            Mode::Migrate => {
                if let Some(migrate) = &mut self.migrate {
                    migrate.paste(text);
                }
            }
            Mode::Browse | Mode::ConfirmRemove | Mode::ConfirmTrust => {}
        }
        UiAction::None
    }

    fn handle_browse_key(&mut self, key: KeyEvent) -> UiAction {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Char('/') => {
                self.filter_before_edit = self.filter.as_str().into();
                self.filter.cursor = self.filter.as_str().len();
                self.mode = Mode::Filter;
            }
            KeyCode::Esc => self.set_filter(""),
            KeyCode::Char('a') => {
                self.form = FormState::default();
                self.mode = Mode::AddForm;
            }
            KeyCode::Char('e') => self.begin_edit(),
            KeyCode::Char('m') => {
                if let Some(host) = self.selected_summary() {
                    return UiAction::OpenMigrate {
                        direction: match host.ownership {
                            Ownership::External => adopt::Direction::Adopt,
                            Ownership::Managed => adopt::Direction::Export,
                        },
                        preselect: host.alias.clone(),
                    };
                }
            }
            KeyCode::Char('x') => {
                if self.selected_is_managed() {
                    self.mode = Mode::ConfirmRemove;
                }
            }
            KeyCode::Enter if key.modifiers.is_empty() => return self.connect_action(false),
            KeyCode::Enter if key.modifiers == KeyModifiers::CONTROL => {
                return self.connect_action(true);
            }
            KeyCode::Char('n') => return self.connect_action(true),
            KeyCode::Char('r') | KeyCode::F(5) => return UiAction::Refresh,
            KeyCode::Char('t') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                return self.test_selected();
            }
            KeyCode::Char('o') if key.modifiers.is_empty() && self.selected_has_foldable_raw() => {
                self.show_raw = !self.show_raw;
            }
            KeyCode::Char('q') => return UiAction::Quit,
            _ => {}
        }
        UiAction::None
    }

    /// Opens the edit form for the selected managed host once its declaration is known.
    fn begin_edit(&mut self) {
        if !self.selected_is_managed() {
            return;
        }
        match self
            .selected_summary()
            .and_then(|host| self.details.get(host))
        {
            // The host is still being looked up: finish the request when the result arrives.
            None => self.edit_pending = true,
            Some(Ok(HostDetails {
                declared: Some(host),
                ..
            })) => {
                self.form = FormState::from_host(host);
                self.mode = Mode::EditForm;
            }
            Some(_) => {}
        }
    }

    fn open_pending_edit(&mut self) {
        if std::mem::take(&mut self.edit_pending) {
            self.begin_edit();
        }
    }

    fn handle_filter_key(&mut self, key: KeyEvent) -> UiAction {
        match key.code {
            KeyCode::Esc => {
                let before = std::mem::take(&mut self.filter_before_edit);
                self.set_filter(&before);
                self.mode = Mode::Browse;
            }
            KeyCode::Enter if key.modifiers.is_empty() => self.mode = Mode::Browse,
            _ => {
                if self.filter.handle_key(key) {
                    self.refilter();
                }
            }
        }
        UiAction::None
    }

    fn handle_form_key(&mut self, key: KeyEvent) -> UiAction {
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
    fn form_input_changed(&mut self, before: &str, cursor: usize) -> UiAction {
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
    fn suggest(&mut self, on_demand: bool) {
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

    fn test_selected(&self) -> UiAction {
        self.selected_summary()
            .map_or(UiAction::None, |host| UiAction::Test {
                origin: Origin::Host(host.clone()),
                target: TestTarget::Saved(host.alias.clone()),
            })
    }

    fn in_form(&self) -> bool {
        matches!(self.mode, Mode::AddForm | Mode::EditForm)
    }

    /// Whether the selected host has an explanation whose ssh output `o` can unfold.
    fn selected_has_foldable_raw(&self) -> bool {
        self.selected_summary().is_some_and(|host| {
            self.results
                .get(host)
                .and_then(|mark| mark.explanation.as_ref())
                .into_iter()
                .chain(self.connections.get(host))
                .any(has_foldable_raw)
        })
    }

    fn form_has_foldable_raw(&self) -> bool {
        self.form
            .test_result
            .as_ref()
            .and_then(|result| result.explanation.as_ref())
            .is_some_and(has_foldable_raw)
    }

    /// Shows how a connection attempt ended, and remembers a failure for the details pane.
    fn finish_connect(&mut self, alias: &str, result: Result<ConnectionOutcome, TuiError>) {
        self.status = Some(match result {
            Ok(ConnectionOutcome::LaunchRequested) => format!("Launch requested for {alias}"),
            Ok(ConnectionOutcome::LaunchFailed(status)) => {
                format!("terminal launcher exited with {status}")
            }
            Ok(ConnectionOutcome::Exited {
                alias: spelled,
                status,
                explanation,
            }) => {
                let host = self
                    .selected_summary()
                    .filter(|host| host.alias.eq_ignore_ascii_case(&spelled))
                    .or_else(|| {
                        self.hosts
                            .iter()
                            .find(|host| host.alias.eq_ignore_ascii_case(&spelled))
                    })
                    .cloned();
                match explanation {
                    Some(explanation) => {
                        let line = format!("{alias}: failed \u{2014} {}", explanation.headline);
                        if let Some(host) = host {
                            self.connections.insert(host, explanation);
                        }
                        line
                    }
                    None => {
                        // A connection that got through supersedes an earlier failure.
                        if let Some(host) = host {
                            self.connections.forget_host(&host);
                        }
                        if status.success() {
                            format!("Disconnected from {alias}")
                        } else {
                            format!("ssh exited with {status}")
                        }
                    }
                }
            }
            Err(error) => error.to_string(),
        });
    }

    /// Shows a finished test: a saved host's result marks it in the list and the details pane
    /// and goes to the status line; a form's result goes to the form, if it is still open.
    fn record_test(&mut self, finished: Finished, finished_at: String) {
        let Finished {
            origin,
            target,
            outcome,
        } = finished;
        let (passed, line, explanation, status_hint) = match &outcome {
            Ok(TestOutcome::UnknownHostKey { key, .. }) if self.can_prompt(&origin) => {
                self.trust = Some(PendingTrust {
                    origin,
                    target,
                    key: key.clone(),
                    previous: self.mode.clone(),
                });
                self.mode = Mode::ConfirmTrust;
                return;
            }
            // The user has moved on; a prompt now would take the keys they are typing.
            Ok(TestOutcome::UnknownHostKey { report, .. }) => (
                false,
                report.summary(),
                report.explanation.clone(),
                Some("press t to review the host key"),
            ),
            Ok(TestOutcome::Done(report)) => (
                report.passed(),
                report.summary(),
                report.explanation.clone(),
                None,
            ),
            Err(error) => (false, format!("failed \u{2014} {error}"), None, None),
        };
        let reason = match &explanation {
            Some(explanation) => format!("failed \u{2014} {}", explanation.headline),
            None => line.clone(),
        };
        let status = match (&origin, status_hint) {
            (Origin::Host(_), Some(hint)) => format!("{}: {reason} ({hint})", target.alias()),
            (Origin::Host(_), None) => format!("{}: {reason}", target.alias()),
            (Origin::Form, _) => String::new(),
        };
        self.show_result(origin, passed, line, explanation, finished_at);
        if !status.is_empty() {
            self.status = Some(status);
        }
    }

    /// Whether the user is still where a test from `origin` started, so its trust prompt may
    /// open: the host list for a saved host, the same open form for a form. Leaving a form
    /// cancels its test, so a form result that arrives finds its own form.
    fn can_prompt(&self, origin: &Origin) -> bool {
        match origin {
            Origin::Host(_) => self.tab == Tab::Hosts && self.mode == Mode::Browse,
            Origin::Form => self.in_form(),
        }
    }

    /// Records a result: a saved host's marks it in the list and the details pane; a form's goes
    /// to the form with its `explanation`, if the form is still open.
    fn show_result(
        &mut self,
        origin: Origin,
        passed: bool,
        line: String,
        explanation: Option<Explanation>,
        finished_at: String,
    ) {
        match origin {
            Origin::Host(host) => {
                // Output unfolded for the old result is not a request to see the new one's.
                if self.selected_summary() == Some(&host) {
                    self.show_raw = false;
                }
                self.results.insert(
                    host,
                    TestMark {
                        passed,
                        line,
                        finished_at,
                        explanation,
                    },
                );
            }
            Origin::Form => {
                if self.in_form() {
                    self.show_raw = false;
                    self.form.test_result = Some(FormTestResult { line, explanation });
                }
            }
        }
    }

    fn handle_trust_key(&mut self, key: KeyEvent) -> UiAction {
        let trusted = match key.code {
            KeyCode::Char('y') => true,
            KeyCode::Char('n') | KeyCode::Esc => false,
            _ => return UiAction::None,
        };
        let Some(pending) = &self.trust else {
            self.mode = Mode::Browse;
            return UiAction::None;
        };
        self.mode = pending.previous.clone();
        if !trusted && let Some(pending) = self.trust.take() {
            self.show_result(
                pending.origin,
                false,
                "failed \u{2014} host key not trusted".into(),
                None,
                local_time(),
            );
            self.status = Some("Host key not trusted".into());
        }
        UiAction::Trust(trusted)
    }

    /// The status line: the last message, otherwise the test in flight.
    fn status_line(&self) -> Option<String> {
        self.status.clone().or_else(|| {
            self.testing
                .as_ref()
                .map(|alias| format!("Testing {alias}\u{2026} (Esc to cancel)"))
        })
    }

    /// Shows an error on the form field it is about, and a plain status otherwise.
    fn report_error(&mut self, error: &AppError) {
        match error {
            AppError::InvalidHost(host_error)
                if matches!(self.mode, Mode::AddForm | Mode::EditForm) =>
            {
                self.report_form_error(host_error);
            }
            AppError::Transaction(TransactionError::Validation(
                rejection @ ConfigRejection::Managed {
                    field: Some(field), ..
                },
            )) if matches!(self.mode, Mode::AddForm | Mode::EditForm) => {
                if let Some(index) = FORM_FIELDS.iter().position(|row| row.field == *field) {
                    self.form.active = index;
                }
                self.form.error = Some(rejection.to_string());
            }
            AppError::AliasCollision(_) if matches!(self.mode, Mode::AddForm | Mode::EditForm) => {
                self.form.active = 0;
                self.form.error = Some(error.to_string());
            }
            _ => self.status = Some(error.to_string()),
        }
    }

    fn report_form_error(&mut self, error: &HostError) {
        if let HostError::Invalid { field, .. } = error
            && let Some(index) = FORM_FIELDS.iter().position(|row| row.field == *field)
        {
            self.form.active = index;
        }
        self.form.error = Some(error.to_string());
    }

    fn handle_remove_key(&mut self, key: KeyEvent) -> UiAction {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Browse;
                UiAction::None
            }
            KeyCode::Enter if key.modifiers.is_empty() => self
                .selected_alias()
                .map_or(UiAction::None, |alias| UiAction::Remove(alias.into())),
            _ => UiAction::None,
        }
    }

    /// Keeps the built identity inventory and, while IdentityFile has the focus, recomputes its
    /// dropdown: what was typed, or an earlier Ctrl+Space, can now find candidates. A failed
    /// build leaves the suggestions to the key files in `~/.ssh`.
    fn finish_identities(&mut self, built: Result<IdentityInventory, String>) {
        match built {
            Ok(inventory) => self.identity_inventory = Some(inventory),
            Err(error) => self.status = Some(format!("Could not list identities: {error}")),
        }
        if self.in_form() && self.form.active == IDENTITY_FILE {
            let pending = std::mem::take(&mut self.form.identity_pending);
            self.suggest(pending);
        }
    }

    /// Opens the migrate screen on the plan the worker built, unless it was closed meanwhile.
    fn finish_plan(
        &mut self,
        planned: Result<adopt::MigrationPlan, AppError>,
        preselect: &str,
        home: &std::path::Path,
    ) {
        if self.mode != Mode::Migrate || self.migrate.is_some() {
            return;
        }
        match planned {
            Ok(plan) => {
                self.migrate = Some(MigrateState::new(plan, Some(preselect)).with_home(home));
            }
            Err(error) => {
                self.mode = Mode::Browse;
                self.report_error(&error);
            }
        }
    }

    fn handle_migrate_key(&mut self, key: KeyEvent) -> UiAction {
        let Some(migrate) = &mut self.migrate else {
            // The plan is still being built; only Esc does anything.
            if key.code != KeyCode::Esc {
                return UiAction::None;
            }
            self.mode = Mode::Browse;
            return UiAction::CancelPlan;
        };
        match migrate.handle_key(key) {
            MigrateAction::None => {}
            MigrateAction::Close => {
                self.mode = Mode::Browse;
                self.migrate = None;
            }
            MigrateAction::Apply(aliases) => return UiAction::Migrate(aliases),
            MigrateAction::Error(message) => self.status = Some(message),
        }
        UiAction::None
    }

    fn move_selection(&mut self, offset: isize) {
        let Some(selected) = self.selected else {
            return;
        };
        let Some(position) = self.visible.iter().position(|&index| index == selected) else {
            return;
        };
        let next = position
            .saturating_add_signed(offset)
            .min(self.visible.len().saturating_sub(1));
        self.selected = self.visible.get(next).copied();
    }

    fn selected_is_managed(&self) -> bool {
        self.selected
            .is_some_and(|index| self.hosts[index].ownership == Ownership::Managed)
    }

    fn connect_action(&self, new_terminal: bool) -> UiAction {
        self.selected_alias()
            .map_or(UiAction::None, |alias| UiAction::Connect {
                alias: alias.into(),
                new_terminal,
            })
    }
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
    fn from_host(host: &Host) -> Self {
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

/// Re-labels details looked up by alias as the selected row: an external row shares its alias
/// with a managed host it shadows, but must neither show nor edit that host's declaration.
fn details_for_selected(selected: &HostSummary, mut details: HostDetails) -> HostDetails {
    details.summary = selected.clone();
    if selected.ownership == Ownership::External {
        details.declared = None;
    }
    details
}

fn render(frame: &mut Frame<'_>, state: &UiState) {
    let [bar, body] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .areas(frame.area());
    frame.render_widget(Paragraph::new(tab_bar(state.tab, bar.width)), bar);
    match state.tab {
        Tab::Hosts => render_hosts(frame, body, state),
        Tab::Tunnels => {
            let content = tab_areas(
                body,
                state.tunnels.status_line(),
                &state.tunnels.footer(),
                frame,
            );
            state.tunnels.render(frame, content);
        }
        Tab::Containers => render_containers(frame, body, state),
    }
}

/// The tab bar: each tab's number and name, the active one highlighted. When the names do not
/// fit, the other tabs show only their numbers, and when even that does not fit only the active
/// tab is shown, so the bar never wraps or hides the active tab.
fn tab_bar(active: Tab, width: u16) -> Line<'static> {
    let separator = " \u{2502} ";
    let full = |tab: Tab| format!("{} {}", tab.number(), tab.label());
    let labels = |short: bool| {
        Tab::ALL
            .iter()
            .map(|&tab| {
                if short && tab != active {
                    tab.number().to_string()
                } else {
                    full(tab)
                }
            })
            .collect::<Vec<_>>()
    };
    let fits = |labels: &[String]| {
        1 + labels
            .iter()
            .map(|label| display_width(label))
            .sum::<usize>()
            + separator.chars().count() * (labels.len() - 1)
            <= usize::from(width)
    };
    let highlight = Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD);
    let mut spans = vec![Span::raw(" ")];
    let (full_labels, short_labels) = (labels(false), labels(true));
    let shown = if fits(&full_labels) {
        Some(full_labels)
    } else if fits(&short_labels) {
        Some(short_labels)
    } else {
        None
    };
    match shown {
        Some(shown) => {
            for (index, (tab, label)) in Tab::ALL.iter().zip(shown).enumerate() {
                if index > 0 {
                    spans.push(Span::raw(separator));
                }
                if *tab == active {
                    spans.push(Span::styled(label, highlight));
                } else {
                    spans.push(Span::raw(label));
                }
            }
        }
        None => spans.push(Span::styled(full(active), highlight)),
    }
    Line::from(spans)
}

/// Draws a tab's status line and footer at the bottom of `area`, and returns the rest.
fn tab_areas(area: Rect, status: Option<String>, footer: &str, frame: &mut Frame<'_>) -> Rect {
    let footer_height = (footer.chars().count() as u16)
        .div_ceil(area.width.max(1))
        .max(1);
    let [content, status_area, footer_area] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(u16::from(status.is_some())),
            Constraint::Length(footer_height),
        ])
        .areas(area);
    frame.render_widget(Paragraph::new(status.unwrap_or_default()), status_area);
    frame.render_widget(
        Paragraph::new(footer.to_owned()).wrap(Wrap { trim: true }),
        footer_area,
    );
    content
}

/// The Containers tab: the target line, the list with the user line, and the selected
/// container's details, with the target picker over them while it is open.
fn render_containers(frame: &mut Frame<'_>, area: Rect, state: &UiState) {
    let panel = &state.containers;
    let content = tab_areas(
        area,
        panel.status().map(str::to_owned),
        &panel.footer(),
        frame,
    );
    let [target, body] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .areas(content);
    frame.render_widget(Paragraph::new(panel.target_line()), target);
    let [left, right] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .areas(body);
    frame.render_widget(
        Paragraph::new(panel.lines(
            true,
            usize::from(left.height.saturating_sub(2)),
            usize::from(left.width.saturating_sub(2)),
        ))
        .block(Block::default().borders(Borders::ALL).title(panel.title())),
        left,
    );
    let details = panel.details().unwrap_or_default();
    frame.render_widget(
        Paragraph::new(
            details
                .lines()
                .map(|line| Line::raw(line.to_owned()))
                .collect::<Vec<_>>(),
        )
        .wrap(Wrap { trim: false })
        .block(Block::default().borders(Borders::ALL).title("Container")),
        right,
    );
    if panel.picker_open() {
        let height = body.height.min(crate::picker::MAX_HEIGHT + 2);
        let area = Rect { height, ..body };
        let block = Block::default()
            .borders(Borders::ALL)
            .title("Containers on");
        let inner = block.inner(area);
        frame.render_widget(ratatui::widgets::Clear, area);
        frame.render_widget(block, area);
        panel.render_picker(frame, inner);
    }
}

/// The Hosts tab: the hosts on the left, the details, a form or a prompt on the right.
fn render_hosts(frame: &mut Frame<'_>, area: Rect, state: &UiState) {
    let footer = footer_text(state);
    let status = state.status_line();
    let footer_height = (footer.chars().count() as u16)
        .div_ceil(area.width.max(1))
        .max(1);
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(state.warnings.len().min(usize::from(area.height / 3)) as u16),
            Constraint::Length(u16::from(status.is_some())),
            Constraint::Length(footer_height),
        ])
        .split(area);
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(areas[0]);

    let hosts = state
        .visible
        .iter()
        .map(|&index| {
            let host = &state.hosts[index];
            let mark = match state.results.get(host) {
                Some(mark) if mark.passed => "\u{2713} ",
                Some(_) => "\u{2717} ",
                None => "  ",
            };
            let positions = fuzzy::match_fields(state.filter.as_str(), &[&host.alias])
                .and_then(|found| found.positions.into_iter().next())
                .unwrap_or_default();
            let mut spans = vec![Span::raw(mark)];
            spans.extend(fuzzy::highlight(
                &host.alias,
                &positions,
                Style::default(),
                HIT,
            ));
            spans.push(Span::raw(format!("  {}", ownership(host.ownership))));
            ListItem::new(Line::from(spans))
        })
        .collect::<Vec<_>>();
    let mut list_state = ListState::default().with_selected(
        state
            .selected
            .and_then(|selected| state.visible.iter().position(|&index| index == selected)),
    );
    let mut title = vec![Span::raw("Hosts /")];
    title.extend(state.filter.spans(state.mode == Mode::Filter));
    frame.render_stateful_widget(
        List::new(hosts)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(Line::from(title)),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        columns[0],
        &mut list_state,
    );
    let (right_title, detail_lines) = match state.mode {
        Mode::AddForm | Mode::EditForm => (
            Line::from(if state.mode == Mode::AddForm {
                "Add host"
            } else {
                "Edit host"
            }),
            form_lines(
                &state.form,
                state.show_raw,
                &state.home,
                usize::from(columns[1].width.saturating_sub(2)),
                usize::from(columns[1].height.saturating_sub(2)),
            ),
        ),
        Mode::ConfirmRemove => (
            Line::from("Remove host"),
            vec![Line::raw(format!(
                "Remove {}?",
                state.selected_alias().unwrap_or("selected host")
            ))],
        ),
        Mode::ConfirmTrust => (
            Line::from("Trust host key"),
            trust_lines(state.trust.as_ref()),
        ),
        Mode::Migrate => state.migrate.as_ref().map_or(
            (
                Line::from("Migrate"),
                vec![Line::raw("Checking each host with OpenSSH…")],
            ),
            |migrate| {
                (
                    migrate.title(),
                    migrate.lines(
                        usize::from(columns[1].width.saturating_sub(2)),
                        usize::from(columns[1].height.saturating_sub(2)),
                    ),
                )
            },
        ),
        Mode::Browse | Mode::Filter => {
            let text = details_text(state);
            (
                Line::from("Details"),
                text.lines()
                    .map(|line| Line::raw(line.to_owned()))
                    .collect(),
            )
        }
    };
    frame.render_widget(
        Paragraph::new(detail_lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title(right_title)),
        columns[1],
    );

    let warnings = state
        .warnings
        .iter()
        .map(|warning| format!("Warning: {}", format_warning(warning)))
        .collect::<Vec<_>>()
        .join("\n");
    frame.render_widget(Paragraph::new(warnings), areas[1]);
    frame.render_widget(Paragraph::new(status.unwrap_or_default()), areas[2]);
    frame.render_widget(Paragraph::new(footer).wrap(Wrap { trim: true }), areas[3]);
}

fn trust_lines(pending: Option<&PendingTrust>) -> Vec<Line<'static>> {
    let Some(pending) = pending else {
        return Vec::new();
    };
    let alias = pending.target.alias();
    let mut lines = pending
        .key
        .fingerprints
        .iter()
        .map(|fingerprint| Line::raw(fingerprint.describe(alias)))
        .collect::<Vec<_>>();
    lines.push(Line::raw(
        "Compare it with the fingerprint from the server's administrator or console.",
    ));
    lines.push(Line::raw(""));
    lines.push(Line::raw("Trust it and test again? y/N"));
    lines
}

/// The Hosts tab's footer.
fn footer_text(state: &UiState) -> String {
    match state.mode {
        Mode::Browse => {
            let new_terminal = if state.ctrl_enter {
                "Ctrl+Enter/n new terminal"
            } else {
                "n new terminal"
            };
            let refresh = "r refresh";
            let output = if state.selected_has_foldable_raw() {
                "o ssh output  "
            } else {
                ""
            };
            let connect = format!(
                "{output}t test  Enter connect  {new_terminal}  c containers  Tab/1-3 tabs  q quit"
            );
            let clear_filter = if state.filter.is_empty() {
                ""
            } else {
                "  Esc clear filter"
            };
            match state.selected.map(|index| state.hosts[index].ownership) {
                Some(Ownership::Managed)
                    if state
                        .selected_details()
                        .is_some_and(|details| details.declared.is_some()) =>
                {
                    format!(
                        "↑↓ move  / filter{clear_filter}  a add  m export  e edit  x remove  {refresh}  {connect}"
                    )
                }
                Some(Ownership::Managed) => {
                    format!(
                        "↑↓ move  / filter{clear_filter}  a add  m export  x remove  {refresh}  {connect}"
                    )
                }
                Some(Ownership::External) => {
                    format!("↑↓ move  / filter{clear_filter}  a add  m adopt  {refresh}  {connect}")
                }
                None => format!("/ filter{clear_filter}  a add  {refresh}  Tab/1-3 tabs  q quit"),
            }
        }
        Mode::Filter => "type to filter  ←→ move  Enter apply  Esc cancel".into(),
        Mode::AddForm | Mode::EditForm => {
            let output = if state.form_has_foldable_raw() {
                "  Ctrl+O ssh output"
            } else {
                ""
            };
            if state.form.suggestions.is_some() {
                return "↑↓ choose  Enter pick  Esc close".into();
            }
            let suggest = if matches!(state.form.active, PROXY_JUMP | IDENTITY_FILE) {
                "  Ctrl+Space suggest"
            } else {
                ""
            };
            format!("↑↓ field  ←→ move  Enter save  Ctrl+T test{output}{suggest}  Esc cancel")
        }
        Mode::ConfirmRemove => "Enter remove  Esc cancel".into(),
        Mode::ConfirmTrust => "y trust and test again  n/Esc keep it untrusted".into(),
        Mode::Migrate => state
            .migrate
            .as_ref()
            .map_or_else(|| "Esc cancel".into(), MigrateState::footer),
    }
}

/// The form's lines for a pane of `width` columns and `height` rows inside its border; only the
/// jump host dropdown adapts to them. Suggestions from files under `home` show it as `~`.
fn form_lines(
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

/// The terminal columns `text` takes.
pub(crate) fn display_width(text: &str) -> usize {
    Span::raw(text).width()
}

/// `text` cut to at most `columns` columns, keeping its start and marking the cut with `…`.
pub(super) fn keep_start(text: &str, columns: usize) -> String {
    if display_width(text) <= columns {
        return text.to_owned();
    }
    let mut kept = String::new();
    for character in text.chars() {
        if display_width(&kept) + display_width(&character.to_string()) + 1 > columns {
            break;
        }
        kept.push(character);
    }
    kept + "…"
}

/// `text` cut to at most `columns` columns, keeping its end and marking the cut with `…`.
pub(super) fn keep_end(text: &str, columns: usize) -> String {
    if display_width(text) <= columns {
        return text.to_owned();
    }
    let mut kept = Vec::new();
    let mut used = 1;
    for character in text.chars().rev() {
        let width = display_width(&character.to_string());
        if used + width > columns {
            break;
        }
        used += width;
        kept.push(character);
    }
    std::iter::once('…').chain(kept.into_iter().rev()).collect()
}

fn details_text(state: &UiState) -> String {
    let Some(selected) = state.selected_summary() else {
        return "No host selected".into();
    };
    let text = match state.details.get(selected) {
        Some(Ok(details)) => detail_text(details, &state.home),
        Some(Err(error)) => format!("Could not load details: {error}"),
        None => "Resolving…".into(),
    };
    let mut text = text.trim_end().to_owned();
    if let Some(mark) = state.results.get(selected) {
        match &mark.explanation {
            Some(explanation) => {
                text.push_str(&format!(
                    "\n\nConnection test: failed \u{2014} {} at {}",
                    explanation.headline, mark.finished_at
                ));
                for line in explanation_lines(explanation, state.show_raw) {
                    text.push('\n');
                    text.push_str(&line);
                }
            }
            None => text.push_str(&format!(
                "\n\nConnection test: {} at {}",
                mark.line, mark.finished_at
            )),
        }
    }
    if let Some(explanation) = state.connections.get(selected) {
        text.push_str(&format!(
            "\n\nLast connection: failed \u{2014} {}",
            explanation.headline
        ));
        for line in explanation_lines(explanation, state.show_raw) {
            text.push('\n');
            text.push_str(&line);
        }
    }
    text
}

/// The explanation's lines below its first, which the caller words itself.
fn explanation_lines(explanation: &Explanation, show_raw: bool) -> impl Iterator<Item = String> {
    explanation.render("", show_raw).into_iter().skip(1)
}

/// Whether the explanation has ssh output that is folded away unless asked for. Output that
/// `always_raw` shows anyway (the facts alone do not explain the failure) has nothing to unfold.
fn has_foldable_raw(explanation: &Explanation) -> bool {
    !explanation.raw.is_empty() && !explanation.always_raw
}

/// The current local time as `HH:MM`.
fn local_time() -> String {
    // SAFETY: `time` accepts a null pointer, and `localtime_r` only writes the `tm` it is given.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            return "--:--".into();
        }
        format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
    }
}

/// One host field and every value OpenSSH may use for it (IdentityFile can repeat).
struct DetailRow {
    label: &'static str,
    values: Vec<String>,
}

impl DetailRow {
    fn new(label: &'static str, values: impl IntoIterator<Item = String>) -> Self {
        let mut unique: Vec<String> = Vec::new();
        for value in values {
            if !unique.contains(&value) {
                unique.push(value);
            }
        }
        Self {
            label,
            values: unique,
        }
    }
}

fn resolved_rows(resolved: &ResolvedConfig) -> [DetailRow; 5] {
    [
        DetailRow::new("HostName", [resolved.hostname.clone()]),
        DetailRow::new("User", resolved.user.clone()),
        DetailRow::new("Port", resolved.port.map(|port| port.to_string())),
        DetailRow::new("IdentityFile", resolved.identity_files.iter().cloned()),
        DetailRow::new("ProxyJump", resolved.proxy_jump.clone()),
    ]
}

fn declared_rows(host: &Host) -> [DetailRow; 5] {
    [
        DetailRow::new("HostName", [host.hostname().to_owned()]),
        DetailRow::new("User", host.user().map(String::from)),
        DetailRow::new("Port", host.port().map(|port| port.to_string())),
        DetailRow::new("IdentityFile", host.identity_file().map(String::from)),
        DetailRow::new("ProxyJump", host.proxy_jump().map(String::from)),
    ]
}

/// Describes a host with each field shown once, as OpenSSH resolves it. Portico's own
/// declaration only appears where it does not take effect, or while resolution is unavailable.
fn detail_text(details: &HostDetails, home: &Path) -> String {
    let mut text = format!(
        "Ownership: {}\nSource: {}\n\n",
        ownership(details.summary.ownership),
        details.summary.source.display(),
    );
    let declared = details.declared.as_ref().map(declared_rows);
    match &details.resolution {
        Ok(resolved) => {
            let rows = resolved_rows(resolved);
            // The identities section shows the IdentityFile paths with their availability.
            let shown = rows
                .iter()
                .filter(|row| details.identities.is_none() || row.label != "IdentityFile");
            for row in shown {
                push_rows(&mut text, std::slice::from_ref(row));
            }
            if let Some(identities) = &details.identities {
                text.push('\n');
                text.push_str(&identities::detail_lines(identities, home));
            }
            let overridden = declared
                .iter()
                .flatten()
                .zip(&rows)
                .filter(|(declared, effective)| !declared_in_effect(declared, effective, home))
                .map(|(declared, _)| format!("  {}: {}\n", declared.label, declared.values[0]))
                .collect::<String>();
            if !overridden.is_empty() {
                text.push_str(&format!(
                    "\nDeclared by Portico but overridden by OpenSSH:\n{overridden}"
                ));
            }
        }
        Err(error) => {
            if let Some(rows) = declared {
                push_rows(&mut text, &rows);
                text.push('\n');
            }
            text.push_str(&format!("Could not resolve with ssh -G: {error}"));
        }
    }
    text
}

fn push_rows(text: &mut String, rows: &[DetailRow]) {
    for row in rows {
        let mut values = row.values.iter();
        text.push_str(&format!(
            "{:<13}{}\n",
            row.label,
            values.next().map_or("—", String::as_str)
        ));
        for value in values {
            text.push_str(&format!("{:<13}{value}\n", ""));
        }
    }
}

/// Whether the value Portico declared is among those OpenSSH uses. An undeclared field never
/// conflicts, and `ssh -G` reports `~` paths already expanded against `home`.
fn declared_in_effect(declared: &DetailRow, effective: &DetailRow, home: &Path) -> bool {
    declared.values.iter().all(|value| {
        effective
            .values
            .iter()
            .any(|used| used == value || *used == crate::home::expand(value, home))
    })
}

fn format_warning(warning: &DiscoveryWarning) -> String {
    match warning {
        DiscoveryWarning::Include { path, message } => {
            format!("{}: {message}", path.display())
        }
        DiscoveryWarning::Parse {
            path,
            line,
            message,
        } => format!("{}:{line}: {message}", path.display()),
        DiscoveryWarning::DuplicateAlias { alias, sources } => format!(
            "duplicate alias {alias}: {}",
            sources
                .iter()
                .map(|source| source.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
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

fn ownership(ownership: Ownership) -> &'static str {
    match ownership {
        Ownership::Managed => "managed",
        Ownership::External => "external",
    }
}

fn init_decision(key: KeyEvent) -> Option<bool> {
    match key.code {
        KeyCode::Char('y') => Some(true),
        KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('n') => Some(false),
        _ => None,
    }
}

fn suspend_for_child<T, L, C, E>(mut leave: L, child: C, mut enter: E) -> Result<T, TuiError>
where
    L: FnMut() -> Result<(), TuiError>,
    C: FnOnce() -> Result<T, TuiError>,
    E: FnMut() -> Result<(), TuiError>,
{
    leave()?;
    let child_result = child();
    enter()?;
    child_result
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

    use crate::{
        app::TestReport,
        app::{HostDetails, HostSummary, Ownership},
        discovery::DiscoveryWarning,
        host::{Change, Host},
        managed::InitPlan,
        openssh::ResolvedConfig,
        probe::ProbeResult,
    };

    use super::*;
    use crate::app::{ContainerList, Target};

    fn test_state(hosts: Vec<HostSummary>) -> UiState {
        UiState::new(
            HostInventory {
                hosts,
                warnings: Vec::new(),
            },
            false,
        )
    }

    fn summary(alias: &str, ownership: Ownership) -> HostSummary {
        HostSummary {
            alias: alias.into(),
            ownership,
            source: PathBuf::from("/home/test/.ssh/config"),
        }
    }

    fn details(alias: &str, ownership: Ownership) -> HostDetails {
        HostDetails {
            summary: summary(alias, ownership),
            declared: (ownership == Ownership::Managed).then(|| {
                Host::new(
                    alias,
                    format!("{alias}.example"),
                    Some("deploy".into()),
                    Some(22),
                    Some("~/.ssh/id_ed25519".into()),
                    Some("bastion".into()),
                )
                .unwrap()
            }),
            resolution: Ok(ResolvedConfig {
                hostname: format!("{alias}.example"),
                user: Some("deploy".into()),
                port: Some(22),
                identity_files: vec!["~/.ssh/id_ed25519".into()],
                proxy_jump: Some("bastion".into()),
                proxy_command: None,
                proxy_use_fdpass: false,
                known_hosts_files: vec!["~/.ssh/known_hosts".into()],
                identity_agent: None,
                identities_only: false,
                pkcs11_provider: None,
            }),
            identities: None,
            warnings: Vec::new(),
        }
    }

    /// Caches details for the selected host, as the resolver does once `ssh -G` answers.
    fn resolve(state: &mut UiState, details: HostDetails) {
        let host = state.selected_summary().unwrap().clone();
        state.details.insert(host, Ok(details));
    }

    fn render_to_buffer(state: &UiState) -> Buffer {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, state)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn rendered(state: &UiState) -> String {
        buffer_text(&render_to_buffer(state))
    }

    fn buffer_text(buffer: &Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn render_init_to_buffer(plan: &InitPlan) -> Buffer {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render_init(frame, plan)).unwrap();
        terminal.backend().buffer().clone()
    }

    fn press(state: &mut UiState, code: KeyCode) -> UiAction {
        state.handle_key(key(code))
    }

    fn type_text(state: &mut UiState, text: &str) {
        for character in text.chars() {
            press(state, KeyCode::Char(character));
        }
    }

    fn fill_form(state: &mut UiState, values: [&str; 6]) {
        for (input, value) in state.form.values.iter_mut().zip(values) {
            *input = TextInput::new(value);
        }
    }

    #[test]
    fn renders_master_detail_and_hides_mutation_actions_for_external_host() {
        let hosts = vec![
            summary("dev", Ownership::Managed),
            summary("legacy", Ownership::External),
        ];
        let mut state = test_state(hosts);
        state.select_alias("legacy");
        resolve(&mut state, details("legacy", Ownership::External));
        let text = rendered(&state);

        assert!(text.contains("legacy"));
        assert!(text.contains("external"));
        assert!(text.contains("Enter connect"));
        assert!(!text.contains("e edit"));
        assert!(!text.contains("x remove"));
    }

    #[test]
    fn filtering_preserves_a_valid_selection() {
        let mut state = test_state(vec![
            summary("dev", Ownership::Managed),
            summary("prod", Ownership::Managed),
        ]);
        state.set_filter("pro");
        assert_eq!(state.selected_alias(), Some("prod"));
    }

    #[test]
    fn unresolved_selection_says_so_and_keeps_navigation_actions() {
        let state = test_state(vec![summary("dev", Ownership::Managed)]);

        let text = rendered(&state);

        assert!(text.contains("Resolving"), "{text}");
        assert!(text.contains("Enter connect"), "{text}");
        assert!(!text.contains("e edit"), "{text}");
    }

    #[test]
    fn inventory_warnings_remain_visible_without_resolved_details() {
        for external in ["", "Host legacy\n"] {
            let dir = tempfile::tempdir().unwrap();
            let paths = crate::managed::ConfigPaths::from_home(dir.path());
            let store = crate::managed::ManagedStore::new(paths.clone());
            store.apply_init(true, |_, _| Ok(())).unwrap();
            std::fs::write(
                &paths.entry,
                format!("Include ~/.ssh/portico/config\nInclude missing.conf\n{external}"),
            )
            .unwrap();
            let app = App::new(
                store,
                crate::openssh::OpenSsh::new("/usr/bin/false".into(), "/usr/bin/false".into()),
            );
            let mut state = UiState::new(app.hosts().unwrap(), false);

            if let Some(host) = state.selected_summary().cloned() {
                let loaded = load_details(&app, &host, &AtomicBool::new(false));
                assert!(loaded.as_ref().unwrap().resolution.is_err());
                state.details.insert(host, loaded);
            }
            let text = rendered(&state);

            assert!(text.contains("Warning"), "{text}");
            assert!(text.contains("missing.conf"), "{text}");
            assert!(text.contains("q quit"), "{text}");
            if !external.is_empty() {
                assert!(text.contains("exited with"), "{text}");
            }
        }
    }

    #[test]
    fn footer_without_selection_omits_host_actions() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        state.set_filter("missing");

        let text = rendered(&state);

        assert!(text.contains("a add"));
        assert!(text.contains("q quit"));
        for action in [
            "Enter connect",
            "n new terminal",
            "e edit",
            "x remove",
            "↑↓ move",
        ] {
            assert!(!text.contains(action), "invalid action: {action}\n{text}");
        }
    }

    #[test]
    fn status_preserves_context_valid_footer_actions() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        resolve(&mut state, details("dev", Ownership::Managed));
        state.status = Some("Saved dev".into());
        let text = rendered(&state);
        assert!(text.contains("Saved dev"));
        assert!(text.contains("e edit"));
        assert!(text.contains("q quit"));

        state.mode = Mode::AddForm;
        state.status = Some("invalid alias".into());
        let text = rendered(&state);
        assert!(text.contains("invalid alias"));
        assert!(text.contains("Enter save"));
        assert!(text.contains("Esc cancel"));
        assert!(!text.contains("e edit"));
    }

    #[test]
    fn footer_advertises_ctrl_enter_only_when_the_terminal_reports_it() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);

        let text = rendered(&state);
        assert!(text.contains("n new terminal"), "{text}");
        assert!(!text.contains("Ctrl+Enter"), "{text}");

        state.ctrl_enter = true;
        // The longer footer wraps at this width, so check its text rather than the screen.
        let text = footer_text(&state);
        assert!(text.contains("m export"), "{text}");
        assert!(text.contains("Enter connect"), "{text}");
        assert!(text.contains("Ctrl+Enter/n new terminal"), "{text}");
    }

    #[test]
    fn footer_wraps_instead_of_dropping_actions_on_narrow_terminals() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        state.ctrl_enter = true;
        resolve(&mut state, details("dev", Ownership::Managed));
        let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
        terminal.draw(|frame| render(frame, &state)).unwrap();

        let text = buffer_text(terminal.backend().buffer());

        assert!(text.contains("quit"), "{text}");
    }

    fn inventory(hosts: Vec<HostSummary>) -> HostInventory {
        HostInventory {
            hosts,
            warnings: Vec::new(),
        }
    }

    fn summary_in(alias: &str, ownership: Ownership, source: &str) -> HostSummary {
        HostSummary {
            source: PathBuf::from(source),
            ..summary(alias, ownership)
        }
    }

    fn visible_aliases(state: &UiState) -> Vec<String> {
        state
            .visible
            .iter()
            .map(|&index| state.hosts[index].alias.clone())
            .collect()
    }

    #[test]
    fn hosts_filter_is_fuzzy_and_keeps_order() {
        let mut state = test_state(vec![
            summary("web-prod", Ownership::Managed),
            summary("db-prod", Ownership::Managed),
            summary("prod-db", Ownership::Managed),
        ]);
        let order = visible_aliases(&state);
        state.set_filter("pdb");
        assert_eq!(visible_aliases(&state), ["prod-db"]);
        state.set_filter("PROD");
        assert_eq!(visible_aliases(&state), order);
    }

    #[test]
    fn hosts_filter_highlights_matches() {
        let mut state = test_state(vec![
            summary("dev", Ownership::Managed),
            summary("prod-db", Ownership::Managed),
        ]);
        state.set_filter("pd");
        let buffer = render_to_buffer(&state);
        let (x, y) = (0..buffer.area.height)
            .find_map(|y| {
                let line: Vec<&str> = (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect();
                line.windows(7)
                    .position(|cells| cells.concat() == "prod-db")
                    .map(|x| (x as u16, y))
            })
            .unwrap();
        let bold = |offset: u16| buffer[(x + offset, y)].modifier.contains(Modifier::BOLD);
        assert!(bold(0) && bold(5), "p and the d after the dash");
        assert!(!bold(1) && !bold(3));
    }

    #[test]
    fn r_and_f5_refresh_in_browse_mode_only() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        assert_eq!(press(&mut state, KeyCode::Char('r')), UiAction::Refresh);
        assert_eq!(press(&mut state, KeyCode::F(5)), UiAction::Refresh);

        press(&mut state, KeyCode::Char('/'));
        assert_eq!(press(&mut state, KeyCode::Char('r')), UiAction::None);
        assert_eq!(state.filter.as_str(), "r");
        assert_eq!(press(&mut state, KeyCode::F(5)), UiAction::None);

        press(&mut state, KeyCode::Esc);
        press(&mut state, KeyCode::Char('a'));
        assert_eq!(press(&mut state, KeyCode::Char('r')), UiAction::FormEdited);
        assert_eq!(state.form.values[0].as_str(), "r");
        assert_eq!(press(&mut state, KeyCode::F(5)), UiAction::None);
    }

    #[test]
    fn footer_advertises_refresh_with_and_without_a_selection() {
        let mut state = test_state(vec![
            summary("dev", Ownership::Managed),
            summary("legacy", Ownership::External),
        ]);
        assert!(rendered(&state).contains("r refresh"));
        state.selected = Some(1);
        assert!(rendered(&state).contains("r refresh"));
        state.set_filter("missing");
        assert!(rendered(&state).contains("r refresh"));
    }

    #[test]
    fn refresh_replaces_the_inventory_and_its_warnings() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        resolve(&mut state, details("dev", Ownership::Managed));
        let mut fresh = inventory(vec![
            summary("dev", Ownership::Managed),
            summary("added", Ownership::External),
        ]);
        fresh.warnings = vec![DiscoveryWarning::Include {
            path: PathBuf::from("/home/test/.ssh/missing"),
            message: "unreadable".into(),
        }];

        assert!(apply_refresh(&mut state, || Ok::<_, String>(fresh)));

        assert_eq!(state.hosts.len(), 2);
        assert_eq!(state.warnings.len(), 1);
        assert_eq!(state.selected_alias(), Some("dev"));
        assert_eq!(state.status.as_deref(), Some("Refreshed: 2 hosts"));
    }

    #[test]
    fn refresh_status_counts_every_host_and_is_singular_for_one() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        state.set_filter("nomatch");
        apply_refresh(&mut state, || {
            Ok::<_, String>(inventory(vec![summary("dev", Ownership::Managed)]))
        });
        assert_eq!(state.status.as_deref(), Some("Refreshed: 1 host"));
        assert_eq!(state.filter.as_str(), "nomatch");
        assert_eq!(state.selected, None);
    }

    #[test]
    fn refresh_keeps_the_selected_host_when_the_list_shifts_around_it() {
        let mut state = test_state(vec![
            summary("alpha", Ownership::Managed),
            summary("beta", Ownership::Managed),
        ]);
        press(&mut state, KeyCode::Down);
        apply_refresh(&mut state, || {
            Ok::<_, String>(inventory(vec![
                summary("aaa", Ownership::External),
                summary("alpha", Ownership::Managed),
                summary("beta", Ownership::Managed),
            ]))
        });
        assert_eq!(state.selected_alias(), Some("beta"));
    }

    #[test]
    fn refresh_follows_a_host_whose_definition_moved_to_another_file() {
        let mut state = test_state(vec![
            summary_in("alpha", Ownership::External, "/home/test/.ssh/config"),
            summary_in("beta", Ownership::External, "/home/test/.ssh/config"),
        ]);
        press(&mut state, KeyCode::Down);
        apply_refresh(&mut state, || {
            Ok::<_, String>(inventory(vec![
                summary_in("alpha", Ownership::External, "/home/test/.ssh/config"),
                summary_in("BETA", Ownership::External, "/home/test/.ssh/extra"),
            ]))
        });
        assert_eq!(state.selected_alias(), Some("BETA"));
    }

    #[test]
    fn refresh_selects_the_first_host_when_the_selected_one_vanished() {
        let mut state = test_state(vec![
            summary("alpha", Ownership::Managed),
            summary("beta", Ownership::Managed),
        ]);
        press(&mut state, KeyCode::Down);
        apply_refresh(&mut state, || {
            Ok::<_, String>(inventory(vec![summary("alpha", Ownership::Managed)]))
        });
        assert_eq!(state.selected_alias(), Some("alpha"));
    }

    #[test]
    fn failed_refresh_changes_nothing_but_the_status() {
        let mut state = test_state(vec![
            summary("alpha", Ownership::Managed),
            summary("beta", Ownership::Managed),
        ]);
        press(&mut state, KeyCode::Down);
        resolve(&mut state, details("beta", Ownership::Managed));
        state.set_filter("b");
        let (hosts, selected) = (state.hosts.clone(), state.selected);

        let changed = apply_refresh(&mut state, || Err::<HostInventory, _>("cannot read config"));

        assert!(!changed);
        assert_eq!(
            state.status.as_deref(),
            Some("Refresh failed: cannot read config")
        );
        assert_eq!(state.hosts, hosts);
        assert_eq!(state.selected, selected);
        assert_eq!(state.filter.as_str(), "b");
        assert!(
            state.selected_details().is_some(),
            "cached details are kept"
        );
    }

    #[test]
    fn browse_keys_move_connect_and_protect_external_hosts() {
        let mut state = test_state(vec![
            summary("dev", Ownership::Managed),
            summary("legacy", Ownership::External),
        ]);
        press(&mut state, KeyCode::Down);
        assert_eq!(state.selected_alias(), Some("legacy"));

        assert_eq!(
            press(&mut state, KeyCode::Enter),
            UiAction::Connect {
                alias: "legacy".into(),
                new_terminal: false,
            }
        );
        assert_eq!(
            press(&mut state, KeyCode::Char('n')),
            UiAction::Connect {
                alias: "legacy".into(),
                new_terminal: true,
            }
        );
        assert_eq!(press(&mut state, KeyCode::Char('e')), UiAction::None);
        assert_eq!(press(&mut state, KeyCode::Char('x')), UiAction::None);
        assert_eq!(state.mode, Mode::Browse);
    }

    #[test]
    fn ctrl_enter_opens_a_new_terminal_like_n() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        let ctrl_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL);

        assert_eq!(
            state.handle_key(ctrl_enter),
            UiAction::Connect {
                alias: "dev".into(),
                new_terminal: true,
            }
        );
        assert_eq!(
            state.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)),
            UiAction::None
        );
    }

    #[test]
    fn ctrl_enter_does_nothing_in_forms_filter_and_confirmation() {
        let ctrl_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL);
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        resolve(&mut state, details("dev", Ownership::Managed));

        press(&mut state, KeyCode::Char('/'));
        assert_eq!(state.handle_key(ctrl_enter), UiAction::None);
        assert_eq!(state.mode, Mode::Filter);
        press(&mut state, KeyCode::Enter);

        press(&mut state, KeyCode::Char('a'));
        fill_form(&mut state, ["new", "new.example", "", "", "", ""]);
        assert_eq!(state.handle_key(ctrl_enter), UiAction::None);
        assert_eq!(state.mode, Mode::AddForm);
        assert!(matches!(
            press(&mut state, KeyCode::Enter),
            UiAction::Add(_)
        ));
        press(&mut state, KeyCode::Esc);

        press(&mut state, KeyCode::Char('x'));
        assert_eq!(state.handle_key(ctrl_enter), UiAction::None);
        assert_eq!(state.mode, Mode::ConfirmRemove);
        assert_eq!(
            press(&mut state, KeyCode::Enter),
            UiAction::Remove("dev".into())
        );
    }

    #[test]
    fn duplicate_external_row_keeps_its_identity_and_cannot_edit_managed_details() {
        let managed = summary("shared", Ownership::Managed);
        let mut external = summary("shared", Ownership::External);
        external.source = PathBuf::from("/home/test/.ssh/external.conf");
        let mut state = test_state(vec![managed.clone(), external.clone()]);
        resolve(&mut state, details("shared", Ownership::Managed));
        assert!(state.selected_details().is_some());

        press(&mut state, KeyCode::Down);
        assert!(
            state.selected_details().is_none(),
            "details belong to the row they were resolved for"
        );
        let relabelled = details_for_selected(
            state.selected_summary().unwrap(),
            details("shared", Ownership::Managed),
        );
        assert_eq!(relabelled.summary, external);
        assert!(relabelled.declared.is_none());
        resolve(&mut state, relabelled);

        press(&mut state, KeyCode::Char('e'));
        assert_eq!(state.mode, Mode::Browse);

        state.replace_hosts(HostInventory {
            hosts: vec![managed, external.clone()],
            warnings: Vec::new(),
        });
        assert_eq!(state.selected_summary(), Some(&external));
    }

    #[test]
    fn managed_remove_requires_confirmation_and_q_quits() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);

        assert_eq!(press(&mut state, KeyCode::Char('x')), UiAction::None);
        assert_eq!(state.mode, Mode::ConfirmRemove);
        assert_eq!(press(&mut state, KeyCode::Esc), UiAction::None);
        assert_eq!(state.mode, Mode::Browse);
        assert_eq!(press(&mut state, KeyCode::Char('q')), UiAction::Quit);
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

    fn form_with_active_field(index: usize) -> UiState {
        let mut state = test_state(Vec::new());
        press(&mut state, KeyCode::Char('a'));
        for _ in 0..index {
            press(&mut state, KeyCode::Tab);
        }
        state
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
    fn details_render_each_field_once_with_the_effective_value() {
        let mut host_details = details("dev", Ownership::Managed);
        host_details.resolution.as_mut().unwrap().identity_files = vec![
            "~/.ssh/id_ed25519".into(),
            "~/.ssh/id_rsa".into(),
            "~/.ssh/id_ed25519".into(),
        ];
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        resolve(&mut state, host_details);

        let text = rendered(&state);

        for label in ["HostName", "User", "Port", "IdentityFile", "ProxyJump"] {
            assert_eq!(text.matches(label).count(), 1, "{label}\n{text}");
        }
        assert_eq!(text.matches("id_ed25519").count(), 1, "{text}");
        assert!(text.contains("id_rsa"), "{text}");
        assert!(!text.contains("overridden"), "{text}");
    }

    #[test]
    fn details_flag_declared_values_that_openssh_overrides() {
        let mut host_details = details("dev", Ownership::Managed);
        host_details.resolution.as_mut().unwrap().hostname = "effective.example".into();
        host_details.warnings = vec![DiscoveryWarning::Include {
            path: PathBuf::from("/home/test/.ssh/missing.conf"),
            message: "no matching files".into(),
        }];
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        state.warnings = host_details.warnings.clone();
        resolve(&mut state, host_details);

        let text = rendered(&state);

        assert!(text.contains("effective.example"));
        assert!(text.contains("overridden by OpenSSH"), "{text}");
        assert!(text.contains("HostName: dev.example"), "{text}");
        assert!(text.contains("Warning:"));
        assert!(text.contains("missing.conf: no matching files"));
    }

    #[test]
    fn declared_home_relative_paths_match_the_expanded_effective_path() {
        let declared = DetailRow::new("IdentityFile", ["~/.ssh/id".to_owned()]);
        let home = Path::new("/home/test");
        let effective = DetailRow::new("IdentityFile", ["/home/test/.ssh/id".to_owned()]);

        assert!(declared_in_effect(&declared, &effective, home));
        assert!(declared_in_effect(
            &DetailRow::new("IdentityFile", []),
            &effective,
            home
        ));
        assert!(!declared_in_effect(
            &DetailRow::new("IdentityFile", ["~/.ssh/other".to_owned()]),
            &effective,
            home
        ));
    }

    #[test]
    fn resolution_failure_keeps_managed_host_editable_and_shows_declared_values() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        let mut host_details = details("dev", Ownership::Managed);
        host_details.resolution = Err("ssh -G failed".into());
        resolve(&mut state, host_details);

        let text = rendered(&state);
        assert!(text.contains("ssh -G failed"));
        assert!(text.contains("dev.example"));

        press(&mut state, KeyCode::Char('e'));

        assert_eq!(state.mode, Mode::EditForm);
        assert_eq!(state.form.values[0].as_str(), "dev");
    }

    #[test]
    fn edit_requested_before_details_load_opens_when_they_arrive() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);

        press(&mut state, KeyCode::Char('e'));
        assert_eq!(state.mode, Mode::Browse);
        resolve(&mut state, details("dev", Ownership::Managed));
        state.open_pending_edit();

        assert_eq!(state.mode, Mode::EditForm);
        assert_eq!(state.form.values[0].as_str(), "dev");
    }

    #[test]
    fn pending_edit_is_dropped_by_any_other_key() {
        let mut state = test_state(vec![
            summary("dev", Ownership::Managed),
            summary("prod", Ownership::Managed),
        ]);

        press(&mut state, KeyCode::Char('e'));
        press(&mut state, KeyCode::Down);
        resolve(&mut state, details("prod", Ownership::Managed));
        state.open_pending_edit();

        assert_eq!(state.mode, Mode::Browse);
    }

    #[test]
    fn edit_form_prefills_and_clears_optional_fields() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        resolve(&mut state, details("dev", Ownership::Managed));
        press(&mut state, KeyCode::Char('e'));
        assert_eq!(
            state.form.values.each_ref().map(TextInput::as_str),
            [
                "dev",
                "dev.example",
                "deploy",
                "22",
                "~/.ssh/id_ed25519",
                "bastion",
            ]
        );
        for index in 2..6 {
            state.form.values[index] = TextInput::default();
        }

        let UiAction::Edit { alias, changes } = press(&mut state, KeyCode::Enter) else {
            panic!("expected edit action")
        };
        assert_eq!(alias, "dev");
        assert_eq!(changes.user, Change::Clear);
        assert_eq!(changes.port, Change::Clear);
        assert_eq!(changes.identity_file, Change::Clear);
        assert_eq!(changes.proxy_jump, Change::Clear);
    }

    #[test]
    fn filter_mode_updates_selection_and_enter_keeps_the_filter() {
        let mut state = test_state(vec![
            summary("dev", Ownership::Managed),
            summary("prod", Ownership::Managed),
        ]);
        press(&mut state, KeyCode::Char('/'));
        type_text(&mut state, "pro");
        assert_eq!(state.selected_alias(), Some("prod"));
        press(&mut state, KeyCode::Enter);
        assert_eq!(state.mode, Mode::Browse);
        assert_eq!(state.filter.as_str(), "pro");

        press(&mut state, KeyCode::Esc);
        assert_eq!(state.filter.as_str(), "");
        assert_eq!(state.visible.len(), 2);
    }

    #[test]
    fn escape_cancels_a_filter_edit_back_to_the_previous_filter() {
        let mut state = test_state(vec![
            summary("dev", Ownership::Managed),
            summary("prod", Ownership::Managed),
        ]);
        press(&mut state, KeyCode::Char('/'));
        type_text(&mut state, "de");
        press(&mut state, KeyCode::Enter);

        press(&mut state, KeyCode::Char('/'));
        type_text(&mut state, "v");
        press(&mut state, KeyCode::Backspace);
        press(&mut state, KeyCode::Backspace);
        press(&mut state, KeyCode::Backspace);
        assert_eq!(state.filter.as_str(), "");
        assert_eq!(state.visible.len(), 2);

        press(&mut state, KeyCode::Esc);

        assert_eq!(state.mode, Mode::Browse);
        assert_eq!(state.filter.as_str(), "de");
        assert_eq!(state.selected_alias(), Some("dev"));
    }

    #[test]
    fn backspace_erases_text_and_only_escape_cancels() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        resolve(&mut state, details("dev", Ownership::Managed));

        press(&mut state, KeyCode::Char('/'));
        type_text(&mut state, "dex");
        press(&mut state, KeyCode::Backspace);
        assert_eq!(state.filter.as_str(), "de");
        assert_eq!(state.mode, Mode::Filter);
        press(&mut state, KeyCode::Esc);
        assert_eq!(state.mode, Mode::Browse);

        press(&mut state, KeyCode::Char('a'));
        fill_form(&mut state, ["dev", "", "", "", "", ""]);
        press(&mut state, KeyCode::Backspace);
        assert_eq!(state.form.values[0].as_str(), "de");
        assert_eq!(state.mode, Mode::AddForm);
        press(&mut state, KeyCode::Esc);
        assert_eq!(state.mode, Mode::Browse);

        press(&mut state, KeyCode::Char('x'));
        assert_eq!(state.mode, Mode::ConfirmRemove);
        assert_eq!(press(&mut state, KeyCode::Backspace), UiAction::None);
        assert_eq!(state.mode, Mode::ConfirmRemove);
        press(&mut state, KeyCode::Esc);
        assert_eq!(state.mode, Mode::Browse);
        assert_eq!(init_decision(key(KeyCode::Backspace)), None);
        assert_eq!(init_decision(key(KeyCode::Esc)), Some(false));
    }

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

    #[test]
    fn paste_inserts_at_the_cursor_without_line_breaks() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        press(&mut state, KeyCode::Char('a'));
        type_text(&mut state, "ad");
        press(&mut state, KeyCode::Left);

        state.handle_paste("bc\r\n");

        assert_eq!(state.form.values[0].as_str(), "abcd");
        press(&mut state, KeyCode::Char('x'));
        assert_eq!(state.form.values[0].as_str(), "abcxd");

        state.mode = Mode::Filter;
        state.handle_paste("de\nv");
        assert_eq!(state.filter.as_str(), "dev");
        assert_eq!(state.visible.len(), 1);
    }

    #[test]
    fn refreshing_hosts_retains_the_selected_alias() {
        let mut state = test_state(vec![
            summary("dev", Ownership::Managed),
            summary("prod", Ownership::Managed),
        ]);
        state.select_alias("prod");

        state.replace_hosts(HostInventory {
            hosts: vec![
                summary("new", Ownership::Managed),
                summary("prod", Ownership::Managed),
            ],
            warnings: Vec::new(),
        });

        assert_eq!(state.selected_alias(), Some("prod"));
    }

    #[test]
    fn completed_mutation_exits_form_even_when_refresh_fails() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        resolve(&mut state, details("dev", Ownership::Managed));
        press(&mut state, KeyCode::Char('e'));
        assert_eq!(state.mode, Mode::EditForm);

        complete_mutation(&mut state, Some("dev"), &[], "Saved dev".into(), || {
            Err::<HostInventory, _>("inventory unavailable")
        });

        assert_eq!(state.mode, Mode::Browse);
        assert!(state.form.original.is_none());
        assert_eq!(
            state.status.as_deref(),
            Some("Saved dev, refresh failed: inventory unavailable")
        );
        assert!(matches!(
            press(&mut state, KeyCode::Enter),
            UiAction::Connect { .. }
        ));
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

    /// Records the terminal operations it is asked for, optionally failing one.
    #[derive(Clone, Default)]
    struct RecordingControl {
        calls: Arc<std::sync::Mutex<Vec<&'static str>>>,
        failing: Option<&'static str>,
    }

    impl RecordingControl {
        fn call(&self, name: &'static str) -> io::Result<()> {
            self.calls.lock().unwrap().push(name);
            if self.failing == Some(name) {
                Err(io::Error::other(name))
            } else {
                Ok(())
            }
        }

        fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl TerminalControl for RecordingControl {
        fn enable_raw_mode(&self) -> io::Result<()> {
            self.call("raw on")
        }
        fn disable_raw_mode(&self) -> io::Result<()> {
            self.call("raw off")
        }
        fn enter_screen(&self) -> io::Result<()> {
            self.call("screen on")
        }
        fn leave_screen(&self) -> io::Result<()> {
            self.call("screen off")
        }
        fn push_keyboard_flags(&self) -> io::Result<()> {
            self.call("push")
        }
        fn pop_keyboard_flags(&self) -> io::Result<()> {
            self.call("pop")
        }
    }

    fn session(control: &RecordingControl) -> TerminalSession {
        TerminalSession::new(Box::new(control.clone()), true)
    }

    #[test]
    fn session_enter_and_leave_are_idempotent() {
        let control = RecordingControl::default();
        let session = session(&control);

        session.leave().unwrap();
        session.enter().unwrap();
        session.enter().unwrap();
        session.leave().unwrap();
        session.leave().unwrap();
        session.enter().unwrap();

        assert_eq!(
            control.calls(),
            [
                "raw on",
                "screen on",
                "push",
                "pop",
                "raw off",
                "screen off",
                "raw on",
                "screen on",
                "push"
            ]
        );
    }

    #[test]
    fn session_never_pops_flags_it_did_not_push() {
        // The screen switch fails before the push, then the guard restores as well.
        let control = RecordingControl {
            failing: Some("screen on"),
            ..RecordingControl::default()
        };
        let session = session(&control);

        assert!(session.enter().is_err());
        session.leave().unwrap();

        assert!(!control.calls().contains(&"pop"));
        assert!(control.calls().contains(&"raw off"));
    }

    #[test]
    fn session_pops_once_when_reentering_fails_after_the_push() {
        let control = RecordingControl {
            failing: Some("push"),
            ..RecordingControl::default()
        };
        let session = session(&control);

        assert!(session.enter().is_err());
        session.leave().unwrap();

        assert_eq!(
            control.calls(),
            ["raw on", "screen on", "push", "raw off", "screen off"]
        );
    }

    #[test]
    fn session_without_keyboard_enhancement_never_touches_flags() {
        let control = RecordingControl::default();
        let session = TerminalSession::new(Box::new(control.clone()), false);

        session.enter().unwrap();
        session.leave().unwrap();

        assert_eq!(
            control.calls(),
            ["raw on", "screen on", "raw off", "screen off"]
        );
    }

    #[test]
    fn leave_reports_a_failure_but_still_restores_every_mode() {
        let control = RecordingControl {
            failing: Some("pop"),
            ..RecordingControl::default()
        };
        let session = session(&control);
        session.enter().unwrap();

        assert!(session.leave().is_err());

        assert_eq!(&control.calls()[3..], ["pop", "raw off", "screen off"]);
    }

    #[test]
    fn panic_hook_restores_the_terminal_before_the_message_is_reported() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let this_thread = thread::current().id();
        let record = move |log: &Arc<std::sync::Mutex<Vec<&'static str>>>, entry| {
            // The hook is process-wide; ignore panics from other tests' threads.
            if thread::current().id() == this_thread {
                log.lock().unwrap().push(entry);
            }
        };
        let original = panic::take_hook();
        let reported = Arc::clone(&log);
        panic::set_hook(Box::new(move |_| record(&reported, "report")));
        let restoring = Arc::clone(&log);
        let hook = PanicHook::install(move || record(&restoring, "restore"));

        let _ = panic::catch_unwind(|| panic!("boom"));
        assert_eq!(*log.lock().unwrap(), ["restore", "report"]);

        drop(hook);
        let _ = panic::catch_unwind(|| panic!("boom"));
        assert_eq!(*log.lock().unwrap(), ["restore", "report", "report"]);
        panic::set_hook(original);
    }

    /// Serialises the tests that install `Signals` and raise signals.
    ///
    /// Signal handlers are process-wide and are never unregistered, so a signal one test raises
    /// runs the handlers every other test installed too: SIGTERM raised while another test holds
    /// a foreground child would set that test's flag. These tests must not overlap.
    fn signal_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn sigint_ends_the_tui_unless_a_child_owns_the_terminal() {
        let _lock = signal_test_lock();
        let signals = Signals::install().unwrap();
        {
            let _child = signals.foreground_child();
            signal_hook::low_level::raise(signal_hook::consts::SIGINT).unwrap();
            assert!(!signals.flag().load(Ordering::Relaxed));
        }
        signal_hook::low_level::raise(signal_hook::consts::SIGINT).unwrap();
        assert!(signals.flag().load(Ordering::Relaxed));
    }

    #[test]
    fn sigterm_and_sighup_end_the_tui_even_while_a_child_runs() {
        let _lock = signal_test_lock();
        for signal in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGHUP] {
            let signals = Signals::install().unwrap();
            let _child = signals.foreground_child();
            signal_hook::low_level::raise(signal).unwrap();
            assert!(signals.flag().load(Ordering::Relaxed), "{signal}");
        }
    }

    fn inventory_of(aliases: &[&str]) -> HostInventory {
        HostInventory {
            hosts: aliases
                .iter()
                .map(|alias| summary(alias, Ownership::Managed))
                .collect(),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn failures_that_may_have_touched_the_file_reload_the_inventory() {
        let uncertain = [
            AppError::Transaction(TransactionError::Rollback {
                path: "managed".into(),
                recovery: "backup".into(),
                sync: io::Error::other("sync"),
                rollback: io::Error::other("rollback"),
            }),
            AppError::Transaction(TransactionError::Io {
                path: "managed".into(),
                source: io::Error::other("rename"),
            }),
            AppError::Transaction(TransactionError::Managed {
                backup: "backup".into(),
                source: crate::managed::ManagedError::Parse {
                    line: 1,
                    message: "bad".into(),
                },
            }),
        ];
        for error in uncertain {
            let mut state = test_state(vec![summary("old", Ownership::Managed)]);
            let changed = fail_mutation(&mut state, &error, || {
                Ok::<_, AppError>(inventory_of(&["old", "new"]))
            });
            assert!(changed, "{error}");
            assert_eq!(state.hosts.len(), 2, "{error}");
            assert!(state.status.is_some());
        }
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
    fn rejections_before_activation_leave_the_inventory_alone() {
        let rejected = [
            AppError::Transaction(TransactionError::Validation(
                ConfigRejection::Unrecognised { raw: "bad".into() },
            )),
            AppError::Transaction(TransactionError::NotInitialized),
            AppError::Transaction(TransactionError::Entry("bad".into())),
            AppError::AliasCollision("dev".into()),
            AppError::UnknownHost("dev".into()),
            AppError::ReadOnlyExternal("dev".into()),
        ];
        for error in rejected {
            let mut state = test_state(vec![summary("old", Ownership::Managed)]);
            let changed = fail_mutation(&mut state, &error, || -> Result<_, AppError> {
                panic!("must not reload for {error}")
            });
            assert!(!changed, "{error}");
            assert_eq!(state.hosts.len(), 1);
        }
    }

    #[test]
    fn restores_terminal_around_child_even_when_child_fails() {
        let events = std::cell::RefCell::new(Vec::new());
        let result: Result<(), TuiError> = suspend_for_child(
            || {
                events.borrow_mut().push("leave");
                Ok(())
            },
            || {
                events.borrow_mut().push("child");
                Err(TuiError::Child("failed".into()))
            },
            || {
                events.borrow_mut().push("enter");
                Ok(())
            },
        );

        assert!(result.is_err());
        assert_eq!(*events.borrow(), ["leave", "child", "enter"]);
    }

    /// An app whose `ssh -G` appends its alias to a log, then runs `behaviour`.
    fn logging_app(dir: &std::path::Path, behaviour: &str) -> (App, PathBuf) {
        let paths = crate::managed::ConfigPaths::from_home(dir);
        let store = crate::managed::ManagedStore::new(paths.clone());
        store.apply_init(true, |_, _| Ok(())).unwrap();
        std::fs::write(&paths.entry, "Include ~/.ssh/portico/config\nHost a b c\n").unwrap();
        let log = dir.join("resolved.log");
        let ssh = dir.join("ssh");
        crate::test_executable::write_executable(
            &ssh,
            &format!(
                "#!/bin/sh\nfor alias do :; done\necho \"$alias\" >> '{}'\n{behaviour}\nprintf 'hostname %s.example\\n' \"$alias\"\n",
                log.display()
            ),
        );
        let app = App::new(
            store,
            crate::openssh::OpenSsh::new(ssh, "/usr/bin/false".into()),
        );
        (app, log)
    }

    #[test]
    fn redraw_from_scratch_repaints_a_screen_the_terminal_lost() {
        let mut terminal = Terminal::new(TestBackend::new(20, 3)).unwrap();
        let draw = |terminal: &mut Terminal<TestBackend>| {
            terminal
                .draw(|frame| frame.render_widget(Paragraph::new("portico"), frame.area()))
                .unwrap();
        };
        draw(&mut terminal);
        // The terminal forgot the frame, as after leaving and re-entering the alternate screen.
        terminal.backend_mut().clear().unwrap();
        draw(&mut terminal);
        assert_eq!(terminal.backend().buffer()[(0, 0)].symbol(), " ");

        terminal.backend_mut().clear().unwrap();
        redraw_from_scratch(&mut terminal).unwrap();
        draw(&mut terminal);

        assert_eq!(terminal.backend().buffer()[(0, 0)].symbol(), "p");
    }

    fn summary_of(app: &App, alias: &str) -> HostSummary {
        app.hosts()
            .unwrap()
            .hosts
            .into_iter()
            .find(|host| host.alias == alias)
            .unwrap()
    }

    fn wait_for_result(resolver: &mut Resolver) -> (HostSummary, Loaded) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(result) = resolver.poll() {
                return result;
            }
            assert!(Instant::now() < deadline, "no resolution arrived");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_burst_of_selection_changes_resolves_only_the_last_host() {
        let dir = tempfile::tempdir().unwrap();
        let (app, log) = logging_app(dir.path(), "");
        thread::scope(|scope| {
            let mut resolver = Resolver::spawn(scope, &app);
            for alias in ["a", "b", "c"] {
                resolver.request(&summary_of(&app, alias));
            }

            let (host, loaded) = wait_for_result(&mut resolver);

            assert_eq!(host.alias, "c");
            assert_eq!(loaded.unwrap().resolution.unwrap().hostname, "c.example");
            assert!(!resolver.is_pending());
        });
        assert_eq!(std::fs::read_to_string(log).unwrap(), "c\n");
    }

    #[test]
    fn a_cancelled_request_never_delivers_a_result() {
        let dir = tempfile::tempdir().unwrap();
        let (app, log) = logging_app(dir.path(), "");
        thread::scope(|scope| {
            let mut resolver = Resolver::spawn(scope, &app);
            resolver.request(&summary_of(&app, "a"));
            resolver.cancel();
            assert!(!resolver.is_pending());

            thread::sleep(RESOLVE_DEBOUNCE * 4);

            assert!(resolver.poll().is_none());
            assert!(!log.exists(), "a cancelled request must not start ssh");
            resolver.request(&summary_of(&app, "b"));
            assert_eq!(wait_for_result(&mut resolver).0.alias, "b");
        });
    }

    #[test]
    fn dropping_the_resolver_interrupts_a_running_ssh() {
        let dir = tempfile::tempdir().unwrap();
        let (app, log) = logging_app(dir.path(), "sleep 10");
        let started = Instant::now();
        thread::scope(|scope| {
            let mut resolver = Resolver::spawn(scope, &app);
            resolver.request(&summary_of(&app, "a"));
            let deadline = Instant::now() + Duration::from_secs(5);
            while !log.exists() {
                assert!(Instant::now() < deadline, "ssh never started");
                thread::sleep(Duration::from_millis(5));
            }
        });
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn syncing_requests_uncached_hosts_and_inventory_changes_invalidate() {
        let dir = tempfile::tempdir().unwrap();
        let (app, _) = logging_app(dir.path(), "");
        let mut state = UiState::new(app.hosts().unwrap(), false);
        thread::scope(|scope| {
            let mut resolver = Resolver::spawn(scope, &app);

            sync_details(&mut state, &mut resolver, false);
            assert!(resolver.is_pending());
            let (host, loaded) = wait_for_result(&mut resolver);
            state.details.insert(host, loaded);
            assert!(state.selected_details().is_some());

            sync_details(&mut state, &mut resolver, false);
            assert!(!resolver.is_pending(), "cached hosts need no ssh");

            sync_details(&mut state, &mut resolver, true);
            assert!(state.selected_details().is_none());
            assert!(resolver.is_pending());
        });
    }

    fn ctrl(character: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL)
    }

    fn report(alias: &str, result: ProbeResult) -> TestReport {
        TestReport {
            alias: alias.into(),
            user: Some("u".into()),
            result,
            key_fetch_failure: None,
            explanation: None,
        }
    }

    fn authenticated(alias: &str) -> TestOutcome {
        TestOutcome::Done(report(
            alias,
            ProbeResult::Authenticated(crate::probe::Authenticated {
                host: "10.0.0.5".into(),
                address: "10.0.0.5".into(),
                port: 22,
                method: "publickey".into(),
                via: None,
            }),
        ))
    }

    fn host_finished(host: HostSummary, outcome: Result<TestOutcome, String>) -> Finished {
        Finished {
            target: TestTarget::Saved(host.alias.clone()),
            origin: Origin::Host(host),
            outcome,
        }
    }

    #[test]
    fn t_starts_a_test_of_the_selected_host_in_browse_mode_only() {
        let dev = summary("dev", Ownership::Managed);
        let mut state = test_state(vec![dev.clone()]);

        assert_eq!(
            press(&mut state, KeyCode::Char('t')),
            UiAction::Test {
                origin: Origin::Host(dev),
                target: TestTarget::Saved("dev".into()),
            }
        );

        press(&mut state, KeyCode::Char('/'));
        assert_eq!(press(&mut state, KeyCode::Char('t')), UiAction::None);
        assert_eq!(state.filter.as_str(), "t");
        press(&mut state, KeyCode::Esc);

        press(&mut state, KeyCode::Char('a'));
        assert_eq!(press(&mut state, KeyCode::Char('t')), UiAction::FormEdited);
        assert_eq!(state.form.values[0].as_str(), "t");

        let mut empty = test_state(Vec::new());
        assert_eq!(press(&mut empty, KeyCode::Char('t')), UiAction::None);
    }

    #[test]
    fn ctrl_t_tests_the_typed_values_in_forms() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        assert_eq!(state.handle_key(ctrl('t')), UiAction::None, "browse");

        press(&mut state, KeyCode::Char('a'));
        fill_form(&mut state, ["new", "h", "", "", "", ""]);
        assert_eq!(
            state.handle_key(ctrl('t')),
            UiAction::Test {
                origin: Origin::Form,
                target: TestTarget::Add(Host::new("new", "h", None, None, None, None).unwrap()),
            }
        );
        assert_eq!(state.mode, Mode::AddForm);

        press(&mut state, KeyCode::Esc);
        resolve(&mut state, details("dev", Ownership::Managed));
        press(&mut state, KeyCode::Char('e'));
        state.form.values[0] = TextInput::new("renamed");
        let UiAction::Test {
            origin: Origin::Form,
            target: TestTarget::Edit { alias, changes },
        } = state.handle_key(ctrl('t'))
        else {
            panic!("expected a test of the edit");
        };
        assert_eq!(alias, "dev");
        assert_eq!(changes.alias, Change::Set("renamed".into()));
        assert_eq!(state.mode, Mode::EditForm);
    }

    #[test]
    fn ctrl_t_with_an_invalid_form_reports_the_field_error() {
        let mut state = test_state(Vec::new());
        press(&mut state, KeyCode::Char('a'));
        fill_form(&mut state, ["new", "", "", "", "", ""]);

        assert_eq!(state.handle_key(ctrl('t')), UiAction::None);

        assert_eq!(state.form.active, 1);
        assert!(state.form.error.is_some());
        assert_eq!(state.mode, Mode::AddForm);
    }

    #[test]
    fn esc_cancels_a_running_test_before_leaving_a_form() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        resolve(&mut state, details("dev", Ownership::Managed));
        press(&mut state, KeyCode::Char('e'));
        state.testing = Some("dev".into());

        assert_eq!(press(&mut state, KeyCode::Esc), UiAction::CancelTest);
        assert_eq!(state.mode, Mode::EditForm);
        assert_eq!(state.status.as_deref(), Some("Test cancelled"));

        state.testing = None;
        assert_eq!(press(&mut state, KeyCode::Esc), UiAction::None);
        assert_eq!(state.mode, Mode::Browse);

        // In browse mode too, Esc cancels before it clears the filter.
        state.set_filter("de");
        state.testing = Some("dev".into());
        assert_eq!(press(&mut state, KeyCode::Esc), UiAction::CancelTest);
        assert_eq!(state.filter.as_str(), "de");
    }

    #[test]
    fn results_mark_the_host_and_appear_in_details_and_status() {
        let dev = summary("dev", Ownership::Managed);
        let prod = summary("prod", Ownership::Managed);
        let web = summary("web", Ownership::External);
        let mut state = test_state(vec![dev.clone(), prod.clone(), web]);
        resolve(&mut state, details("dev", Ownership::Managed));

        state.record_test(
            host_finished(dev.clone(), Ok(authenticated("dev"))),
            "12:34".into(),
        );

        let text = rendered(&state);
        assert!(text.contains("✓ dev  managed"), "{text}");
        assert!(text.contains("  web  external"), "{text}");
        assert!(
            text.contains("dev: authenticated as u@10.0.0.5:22 (publickey)"),
            "{text}"
        );
        assert!(
            details_text(&state)
                .contains("Connection test: authenticated as u@10.0.0.5:22 (publickey) at 12:34"),
            "{}",
            details_text(&state)
        );

        let unknown_key = TestOutcome::UnknownHostKey {
            report: report(
                "prod",
                ProbeResult::Failed(crate::probe::Failure::HostKeyUnknown),
            ),
            key: crate::openssh::PresentedKey {
                lines: Vec::new(),
                fingerprints: Vec::new(),
            },
        };
        state.record_test(host_finished(prod.clone(), Ok(unknown_key)), "12:35".into());
        assert_eq!(state.mode, Mode::ConfirmTrust);
        assert_eq!(
            press(&mut state, KeyCode::Char('n')),
            UiAction::Trust(false)
        );
        let text = rendered(&state);
        assert!(text.contains("✗ prod  managed"), "{text}");
        assert!(text.contains("Host key not trusted"), "{text}");

        state.record_test(
            host_finished(prod, Err("unknown host: prod".into())),
            "12:36".into(),
        );
        assert_eq!(
            state.status.as_deref(),
            Some("prod: failed — unknown host: prod")
        );
        assert!(state.form.test_result.is_none());
    }

    #[test]
    fn a_result_for_a_closed_form_is_dropped() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        let form_finished = || Finished {
            origin: Origin::Form,
            target: TestTarget::Add(Host::new("new", "h", None, None, None, None).unwrap()),
            outcome: Ok(authenticated("new")),
        };

        state.record_test(form_finished(), "12:34".into());
        assert!(state.form.test_result.is_none());
        assert!(state.status.is_none());
        assert!(!rendered(&state).contains('✓'));

        press(&mut state, KeyCode::Char('a'));
        state.record_test(form_finished(), "12:34".into());
        assert_eq!(
            state
                .form
                .test_result
                .as_ref()
                .map(|result| result.line.as_str()),
            Some("authenticated as u@10.0.0.5:22 (publickey)")
        );
        let text = rendered(&state);
        assert!(text.contains("Connection test: authenticated"), "{text}");
        assert!(state.status.is_none());

        // Editing the form clears the result, as it clears an error.
        press(&mut state, KeyCode::Char('x'));
        assert!(state.form.test_result.is_none());
    }

    #[test]
    fn results_are_cleared_on_edit_removal_and_refresh() {
        let dev = summary("dev", Ownership::Managed);
        let prod = summary("prod", Ownership::Managed);
        let inventory = || HostInventory {
            hosts: vec![
                summary("dev", Ownership::Managed),
                summary("prod", Ownership::Managed),
            ],
            warnings: Vec::new(),
        };
        let mut state = test_state(inventory().hosts);
        for host in [&dev, &prod] {
            state.record_test(
                host_finished(host.clone(), Ok(authenticated(&host.alias))),
                "12:34".into(),
            );
        }

        complete_mutation(
            &mut state,
            Some("dev"),
            &["DEV"],
            "Saved dev".into(),
            || Ok::<_, String>(inventory()),
        );
        assert!(state.results.get(&dev).is_none());
        assert!(state.results.get(&prod).is_some());

        state.record_test(
            host_finished(dev.clone(), Ok(authenticated("dev"))),
            "12:34".into(),
        );
        assert!(apply_refresh(&mut state, || Ok::<_, String>(inventory())));
        assert!(state.results.get(&dev).is_none());
        assert!(state.results.get(&prod).is_none());
    }

    #[test]
    fn footers_advertise_the_test_keys() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        assert!(footer_text(&state).contains("t test"));

        state.set_filter("missing");
        assert!(!footer_text(&state).contains("t test"));

        press(&mut state, KeyCode::Char('a'));
        assert!(footer_text(&state).contains("Ctrl+T test"));
    }

    #[test]
    fn the_status_shows_the_test_in_flight() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        state.testing = Some("dev".into());

        assert!(rendered(&state).contains("Testing dev… (Esc to cancel)"));

        state.status = Some("Saved prod".into());
        let text = rendered(&state);
        assert!(text.contains("Saved prod"), "{text}");
        assert!(!text.contains("Testing dev"), "{text}");
    }

    fn unknown_key(alias: &str, host: &str) -> TestOutcome {
        TestOutcome::UnknownHostKey {
            report: report(
                alias,
                ProbeResult::Failed(crate::probe::Failure::HostKeyUnknown),
            ),
            key: crate::openssh::PresentedKey {
                lines: b"h ssh-ed25519 AAAA\n".to_vec(),
                fingerprints: vec![crate::openssh::KeyFingerprint {
                    host: host.into(),
                    fingerprint: "SHA256:abc".into(),
                    key_type: "ED25519".into(),
                }],
            },
        }
    }

    #[test]
    fn an_unknown_key_opens_the_trust_prompt_with_the_fingerprint() {
        let dev = summary("dev", Ownership::Managed);
        let mut state = test_state(vec![dev.clone()]);

        state.record_test(
            host_finished(dev.clone(), Ok(unknown_key("dev", "|1|salt|hash"))),
            "12:34".into(),
        );

        assert_eq!(state.mode, Mode::ConfirmTrust);
        let backend = TestBackend::new(140, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &state)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("Trust host key"), "{text}");
        assert!(
            text.contains("dev presents an ED25519 key with fingerprint SHA256:abc"),
            "{text}"
        );
        assert!(
            text.contains(
                "Compare it with the fingerprint from the server's administrator or console."
            ),
            "{text}"
        );
        assert!(text.contains("Trust it and test again? y/N"), "{text}");
        assert!(
            text.contains("y trust and test again  n/Esc keep it untrusted"),
            "{text}"
        );
        assert!(state.results.get(&dev).is_none(), "nothing recorded yet");
    }

    #[test]
    fn y_trusts_and_retests_and_returns_to_the_previous_mode() {
        let dev = summary("dev", Ownership::Managed);
        let mut state = test_state(vec![dev.clone()]);
        press(&mut state, KeyCode::Char('a'));
        state.mode = Mode::EditForm;
        let finished = Finished {
            origin: Origin::Form,
            target: TestTarget::Saved("dev".into()),
            outcome: Ok(unknown_key("dev", "dev.example")),
        };
        state.record_test(finished, "12:34".into());
        assert_eq!(state.mode, Mode::ConfirmTrust);

        assert_eq!(press(&mut state, KeyCode::Char('x')), UiAction::None);
        assert_eq!(state.mode, Mode::ConfirmTrust);
        assert_eq!(press(&mut state, KeyCode::Char('y')), UiAction::Trust(true));

        assert_eq!(state.mode, Mode::EditForm);
        let pending = state.trust.take().expect("the key to trust");
        assert_eq!(pending.origin, Origin::Form);
        assert_eq!(pending.target, TestTarget::Saved("dev".into()));
        assert_eq!(pending.key.lines, b"h ssh-ed25519 AAAA\n");
        assert!(state.form.test_result.is_none(), "nothing recorded on y");
    }

    #[test]
    fn n_and_esc_decline_and_mark_the_host_untrusted() {
        for code in [KeyCode::Char('n'), KeyCode::Esc] {
            let dev = summary("dev", Ownership::Managed);
            let mut state = test_state(vec![dev.clone()]);
            state.record_test(
                host_finished(dev.clone(), Ok(unknown_key("dev", "dev.example"))),
                "12:34".into(),
            );

            assert_eq!(press(&mut state, code), UiAction::Trust(false));

            assert_eq!(state.mode, Mode::Browse);
            assert!(state.trust.is_none());
            assert_eq!(state.status.as_deref(), Some("Host key not trusted"));
            let mark = state.results.get(&dev).expect("a recorded result");
            assert!(!mark.passed);
            assert_eq!(mark.line, "failed \u{2014} host key not trusted");
        }
    }

    #[test]
    fn a_changed_key_shows_its_warning_without_a_prompt() {
        let dev = summary("dev", Ownership::Managed);
        let mut state = test_state(vec![dev.clone()]);
        resolve(&mut state, details("dev", Ownership::Managed));
        let home = std::path::PathBuf::from("/home/me");
        let mut changed = report(
            "dev",
            ProbeResult::Failed(crate::probe::Failure::HostKeyChanged { known_hosts: None }),
        );
        changed.explanation = Some(crate::explain::explain(
            &crate::probe::Diagnosis {
                failure: crate::probe::Failure::HostKeyChanged { known_hosts: None },
                raw: Vec::new(),
            },
            &crate::explain::Context {
                alias: "dev",
                operation: crate::explain::Operation::Test,
                user: None,
                hostname: Some("dev.example"),
                port: None,
                identity_files: &[],
                jump_is_known: false,
                home: &home,
                managed_file: None,
                adding: false,
                in_tui: true,
            },
        ));
        let changed = TestOutcome::Done(changed);

        state.record_test(host_finished(dev.clone(), Ok(changed)), "12:34".into());

        assert_eq!(state.mode, Mode::Browse);
        assert!(state.trust.is_none());
        assert!(!state.results.get(&dev).unwrap().passed);
        assert_eq!(
            state.status.as_deref(),
            Some("dev: failed \u{2014} the host key changed, which can mean an attack")
        );
        let text = details_text(&state);
        assert!(text.contains("`ssh-keygen -R dev.example`"), "{text}");
    }

    #[test]
    fn a_form_result_for_a_closed_form_opens_no_prompt() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        let finished = Finished {
            origin: Origin::Form,
            target: TestTarget::Saved("dev".into()),
            outcome: Ok(unknown_key("dev", "dev.example")),
        };
        state.record_test(finished, "12:34".into());
        assert_eq!(state.mode, Mode::Browse);
        assert!(state.trust.is_none());
    }

    #[test]
    fn a_host_test_opens_the_prompt_only_from_browse_mode() {
        let dev = summary("dev", Ownership::Managed);
        let mut state = test_state(vec![dev.clone()]);
        press(&mut state, KeyCode::Char('/'));

        state.record_test(
            host_finished(dev.clone(), Ok(unknown_key("dev", "dev.example"))),
            "12:34".into(),
        );

        assert_eq!(state.mode, Mode::Filter);
        assert!(state.trust.is_none());
        assert_eq!(
            state.status.as_deref(),
            Some("dev: failed \u{2014} host key not trusted (press t to review the host key)")
        );
        type_text(&mut state, "yd");
        assert_eq!(
            state.filter.as_str(),
            "yd",
            "typed-ahead keys were swallowed"
        );
        let mark = state.results.get(&dev).expect("a recorded result");
        assert!(!mark.passed);
        assert_eq!(mark.line, "failed \u{2014} host key not trusted");

        // From an open form, too, the host's result is recorded without a prompt.
        let mut state = test_state(vec![dev.clone()]);
        press(&mut state, KeyCode::Char('a'));
        state.record_test(
            host_finished(dev.clone(), Ok(unknown_key("dev", "dev.example"))),
            "12:34".into(),
        );
        assert_eq!(state.mode, Mode::AddForm);
        assert!(state.trust.is_none());
        press(&mut state, KeyCode::Char('y'));
        assert_eq!(state.form.values[0].as_str(), "y");
        assert!(state.results.get(&dev).is_some());
    }

    #[test]
    fn a_form_test_opens_the_prompt_only_in_its_open_form() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        let finished = || Finished {
            origin: Origin::Form,
            target: TestTarget::Add(Host::new("new", "h", None, None, None, None).unwrap()),
            outcome: Ok(unknown_key("new", "h")),
        };
        press(&mut state, KeyCode::Char('/'));
        state.record_test(finished(), "12:34".into());
        assert_eq!(state.mode, Mode::Filter);
        assert!(state.trust.is_none());

        press(&mut state, KeyCode::Esc);
        press(&mut state, KeyCode::Char('a'));
        state.record_test(finished(), "12:34".into());
        assert_eq!(state.mode, Mode::ConfirmTrust);
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

    #[test]
    fn editing_or_pasting_into_a_form_cancels_its_test() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        press(&mut state, KeyCode::Char('a'));
        state.testing = Some("new".into());

        assert_eq!(press(&mut state, KeyCode::Char('x')), UiAction::FormEdited);
        assert_eq!(state.handle_paste("y"), UiAction::FormEdited);
        assert_eq!(press(&mut state, KeyCode::Left), UiAction::None, "no edit");
        assert_eq!(press(&mut state, KeyCode::Tab), UiAction::None);
        assert_eq!(state.form.values[0].as_str(), "xy");
    }

    fn explanation(headline: &str, raw: &[&str], always_raw: bool) -> crate::explain::Explanation {
        crate::explain::Explanation {
            headline: headline.into(),
            facts: vec![(crate::explain::Label::User, vec!["u".into()])],
            next: vec!["try this".into()],
            raw: raw.iter().map(|line| (*line).into()).collect(),
            always_raw,
        }
    }

    fn explained(alias: &str, headline: &str, raw: &[&str], always_raw: bool) -> TestOutcome {
        let mut failed = report(
            alias,
            ProbeResult::Failed(crate::probe::Failure::HostKeyChanged { known_hosts: None }),
        );
        failed.explanation = Some(explanation(headline, raw, always_raw));
        TestOutcome::Done(failed)
    }

    fn exited_255(
        alias: &str,
        explanation: Option<crate::explain::Explanation>,
    ) -> ConnectionOutcome {
        use std::os::unix::process::ExitStatusExt;
        ConnectionOutcome::Exited {
            alias: alias.into(),
            status: std::process::ExitStatus::from_raw(255 << 8),
            explanation,
        }
    }

    #[test]
    fn a_failed_test_shows_its_explanation_in_the_details() {
        let dev = summary("dev", Ownership::Managed);
        let mut state = test_state(vec![dev.clone()]);
        resolve(&mut state, details("dev", Ownership::Managed));

        state.record_test(
            host_finished(
                dev,
                Ok(explained(
                    "dev",
                    "the server refused the login",
                    &["denied"],
                    false,
                )),
            ),
            "12:34".into(),
        );

        let text = details_text(&state);
        let expected = "Connection test: failed \u{2014} the server refused the login at 12:34\n  user: u\n  next: try this";
        assert!(text.ends_with(expected), "{text}");
        assert!(!text.contains("dev: failed"), "{text}");
        assert!(!text.contains("ssh said"), "{text}");
    }

    #[test]
    fn o_unfolds_the_ssh_output_and_the_footer_offers_it_only_when_there_is_some() {
        let dev = summary("dev", Ownership::Managed);
        let prod = summary("prod", Ownership::Managed);
        let mut state = test_state(vec![dev.clone(), prod.clone()]);
        resolve(&mut state, details("dev", Ownership::Managed));
        assert!(!footer_text(&state).contains("o ssh output"));

        state.record_test(
            host_finished(dev.clone(), Ok(explained("dev", "refused", &[], false))),
            "12:34".into(),
        );
        assert!(
            !footer_text(&state).contains("o ssh output"),
            "no raw lines"
        );

        state.record_test(
            host_finished(
                dev,
                Ok(explained("dev", "refused", &["Permission denied"], false)),
            ),
            "12:35".into(),
        );
        assert!(footer_text(&state).contains("o ssh output"));
        assert!(!details_text(&state).contains("ssh said:"));

        press(&mut state, KeyCode::Char('o'));
        let text = details_text(&state);
        assert!(
            text.contains("  ssh said:\n    Permission denied"),
            "{text}"
        );
        press(&mut state, KeyCode::Char('o'));
        assert!(!details_text(&state).contains("ssh said:"));

        press(&mut state, KeyCode::Char('o'));
        assert!(state.show_raw);
        press(&mut state, KeyCode::Char('j'));
        assert!(!state.show_raw, "moving the selection hides it");
        assert!(
            !footer_text(&state).contains("o ssh output"),
            "prod has none"
        );

        // `o` does nothing where there is no output to show.
        press(&mut state, KeyCode::Char('o'));
        assert!(!state.show_raw);

        // Opening or closing a form hides it too.
        press(&mut state, KeyCode::Char('k'));
        press(&mut state, KeyCode::Char('o'));
        press(&mut state, KeyCode::Char('a'));
        assert!(!state.show_raw);
        press(&mut state, KeyCode::Esc);
        assert!(!state.show_raw);
    }

    #[test]
    fn a_new_result_or_a_form_edit_folds_the_output_again() {
        let dev = summary("dev", Ownership::Managed);
        let mut state = test_state(vec![dev.clone()]);
        resolve(&mut state, details("dev", Ownership::Managed));
        let failed = || Ok(explained("dev", "refused", &["Permission denied"], false));

        state.record_test(host_finished(dev.clone(), failed()), "12:34".into());
        press(&mut state, KeyCode::Char('o'));
        assert!(state.show_raw);
        state.record_test(host_finished(dev, failed()), "12:35".into());
        assert!(!state.show_raw, "a new result of the selected host");

        press(&mut state, KeyCode::Char('a'));
        let form_finished = || Finished {
            origin: Origin::Form,
            target: TestTarget::Add(Host::new("new", "h", None, None, None, None).unwrap()),
            outcome: failed(),
        };
        state.record_test(form_finished(), "12:36".into());
        state.handle_key(ctrl('o'));
        assert!(state.show_raw);
        state.record_test(form_finished(), "12:37".into());
        assert!(!state.show_raw, "a new result of the form");

        state.handle_key(ctrl('o'));
        assert!(state.show_raw);
        press(&mut state, KeyCode::Char('x'));
        assert!(!state.show_raw, "typing in a field");

        state.record_test(form_finished(), "12:38".into());
        state.handle_key(ctrl('o'));
        state.handle_paste("y");
        assert!(!state.show_raw, "pasting into a field");
    }

    #[test]
    fn output_that_is_always_shown_is_not_offered_as_a_fold() {
        let dev = summary("dev", Ownership::Managed);
        let mut state = test_state(vec![dev.clone()]);
        resolve(&mut state, details("dev", Ownership::Managed));

        state.record_test(
            host_finished(
                dev,
                Ok(explained("dev", "unexplained", &["odd output"], true)),
            ),
            "12:34".into(),
        );

        assert!(details_text(&state).contains("  ssh said:\n    odd output"));
        assert!(!footer_text(&state).contains("o ssh output"));
    }

    #[test]
    fn a_failed_connection_adds_a_last_connection_section() {
        let dev = summary("dev", Ownership::Managed);
        let prod = summary("prod", Ownership::Managed);
        let inventory = || HostInventory {
            hosts: vec![
                summary("dev", Ownership::Managed),
                summary("prod", Ownership::Managed),
            ],
            warnings: Vec::new(),
        };
        let mut state = test_state(inventory().hosts);
        resolve(&mut state, details("dev", Ownership::Managed));
        let failed = || {
            exited_255(
                "dev",
                Some(explanation(
                    "the connection timed out",
                    &["timed out"],
                    false,
                )),
            )
        };

        state.finish_connect("dev", Ok(failed()));

        assert_eq!(
            state.status.as_deref(),
            Some("dev: failed \u{2014} the connection timed out")
        );
        let text = details_text(&state);
        assert!(
            text.ends_with("Last connection: failed \u{2014} the connection timed out\n  user: u\n  next: try this"),
            "{text}"
        );
        assert!(footer_text(&state).contains("o ssh output"));
        press(&mut state, KeyCode::Char('o'));
        assert!(details_text(&state).contains("  ssh said:\n    timed out"));

        // A later clean connection replaces it.
        state.finish_connect("dev", Ok(exited_255("dev", None)));
        assert!(state.connections.get(&dev).is_none());

        state.finish_connect("dev", Ok(failed()));
        complete_mutation(
            &mut state,
            Some("dev"),
            &["DEV"],
            "Saved dev".into(),
            || Ok::<_, String>(inventory()),
        );
        assert!(state.connections.get(&dev).is_none(), "edit or removal");

        state.finish_connect("dev", Ok(failed()));
        state.finish_connect(
            "prod",
            Ok(exited_255("prod", Some(explanation("x", &[], false)))),
        );
        assert!(state.connections.get(&prod).is_some());
        assert!(apply_refresh(&mut state, || Ok::<_, String>(inventory())));
        assert!(state.connections.get(&dev).is_none(), "refresh");
        assert!(state.connections.get(&prod).is_none(), "refresh");
    }

    #[test]
    fn local_time_is_hours_and_minutes() {
        let time = local_time();
        let (hours, minutes) = time.split_once(':').unwrap();
        assert_eq!((hours.len(), minutes.len()), (2, 2), "{time}");
        assert!(hours.parse::<u8>().unwrap() < 24);
        assert!(minutes.parse::<u8>().unwrap() < 60);
    }

    #[test]
    fn the_migrate_screen_waits_for_its_plan_and_esc_cancels_it() {
        let plan = || {
            adopt::MigrationPlan::for_tests(
                adopt::Direction::Adopt,
                vec![adopt::Candidate {
                    alias: "web".into(),
                    source: "/home/test/.ssh/config".into(),
                    reason: None,
                }],
            )
        };
        let home = std::path::Path::new("/home/test");
        let mut state = test_state(vec![summary("web", Ownership::External)]);
        state.mode = Mode::Migrate;
        assert_eq!(press(&mut state, KeyCode::Char('a')), UiAction::None);
        assert_eq!(state.mode, Mode::Migrate);
        state.finish_plan(Ok(plan()), "web", home);
        assert!(state.migrate.is_some());

        let mut state = test_state(vec![summary("web", Ownership::External)]);
        state.mode = Mode::Migrate;
        assert_eq!(press(&mut state, KeyCode::Esc), UiAction::CancelPlan);
        assert_eq!(state.mode, Mode::Browse);
        state.finish_plan(Ok(plan()), "web", home);
        assert!(state.migrate.is_none());
    }

    #[test]
    fn m_opens_migration_for_the_selected_host() {
        let mut state = test_state(vec![
            summary("web", Ownership::External),
            summary("dev", Ownership::Managed),
        ]);
        assert_eq!(
            press(&mut state, KeyCode::Char('m')),
            UiAction::OpenMigrate {
                direction: adopt::Direction::Adopt,
                preselect: "web".into(),
            }
        );
        press(&mut state, KeyCode::Down);
        assert_eq!(
            press(&mut state, KeyCode::Char('m')),
            UiAction::OpenMigrate {
                direction: adopt::Direction::Export,
                preselect: "dev".into(),
            }
        );
        assert!(footer_text(&state).contains("a add  m export  x remove"));
        press(&mut state, KeyCode::Up);
        assert!(footer_text(&state).contains("a add  m adopt  r refresh"));

        let mut empty = test_state(Vec::new());
        assert_eq!(press(&mut empty, KeyCode::Char('m')), UiAction::None);
        assert!(!footer_text(&empty).contains("m adopt"));
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn proxy_jump_form(hosts: Vec<HostSummary>) -> UiState {
        let mut state = test_state(hosts);
        press(&mut state, KeyCode::Char('a'));
        for _ in 0..PROXY_JUMP {
            press(&mut state, KeyCode::Tab);
        }
        state
    }

    fn listed(state: &UiState) -> Vec<&str> {
        state.form.suggestions.as_ref().map_or(Vec::new(), |list| {
            list.items.iter().map(|item| item.alias.as_str()).collect()
        })
    }

    fn jump_hosts() -> Vec<HostSummary> {
        let mut eu = summary("bastion-eu", Ownership::External);
        eu.source = PathBuf::from("/home/test/.ssh/config");
        vec![
            summary("bastion", Ownership::Managed),
            eu,
            summary("web", Ownership::Managed),
        ]
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
        use crate::identity::{Availability, InventoryFile, MissingReason};
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
    fn identities_replace_the_identity_file_row_in_the_details() {
        let mut details = details("web", Ownership::Managed);
        let plain = detail_text(&details, Path::new("/home/test"));
        assert!(plain.contains("IdentityFile ~/.ssh/id_ed25519"), "{plain}");
        details.identities = Some(crate::identity::HostIdentities {
            agent: crate::identity::Agent::Disabled,
            defaults: false,
            identities: vec![crate::identity::Identity {
                configured: "~/.ssh/id_ed25519".into(),
                path: None,
                public_key: None,
                availability: crate::identity::Availability::KeyFile {
                    public_key_found: false,
                },
            }],
            identities_only: false,
            also_offered: Vec::new(),
            pkcs11_provider: None,
        });
        let text = detail_text(&details, Path::new("/home/test"));
        assert_eq!(text.matches("~/.ssh/id_ed25519").count(), 1, "{text}");
        assert!(
            text.contains(
                "Agent        none (IdentityAgent none)\n\
                 IdentityFile ~/.ssh/id_ed25519  ✓ key file (agent not checked: no public key file)\n"
            ),
            "{text}"
        );
        assert!(text.contains("ProxyJump"), "{text}");
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
        });
        assert_eq!(listed(&state), ["bastion"]);
        assert_eq!(suggestion(&state).selected, 0);
        state.replace_hosts(HostInventory {
            hosts: vec![summary("web", Ownership::Managed)],
            warnings: Vec::new(),
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
    fn footer_shows_suggestion_keys_while_open() {
        let mut state = proxy_jump_form(jump_hosts());
        type_text(&mut state, "bas");
        assert_eq!(footer_text(&state), "↑↓ choose  Enter pick  Esc close");
    }

    #[test]
    fn footer_advertises_ctrl_space_on_proxy_jump() {
        let state = proxy_jump_form(jump_hosts());
        assert!(
            footer_text(&state).contains("Ctrl+Space suggest  Esc cancel"),
            "{}",
            footer_text(&state)
        );
        let state = form_with_active_field(0);
        assert!(!footer_text(&state).contains("Ctrl+Space"));
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
    fn picking_after_multibyte_text_keeps_earlier_hops() {
        let mut state = proxy_jump_form(vec![summary("bastion", Ownership::Managed)]);
        type_text(&mut state, "café,bas");
        assert_eq!(listed(&state), ["bastion"]);
        press(&mut state, KeyCode::Enter);
        let input = &state.form.values[PROXY_JUMP];
        assert_eq!(input.as_str(), "café,bastion");
        assert_eq!(input.cursor, input.as_str().len());
    }

    fn shop_list(target: Target) -> ContainerList {
        ContainerList {
            target,
            listing: crate::container::Listing {
                containers: ["web", "db"]
                    .into_iter()
                    .map(|service| crate::container::Container {
                        id: format!("{service}-id"),
                        name: format!("shop-{service}-1"),
                        image: format!("{service}:latest"),
                        command: "run".into(),
                        running_for: "3 hours ago".into(),
                        status: "Up 3 hours".into(),
                        ports: String::new(),
                        networks: "shop_default".into(),
                        mounts: String::new(),
                        project: Some("shop".into()),
                        service: Some(service.into()),
                        number: Some(1),
                        working_dir: None,
                    })
                    .collect(),
                skipped: 0,
            },
            via: None,
            note: None,
        }
    }

    fn char(state: &mut UiState, character: char) -> UiAction {
        press(state, KeyCode::Char(character))
    }

    fn dev_listed(state: &mut UiState) {
        let dev = Target::Host("dev".into());
        state.containers.finish(dev.clone(), Ok(shop_list(dev)));
    }

    fn tunnels_state() -> UiState {
        use crate::tunnel::{Forward, Kind, Tunnel, TunnelReport, TunnelStatus};
        let mut state = test_state(vec![
            summary("dev", Ownership::Managed),
            summary("web", Ownership::External),
        ]);
        state.tunnels = tunnels::tests::screen_with(vec![TunnelReport {
            tunnel: Tunnel::new("db", "dev", Forward::parse(Kind::Dynamic, "1080").unwrap())
                .unwrap(),
            status: TunnelStatus::Running { pid: 7, started: 0 },
            warnings: Vec::new(),
        }]);
        state
    }

    #[test]
    fn digits_tab_and_the_aliases_switch_tabs() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        assert_eq!(state.tab, Tab::Hosts);
        assert_eq!(char(&mut state, '2'), UiAction::None);
        assert_eq!(state.tab, Tab::Tunnels);
        assert_eq!(
            char(&mut state, '3'),
            UiAction::Containers(PanelAction::List(Target::Local)),
            "the Containers tab lists its target when first shown"
        );
        assert_eq!(state.tab, Tab::Containers);
        assert_eq!(char(&mut state, '1'), UiAction::None);
        assert_eq!(state.tab, Tab::Hosts);

        for expected in [Tab::Tunnels, Tab::Containers, Tab::Hosts] {
            press(&mut state, KeyCode::Tab);
            assert_eq!(state.tab, expected);
        }
        for expected in [Tab::Containers, Tab::Tunnels, Tab::Hosts] {
            press(&mut state, KeyCode::BackTab);
            assert_eq!(state.tab, expected);
        }

        char(&mut state, 'T');
        assert_eq!(state.tab, Tab::Tunnels);
        assert_eq!(
            state.handle_key(KeyEvent::new(KeyCode::Char('T'), KeyModifiers::SHIFT)),
            UiAction::None
        );
        assert_eq!(state.tab, Tab::Tunnels, "Shift+T is T");
        char(&mut state, 'c');
        assert_eq!(state.tab, Tab::Containers);
        assert_eq!(
            state.containers.target(),
            Target::Local,
            "c off the Hosts tab keeps the target"
        );
        char(&mut state, 'T');
        assert_eq!(state.tab, Tab::Tunnels);
        assert_eq!(
            state.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            UiAction::Tunnels(TunnelAction::None),
            "Ctrl+C is the main loop's"
        );
        assert_eq!(state.tab, Tab::Tunnels);
    }

    #[test]
    fn t_still_tests_on_the_hosts_tab() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        assert!(matches!(char(&mut state, 't'), UiAction::Test { .. }));
        assert_eq!(state.tab, Tab::Hosts);
    }

    #[test]
    fn c_targets_the_selected_host_and_lists_it_once() {
        let mut state = test_state(vec![
            summary("dev", Ownership::Managed),
            summary("web", Ownership::External),
        ]);
        press(&mut state, KeyCode::Down);
        let web = Target::Host("web".into());
        assert_eq!(
            char(&mut state, 'c'),
            UiAction::Containers(PanelAction::List(web.clone()))
        );
        assert_eq!(state.tab, Tab::Containers);
        assert_eq!(state.containers.target(), web);
        state.containers.set_listing(web.clone());
        char(&mut state, '1');
        assert_eq!(
            char(&mut state, 'c'),
            UiAction::Containers(PanelAction::None),
            "a listing already running or kept is not started again"
        );
        char(&mut state, '1');
        assert_eq!(
            char(&mut state, '3'),
            UiAction::Containers(PanelAction::None)
        );

        let mut empty = test_state(Vec::new());
        assert_eq!(
            char(&mut empty, 'c'),
            UiAction::Containers(PanelAction::List(Target::Local)),
            "without a host, c only shows the tab"
        );
        assert_eq!(empty.containers.target(), Target::Local);
    }

    #[test]
    fn nothing_is_listed_before_the_containers_tab_opens() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        for code in [KeyCode::Down, KeyCode::Char('2'), KeyCode::Char('1')] {
            assert!(!matches!(
                press(&mut state, code),
                UiAction::Containers(PanelAction::List(_))
            ));
        }
        assert!(!rendered(&state).contains("Target:"));
    }

    #[test]
    fn digits_are_typed_in_every_text_input() {
        let mut state = tunnels_state();
        // The hosts filter.
        char(&mut state, '/');
        char(&mut state, '2');
        assert_eq!((state.tab, state.filter.as_str()), (Tab::Hosts, "2"));
        press(&mut state, KeyCode::Esc);
        // The host form, where Tab still moves between fields.
        char(&mut state, 'a');
        char(&mut state, '3');
        press(&mut state, KeyCode::Tab);
        assert_eq!((state.tab, state.form.active), (Tab::Hosts, 1));
        assert_eq!(state.form.values[0].as_str(), "3");
        press(&mut state, KeyCode::BackTab);
        assert_eq!((state.tab, state.form.active), (Tab::Hosts, 0));
        press(&mut state, KeyCode::Esc);
        // The tunnels filter and form.
        char(&mut state, '2');
        char(&mut state, '/');
        char(&mut state, '1');
        assert_eq!(state.tab, Tab::Tunnels);
        assert!(!state.tunnels.is_browsing());
        press(&mut state, KeyCode::Esc);
        char(&mut state, 'a');
        char(&mut state, '1');
        press(&mut state, KeyCode::Tab);
        assert_eq!(state.tab, Tab::Tunnels);
        assert!(state.tunnels.footer().contains("Ctrl+Space suggest"));
        press(&mut state, KeyCode::Esc);
        // The containers filter, user and target picker.
        char(&mut state, '3');
        dev_listed(&mut state);
        for opener in ['/', 'u', 'h'] {
            char(&mut state, opener);
            char(&mut state, '1');
            press(&mut state, KeyCode::Tab);
            assert_eq!(state.tab, Tab::Containers, "after {opener}");
            press(&mut state, KeyCode::Esc);
        }
    }

    #[test]
    fn questions_ignore_digits_and_tab() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        for mode in [Mode::ConfirmRemove, Mode::ConfirmTrust, Mode::Migrate] {
            state.mode = mode.clone();
            for code in [KeyCode::Char('2'), KeyCode::Char('3'), KeyCode::Tab] {
                press(&mut state, code);
                assert_eq!(state.tab, Tab::Hosts, "{mode:?}");
            }
        }
        // The tunnel form's Kind row takes no text but is part of the form.
        let mut state = tunnels_state();
        char(&mut state, '2');
        char(&mut state, 'a');
        press(&mut state, KeyCode::Down);
        press(&mut state, KeyCode::Down);
        for code in [KeyCode::Char('1'), KeyCode::Char('3'), KeyCode::Tab] {
            press(&mut state, code);
            assert_eq!(state.tab, Tab::Tunnels);
        }
        assert!(!state.tunnels.is_browsing());
    }

    #[test]
    fn each_tab_keeps_its_selection_and_filter() {
        let mut state = tunnels_state();
        press(&mut state, KeyCode::Down);
        state.set_filter("w");
        char(&mut state, '2');
        char(&mut state, '/');
        state.handle_paste("d");
        press(&mut state, KeyCode::Enter);
        char(&mut state, '3');
        dev_listed(&mut state);
        state.containers.show(Target::Host("dev".into()));
        dev_listed(&mut state);
        char(&mut state, 'j');
        let details = state.containers.details();

        char(&mut state, '1');
        assert_eq!(state.filter.as_str(), "w");
        assert_eq!(state.selected_alias(), Some("web"));
        char(&mut state, '2');
        assert!(render_text_at(&state, 100).contains("Tunnels /d"));
        char(&mut state, '3');
        assert_eq!(state.containers.details(), details);
        assert_eq!(state.containers.target(), Target::Host("dev".into()));
    }

    fn render_text_at(state: &UiState, width: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
        terminal.draw(|frame| render(frame, state)).unwrap();
        buffer_text(terminal.backend().buffer())
    }

    #[test]
    fn refresh_quit_and_esc_belong_to_the_active_tab() {
        let mut state = tunnels_state();
        assert_eq!(char(&mut state, 'r'), UiAction::Refresh);
        assert_eq!(press(&mut state, KeyCode::F(5)), UiAction::Refresh);
        char(&mut state, '2');
        assert_eq!(
            char(&mut state, 'r'),
            UiAction::Tunnels(TunnelAction::Refresh)
        );
        assert_eq!(
            press(&mut state, KeyCode::F(5)),
            UiAction::Tunnels(TunnelAction::Refresh)
        );
        assert_eq!(char(&mut state, 'q'), UiAction::Quit);
        press(&mut state, KeyCode::Esc);
        assert_eq!(state.tab, Tab::Tunnels, "Esc never switches");
        char(&mut state, '3');
        state
            .containers
            .finish(Target::Local, Ok(shop_list(Target::Local)));
        assert_eq!(
            char(&mut state, 'r'),
            UiAction::Containers(PanelAction::List(Target::Local))
        );
        state
            .containers
            .finish(Target::Local, Ok(shop_list(Target::Local)));
        assert_eq!(
            press(&mut state, KeyCode::F(5)),
            UiAction::Containers(PanelAction::List(Target::Local))
        );
        state
            .containers
            .finish(Target::Local, Ok(shop_list(Target::Local)));
        press(&mut state, KeyCode::Esc);
        assert_eq!(state.tab, Tab::Containers, "Esc never switches");
        assert_eq!(char(&mut state, 'q'), UiAction::Quit);
        char(&mut state, '1');
        assert_eq!(char(&mut state, 'q'), UiAction::Quit);
    }

    #[test]
    fn a_trust_prompt_waits_for_the_hosts_tab() {
        let dev = summary("dev", Ownership::Managed);
        let mut state = test_state(vec![dev.clone()]);
        char(&mut state, '2');
        state.record_test(
            Finished {
                origin: Origin::Host(dev.clone()),
                target: TestTarget::Saved("dev".into()),
                outcome: Ok(unknown_key("dev", "dev.example")),
            },
            "12:00".into(),
        );
        assert_eq!(state.mode, Mode::Browse, "no prompt on another tab");
        assert!(state.trust.is_none());
        assert!(state.results.get(&dev).is_some_and(|mark| !mark.passed));
    }

    #[test]
    fn the_tab_bar_highlights_the_active_tab_and_never_wraps() {
        let line = tab_bar(Tab::Tunnels, 100);
        assert_eq!(
            line.to_string(),
            " 1 Hosts \u{2502} 2 Tunnels \u{2502} 3 Containers"
        );
        let active = line
            .spans
            .iter()
            .find(|span| span.content == "2 Tunnels")
            .unwrap();
        assert!(active.style.add_modifier.contains(Modifier::REVERSED));
        assert_eq!(
            tab_bar(Tab::Containers, 30).to_string(),
            " 1 \u{2502} 2 \u{2502} 3 Containers"
        );
        assert_eq!(tab_bar(Tab::Containers, 14).to_string(), " 3 Containers");

        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        char(&mut state, '3');
        for width in [40, 30, 20, 12] {
            let text = render_text_at(&state, width);
            let first = text.lines().next().unwrap();
            assert!(first.contains("3 Contain"), "{width}: {first}");
            if width >= 40 {
                assert!(first.contains("1 Hosts"), "{first}");
            }
        }
    }

    #[test]
    fn footers_name_the_tab_keys() {
        let state = test_state(vec![summary("dev", Ownership::Managed)]);
        let footer = footer_text(&state);
        assert!(
            footer.contains("c containers  Tab/1-3 tabs  q quit"),
            "{footer}"
        );
        assert!(!footer.contains("T tunnels"));
        let state = test_state(Vec::new());
        assert!(footer_text(&state).contains("Tab/1-3 tabs"));
        let state = tunnels_state();
        assert!(state.tunnels.footer().contains("Tab/1-3 tabs  q quit"));
        assert!(state.containers.footer().contains("h target"));
    }

    #[test]
    fn the_containers_tab_shows_the_target_list_and_details() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        char(&mut state, 'c');
        dev_listed(&mut state);
        let text = rendered(&state);
        assert!(
            text.contains("Target: dev   h change  L this machine"),
            "{text}"
        );
        assert!(text.contains("Containers /"), "{text}");
        assert!(text.contains("shop"), "{text}");
        assert!(text.contains("> db"), "{text}");
        assert!(text.contains("image: db:latest"), "{text}");
        assert!(text.contains("user: image default"), "{text}");
        assert!(!text.contains("Hosts /"), "{text}");
    }

    #[test]
    fn h_picks_the_containers_target() {
        let mut state = test_state(vec![
            summary("dev", Ownership::Managed),
            summary("web", Ownership::External),
        ]);
        char(&mut state, '3');
        assert_eq!(char(&mut state, 'h'), UiAction::None);
        assert!(state.containers.picker_open());
        let text = rendered(&state);
        assert!(text.contains("this machine"), "{text}");
        assert!(text.contains("web"), "{text}");
        assert!(text.contains("managed"), "{text}");
        assert!(footer_text_of(&state).starts_with("type to search"));
        state.handle_paste("we");
        assert_eq!(
            press(&mut state, KeyCode::Enter),
            UiAction::Containers(PanelAction::List(Target::Host("web".into())))
        );
        assert!(!state.containers.picker_open());
        assert_eq!(state.containers.target(), Target::Host("web".into()));

        char(&mut state, 'h');
        type_text(&mut state, "dev");
        press(&mut state, KeyCode::Esc);
        assert!(!state.containers.picker_open());
        assert_eq!(
            state.containers.target(),
            Target::Host("web".into()),
            "Esc leaves the target alone"
        );
        char(&mut state, 'h');
        type_text(&mut state, "this");
        assert_eq!(
            press(&mut state, KeyCode::Enter),
            UiAction::Containers(PanelAction::List(Target::Local))
        );
    }

    fn footer_text_of(state: &UiState) -> String {
        state.containers.footer()
    }

    #[test]
    fn paste_goes_to_the_containers_user_while_editing() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        char(&mut state, 'c');
        dev_listed(&mut state);
        state.handle_paste("ignored");
        char(&mut state, 'u');
        state.handle_paste("root\n");
        press(&mut state, KeyCode::Enter);
        assert!(
            state.containers.details().unwrap().ends_with("user: root"),
            "{:?}",
            state.containers.details()
        );
    }

    #[test]
    fn container_results_show_on_the_containers_tab() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        char(&mut state, 'c');
        dev_listed(&mut state);
        state
            .containers
            .set_status(Some("Shell on dev ended".into()));
        assert!(rendered(&state).contains("Shell on dev ended"));
        char(&mut state, 'j');
        assert!(!rendered(&state).contains("Shell on dev ended"));
        char(&mut state, '1');
        assert!(state.status.is_none());
    }

    #[test]
    fn a_narrow_containers_tab_wraps_the_fix_command_in_full() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        char(&mut state, 'c');
        let offer = crate::app::Offer::JoinGroup {
            user: "deploy".into(),
            socket: "/var/run/docker.sock".into(),
        };
        let command = crate::container::remote_command(&offer.words());
        let issue = crate::app::DockerIssue {
            target: Target::Host("dev".into()),
            problem: crate::container::DockerProblem::PermissionDenied,
            headline: "deploy cannot use Docker on dev.".into(),
            next: vec![format!("Add deploy to the docker group: {command}")],
            offer: Some(offer),
            stderr: String::new(),
        };
        state.containers.finish(
            Target::Host("dev".into()),
            Err(crate::app::ContainerError::from(issue)),
        );
        let backend = TestBackend::new(40, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, &state)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        let panel: String = (0..buffer.area.height)
            .map(|y| (1..19).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect();
        let squashed = |text: &str| text.split_whitespace().collect::<String>();
        assert!(
            squashed(&panel).contains(&squashed(&command)),
            "{}",
            buffer_text(&buffer)
        );
    }

    #[test]
    fn the_tunnels_tab_takes_keys_and_draws() {
        let mut state = tunnels_state();
        char(&mut state, '2');
        assert_eq!(
            char(&mut state, 's'),
            UiAction::Tunnels(TunnelAction::Stop("db".into()))
        );
        let text = render_text_at(&state, 100);
        assert!(text.contains("Tunnels /"), "{text}");
        assert!(text.contains("socks 1080"), "{text}");
        assert!(text.contains("running"), "{text}");
        assert!(text.contains("keep running after q"), "{text}");
        assert!(text.contains("Tab/1-3 tabs"), "{text}");
        assert_eq!(char(&mut state, 'T'), UiAction::None);
        assert_eq!(state.tab, Tab::Tunnels);
    }
}
