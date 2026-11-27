//! The event loop: the tray icon, its menu, and `portico` run on the worker.
//! Nothing runs on a timer: the loop sleeps until a click or an answer.
//!
//! The menu is never replaced while it may be open. muda destroys a menu that
//! is dropped (macOS cancels its tracking, Windows calls `DestroyMenu`), and on
//! Windows a click in a menu that was replaced while open is looked up in the
//! new one. So a change of state only marks the menu out of date, and the new
//! menu is installed at the end of an event that runs while no menu is open:
//! on Windows the tray icon's mouse-down (the menu shows on mouse-up) or any
//! event outside `TrackPopupMenu`; on macOS, where the menu shows inside the
//! mouse-down and every event of ours runs inside its tracking loop, the first
//! event once the run loop is back in its default mode (tao reports one each
//! time the loop is about to wait). The replaced menu is kept alive until the
//! next install all the same. An answer that arrives while the menu is open
//! therefore shows the next time it opens.

use crate::menu::{self, Click, State};
use portico_tray::instance;
use portico_tray::portico::{self, Output, Request, Worker};
use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use tao::event::{Event, StartCause};
use tao::event_loop::EventLoopProxy;
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::{Menu, MenuEvent, MenuId};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

enum UserEvent {
    /// An answer of the worker of that generation.
    Answer(u64, Request, io::Result<Output>),
    Tray(TrayIconEvent),
    Menu(MenuEvent),
}

#[cfg(target_os = "macos")]
const ICON: &[u8] = include_bytes!("../assets/tray-template.png");
#[cfg(windows)]
const ICON: &[u8] = include_bytes!("../assets/tray.png");

fn icon() -> Option<Icon> {
    let mut reader = png::Decoder::new(io::Cursor::new(ICON)).read_info().ok()?;
    let mut pixels = vec![0; reader.output_buffer_size()?];
    let frame = reader.next_frame(&mut pixels).ok()?;
    if frame.color_type != png::ColorType::Rgba || frame.bit_depth != png::BitDepth::Eight {
        return None;
    }
    pixels.truncate(frame.buffer_size());
    Icon::from_rgba(pixels, frame.width, frame.height).ok()
}

/// An environment variable that holds an absolute path.
fn absolute(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

fn home() -> Option<PathBuf> {
    #[cfg(windows)]
    let var = "USERPROFILE";
    #[cfg(not(windows))]
    let var = "HOME";
    absolute(var)
}

/// Where the app keeps its lock and update log; `None` without a home (never
/// a path relative to wherever the app was started).
fn support_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let base = absolute("LOCALAPPDATA").or_else(|| home().map(|h| h.join(r"AppData\Local")));
    #[cfg(not(windows))]
    let base = home().map(|h| h.join("Library/Application Support"));
    base.map(|dir| dir.join("Portico"))
}

/// The `portico` to run: `recorded` when it still is one, otherwise the usual places.
fn locate(recorded: Option<&Path>) -> Option<PathBuf> {
    let path: Option<OsString> = std::env::var_os("PATH");
    // Without a home, the home-relative places are looked for under the root.
    let home = home().unwrap_or_else(|| PathBuf::from(std::path::MAIN_SEPARATOR_STR));
    portico::locate(recorded, path.as_deref(), &home)
}

/// Whether a menu of this thread may be open right now.
#[cfg(windows)]
fn menu_may_be_open() -> bool {
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GUI_INMENUMODE, GUI_POPUPMENUMODE, GUITHREADINFO, GetGUIThreadInfo,
    };
    // SAFETY: an all-zero GUITHREADINFO is valid; `cbSize` is set as the call requires.
    let mut info: GUITHREADINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<GUITHREADINFO>() as u32;
    // SAFETY: `info` is a valid, writable GUITHREADINFO for the call's duration.
    if unsafe { GetGUIThreadInfo(GetCurrentThreadId(), &mut info) } == 0 {
        // Unknown: assume open, and install at a later event.
        return true;
    }
    info.flags & (GUI_INMENUMODE | GUI_POPUPMENUMODE) != 0
}

/// Whether a menu may be open right now: menu tracking runs the run loop in
/// its own mode, so any mode other than the default one counts as open.
#[cfg(target_os = "macos")]
fn menu_may_be_open() -> bool {
    use std::ffi::c_void;
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFRunLoopDefaultMode: *const c_void;
        fn CFRunLoopGetCurrent() -> *mut c_void;
        fn CFRunLoopCopyCurrentMode(run_loop: *mut c_void) -> *const c_void;
        fn CFEqual(a: *const c_void, b: *const c_void) -> u8;
        fn CFRelease(object: *const c_void);
    }
    // SAFETY: plain CoreFoundation calls on this thread's run loop; the copied
    // mode is released once compared.
    unsafe {
        let mode = CFRunLoopCopyCurrentMode(CFRunLoopGetCurrent());
        if mode.is_null() {
            // The run loop is not running: no menu is tracking.
            return false;
        }
        let default = CFEqual(mode, kCFRunLoopDefaultMode) != 0;
        CFRelease(mode);
        !default
    }
}

/// A worker of its own generation: its answers reach the event loop tagged,
/// so those of a worker that was replaced are dropped.
fn spawn_worker(program: PathBuf, generation: u64, proxy: EventLoopProxy<UserEvent>) -> Worker {
    let (sender, answers) = mpsc::channel();
    // Ends once the worker and its threads are gone.
    std::thread::spawn(move || {
        for (request, result) in answers {
            let answer = UserEvent::Answer(generation, request, result);
            if proxy.send_event(answer).is_err() {
                break;
            }
        }
    });
    Worker::spawn(program, sender)
}

struct App {
    /// The single-instance lock, held for the app's life.
    _lock: Option<instance::Instance>,
    recorded: Option<PathBuf>,
    /// The self-update log; `None` without a support directory.
    log: Option<PathBuf>,
    state: State,
    program: Option<PathBuf>,
    worker: Option<Worker>,
    /// The current worker's generation; answers of others are dropped.
    generation: u64,
    proxy: EventLoopProxy<UserEvent>,
    tray: Option<TrayIcon>,
    /// The menu the tray shows, and what its items do.
    shown: Option<Menu>,
    clicks: HashMap<MenuId, Click>,
    /// The state changed since `shown` was built.
    outdated: bool,
    /// The menus replaced at the last install, kept alive until the next one.
    retired: Vec<Menu>,
}

impl App {
    fn send(&self, requests: Vec<Request>) {
        if let Some(worker) = &self.worker {
            for request in requests {
                worker.send(request);
            }
        }
    }

    /// Finds `portico` again (it may have been installed or moved since), then
    /// reads everything.
    fn refresh(&mut self) {
        let found = locate(self.recorded.as_deref());
        if found != self.program || self.worker.is_none() {
            self.state.forget_pending();
            self.generation += 1;
            let generation = self.generation;
            self.worker = found
                .clone()
                .map(|program| spawn_worker(program, generation, self.proxy.clone()));
        }
        self.state.program = found
            .as_deref()
            .or(self.recorded.as_deref())
            .map(|p| p.display().to_string());
        self.state.missing = found.is_none();
        self.program = found;
        let requests = self.state.refresh();
        self.send(requests);
    }

    /// Installs a menu for the current state if it changed and no menu may be
    /// open (see the module's comment).
    fn install_if_closed(&mut self) {
        let Some(tray) = &mut self.tray else { return };
        if !self.outdated || menu_may_be_open() {
            return;
        }
        let (menu, clicks) = menu::build(&self.state);
        self.retired.clear();
        self.retired.extend(self.shown.replace(menu.clone()));
        tray.set_menu(Some(Box::new(menu)));
        self.clicks = clicks;
        self.outdated = false;
    }

    /// Whether the app goes on.
    fn on_click(&mut self, click: Click) -> bool {
        match click {
            Click::Quit => return false,
            Click::Refresh => self.refresh(),
            Click::Update => {
                let Some(program) = &self.program else {
                    return true;
                };
                let Some(log) = &self.log else {
                    self.state.update_failure =
                        Some("no folder for the update log (HOME is not set)".into());
                    return true;
                };
                match portico::start_update(program, log) {
                    Ok(()) => return false,
                    Err(error) => {
                        self.state.update_failure =
                            Some(format!("could not start portico: {error}"));
                    }
                }
            }
            other => {
                if let Some(request) = self.state.click(&other) {
                    self.send(vec![request]);
                }
            }
        }
        true
    }

    fn start(&mut self) {
        let (menu, clicks) = menu::build(&self.state);
        self.clicks = clicks;
        self.shown = Some(menu.clone());
        self.outdated = false;
        let builder = TrayIconBuilder::new()
            .with_tooltip("Portico")
            .with_menu(Box::new(menu));
        let builder = match icon() {
            #[cfg(target_os = "macos")]
            Some(icon) => builder.with_icon_templated(icon),
            #[cfg(windows)]
            Some(icon) => builder.with_icon(icon),
            None => builder.with_title("Portico"),
        };
        match builder.build() {
            Ok(tray) => self.tray = Some(tray),
            Err(error) => {
                eprintln!("portico-tray: cannot show the tray icon: {error}");
                std::process::exit(1);
            }
        }
    }
}

/// Runs the app until Quit. `recorded` is the `--portico` path.
pub fn run(version: &str, recorded: Option<PathBuf>) -> ! {
    let dir = support_dir();
    let lock = match dir.as_deref().map(instance::acquire) {
        Some(Ok(Some(lock))) => Some(lock),
        Some(Ok(None)) => std::process::exit(0),
        Some(Err(error)) => {
            eprintln!("portico-tray: cannot take the single-instance lock: {error}");
            None
        }
        None => {
            eprintln!("portico-tray: no home folder: running without the single-instance lock");
            None
        }
    };
    let log = dir.map(|dir| dir.join("tray-update.log"));

    #[allow(unused_mut)]
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        event_loop.set_activation_policy(ActivationPolicy::Accessory);
    }

    let proxy = event_loop.create_proxy();
    TrayIconEvent::set_event_handler(Some(move |event| {
        let _ = proxy.send_event(UserEvent::Tray(event));
    }));
    let proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(std::sync::Arc::new(move |event| {
        let _ = proxy.send_event(UserEvent::Menu(event));
    })));

    let mut state = State::new(version, None);
    state.update_failure = std::env::current_exe()
        .and_then(std::fs::metadata)
        .and_then(|meta| meta.modified())
        .ok()
        .zip(log.as_deref())
        .and_then(|(modified, log)| portico::update_failure(log, modified))
        .map(|line| line.trim_start_matches("error:").trim().to_string());
    let mut app = App {
        _lock: lock,
        recorded,
        log,
        state,
        program: None,
        worker: None,
        generation: 0,
        proxy: event_loop.create_proxy(),
        tray: None,
        shown: None,
        clicks: HashMap::new(),
        outdated: false,
        retired: Vec::new(),
    };
    app.refresh();

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            // macOS wants the tray icon created once the loop runs.
            Event::NewEvents(StartCause::Init) => app.start(),
            Event::UserEvent(UserEvent::Answer(generation, request, result)) => {
                if generation == app.generation {
                    app.state.apply(request, result);
                    app.outdated = true;
                }
            }
            Event::UserEvent(UserEvent::Tray(TrayIconEvent::Click {
                button: MouseButton::Left | MouseButton::Right,
                button_state: MouseButtonState::Down,
                ..
            })) => {
                // The menu opens: list the tunnels for the next time.
                let requests = app.state.menu_opened();
                app.send(requests);
            }
            Event::UserEvent(UserEvent::Menu(event)) => {
                if let Some(click) = app.clicks.get(&event.id).cloned() {
                    if app.on_click(click) {
                        app.outdated = true;
                    } else {
                        // Removes the icon now rather than when Windows next notices.
                        app.tray = None;
                        *control_flow = ControlFlow::Exit;
                    }
                }
            }
            _ => {}
        }
        app.install_if_closed();
    })
}
