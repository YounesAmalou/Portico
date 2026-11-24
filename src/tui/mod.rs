use std::{
    io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError},
    },
    thread,
    time::{Duration, Instant},
};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame, Terminal,
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};

mod containers;
mod dropdown;
mod events;
mod form;
mod hangup;
mod hosts;
mod identities;
mod init;
mod lister;
mod migrate;
mod planner;
mod settings;
mod terminal;
mod tester;
mod text_input;
mod tunnels;
mod watch;

use crate::{
    app::{App, AppError, HostDetails, HostInventory, HostSummary, Ownership, TestTarget},
    backend::{AUTOMATIC, BackendId, Target},
    desktop::{
        DesktopEnv,
        status::{Status, status},
    },
    domain::adopt,
    domain::config_error::ConfigRejection,
    domain::discovery::{DiscoveryWarning, Input},
    domain::explain::Explanation,
    domain::host::{Change, Host, HostChanges},
    domain::identity::IdentityInventory,
    domain::managed::TransactionError,
    domain::settings::{Key, SettingsError, Tabs},
    domain::suggest,
    platform::paths::tilde,
};
use containers::{
    ContainerTabs, PanelAction, fix_status, render_containers, shell_status, show_fix,
};
use events::{Step, TerminalEvents};
use form::{FORM_FIELDS, FormState, IDENTITY_FILE, PROXY_JUMP};
use hangup::HangupWatch;
use hosts::{DetailsCache, PendingTrust, PerHost, TestMark, details_for_selected, render_hosts};
use identities::Indexer;
use init::confirm_initialization;
use lister::Lister;
use migrate::MigrateState;
use planner::Planner;
use settings::{DesktopRun, OverlayAction, SettingsOverlay};
use terminal::{PanicHook, Signals, TerminalGuard, TerminalSession, suspended};
use tester::{Job, Origin, Tester};
pub(crate) use text_input::TextInput;
use thiserror::Error;
use tunnels::{Starter, TunnelAction, TunnelsScreen};
use watch::ConfigWatch;

/// Longest quiet period after a selection change before the host is resolved.
const RESOLVE_DEBOUNCE: Duration = Duration::from_millis(60);
/// How often background work is looked at while a worker runs, so its result appears promptly.
const BUSY_TICK: Duration = Duration::from_millis(25);
/// How often background work is looked at otherwise.
const IDLE_TICK: Duration = Duration::from_millis(100);

#[derive(Debug, Error)]
pub enum TuiError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    App(#[from] AppError),
    #[error("child process failed: {0}")]
    Child(String),
}

pub fn run(app: &App, desktop: &DesktopEnv) -> Result<(), TuiError> {
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
    let mut events = TerminalEvents::terminal();
    let plan = app.init_plan()?;
    if plan.requires_confirmation()
        && !confirm_initialization(&mut terminal, &mut events, &plan, terminated)?
    {
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

    browse(app, desktop, &mut terminal, &mut events, &signals, &session)
}

fn browse<B: Backend<Error = io::Error>>(
    app: &App,
    desktop: &DesktopEnv,
    terminal: &mut Terminal<B>,
    events: &mut TerminalEvents,
    signals: &Signals,
    session: &TerminalSession,
) -> Result<(), TuiError> {
    thread::scope(|scope| {
        let workers = Workers::spawn(scope, app);
        browse_hosts(app, desktop, terminal, events, signals, session, workers)
    })
}

fn browse_hosts<B: Backend<Error = io::Error>>(
    app: &App,
    desktop: &DesktopEnv,
    terminal: &mut Terminal<B>,
    events: &mut TerminalEvents,
    signals: &Signals,
    session: &TerminalSession,
    mut workers: Workers,
) -> Result<(), TuiError> {
    let terminated = signals.flag();
    let inventory = app.hosts()?;
    let mut watch = ConfigWatch::new(&inventory.inputs, Instant::now());
    let mut state = UiState::new(inventory, session.keyboard_enhancement());
    state.home = app.home().to_owned();
    state.settings_path = app.settings_path().to_owned();
    match app.settings() {
        Ok(settings) => state.apply_tabs(settings.tabs),
        Err(error) => state.status = Some(settings_problem(&error, &state.home)),
    }
    sync_details(&mut state, &mut workers.resolver, false);
    let mut redraw = true;

    loop {
        if terminated.load(Ordering::Relaxed) {
            return Ok(());
        }
        if redraw {
            terminal.draw(|frame| render(frame, &state))?;
            redraw = false;
        }
        let every = if workers.busy() { BUSY_TICK } else { IDLE_TICK };
        let event = match events.next(every)? {
            Step::Input(event) => event,
            Step::Closed => return Ok(()),
            Step::Tick(now) => {
                redraw |= on_tick(app, &mut state, &mut workers, &mut watch, now);
                continue;
            }
        };
        if matches!(event, Event::Resize(_, _)) {
            redraw = true;
            continue;
        }
        if let Event::Paste(text) = event {
            if state.handle_paste(&text) == UiAction::FormEdited {
                workers
                    .tester
                    .cancel_if(|origin, _| *origin == Origin::Form);
            }
            sync_details(&mut state, &mut workers.resolver, false);
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
            UiAction::Tunnels(action) => {
                run_tunnel_action(app, &mut state, &mut workers.starter, action)
            }
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
            UiAction::ListContainers(lists) => {
                for (backend, target) in lists {
                    workers
                        .lister(backend)
                        .list(state.containers.panel_mut(backend), target);
                }
            }
            UiAction::Containers(backend, PanelAction::List(target)) => {
                workers
                    .lister(backend)
                    .list(state.containers.panel_mut(backend), target);
            }
            UiAction::Containers(backend, PanelAction::CancelList) => {
                workers.lister(backend).cancel();
            }
            UiAction::Containers(
                panel,
                PanelAction::Shell {
                    target,
                    backend,
                    id,
                    user,
                    via,
                    new_terminal,
                },
            ) => {
                let shell = |cancelled| {
                    let user = user.as_deref();
                    app.container_shell(
                        &target,
                        backend,
                        &id,
                        user,
                        via,
                        new_terminal,
                        cancelled,
                        true,
                    )
                };
                let result = if new_terminal {
                    Ok(shell(None))
                } else {
                    suspended(terminal, session, signals, || Ok(shell(Some(terminated))))?
                };
                state
                    .containers
                    .panel_mut(panel)
                    .set_status(Some(shell_status(&target, result)));
            }
            UiAction::Containers(backend, PanelAction::Fix { target, offer }) => {
                let result = suspended(terminal, session, signals, || {
                    show_fix(&mut io::stdout(), &target, &offer)?;
                    Ok(app.run_offer(&target, &offer))
                })?;
                let panel = state.containers.panel_mut(backend);
                panel.set_status(fix_status(&offer, result));
                workers.lister(backend).list(panel, target);
            }
            UiAction::Containers(..) => {}
            UiAction::Refresh => {
                let reason = Reload::Requested;
                inventory_changed = reload(app, &mut state, &mut workers, &mut watch, reason);
            }
            UiAction::Add(host) => {
                let alias = host.alias().to_owned();
                let result = app.add_cancellable(host, terminated);
                inventory_changed = finish_mutation(
                    app,
                    &mut state,
                    &mut watch,
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
                    cancel_host_test(&mut workers.tester, &edited);
                }
                inventory_changed = finish_mutation(
                    app,
                    &mut state,
                    &mut watch,
                    result,
                    Some(&selected),
                    &edited,
                    format!("Saved {selected}"),
                );
            }
            UiAction::Remove(alias) => {
                let result = app.remove_cancellable(&alias, terminated);
                if result.is_ok() {
                    cancel_host_test(&mut workers.tester, &[&alias]);
                }
                inventory_changed = finish_mutation(
                    app,
                    &mut state,
                    &mut watch,
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
                if !workers.planner.start(direction, preselect) {
                    state.mode = Mode::Browse;
                    state.status = Some("the adoption worker stopped".into());
                }
            }
            UiAction::CancelPlan => workers.planner.cancel(),
            UiAction::OpenSettings => state.open_settings(status(desktop)),
            UiAction::Setting(key) => toggle_setting(app, &mut state, key),
            UiAction::Desktop(run) => {
                let result = match std::env::current_exe() {
                    Ok(exe) => {
                        let pause = || {
                            signals.child_finished();
                            wait_for_enter(terminated);
                        };
                        let child = || Ok(run_desktop(&exe, run, pause));
                        match suspended(terminal, session, signals, child)? {
                            Ok(result) => result,
                            Err(error) => Err(error.to_string()),
                        }
                    }
                    Err(error) => Err(format!("cannot find portico's executable: {error}")),
                };
                state.finish_desktop(result, status(desktop));
            }
            UiAction::Migrate(aliases) => {
                if let Some(migrate) = state.migrate.take() {
                    let result = app.migrate_cancellable(&migrate.plan, &aliases, terminated);
                    let moved = result.as_ref().map_or_else(|_| Vec::new(), Clone::clone);
                    let changed = moved.iter().map(String::as_str).collect::<Vec<_>>();
                    if result.is_ok() {
                        cancel_host_test(&mut workers.tester, &changed);
                    }
                    let verb = match migrate.plan.direction {
                        adopt::Direction::Adopt => "Adopted",
                        adopt::Direction::Export => "Exported",
                    };
                    inventory_changed = finish_mutation(
                        app,
                        &mut state,
                        &mut watch,
                        result.map(drop),
                        None,
                        &changed,
                        format!("{verb}: {}", moved.join(", ")),
                    );
                }
                // A failure ends the screen too, as its plan may no longer match the files.
                state.mode = Mode::Browse;
            }
            UiAction::Test { origin, target } => workers.tester.start(origin, Job::Test(target)),
            UiAction::CancelTest => workers.tester.cancel(),
            UiAction::FormEdited => workers
                .tester
                .cancel_if(|origin, _| *origin == Origin::Form),
            UiAction::Trust(trusted) => {
                if let Some(pending) = state.trust.take()
                    && trusted
                {
                    let job = Job::Trust {
                        target: pending.target,
                        key: pending.key,
                    };
                    workers.tester.start(pending.origin, job);
                }
            }
        }
        // A form's result is only shown in that form.
        if !state.in_form() {
            workers
                .tester
                .cancel_if(|origin, _| *origin == Origin::Form);
        }

        sync_details(&mut state, &mut workers.resolver, inventory_changed);
        redraw = true;
    }
}

/// Looks after what happens between keys: collects what the workers finished, keeps the
/// tunnels' statuses fresh, and applies a change of the config files where `r` would be
/// accepted, announcing it until the form or prompt closes otherwise. Returns whether the screen
/// needs drawing again.
fn on_tick(
    app: &App,
    state: &mut UiState,
    workers: &mut Workers,
    watch: &mut ConfigWatch,
    now: Instant,
) -> bool {
    let mut redraw = false;
    if let Some((host, loaded)) = workers.resolver.poll() {
        state.details.insert(host, loaded);
        state.open_pending_edit();
        redraw = true;
    }
    if let Some(finished) = workers.tester.poll() {
        state.record_test(finished, local_time());
        redraw = true;
    }
    for (backend, lister) in &mut workers.listers {
        if let Some((target, result)) = lister.try_finished() {
            state.containers.panel_mut(*backend).finish(target, result);
            redraw = true;
        }
    }
    if let Some((planned, preselect)) = workers.planner.try_finished() {
        state.finish_plan(planned, &preselect, app.home());
        redraw = true;
    }
    if let Some(built) = workers.indexer.try_finished() {
        state.finish_identities(built);
        redraw = true;
    }
    // The IdentityFile suggestions draw on every host's identities, built once a form opens.
    if state.in_form() {
        workers.indexer.request();
    }
    if let Some((name, result)) = workers.starter.poll() {
        state.tunnels.finish_start(&name, result);
        state.tunnels.reload(app);
        redraw = true;
    }
    if state.tab == Tab::Tunnels && state.tunnels.is_stale() {
        state.tunnels.reload(app);
        redraw = true;
    }
    let starting = workers.starter.running().map(str::to_owned);
    state.tunnels.set_starting(starting);
    let testing = workers.tester.running().map(str::to_owned);
    if state.testing != testing {
        state.testing = testing;
        redraw = true;
    }
    // A change of the config files is applied where `r` would be, and otherwise waits,
    // announced, for the form or prompt to close.
    let changed = watch.tick(now);
    if changed && state.mode == Mode::Browse {
        let replaced = reload(app, state, workers, watch, Reload::ConfigChanged);
        sync_details(state, &mut workers.resolver, replaced);
        redraw = true;
    }
    let waiting = changed && state.mode != Mode::Browse;
    if state.config_changed != waiting {
        state.config_changed = waiting;
        redraw = true;
    }
    redraw
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

/// Writes the open settings overlay's `key` turned over, showing a failure in the overlay; the
/// tabs change when it closes.
fn toggle_setting(app: &App, state: &mut UiState, key: Key) {
    let home = state.home.clone();
    let Some(overlay) = state.settings.as_mut() else {
        return;
    };
    let value = !overlay.value(key);
    let result = app
        .set_setting(key, value)
        .map(drop)
        .map_err(|error| settings_problem(&error, &home));
    overlay.finish_toggle(key, value, result);
}

/// Runs `portico desktop install` or `uninstall` with `exe` in the terminal, then asks for Enter
/// and calls `pause` to wait for it, so the output can be read. A child that cannot start, which
/// printed nothing and is not waited for, or that fails is the error.
fn run_desktop(exe: &Path, run: DesktopRun, pause: impl FnOnce()) -> Result<(), String> {
    let command = run.command();
    let failed = |reason: String| format!("portico desktop {command} failed: {reason}");
    let exit = std::process::Command::new(exe)
        .args(["desktop", command])
        .status()
        .map_err(|error| failed(error.to_string()))?;
    print!("\nPress Enter to return to Portico.");
    let _ = io::Write::flush(&mut io::stdout());
    pause();
    if exit.success() {
        Ok(())
    } else {
        Err(failed(exit.to_string()))
    }
}

/// Waits for a line on the terminal, an end of input, or `terminated` (Ctrl+C, a signal or a
/// hang-up).
///
/// Reads the descriptor directly: `main` holds the lock of `io::stdin()` for the whole session,
/// so reading through it would wait for ever. One byte at a time, so nothing after the line is
/// taken from the terminal.
fn wait_for_enter(terminated: &AtomicBool) {
    use std::{
        io::Read,
        os::fd::{AsFd, AsRawFd},
    };
    let Ok(fd) = io::stdin().as_fd().try_clone_to_owned() else {
        return;
    };
    let mut input = std::fs::File::from(fd);
    let mut byte = [0];
    while !terminated.load(Ordering::Relaxed) {
        let mut poll = libc::pollfd {
            fd: input.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `poll` reads and writes only the one `pollfd` it is given.
        let ready = unsafe { libc::poll(&mut poll, 1, IDLE_TICK.as_millis() as libc::c_int) };
        if ready < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return;
        }
        if ready <= 0 {
            continue;
        }
        match input.read(&mut byte) {
            Ok(0) => return,
            Ok(_) if matches!(byte[0], b'\n' | b'\r') => return,
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
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
/// On success, the test results of `changed` aliases are dropped. When the inventory was read
/// again, the config watch starts over from there, so Portico's own write is never taken for an
/// outside change.
fn finish_mutation(
    app: &App,
    state: &mut UiState,
    watch: &mut ConfigWatch,
    result: Result<(), AppError>,
    preferred: Option<&str>,
    changed: &[&str],
    status: String,
) -> bool {
    let reread = match result {
        Ok(()) => {
            complete_mutation(state, preferred, changed, status, || app.hosts());
            true
        }
        Err(error) => fail_mutation(state, &error, || app.hosts()),
    };
    if reread {
        watch.rebase(&state.inputs, Instant::now());
    }
    reread
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
            crate::domain::adopt::AdoptError::Io { .. }
                | crate::domain::adopt::AdoptError::Rollback { .. }
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

/// Why the inventory is read again, which only changes how the result is reported.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Reload {
    /// `r` or F5.
    Requested,
    /// The config watch saw the files change.
    ConfigChanged,
}

/// Reads the inventory again: the one path for `r` and for a change of the config files. Agents
/// and key files are asked again too, host tests in flight are cancelled when the inventory was
/// replaced, and the config watch starts over from what was read, or from the paths it had when
/// the read failed, so a broken file is not read again until it changes. Returns whether the
/// inventory was replaced.
fn reload(
    app: &App,
    state: &mut UiState,
    workers: &mut Workers,
    watch: &mut ConfigWatch,
    reason: Reload,
) -> bool {
    // Agents and key files may have changed too: ask them again on the next look.
    app.forget_identities();
    workers.indexer.reset();
    state.identity_inventory = None;
    let replaced = apply_refresh(state, reason, || app.hosts());
    if replaced {
        workers
            .tester
            .cancel_if(|origin, _| matches!(origin, Origin::Host(_)));
    }
    watch.rebase(&state.inputs, Instant::now());
    replaced
}

/// Re-reads the inventory, so changes made outside Portico appear. Returns whether the
/// inventory was replaced, which makes the caller discard the cached details. A failed read
/// leaves everything as it was and only reports why.
fn apply_refresh<E, F>(state: &mut UiState, reason: Reload, read: F) -> bool
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
            state.status = Some(match reason {
                Reload::Requested => format!("Refreshed: {count} {noun}"),
                Reload::ConfigChanged => format!("Config changed: reloaded {count} {noun}"),
            });
            true
        }
        Err(error) => {
            state.status = Some(match reason {
                Reload::Requested => format!("Refresh failed: {error}"),
                Reload::ConfigChanged => format!("Config changed, reload failed: {error}"),
            });
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

/// The background workers of a TUI session, spawned in its thread scope. Host details come
/// from `ssh -G` and connection tests run `ssh`, so the UI keeps handling keys meanwhile.
/// Dropping them cancels what runs (the resolver and the tester first, which may be running
/// `ssh`) before the scope waits for their threads.
struct Workers {
    resolver: Resolver,
    tester: Tester,
    /// One per container backend, in the order of `backend::AUTOMATIC`.
    listers: Vec<(BackendId, Lister)>,
    planner: Planner,
    indexer: Indexer,
    starter: Starter,
}

impl Workers {
    fn spawn<'scope>(scope: &'scope thread::Scope<'scope, '_>, app: &'scope App) -> Self {
        Self {
            resolver: Resolver::spawn(scope, app),
            tester: Tester::spawn(scope, app),
            listers: AUTOMATIC
                .into_iter()
                .map(|backend| (backend, Lister::spawn(scope, app, backend)))
                .collect(),
            planner: Planner::spawn(scope, app),
            indexer: Indexer::spawn(scope, app),
            starter: Starter::spawn(scope, app),
        }
    }

    /// The listing worker of `backend`.
    fn lister(&mut self, backend: BackendId) -> &mut Lister {
        self.listers
            .iter_mut()
            .find_map(|(listing, lister)| (*listing == backend).then_some(lister))
            .expect("a lister for every backend")
    }

    /// Whether any worker is running, so its result is looked for more often.
    fn busy(&self) -> bool {
        self.resolver.is_pending()
            || self.tester.running().is_some()
            || self.listers.iter().any(|(_, lister)| lister.is_running())
            || self.planner.is_running()
            || self.indexer.is_running()
            || self.starter.running().is_some()
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

#[derive(Clone, Debug, Eq, PartialEq)]
enum Mode {
    Browse,
    Filter,
    AddForm,
    EditForm,
    ConfirmRemove,
    ConfirmTrust,
    Migrate,
    /// The settings overlay, over whichever tab is shown.
    Settings,
}

/// The tabs, in the order the tab bar shows them and Tab cycles them: one per container
/// backend, in the order of `backend::AUTOMATIC`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Tab {
    #[default]
    Hosts,
    Tunnels,
    Containers(BackendId),
}

impl Tab {
    const ALL: [Self; 5] = [
        Self::Hosts,
        Self::Tunnels,
        Self::Containers(BackendId::Docker),
        Self::Containers(BackendId::Podman),
        Self::Containers(BackendId::Proxmox),
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Hosts => "Hosts",
            Self::Tunnels => "Tunnels",
            Self::Containers(backend) => backend.title(),
        }
    }

    /// Whether the tab is shown with `tabs` turned on; the hosts always are.
    fn shown(self, tabs: &Tabs) -> bool {
        match self {
            Self::Hosts => true,
            Self::Tunnels => tabs.tunnels,
            Self::Containers(backend) => tabs.shows(backend),
        }
    }

    /// The number that selects the tab among the `visible` ones.
    fn number(self, visible: &[Self]) -> usize {
        visible.iter().position(|&tab| tab == self).unwrap_or(0) + 1
    }

    /// The tab `by` places after this one among the `visible` ones, wrapping around.
    fn cycled(self, by: isize, visible: &[Self]) -> Self {
        let count = visible.len() as isize;
        visible[(self.number(visible) as isize - 1 + by).rem_euclid(count) as usize]
    }
}

#[derive(Debug)]
struct UiState {
    hosts: Vec<HostSummary>,
    warnings: Vec<DiscoveryWarning>,
    /// What the inventory was read from, which the config watch follows.
    inputs: Vec<Input>,
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
    /// Set while a change of the config files waits for the open form or prompt to close.
    config_changed: bool,
    containers: ContainerTabs,
    /// The container tab shown last, which `c` shows again.
    container_tab: BackendId,
    /// The tab shown and receiving keys.
    tab: Tab,
    /// Which optional tabs are turned on.
    tabs: Tabs,
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
    /// The settings overlay while in `Mode::Settings`.
    settings: Option<SettingsOverlay>,
    /// The settings file, shown in the overlay.
    settings_path: PathBuf,
}

type TestResults = PerHost<TestMark>;

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
    /// An action of the container tab of a backend.
    Containers(BackendId, PanelAction),
    /// Lists targets with backends, each on its backend's worker.
    ListContainers(Vec<(BackendId, Target)>),
    /// Opens the migrate screen of every host owned like the selected one.
    OpenMigrate {
        direction: adopt::Direction,
        preselect: String,
    },
    /// Moves the checked hosts of the open migrate screen.
    Migrate(Vec<String>),
    /// Stops building the plan of the migrate screen, which closed before it arrived.
    CancelPlan,
    /// Opens the settings overlay, once the desktop integration's status is read.
    OpenSettings,
    /// Turns over a setting of the open overlay.
    Setting(Key),
    /// Runs a desktop integration action of the open overlay.
    Desktop(DesktopRun),
}

impl UiState {
    fn new(inventory: HostInventory, ctrl_enter: bool) -> Self {
        let HostInventory {
            hosts,
            warnings,
            inputs,
        } = inventory;
        let visible = (0..hosts.len()).collect::<Vec<_>>();
        let selected = visible.first().copied();
        Self {
            hosts,
            warnings,
            inputs,
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
            config_changed: false,
            containers: ContainerTabs::default(),
            container_tab: BackendId::Docker,
            tab: Tab::Hosts,
            tabs: Tabs::default(),
            migrate: None,
            identity_inventory: None,
            home: PathBuf::new(),
            tunnels: TunnelsScreen::default(),
            settings: None,
            settings_path: PathBuf::new(),
        }
    }

    /// The footer of the tab shown, `footer`, or none while the overlay, with its own, is open.
    fn tab_footer(&self, footer: impl FnOnce() -> String) -> String {
        if self.mode == Mode::Settings {
            String::new()
        } else {
            self.with_tab_keys(footer())
        }
    }

    /// `footer` with its `Tab/1-5 tabs` hint worded for the tabs shown: `Tab/1-n tabs`, or
    /// nothing when the hosts are the only tab.
    fn with_tab_keys(&self, footer: String) -> String {
        footer.replace("Tab/1-5 tabs  ", &self.tab_keys_hint())
    }

    /// The footer hint for the tab keys, with its trailing separator, or empty with one tab.
    fn tab_keys_hint(&self) -> String {
        match self.visible_tabs().len() {
            0 | 1 => String::new(),
            count => format!("Tab/1-{count} tabs  "),
        }
    }

    /// The footer hint for `c`, with its trailing separator, or empty when no container tab is shown.
    fn containers_hint(&self) -> &'static str {
        if (self.visible_tabs().iter()).any(|tab| matches!(tab, Tab::Containers(_))) {
            "c containers  "
        } else {
            ""
        }
    }

    /// Opens the settings overlay with the tabs as they are and the desktop's `status`.
    fn open_settings(&mut self, desktop: Status) {
        self.settings = Some(SettingsOverlay::open(self.tabs, desktop));
        self.mode = Mode::Settings;
    }

    /// Records the outcome of a desktop action of the open overlay, and the status after it.
    fn finish_desktop(&mut self, result: Result<(), String>, desktop: Status) {
        if let Some(overlay) = &mut self.settings {
            overlay.finish_run(result, desktop);
        }
    }

    /// The settings overlay's keys. Closing it applies the tabs it wrote.
    fn handle_settings_key(&mut self, key: KeyEvent) -> UiAction {
        let Some(overlay) = &mut self.settings else {
            self.mode = Mode::Browse;
            return UiAction::None;
        };
        match overlay.handle_key(key) {
            OverlayAction::None => UiAction::None,
            OverlayAction::Toggle(key) => UiAction::Setting(key),
            OverlayAction::Run(run) => UiAction::Desktop(run),
            OverlayAction::Close => {
                let tabs = overlay.tabs;
                self.settings = None;
                self.mode = Mode::Browse;
                self.apply_tabs(tabs);
                UiAction::None
            }
        }
    }

    /// The tabs the tab bar shows and Tab cycles, in order.
    fn visible_tabs(&self) -> Vec<Tab> {
        Tab::ALL
            .into_iter()
            .filter(|tab| tab.shown(&self.tabs))
            .collect()
    }

    /// Turns tabs on or off. The tab shown returns to the hosts when it is turned off, and the
    /// container tab `c` shows moves to the first one left.
    fn apply_tabs(&mut self, tabs: Tabs) {
        self.tabs = tabs;
        self.containers.set_tabs(tabs);
        if !self.tab.shown(&tabs) {
            self.tab = Tab::Hosts;
        }
        if !tabs.shows(self.container_tab)
            && let Some(backend) = AUTOMATIC.into_iter().find(|&backend| tabs.shows(backend))
        {
            self.container_tab = backend;
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> UiAction {
        // An `e` waiting for details is dropped by any other key, a tab switch included.
        self.edit_pending = false;
        if self.mode == Mode::Settings {
            return self.handle_settings_key(key);
        }
        if let Some(action) = self.handle_tab_key(key) {
            return action;
        }
        match self.tab {
            Tab::Hosts => self.handle_hosts_key(key),
            Tab::Tunnels => match self.tunnels.handle_key(key) {
                TunnelAction::Quit => UiAction::Quit,
                action => UiAction::Tunnels(action),
            },
            Tab::Containers(backend) => {
                let before = self.containers.target();
                match self.containers.panel_mut(backend).handle_key(key) {
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
                        self.containers.panel_mut(backend).choose_target(hosts);
                        UiAction::None
                    }
                    action => self.containers.after_key(backend, &before, action),
                }
            }
        }
    }

    /// Whether the active tab is browsing, so the keys shared by every tab apply: no text input,
    /// form, confirmation or checklist has them.
    fn browsing(&self) -> bool {
        match self.tab {
            Tab::Hosts => self.mode == Mode::Browse,
            Tab::Tunnels => self.tunnels.is_browsing(),
            Tab::Containers(backend) => self.containers.panel(backend).is_browsing(),
        }
    }

    /// The keys shared by every browsing tab: `1` to `5`, Tab and Shift+Tab switch tabs, `T`
    /// shows the tunnels and `c` the container tab shown last, for the selected host when on
    /// the hosts, and `,` opens the settings.
    fn handle_tab_key(&mut self, key: KeyEvent) -> Option<UiAction> {
        if !self.browsing()
            || key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        let visible = self.visible_tabs();
        let tab = match key.code {
            KeyCode::Char(digit @ '1'..='5') => match visible.get(digit as usize - '1' as usize) {
                Some(&tab) => tab,
                None => return Some(UiAction::None),
            },
            KeyCode::Tab => self.tab.cycled(1, &visible),
            KeyCode::BackTab => self.tab.cycled(-1, &visible),
            // Terminals report Shift+T as `T`, with or without the Shift modifier.
            KeyCode::Char('T') if self.tabs.tunnels => Tab::Tunnels,
            KeyCode::Char('c') => {
                // The tab shown last, or the first shown when that one is turned off.
                let Some(backend) = [self.container_tab]
                    .into_iter()
                    .chain(AUTOMATIC)
                    .find(|&backend| self.tabs.shows(backend))
                else {
                    return Some(UiAction::None);
                };
                self.container_tab = backend;
                let containers = Tab::Containers(backend);
                if self.tab == Tab::Hosts
                    && let Some(alias) = self.selected_alias()
                {
                    let target = Target::Host(alias.to_owned());
                    self.tab = containers;
                    return Some(list_containers(self.containers.show(&target)));
                }
                containers
            }
            KeyCode::Char('T') => return Some(UiAction::None),
            KeyCode::Char(',') => return Some(UiAction::OpenSettings),
            _ => return None,
        };
        let from = self.tab;
        self.tab = tab;
        // A target is listed once a container tab is shown, never before: coming from another
        // tab, with every backend at once; from another container tab, with its backend.
        let Tab::Containers(backend) = tab else {
            return Some(UiAction::None);
        };
        self.container_tab = backend;
        if from == tab {
            return Some(UiAction::None);
        }
        let target = self.containers.target();
        Some(match from {
            Tab::Containers(_) => match self.containers.panel_mut(backend).show(target) {
                PanelAction::List(target) => UiAction::ListContainers(vec![(backend, target)]),
                _ => UiAction::None,
            },
            _ => list_containers(self.containers.show(&target)),
        })
    }

    fn in_form(&self) -> bool {
        matches!(self.mode, Mode::AddForm | Mode::EditForm)
    }

    /// The status line: the last message, otherwise the test in flight, otherwise a change of
    /// the config files waiting to be applied.
    fn status_line(&self) -> Option<String> {
        self.status
            .clone()
            .or_else(|| {
                self.testing
                    .as_ref()
                    .map(|alias| format!("Testing {alias}\u{2026} (Esc to cancel)"))
            })
            .or_else(|| {
                self.config_changed
                    .then(|| "Config changed: the list reloads when this closes".into())
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
}

fn render(frame: &mut Frame<'_>, state: &UiState) {
    let [bar, body] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .areas(frame.area());
    frame.render_widget(
        Paragraph::new(tab_bar(&state.visible_tabs(), state.tab, bar.width)),
        bar,
    );
    match state.tab {
        Tab::Hosts => render_hosts(frame, body, state),
        Tab::Tunnels => {
            let content = tab_areas(
                body,
                state.tunnels.status_line(),
                &state.tab_footer(|| state.tunnels.footer()),
                frame,
            );
            state.tunnels.render(frame, content);
        }
        Tab::Containers(backend) => render_containers(frame, body, state, backend),
    }
    if let Some(overlay) = &state.settings {
        overlay.render(
            frame,
            frame.area(),
            &tilde(&state.settings_path, &state.home),
        );
    }
}

/// The error's line, with the settings file's path shown from `home` as `~`.
fn settings_problem(error: &SettingsError, home: &Path) -> String {
    let (SettingsError::Invalid { path, .. } | SettingsError::Io { path, .. }) = error;
    error
        .to_string()
        .replacen(&path.display().to_string(), &tilde(path, home), 1)
}

/// Starts `lists`, or nothing when there are none.
fn list_containers(lists: Vec<(BackendId, Target)>) -> UiAction {
    if lists.is_empty() {
        UiAction::None
    } else {
        UiAction::ListContainers(lists)
    }
}

/// The tab bar: each tab's number and name, the active one highlighted. When the names do not
/// fit, the other tabs show only their numbers, and when even that does not fit only the active
/// tab is shown, so the bar never wraps or hides the active tab.
fn tab_bar(visible: &[Tab], active: Tab, width: u16) -> Line<'static> {
    let separator = " \u{2502} ";
    let full = |tab: Tab| format!("{} {}", tab.number(visible), tab.label());
    let labels = |short: bool| {
        visible
            .iter()
            .map(|&tab| {
                if short && tab != active {
                    tab.number(visible).to_string()
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
            for (index, (tab, label)) in visible.iter().zip(shown).enumerate() {
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
            let containers = state.containers_hint();
            let tab_keys = state.tab_keys_hint();
            let connect = format!(
                "{output}t test  Enter connect  {new_terminal}  {containers}{tab_keys}, settings  q quit"
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
                None => format!(
                    "/ filter{clear_filter}  a add  {refresh}  {tab_keys}, settings  q quit"
                ),
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
        // The overlay draws its own footer.
        Mode::Settings => String::new(),
    }
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

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};

    use crate::{
        app::{HostDetails, HostSummary, Ownership, TestOutcome, TestReport},
        backend::{ContainerList, Target},
        domain::discovery::DiscoveryWarning,
        domain::host::Host,
        domain::openssh::{ConnectionOutcome, ResolvedConfig},
        domain::probe::ProbeResult,
    };

    use super::*;
    use hosts::details_text;
    use tester::Finished;

    pub(super) fn test_state(hosts: Vec<HostSummary>) -> UiState {
        UiState::new(
            HostInventory {
                hosts,
                warnings: Vec::new(),
                inputs: Vec::new(),
            },
            false,
        )
    }

    pub(super) fn summary(alias: &str, ownership: Ownership) -> HostSummary {
        HostSummary {
            alias: alias.into(),
            ownership,
            source: PathBuf::from("/home/test/.ssh/config"),
        }
    }

    pub(super) fn details(alias: &str, ownership: Ownership) -> HostDetails {
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
    pub(super) fn resolve(state: &mut UiState, details: HostDetails) {
        let host = state.selected_summary().unwrap().clone();
        state.details.insert(host, Ok(details));
    }

    pub(super) fn render_to_buffer(state: &UiState) -> Buffer {
        let backend = TestBackend::new(100, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, state)).unwrap();
        terminal.backend().buffer().clone()
    }

    pub(super) fn rendered(state: &UiState) -> String {
        buffer_text(&render_to_buffer(state))
    }

    pub(super) fn buffer_text(buffer: &Buffer) -> String {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub(super) fn press(state: &mut UiState, code: KeyCode) -> UiAction {
        state.handle_key(key(code))
    }

    pub(super) fn type_text(state: &mut UiState, text: &str) {
        for character in text.chars() {
            press(state, KeyCode::Char(character));
        }
    }

    pub(super) fn fill_form(state: &mut UiState, values: [&str; 6]) {
        for (input, value) in state.form.values.iter_mut().zip(values) {
            *input = TextInput::new(value);
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

    pub(super) fn inventory(hosts: Vec<HostSummary>) -> HostInventory {
        HostInventory {
            hosts,
            warnings: Vec::new(),
            inputs: Vec::new(),
        }
    }

    fn summary_in(alias: &str, ownership: Ownership, source: &str) -> HostSummary {
        HostSummary {
            source: PathBuf::from(source),
            ..summary(alias, ownership)
        }
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

    /// Applies a requested refresh that reads `inventory`.
    fn refreshed(state: &mut UiState, inventory: HostInventory) -> bool {
        apply_refresh(state, Reload::Requested, || Ok::<_, String>(inventory))
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

        assert!(refreshed(&mut state, fresh));

        assert_eq!(state.hosts.len(), 2);
        assert_eq!(state.warnings.len(), 1);
        assert_eq!(state.selected_alias(), Some("dev"));
        assert_eq!(state.status.as_deref(), Some("Refreshed: 2 hosts"));
    }

    #[test]
    fn refresh_status_counts_every_host_and_is_singular_for_one() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        state.set_filter("nomatch");
        apply_refresh(&mut state, Reload::Requested, || {
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
        apply_refresh(&mut state, Reload::Requested, || {
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
        apply_refresh(&mut state, Reload::Requested, || {
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
        apply_refresh(&mut state, Reload::Requested, || {
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

        let changed = apply_refresh(&mut state, Reload::Requested, || {
            Err::<HostInventory, _>("cannot read config")
        });

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
    fn a_config_change_reloads_like_a_refresh_and_says_why() {
        let mut state = test_state(vec![
            summary("alpha", Ownership::Managed),
            summary("beta", Ownership::Managed),
        ]);
        press(&mut state, KeyCode::Down);
        state.set_filter("a");
        let fresh = inventory(vec![
            summary("added", Ownership::External),
            summary("alpha", Ownership::Managed),
            summary("beta", Ownership::Managed),
        ]);

        assert!(apply_refresh(&mut state, Reload::ConfigChanged, || {
            Ok::<_, String>(fresh)
        }));

        assert_eq!(state.selected_alias(), Some("beta"));
        assert_eq!(state.filter.as_str(), "a");
        assert_eq!(
            state.status.as_deref(),
            Some("Config changed: reloaded 3 hosts")
        );

        let hosts = state.hosts.clone();
        assert!(!apply_refresh(&mut state, Reload::ConfigChanged, || {
            Err::<HostInventory, _>("cannot read config")
        }));
        assert_eq!(state.hosts, hosts);
        assert_eq!(
            state.status.as_deref(),
            Some("Config changed, reload failed: cannot read config")
        );
    }

    #[test]
    fn a_waiting_config_change_shows_only_when_nothing_else_is_said() {
        let mut state = test_state(vec![summary("alpha", Ownership::Managed)]);
        press(&mut state, KeyCode::Char('a'));
        state.config_changed = true;
        let waiting = "Config changed: the list reloads when this closes";
        assert_eq!(state.status_line().as_deref(), Some(waiting));
        assert!(rendered(&state).contains(waiting));

        state.testing = Some("alpha".into());
        assert_eq!(
            state.status_line().as_deref(),
            Some("Testing alpha\u{2026} (Esc to cancel)")
        );
        state.status = Some("Saved alpha".into());
        assert_eq!(state.status_line().as_deref(), Some("Saved alpha"));
    }

    pub(super) fn form_with_active_field(index: usize) -> UiState {
        let mut state = test_state(Vec::new());
        press(&mut state, KeyCode::Char('a'));
        for _ in 0..index {
            press(&mut state, KeyCode::Tab);
        }
        state
    }

    #[test]
    fn a_tab_switch_drops_a_pending_edit() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        press(&mut state, KeyCode::Char('e'));
        press(&mut state, KeyCode::Char('2'));
        resolve(&mut state, details("dev", Ownership::Managed));
        state.open_pending_edit();
        assert_eq!(state.mode, Mode::Browse);
        press(&mut state, KeyCode::Char('1'));
        assert_eq!(state.mode, Mode::Browse);
        assert!(!state.in_form());
        assert!(rendered(&state).contains("Details"));

        // Even a pending edit left on a hidden tab never opens there.
        press(&mut state, KeyCode::Char('2'));
        state.edit_pending = true;
        state.open_pending_edit();
        assert_eq!(state.mode, Mode::Browse);
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

    fn inventory_of(aliases: &[&str]) -> HostInventory {
        HostInventory {
            hosts: aliases
                .iter()
                .map(|alias| summary(alias, Ownership::Managed))
                .collect(),
            warnings: Vec::new(),
            inputs: Vec::new(),
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
                source: crate::domain::managed::ManagedError::Parse {
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

    /// An app whose `ssh -G` appends its alias to a log, then runs `behaviour`.
    fn logging_app(dir: &std::path::Path, behaviour: &str) -> (App, PathBuf) {
        let paths = crate::domain::managed::ConfigPaths::from_home(dir);
        let store = crate::domain::managed::ManagedStore::new(paths.clone());
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
            crate::domain::openssh::OpenSsh::new(ssh, "/usr/bin/false".into()),
        );
        (app, log)
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

    pub(super) fn ctrl(character: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL)
    }

    pub(super) fn report(alias: &str, result: ProbeResult) -> TestReport {
        TestReport {
            alias: alias.into(),
            user: Some("u".into()),
            result,
            key_fetch_failure: None,
            explanation: None,
        }
    }

    pub(super) fn authenticated(alias: &str) -> TestOutcome {
        TestOutcome::Done(report(
            alias,
            ProbeResult::Authenticated(crate::domain::probe::Authenticated {
                host: "10.0.0.5".into(),
                address: "10.0.0.5".into(),
                port: 22,
                method: "publickey".into(),
                via: None,
            }),
        ))
    }

    pub(super) fn host_finished(
        host: HostSummary,
        outcome: Result<TestOutcome, String>,
    ) -> Finished {
        Finished {
            target: TestTarget::Saved(host.alias.clone()),
            origin: Origin::Host(host),
            outcome,
        }
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
            inputs: Vec::new(),
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
        assert!(refreshed(&mut state, inventory()));
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

    pub(super) fn unknown_key(alias: &str, host: &str) -> TestOutcome {
        TestOutcome::UnknownHostKey {
            report: report(
                alias,
                ProbeResult::Failed(crate::domain::probe::Failure::HostKeyUnknown),
            ),
            key: crate::domain::openssh::PresentedKey {
                lines: b"h ssh-ed25519 AAAA\n".to_vec(),
                fingerprints: vec![crate::domain::openssh::KeyFingerprint {
                    host: host.into(),
                    fingerprint: "SHA256:abc".into(),
                    key_type: "ED25519".into(),
                }],
            },
        }
    }

    fn explanation(
        headline: &str,
        raw: &[&str],
        always_raw: bool,
    ) -> crate::domain::explain::Explanation {
        crate::domain::explain::Explanation {
            headline: headline.into(),
            facts: vec![(crate::domain::explain::Label::User, vec!["u".into()])],
            next: vec!["try this".into()],
            raw: raw.iter().map(|line| (*line).into()).collect(),
            always_raw,
        }
    }

    pub(super) fn explained(
        alias: &str,
        headline: &str,
        raw: &[&str],
        always_raw: bool,
    ) -> TestOutcome {
        let mut failed = report(
            alias,
            ProbeResult::Failed(crate::domain::probe::Failure::HostKeyChanged {
                known_hosts: None,
            }),
        );
        failed.explanation = Some(explanation(headline, raw, always_raw));
        TestOutcome::Done(failed)
    }

    fn exited_255(
        alias: &str,
        explanation: Option<crate::domain::explain::Explanation>,
    ) -> ConnectionOutcome {
        use std::os::unix::process::ExitStatusExt;
        ConnectionOutcome::Exited {
            alias: alias.into(),
            status: std::process::ExitStatus::from_raw(255 << 8),
            explanation,
        }
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
            inputs: Vec::new(),
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
        assert!(refreshed(&mut state, inventory()));
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

    pub(super) fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    pub(super) fn proxy_jump_form(hosts: Vec<HostSummary>) -> UiState {
        let mut state = test_state(hosts);
        press(&mut state, KeyCode::Char('a'));
        for _ in 0..PROXY_JUMP {
            press(&mut state, KeyCode::Tab);
        }
        state
    }

    pub(super) fn jump_hosts() -> Vec<HostSummary> {
        let mut eu = summary("bastion-eu", Ownership::External);
        eu.source = PathBuf::from("/home/test/.ssh/config");
        vec![
            summary("bastion", Ownership::Managed),
            eu,
            summary("web", Ownership::Managed),
        ]
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

    fn shop_list(target: Target) -> ContainerList {
        ContainerList {
            backend: crate::backend::BackendId::Docker,
            target,
            listing: crate::backend::Listing {
                containers: ["web", "db"]
                    .into_iter()
                    .map(|service| crate::backend::Container {
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

    const DOCKER: Tab = Tab::Containers(BackendId::Docker);

    /// Listing `target` with every backend, as showing it does.
    fn every_backend(target: Target) -> UiAction {
        UiAction::ListContainers(
            AUTOMATIC
                .into_iter()
                .map(|backend| (backend, target.clone()))
                .collect(),
        )
    }

    fn char(state: &mut UiState, character: char) -> UiAction {
        press(state, KeyCode::Char(character))
    }

    fn dev_listed(state: &mut UiState) {
        let dev = Target::Host("dev".into());
        state
            .containers
            .panel_mut(BackendId::Docker)
            .finish(dev.clone(), Ok(shop_list(dev)));
    }

    fn tunnels_state() -> UiState {
        use crate::domain::tunnel::{Forward, Kind, Tunnel, TunnelReport, TunnelStatus};
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
            every_backend(Target::Local),
            "the Containers tab lists its target when first shown"
        );
        assert_eq!(state.tab, DOCKER);
        assert_eq!(char(&mut state, '1'), UiAction::None);
        assert_eq!(state.tab, Tab::Hosts);

        let podman = Tab::Containers(BackendId::Podman);
        let proxmox = Tab::Containers(BackendId::Proxmox);
        for expected in [Tab::Tunnels, DOCKER, podman, proxmox, Tab::Hosts] {
            press(&mut state, KeyCode::Tab);
            assert_eq!(state.tab, expected);
        }
        for expected in [proxmox, podman, DOCKER, Tab::Tunnels, Tab::Hosts] {
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
        char(&mut state, '4');
        char(&mut state, 'T');
        char(&mut state, 'c');
        assert_eq!(state.tab, podman, "c shows the container tab shown last");
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

    fn without(configure: impl FnOnce(&mut Tabs)) -> UiState {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        let mut tabs = Tabs::default();
        configure(&mut tabs);
        state.apply_tabs(tabs);
        state
    }

    #[test]
    fn hidden_tabs_leave_the_bar_and_renumber() {
        let mut state = without(|tabs| tabs.podman = false);
        let podman = Tab::Containers(BackendId::Podman);
        assert!(!state.visible_tabs().contains(&podman));
        assert!(matches!(char(&mut state, '4'), UiAction::ListContainers(_)));
        assert_eq!(state.tab, Tab::Containers(BackendId::Proxmox));
        assert_eq!(char(&mut state, '5'), UiAction::None);
        assert_eq!(state.tab, Tab::Containers(BackendId::Proxmox));
        let text = tab_bar(&state.visible_tabs(), state.tab, 100).to_string();
        assert!(!text.contains("Podman"), "{text}");
        assert!(text.contains("4 Proxmox"), "{text}");
    }

    #[test]
    fn tab_cycles_over_visible_tabs_only() {
        let mut state = without(|tabs| tabs.tunnels = false);
        press(&mut state, KeyCode::Tab);
        assert_eq!(state.tab, DOCKER);
        press(&mut state, KeyCode::BackTab);
        assert_eq!(state.tab, Tab::Hosts);
        press(&mut state, KeyCode::BackTab);
        assert_eq!(state.tab, Tab::Containers(BackendId::Proxmox));
    }

    #[test]
    fn t_does_nothing_when_tunnels_are_off() {
        let mut state = without(|tabs| tabs.tunnels = false);
        assert_eq!(char(&mut state, 'T'), UiAction::None);
        assert_eq!(state.tab, Tab::Hosts);
    }

    #[test]
    fn c_skips_a_hidden_container_tab() {
        let mut state = without(|tabs| tabs.docker = false);
        assert_eq!(
            state.container_tab,
            BackendId::Podman,
            "moved on by apply_tabs"
        );
        state.container_tab = BackendId::Docker;
        char(&mut state, 'c');
        assert_eq!(state.tab, Tab::Containers(BackendId::Podman));
    }

    #[test]
    fn c_does_nothing_with_every_backend_off() {
        let mut state = without(|tabs| {
            tabs.docker = false;
            tabs.podman = false;
            tabs.proxmox = false;
        });
        assert_eq!(char(&mut state, 'c'), UiAction::None);
        assert_eq!(state.tab, Tab::Hosts);
    }

    #[test]
    fn turning_off_the_open_tab_returns_to_hosts() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        char(&mut state, '4');
        assert_eq!(state.tab, Tab::Containers(BackendId::Podman));
        state.apply_tabs(Tabs {
            podman: false,
            ..Tabs::default()
        });
        assert_eq!(state.tab, Tab::Hosts);
        assert_eq!(state.container_tab, BackendId::Docker, "Docker is first");
    }

    #[test]
    fn showing_a_target_lists_only_visible_backends() {
        let mut state = without(|tabs| tabs.docker = false);
        let dev = Target::Host("dev".into());
        assert_eq!(
            char(&mut state, 'c'),
            UiAction::ListContainers(vec![
                (BackendId::Podman, dev.clone()),
                (BackendId::Proxmox, dev)
            ])
        );
    }

    #[test]
    fn a_settings_problem_shows_the_path_with_a_tilde() {
        let error = SettingsError::Invalid {
            path: PathBuf::from("/home/test/.config/portico/settings.toml"),
            line: Some(2),
            reason: "expected a boolean".into(),
        };
        let line = settings_problem(&error, Path::new("/home/test"));
        assert!(line.contains("~/.config/portico/settings.toml"), "{line}");
        assert!(!line.contains("/home/test"), "{line}");
    }

    #[test]
    fn the_container_tabs_follow_the_backend_order() {
        let tabs: Vec<Tab> = Tab::ALL[2..].to_vec();
        let expected: Vec<Tab> = AUTOMATIC.into_iter().map(Tab::Containers).collect();
        assert_eq!(tabs, expected);
    }

    #[test]
    fn the_container_tabs_share_one_target_and_list_it_with_every_backend() {
        let mut state = test_state(vec![
            summary("dev", Ownership::Managed),
            summary("web", Ownership::External),
        ]);
        let web = Target::Host("web".into());
        assert_eq!(char(&mut state, '3'), every_backend(Target::Local));
        for backend in AUTOMATIC {
            state
                .containers
                .panel_mut(backend)
                .finish(Target::Local, Ok(shop_list(Target::Local)));
        }
        // Another container tab lists only what it has not listed.
        assert_eq!(char(&mut state, '4'), UiAction::None);
        assert_eq!(
            state
                .containers
                .panel(BackendId::Podman)
                .title()
                .to_string(),
            "Podman /"
        );

        char(&mut state, 'h');
        type_text(&mut state, "web");
        assert_eq!(
            press(&mut state, KeyCode::Enter),
            every_backend(web.clone())
        );
        assert_eq!(state.containers.panel(BackendId::Docker).target(), web);
        assert_eq!(state.containers.panel(BackendId::Proxmox).target(), web);

        // `r` and Esc are the shown tab's own.
        state
            .containers
            .panel_mut(BackendId::Podman)
            .finish(web.clone(), Ok(shop_list(web.clone())));
        assert_eq!(
            char(&mut state, 'r'),
            UiAction::ListContainers(vec![(BackendId::Podman, web.clone())])
        );
        state
            .containers
            .panel_mut(BackendId::Podman)
            .set_listing(web.clone());
        assert_eq!(
            press(&mut state, KeyCode::Esc),
            UiAction::Containers(BackendId::Podman, PanelAction::CancelList)
        );

        // A shown container tab lists again only its own failed or missing listing.
        state
            .containers
            .panel_mut(BackendId::Proxmox)
            .set_listing(web.clone());
        assert_eq!(
            char(&mut state, '5'),
            UiAction::None,
            "Proxmox's listing of web is running"
        );
        state
            .containers
            .panel_mut(BackendId::Docker)
            .abandon(web.clone());
        assert_eq!(
            char(&mut state, '3'),
            UiAction::ListContainers(vec![(BackendId::Docker, web)])
        );
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
        assert_eq!(char(&mut state, 'c'), every_backend(web.clone()));
        assert_eq!(state.tab, DOCKER);
        assert_eq!(state.containers.target(), web);
        for backend in AUTOMATIC {
            state.containers.panel_mut(backend).set_listing(web.clone());
        }
        char(&mut state, '1');
        assert_eq!(
            char(&mut state, 'c'),
            UiAction::None,
            "a listing already running or kept is not started again"
        );
        char(&mut state, '1');
        assert_eq!(char(&mut state, '3'), UiAction::None);

        let mut empty = test_state(Vec::new());
        assert_eq!(
            char(&mut empty, 'c'),
            every_backend(Target::Local),
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
                UiAction::ListContainers(_)
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
            assert_eq!(state.tab, DOCKER, "after {opener}");
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
        state
            .containers
            .panel_mut(BackendId::Docker)
            .show(Target::Host("dev".into()));
        dev_listed(&mut state);
        char(&mut state, 'j');
        let details = state.containers.panel(BackendId::Docker).details();

        char(&mut state, '1');
        assert_eq!(state.filter.as_str(), "w");
        assert_eq!(state.selected_alias(), Some("web"));
        char(&mut state, '2');
        assert!(render_text_at(&state, 100).contains("Tunnels /d"));
        char(&mut state, '3');
        assert_eq!(state.containers.panel(BackendId::Docker).details(), details);
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
            .panel_mut(BackendId::Docker)
            .finish(Target::Local, Ok(shop_list(Target::Local)));
        assert_eq!(
            char(&mut state, 'r'),
            UiAction::ListContainers(vec![(BackendId::Docker, Target::Local)])
        );
        state
            .containers
            .panel_mut(BackendId::Docker)
            .finish(Target::Local, Ok(shop_list(Target::Local)));
        assert_eq!(
            press(&mut state, KeyCode::F(5)),
            UiAction::ListContainers(vec![(BackendId::Docker, Target::Local)])
        );
        state
            .containers
            .panel_mut(BackendId::Docker)
            .finish(Target::Local, Ok(shop_list(Target::Local)));
        press(&mut state, KeyCode::Esc);
        assert_eq!(state.tab, DOCKER, "Esc never switches");
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
        let line = tab_bar(&Tab::ALL, Tab::Tunnels, 100);
        assert_eq!(
            line.to_string(),
            " 1 Hosts \u{2502} 2 Tunnels \u{2502} 3 Docker \u{2502} 4 Podman \u{2502} 5 Proxmox"
        );
        let active = line
            .spans
            .iter()
            .find(|span| span.content == "2 Tunnels")
            .unwrap();
        assert!(active.style.add_modifier.contains(Modifier::REVERSED));
        assert_eq!(
            tab_bar(&Tab::ALL, DOCKER, 30).to_string(),
            " 1 \u{2502} 2 \u{2502} 3 Docker \u{2502} 4 \u{2502} 5"
        );
        assert_eq!(tab_bar(&Tab::ALL, DOCKER, 14).to_string(), " 3 Docker");

        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        char(&mut state, '3');
        for width in [60, 30, 20, 12] {
            let text = render_text_at(&state, width);
            let first = text.lines().next().unwrap();
            assert!(first.contains("3 Docker"), "{width}: {first}");
            if width >= 60 {
                assert!(first.contains("1 Hosts"), "{first}");
            }
        }
    }

    #[test]
    fn footers_name_the_tab_keys() {
        let state = test_state(vec![summary("dev", Ownership::Managed)]);
        let footer = footer_text(&state);
        assert!(
            footer.contains("c containers  Tab/1-5 tabs  , settings  q quit"),
            "{footer}"
        );
        assert!(!footer.contains("T tunnels"));
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        state.apply_tabs(Tabs {
            tunnels: false,
            docker: false,
            podman: true,
            proxmox: false,
        });
        let footer = footer_text(&state);
        assert!(
            footer.contains("c containers  Tab/1-2 tabs  , settings"),
            "{footer}"
        );
        state.apply_tabs(Tabs {
            tunnels: true,
            docker: false,
            podman: false,
            proxmox: false,
        });
        let footer = footer_text(&state);
        assert!(!footer.contains("c containers"), "{footer}");
        assert!(footer.contains("Tab/1-2 tabs"), "{footer}");
        let tunnels = state.tab_footer(|| state.tunnels.footer());
        assert!(tunnels.contains("Tab/1-2 tabs  , settings"), "{tunnels}");
        let state = test_state(Vec::new());
        assert!(footer_text(&state).contains("Tab/1-5 tabs"));
        let state = tunnels_state();
        assert!(
            state
                .tunnels
                .footer()
                .contains("Tab/1-5 tabs  , settings  q quit")
        );
        assert!(
            state
                .containers
                .panel(BackendId::Docker)
                .footer()
                .contains("h target")
        );
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
        assert!(text.contains("Docker /"), "{text}");
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
        assert!(state.containers.panel(BackendId::Docker).picker_open());
        let text = rendered(&state);
        assert!(text.contains("this machine"), "{text}");
        assert!(text.contains("web"), "{text}");
        assert!(text.contains("managed"), "{text}");
        assert!(footer_text_of(&state).starts_with("type to search"));
        state.handle_paste("we");
        assert_eq!(
            press(&mut state, KeyCode::Enter),
            every_backend(Target::Host("web".into()))
        );
        assert!(!state.containers.panel(BackendId::Docker).picker_open());
        assert_eq!(state.containers.target(), Target::Host("web".into()));

        char(&mut state, 'h');
        type_text(&mut state, "dev");
        press(&mut state, KeyCode::Esc);
        assert!(!state.containers.panel(BackendId::Docker).picker_open());
        assert_eq!(
            state.containers.target(),
            Target::Host("web".into()),
            "Esc leaves the target alone"
        );
        char(&mut state, 'h');
        type_text(&mut state, "this");
        assert_eq!(
            press(&mut state, KeyCode::Enter),
            every_backend(Target::Local)
        );
    }

    fn footer_text_of(state: &UiState) -> String {
        state.containers.panel(BackendId::Docker).footer()
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
            state
                .containers
                .panel(BackendId::Docker)
                .details()
                .unwrap()
                .ends_with("user: root"),
            "{:?}",
            state.containers.panel(BackendId::Docker).details()
        );
    }

    #[test]
    fn container_results_show_on_the_containers_tab() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        char(&mut state, 'c');
        dev_listed(&mut state);
        state
            .containers
            .panel_mut(BackendId::Docker)
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
        let offer = crate::backend::docker::join_group_offer(
            &Target::Host("dev".into()),
            "deploy",
            "/var/run/docker.sock",
        );
        let command = crate::backend::remote_command(&offer.words);
        let issue = crate::backend::BackendIssue {
            backend: crate::backend::BackendId::Docker,
            target: Target::Host("dev".into()),
            problem: crate::backend::Problem::PermissionDenied,
            headline: "deploy cannot use Docker on dev.".into(),
            next: vec![format!("Add deploy to the docker group: {command}")],
            offer: Some(offer),
            stderr: String::new(),
        };
        state.containers.panel_mut(BackendId::Docker).finish(
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
        assert!(text.contains("Tab/1-5 tabs"), "{text}");
        assert_eq!(char(&mut state, 'T'), UiAction::None);
        assert_eq!(state.tab, Tab::Tunnels);
    }

    fn no_desktop() -> crate::desktop::status::Status {
        crate::desktop::status::Status {
            lines: Vec::new(),
            installed: false,
        }
    }

    /// An app whose home, and so whose settings file, is `dir`.
    fn settings_app(dir: &std::path::Path) -> App {
        App::new(
            crate::domain::managed::ManagedStore::new(
                crate::domain::managed::ConfigPaths::from_home(dir),
            ),
            crate::domain::openssh::OpenSsh::new("/usr/bin/false".into(), "/usr/bin/false".into()),
        )
    }

    /// The state of `app` with the settings overlay open.
    fn overlay_state(app: &App) -> UiState {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        state.home = app.home().to_owned();
        state.settings_path = app.settings_path().to_owned();
        assert_eq!(char(&mut state, ','), UiAction::OpenSettings);
        state.open_settings(no_desktop());
        state
    }

    #[test]
    fn comma_opens_the_overlay_only_while_browsing() {
        let mut state = test_state(vec![summary("dev", Ownership::Managed)]);
        assert_eq!(char(&mut state, ','), UiAction::OpenSettings);
        state.open_settings(no_desktop());
        assert_eq!(state.mode, Mode::Settings);
        let text = rendered(&state);
        assert!(text.contains("Settings"), "{text}");
        assert!(text.contains("[x] Podman"), "{text}");
        assert!(text.contains("Install or update"), "{text}");
        assert!(text.contains("Esc close"), "{text}");
        assert_eq!(char(&mut state, ','), UiAction::None);
        assert_eq!(state.mode, Mode::Browse);

        char(&mut state, '/');
        assert_eq!(char(&mut state, ','), UiAction::None);
        assert_eq!(state.mode, Mode::Filter);
        assert_eq!(state.filter.as_str(), ",");

        // Any browsing tab opens it, and its keys go to the overlay, not to the tab.
        let mut state = tunnels_state();
        char(&mut state, '2');
        assert_eq!(char(&mut state, ','), UiAction::OpenSettings);
        state.open_settings(no_desktop());
        // The tab's footer gives way to the overlay's, on every tab.
        let text = rendered(&state);
        assert!(!text.contains("q quit"), "{text}");
        char(&mut state, ',');
        char(&mut state, '3');
        char(&mut state, ',');
        state.open_settings(no_desktop());
        let text = rendered(&state);
        assert!(!text.contains("q quit"), "{text}");
        char(&mut state, ',');
        char(&mut state, '2');
        char(&mut state, ',');
        state.open_settings(no_desktop());
        assert_eq!(char(&mut state, 's'), UiAction::None);
        assert_eq!(char(&mut state, '1'), UiAction::None);
        assert_eq!(state.tab, Tab::Tunnels);
        press(&mut state, KeyCode::Esc);
        assert_eq!(state.mode, Mode::Browse);
        assert!(footer_text(&state).contains(", settings"));
        assert!(state.tunnels.footer().contains(", settings"));
    }

    #[test]
    fn toggle_writes_and_applies_on_close() {
        let dir = tempfile::tempdir().unwrap();
        let app = settings_app(dir.path());
        let mut state = overlay_state(&app);
        let podman = Tab::Containers(BackendId::Podman);
        press(&mut state, KeyCode::Down);
        press(&mut state, KeyCode::Down);
        let action = char(&mut state, ' ');
        assert_eq!(action, UiAction::Setting(Key::TabsPodman));
        toggle_setting(&app, &mut state, Key::TabsPodman);

        let written = std::fs::read_to_string(app.settings_path()).unwrap();
        assert!(written.contains("podman = false"), "{written}");
        assert!(rendered(&state).contains("[ ] Podman"));
        assert!(state.visible_tabs().contains(&podman), "applied on close");

        press(&mut state, KeyCode::Esc);
        assert_eq!(state.mode, Mode::Browse);
        assert!(!state.visible_tabs().contains(&podman));
        assert!(!rendered(&state).contains("Podman"));
    }

    #[test]
    fn toggle_failure_keeps_the_value_and_shows_the_error() {
        let dir = tempfile::tempdir().unwrap();
        let app = settings_app(dir.path());
        std::fs::create_dir_all(app.settings_path().parent().unwrap()).unwrap();
        std::fs::write(app.settings_path(), "[tabs\n").unwrap();
        let mut state = overlay_state(&app);
        press(&mut state, KeyCode::Down);
        press(&mut state, KeyCode::Down);
        let Some(UiAction::Setting(key)) = Some(press(&mut state, KeyCode::Enter)) else {
            panic!("no toggle");
        };
        toggle_setting(&app, &mut state, key);

        let overlay = state.settings.as_ref().unwrap();
        let error = overlay.error.as_deref().unwrap();
        assert!(error.contains("line 1"), "{error}");
        assert!(error.contains("~/.config/portico/settings.toml"), "{error}");
        assert!(overlay.tabs.podman);
        assert!(rendered(&state).contains("line 1"));
        press(&mut state, KeyCode::Esc);
        assert!(state.tabs.podman);
    }

    #[test]
    fn desktop_action_failure_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let app = settings_app(dir.path());
        let mut state = overlay_state(&app);
        let result = run_desktop(&dir.path().join("missing"), DesktopRun::Install, || {
            panic!("a child that never started is not waited for")
        });
        let error = result.clone().unwrap_err();
        assert!(
            error.starts_with("portico desktop install failed: "),
            "{error}"
        );

        state.finish_desktop(result, no_desktop());

        assert_eq!(
            state.settings.as_ref().unwrap().error.as_deref(),
            Some(error.as_str())
        );
        assert!(rendered(&state).contains("portico desktop install failed"));
    }
}
