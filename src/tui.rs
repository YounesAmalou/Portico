use std::{
    io, panic,
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
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
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

mod tester;

use crate::{
    app::{
        App, AppError, HostDetails, HostInventory, HostSummary, Ownership, TestOutcome, TestTarget,
    },
    discovery::DiscoveryWarning,
    host::{Change, ClearField, Host, HostChanges, HostError, HostField, parse_port},
    managed::{InitPlan, TransactionError},
    openssh::{ConnectionOutcome, PresentedKey, ResolvedConfig},
    probe::ProbeResult,
};
use tester::{Finished, Job, Origin, Tester};
use thiserror::Error;

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
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        match event::read()? {
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
        browse_hosts(app, terminal, signals, session, resolver, tester)
    })
}

fn browse_hosts<B: Backend<Error = io::Error>>(
    app: &App,
    terminal: &mut Terminal<B>,
    signals: &Signals,
    session: &TerminalSession,
    mut resolver: Resolver,
    mut tester: Tester,
) -> Result<(), TuiError> {
    let terminated = signals.flag();
    let mut state = UiState::new(app.hosts()?, session.keyboard_enhancement());
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
        let wait = if resolver.is_pending() || tester.running().is_some() {
            Duration::from_millis(25)
        } else {
            Duration::from_millis(100)
        };
        if !event::poll(wait)? {
            continue;
        }
        let event = event::read()?;
        if matches!(event, Event::Resize(_, _)) {
            redraw = true;
            continue;
        }
        if let Event::Paste(text) = event {
            state.handle_paste(&text);
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
        match state.handle_key(key) {
            UiAction::None => {}
            UiAction::Quit => return Ok(()),
            UiAction::Connect {
                alias,
                new_terminal,
            } => {
                let result = if new_terminal {
                    app.connect(&alias, true).map_err(TuiError::from)
                } else {
                    let result = suspend_for_child(
                        || session.leave(),
                        || {
                            // The terminal's Ctrl+C is for ssh now; Portico waits for it.
                            let _foreground = signals.foreground_child();
                            app.connect_cancellable(&alias, terminated)
                                .map(ConnectionOutcome::Exited)
                                .map_err(TuiError::from)
                        },
                        || session.enter(),
                    );
                    match result {
                        Err(TuiError::Io(error)) => return Err(TuiError::Io(error)),
                        result => {
                            redraw_from_scratch(terminal)?;
                            result
                        }
                    }
                };
                state.status = Some(match result {
                    Ok(ConnectionOutcome::LaunchRequested) => {
                        format!("Launch requested for {alias}")
                    }
                    Ok(ConnectionOutcome::LaunchFailed(status)) => {
                        format!("terminal launcher exited with {status}")
                    }
                    Ok(ConnectionOutcome::Exited(status)) if status.success() => {
                        format!("Disconnected from {alias}")
                    }
                    Ok(ConnectionOutcome::Exited(status)) => format!("ssh exited with {status}"),
                    Err(error) => error.to_string(),
                });
            }
            UiAction::Refresh => {
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
            UiAction::Test { origin, target } => tester.start(origin, Job::Test(target)),
            UiAction::CancelTest => tester.cancel(),
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
        AppError::Discovery(_)
        | AppError::OpenSsh(_)
        | AppError::InvalidHost(_)
        | AppError::ReadOnlyExternal(_)
        | AppError::UnknownHost(_)
        | AppError::AliasCollision(_)
        | AppError::IncompleteDiscovery(_)
        | AppError::KnownHosts { .. } => false,
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
    }
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
}

/// A single-line text field edited at a cursor.
///
/// The cursor is a byte offset that always sits on a character boundary. An input bound to a
/// host field applies an edit only when the result is something that field accepts, so it never
/// holds text the host model would reject for its characters.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct TextInput {
    text: String,
    cursor: usize,
    field: Option<HostField>,
}

impl TextInput {
    fn new(text: &str) -> Self {
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

    fn as_str(&self) -> &str {
        &self.text
    }

    fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Applies an editing key and reports whether the key was an editing key.
    fn handle_key(&mut self, key: KeyEvent) -> bool {
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
    fn paste(&mut self, text: &str) {
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
    fn spans(&self, focused: bool) -> Vec<Span<'static>> {
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

#[derive(Clone, Debug)]
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
    /// The host key awaiting the user's decision while in `Mode::ConfirmTrust`.
    trust: Option<PendingTrust>,
}

/// The outcome of a host's last connection test.
#[derive(Clone, Debug, Eq, PartialEq)]
struct TestMark {
    passed: bool,
    line: String,
    finished_at: String,
}

/// Connection test results by host. Like `DetailsCache`, lookup is linear.
#[derive(Clone, Debug, Default)]
struct TestResults {
    entries: Vec<(HostSummary, TestMark)>,
}

impl TestResults {
    fn get(&self, host: &HostSummary) -> Option<&TestMark> {
        self.entries
            .iter()
            .find(|(tested, _)| tested == host)
            .map(|(_, mark)| mark)
    }

    fn insert(&mut self, host: HostSummary, mark: TestMark) {
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
    test_result: Option<String>,
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
    /// The answer to the trust prompt; on `true` the key to trust is in `UiState::trust`.
    Trust(bool),
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
            trust: None,
        }
    }

    fn set_filter(&mut self, filter: &str) {
        self.filter = TextInput::new(filter);
        self.refilter();
    }

    /// Recomputes the visible hosts from the filter, keeping the selection when it still matches.
    fn refilter(&mut self) {
        let filter = self.filter.as_str().to_ascii_lowercase();
        self.visible = self
            .hosts
            .iter()
            .enumerate()
            .filter(|(_, host)| host.alias.to_ascii_lowercase().contains(&filter))
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
    }

    fn handle_key(&mut self, key: KeyEvent) -> UiAction {
        self.status = None;
        self.edit_pending = false;
        if key.code == KeyCode::Esc && self.testing.is_some() {
            self.status = Some("Test cancelled".into());
            return UiAction::CancelTest;
        }
        match self.mode {
            Mode::Browse => self.handle_browse_key(key),
            Mode::Filter => self.handle_filter_key(key),
            Mode::AddForm | Mode::EditForm => self.handle_form_key(key),
            Mode::ConfirmRemove => self.handle_remove_key(key),
            Mode::ConfirmTrust => self.handle_trust_key(key),
        }
    }

    fn handle_paste(&mut self, text: &str) {
        self.status = None;
        match self.mode {
            Mode::Filter => {
                self.filter.paste(text);
                self.refilter();
            }
            Mode::AddForm | Mode::EditForm => {
                self.form.values[self.form.active].paste(text);
                self.form.error = None;
                self.form.test_result = None;
            }
            Mode::Browse | Mode::ConfirmRemove | Mode::ConfirmTrust => {}
        }
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
        match key.code {
            KeyCode::Esc => self.mode = Mode::Browse,
            KeyCode::Up | KeyCode::BackTab => self.form.active = self.form.active.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab => {
                self.form.active = (self.form.active + 1).min(FORM_FIELDS.len() - 1)
            }
            KeyCode::Enter if key.modifiers.is_empty() => return self.submit_form(),
            KeyCode::Char('t') if key.modifiers == KeyModifiers::CONTROL => {
                return self.test_form();
            }
            _ => {
                if self.form.values[self.form.active].handle_key(key) {
                    self.form.error = None;
                    self.form.test_result = None;
                }
            }
        }
        UiAction::None
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

    /// Shows a finished test: a saved host's result marks it in the list and the details pane
    /// and goes to the status line; a form's result goes to the form, if it is still open.
    fn record_test(&mut self, finished: Finished, finished_at: String) {
        let Finished {
            origin,
            target,
            outcome,
        } = finished;
        let (passed, line, hint) = match &outcome {
            Ok(TestOutcome::UnknownHostKey { key, .. }) if self.can_show(&origin) => {
                self.trust = Some(PendingTrust {
                    origin,
                    target,
                    key: key.clone(),
                    previous: self.mode.clone(),
                });
                self.mode = Mode::ConfirmTrust;
                return;
            }
            Ok(TestOutcome::Done(report) | TestOutcome::UnknownHostKey { report, .. }) => {
                let hint = match &report.result {
                    ProbeResult::Failed(failure) => report
                        .key_fetch_failure
                        .as_ref()
                        .map(|fetch| format!("could not fetch the host key: {fetch}"))
                        .or_else(|| failure.hint().map(str::to_owned)),
                    ProbeResult::Authenticated(_) => None,
                };
                (report.passed(), report.summary(), hint)
            }
            Err(error) => (false, format!("failed \u{2014} {error}"), None),
        };
        let status = match (&origin, hint) {
            (Origin::Host(_), Some(hint)) => format!("{}: {line} ({hint})", target.alias()),
            (Origin::Host(_), None) => format!("{}: {line}", target.alias()),
            (Origin::Form, _) => String::new(),
        };
        self.show_result(origin, passed, line, finished_at);
        if !status.is_empty() {
            self.status = Some(status);
        }
    }

    /// Whether a result from `origin` still has somewhere to be shown.
    fn can_show(&self, origin: &Origin) -> bool {
        match origin {
            Origin::Host(_) => true,
            Origin::Form => self.in_form(),
        }
    }

    /// Records a result: a saved host's marks it in the list and the details pane; a form's goes
    /// to the form, if it is still open.
    fn show_result(&mut self, origin: Origin, passed: bool, line: String, finished_at: String) {
        match origin {
            Origin::Host(host) => {
                self.results.insert(
                    host,
                    TestMark {
                        passed,
                        line,
                        finished_at,
                    },
                );
            }
            Origin::Form => {
                if self.in_form() {
                    self.form.test_result = Some(line);
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
    let footer = footer_text(state);
    let status = state.status_line();
    let footer_height = (footer.chars().count() as u16)
        .div_ceil(frame.area().width.max(1))
        .max(1);
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(
                state
                    .warnings
                    .len()
                    .min(usize::from(frame.area().height / 3)) as u16,
            ),
            Constraint::Length(u16::from(status.is_some())),
            Constraint::Length(footer_height),
        ])
        .split(frame.area());
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
            ListItem::new(format!(
                "{mark}{}  {}",
                host.alias,
                ownership(host.ownership)
            ))
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
            if state.mode == Mode::AddForm {
                "Add host"
            } else {
                "Edit host"
            },
            form_lines(&state.form),
        ),
        Mode::ConfirmRemove => (
            "Remove host",
            vec![Line::raw(format!(
                "Remove {}?",
                state.selected_alias().unwrap_or("selected host")
            ))],
        ),
        Mode::ConfirmTrust => ("Trust host key", trust_lines(state.trust.as_ref())),
        _ => (
            "Details",
            details_text(state)
                .lines()
                .map(|line| Line::raw(line.to_owned()))
                .collect(),
        ),
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

fn footer_text(state: &UiState) -> String {
    match state.mode {
        Mode::Browse => {
            let new_terminal = if state.ctrl_enter {
                "Ctrl+Enter/n new terminal"
            } else {
                "n new terminal"
            };
            let refresh = "r refresh";
            let connect = format!("t test  Enter connect  {new_terminal}  q quit");
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
                        "↑↓ move  / filter{clear_filter}  a add  e edit  x remove  {refresh}  {connect}"
                    )
                }
                Some(Ownership::Managed) => {
                    format!(
                        "↑↓ move  / filter{clear_filter}  a add  x remove  {refresh}  {connect}"
                    )
                }
                Some(Ownership::External) => {
                    format!("↑↓ move  / filter{clear_filter}  a add  {refresh}  {connect}")
                }
                None => format!("/ filter{clear_filter}  a add  {refresh}  q quit"),
            }
        }
        Mode::Filter => "type to filter  ←→ move  Enter apply  Esc cancel".into(),
        Mode::AddForm | Mode::EditForm => {
            "↑↓ field  ←→ move  Enter save  Ctrl+T test  Esc cancel".into()
        }
        Mode::ConfirmRemove => "Enter remove  Esc cancel".into(),
        Mode::ConfirmTrust => "y trust and test again  n/Esc keep it untrusted".into(),
    }
}

fn form_lines(form: &FormState) -> Vec<Line<'static>> {
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
        lines.push(Line::raw(format!("Connection test: {result}")));
    }
    lines
}

fn details_text(state: &UiState) -> String {
    let Some(selected) = state.selected_summary() else {
        return "No host selected".into();
    };
    let text = match state.details.get(selected) {
        Some(Ok(details)) => detail_text(details),
        Some(Err(error)) => format!("Could not load details: {error}"),
        None => "Resolving…".into(),
    };
    match state.results.get(selected) {
        Some(mark) => format!(
            "{}\n\nConnection test: {} at {}",
            text.trim_end(),
            mark.line,
            mark.finished_at
        ),
        None => text,
    }
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
fn detail_text(details: &HostDetails) -> String {
    let mut text = format!(
        "Ownership: {}\nSource: {}\n\n",
        ownership(details.summary.ownership),
        details.summary.source.display(),
    );
    let declared = details.declared.as_ref().map(declared_rows);
    match &details.resolution {
        Ok(resolved) => {
            let rows = resolved_rows(resolved);
            push_rows(&mut text, &rows);
            let overridden = declared
                .iter()
                .flatten()
                .zip(&rows)
                .filter(|(declared, effective)| !declared_in_effect(declared, effective))
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
/// conflicts, and `ssh -G` reports `~` paths already expanded.
fn declared_in_effect(declared: &DetailRow, effective: &DetailRow) -> bool {
    declared.values.iter().all(|value| {
        effective
            .values
            .iter()
            .any(|used| used == value || *used == expand_home(value))
    })
}

fn expand_home(path: &str) -> String {
    match (path.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{}/{rest}", home.trim_end_matches('/')),
        _ => path.into(),
    }
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
    use std::{path::PathBuf, time::Instant};

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
                known_hosts_files: vec!["~/.ssh/known_hosts".into()],
            }),
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
        let text = rendered(&state);
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
        assert_eq!(press(&mut state, KeyCode::Char('r')), UiAction::None);
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
        let before = state.clone();

        let changed = apply_refresh(&mut state, || Err::<HostInventory, _>("cannot read config"));

        assert!(!changed);
        assert_eq!(
            state.status.as_deref(),
            Some("Refresh failed: cannot read config")
        );
        assert_eq!(state.hosts, before.hosts);
        assert_eq!(state.selected, before.selected);
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
        let home = std::env::var("HOME").unwrap();
        let effective = DetailRow::new("IdentityFile", [format!("{home}/.ssh/id")]);

        assert!(declared_in_effect(&declared, &effective));
        assert!(declared_in_effect(
            &DetailRow::new("IdentityFile", []),
            &effective
        ));
        assert!(!declared_in_effect(
            &DetailRow::new("IdentityFile", ["~/.ssh/other".to_owned()]),
            &effective
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

    #[test]
    fn sigint_ends_the_tui_unless_a_child_owns_the_terminal() {
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
    fn rejections_before_activation_leave_the_inventory_alone() {
        let rejected = [
            AppError::Transaction(TransactionError::Validation("bad".into())),
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
        assert_eq!(press(&mut state, KeyCode::Char('t')), UiAction::None);
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
            state.form.test_result.as_deref(),
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
        let changed = TestOutcome::Done(report(
            "dev",
            ProbeResult::Failed(crate::probe::Failure::HostKeyChanged),
        ));

        state.record_test(host_finished(dev.clone(), Ok(changed)), "12:34".into());

        assert_eq!(state.mode, Mode::Browse);
        assert!(state.trust.is_none());
        assert!(!state.results.get(&dev).unwrap().passed);
        let status = state.status.unwrap();
        assert!(status.starts_with("dev: failed"), "{status}");
        assert!(status.contains("can mean an attack"), "{status}");
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
    fn local_time_is_hours_and_minutes() {
        let time = local_time();
        let (hours, minutes) = time.split_once(':').unwrap();
        assert_eq!((hours.len(), minutes.len()), (2, 2), "{time}");
        assert!(hours.parse::<u8>().unwrap() < 24);
        assert!(minutes.parse::<u8>().unwrap() < 60);
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
}
