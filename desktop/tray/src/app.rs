//! The event loop: the tray icon, its menu, and `portico` run on the worker.
//! Nothing runs on a timer: the loop sleeps until a click or an answer.

use crate::menu::{self, Click, State};
use portico_tray::instance;
use portico_tray::portico::{self, Output, Request, Worker};
use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::menu::{Menu, MenuEvent, MenuId};
use tray_icon::{Icon, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

enum UserEvent {
    Answer(Request, io::Result<Output>),
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

fn home() -> PathBuf {
    #[cfg(windows)]
    let var = "USERPROFILE";
    #[cfg(not(windows))]
    let var = "HOME";
    std::env::var_os(var).map(PathBuf::from).unwrap_or_default()
}

/// Where the app keeps its lock and update log.
fn support_dir() -> PathBuf {
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(r"AppData\Local"))
            .join("Portico")
    }
    #[cfg(not(windows))]
    {
        home().join("Library/Application Support/Portico")
    }
}

/// The `portico` to run: `recorded` when it still is one, otherwise the usual places.
fn locate(recorded: Option<&Path>) -> Option<PathBuf> {
    let path: Option<OsString> = std::env::var_os("PATH");
    portico::locate(recorded, path.as_deref(), &home())
}

struct App {
    /// The single-instance lock, held for the app's life.
    _lock: Option<instance::Instance>,
    recorded: Option<PathBuf>,
    log: PathBuf,
    state: State,
    program: Option<PathBuf>,
    worker: Option<Worker>,
    sender: mpsc::Sender<(Request, io::Result<Output>)>,
    tray: Option<TrayIcon>,
    clicks: HashMap<MenuId, Click>,
    /// Menus replaced since the tray icon was last clicked: one may still be
    /// open, so it is kept alive until the next click.
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
            self.worker = found
                .clone()
                .map(|program| Worker::spawn(program, self.sender.clone()));
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

    fn rebuild(&mut self) {
        let Some(tray) = &mut self.tray else { return };
        let (menu, clicks) = menu::build(&self.state);
        self.retired.push(menu.clone());
        tray.set_menu(Some(Box::new(menu)));
        self.clicks = clicks;
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
                match portico::start_update(program, &self.log) {
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
    let lock = match instance::acquire(&dir) {
        Ok(Some(lock)) => Some(lock),
        Ok(None) => std::process::exit(0),
        Err(error) => {
            eprintln!("portico-tray: cannot take the single-instance lock: {error}");
            None
        }
    };
    let log = dir.join("tray-update.log");

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
    let (sender, answers) = mpsc::channel();
    let proxy = event_loop.create_proxy();
    std::thread::spawn(move || {
        for (request, result) in answers {
            if proxy
                .send_event(UserEvent::Answer(request, result))
                .is_err()
            {
                break;
            }
        }
    });

    let mut state = State::new(version, None);
    state.update_failure = std::env::current_exe()
        .and_then(std::fs::metadata)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| portico::update_failure(&log, modified))
        .map(|line| line.trim_start_matches("error:").trim().to_string());
    let mut app = App {
        _lock: lock,
        recorded,
        log,
        state,
        program: None,
        worker: None,
        sender,
        tray: None,
        clicks: HashMap::new(),
        retired: Vec::new(),
    };
    app.refresh();

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            // macOS wants the tray icon created once the loop runs.
            Event::NewEvents(StartCause::Init) => app.start(),
            Event::UserEvent(UserEvent::Answer(request, result)) => {
                app.state.apply(request, result);
                app.rebuild();
            }
            Event::UserEvent(UserEvent::Tray(TrayIconEvent::Click {
                button_state: MouseButtonState::Down,
                ..
            })) => {
                // A menu opens: none replaced earlier is still open.
                app.retired.clear();
                let requests = app.state.menu_opened();
                app.send(requests);
            }
            Event::UserEvent(UserEvent::Menu(event)) => {
                let Some(click) = app.clicks.get(&event.id).cloned() else {
                    return;
                };
                if app.on_click(click) {
                    app.rebuild();
                } else {
                    // Removes the icon now rather than when Windows next notices.
                    app.tray = None;
                    *control_flow = ControlFlow::Exit;
                }
            }
            _ => {}
        }
    })
}
