//! What `portico desktop status` reports, as data: the CLI prints it and the TUI shows it.

use super::{
    DesktopEnv, Link, Target, installed_bindings, launcher, link_state, omarchy_enabled, panel,
    tray::{self, System, SystemHost, TrayHost},
};

/// One row of the status. `label` is `"Panel"`, `"Launcher"` or `"Keybinding"` on a
/// component's first row and `""` on continuation rows and on the indented per-target rows
/// (whose `text` carries its own two-space indent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub label: &'static str,
    pub text: String,
}

impl Line {
    /// The row as `portico desktop status` prints it: the label in a 12-column column, then
    /// the text. Invariant: a row with an empty label whose text starts with two spaces is a
    /// per-target row, already laid out, and is printed as it is; no other row's text starts
    /// with spaces.
    pub fn render(&self) -> String {
        if self.label.is_empty() && self.text.starts_with("  ") {
            self.text.clone()
        } else {
            format!("{:<12} {}", self.label, self.text)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub lines: Vec<Line>,
    /// Whether any part of the desktop integration is installed.
    pub installed: bool,
}

/// What is installed: the tray app on macOS and Windows, the panel and the rest on Linux.
pub fn status(env: &DesktopEnv) -> Status {
    match System::current() {
        Some(system) => tray_status(env, system, &SystemHost::new(env)),
        None => linux_status(env),
    }
}

/// The tray app's status: the app, its login entry and whether it runs.
pub fn tray_status(env: &DesktopEnv, system: System, host: &dyn TrayHost) -> Status {
    tray_status_with(env, system, host, env!("CARGO_PKG_VERSION"))
}

/// `tray_status` against a `portico` of `version`.
pub fn tray_status_with(
    env: &DesktopEnv,
    system: System,
    host: &dyn TrayHost,
    version: &str,
) -> Status {
    let paths = tray::paths(system, &env.home, env.local_app_data.as_deref());
    let placed = paths.placed(system);
    let present = std::fs::symlink_metadata(placed).is_ok();
    let app = if !present {
        "not installed".to_string()
    } else {
        match host.app_version(placed) {
            Some(installed) if installed == version => format!(
                "installed in {}, {installed} (matches this portico)",
                env.show(placed)
            ),
            Some(installed) => format!(
                "installed in {}, {installed}; this portico is {version}: out of date, run portico desktop install",
                env.show(placed)
            ),
            None => format!("installed in {}, version unknown", env.show(placed)),
        }
    };
    let entry = host.login_entry();
    let login = match &entry {
        None => "not set up".to_string(),
        Some(text) => {
            let name = system.login_name();
            match tray::entry_portico(system, text) {
                Some(portico) if !portico.exists() => format!(
                    "set up ({name}), but points at {}, which no longer exists",
                    env.show(&portico)
                ),
                Some(portico) => format!("set up ({name}), runs {}", env.show(&portico)),
                None => format!("set up ({name}), in a form Portico does not write"),
            }
        }
    };
    let running = host.running();
    let running = if running.is_empty() {
        "no".to_string()
    } else {
        let pids = running
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let noun = if running.len() == 1 {
            "process"
        } else {
            "processes"
        };
        format!("yes ({noun} {pids})")
    };
    Status {
        lines: vec![
            Line {
                label: system.app_name(),
                text: app,
            },
            Line {
                label: "At login",
                text: login,
            },
            Line {
                label: "Running",
                text: running,
            },
        ],
        installed: present || entry.is_some(),
    }
}

/// The panel, the launcher entry and the keybinding.
fn linux_status(env: &DesktopEnv) -> Status {
    let mut lines = Vec::new();
    let mut installed = false;
    let dir = env.panel_dir();
    let version = env!("CARGO_PKG_VERSION");
    let state = panel::state(&dir);
    installed |= matches!(
        state,
        panel::State::Current | panel::State::Stale(_) | panel::State::Modified
    );
    let panel = match state {
        panel::State::Blocked => format!(
            "blocked: {} is a link or a file, not Portico's directory",
            env.show(&dir)
        ),
        panel::State::Missing => "not installed".to_string(),
        panel::State::Current => format!(
            "installed in {}, {version} (matches this portico)",
            env.show(&dir)
        ),
        panel::State::Stale(written) => format!(
            "installed in {}, written by {written}; this portico is {version}, run portico desktop install",
            env.show(&dir)
        ),
        panel::State::Modified => format!(
            "installed in {}, but its files differ from this portico's; run portico desktop install",
            env.show(&dir)
        ),
    };
    lines.push(Line {
        label: "Panel",
        text: panel,
    });
    for target in [Target::Omarchy, Target::Quickshell] {
        let link = target.link(env);
        let state = match link_state(&link, &dir) {
            Link::Missing => "not linked".to_string(),
            Link::Ours => {
                installed = true;
                let mut text = format!("{} -> {}", env.show(&link), env.show(&dir));
                if target == Target::Omarchy {
                    text.push_str(match omarchy_enabled(env) {
                        Some(true) => ", enabled",
                        Some(false) => ", Omarchy plugin disabled",
                        None => ", enabled state unknown (omarchy could not be asked)",
                    });
                }
                text
            }
            Link::Elsewhere(other) => format!(
                "{} links to {}, not Portico's copy",
                env.show(&link),
                env.show(&other)
            ),
            Link::Blocked => format!("{} is not a link, not Portico's", env.show(&link)),
        };
        lines.push(Line {
            label: "",
            text: format!("  {:<10} {state}", target.name()),
        });
    }

    let (desktop_path, icon_path) = launcher::paths(env);
    let mut rows = Vec::new();
    if launcher::is_marked(&desktop_path) {
        installed = true;
        let exec = launcher::exec_of(&desktop_path).unwrap_or_default();
        let runs = if exec.starts_with('/') && !std::path::Path::new(&exec).exists() {
            format!("runs {exec}, which no longer exists")
        } else {
            format!("runs {exec}")
        };
        rows.push(format!("{} ({runs})", env.show(&desktop_path)));
    } else if desktop_path.exists() {
        rows.push(format!(
            "{} is an unmarked file, not Portico's",
            env.show(&desktop_path)
        ));
    }
    if launcher::is_marked(&icon_path) {
        installed = true;
        rows.push(env.show(&icon_path));
    }
    if rows.is_empty() {
        rows.push(match launcher::system_entry(env) {
            Some(system) => format!("provided by {}", system.display()),
            None => "not installed".into(),
        });
    }
    for (index, text) in rows.into_iter().enumerate() {
        lines.push(Line {
            label: if index == 0 { "Launcher" } else { "" },
            text,
        });
    }

    let bindings = installed_bindings(env);
    if bindings.is_empty() {
        lines.push(Line {
            label: "Keybinding",
            text: "not installed".into(),
        });
    }
    for (_, file, block) in bindings {
        let text = match block {
            Ok(block) => {
                installed = true;
                format!(
                    "{}, line {}: {}",
                    env.show(&file),
                    block.first + 2,
                    block.line
                )
            }
            Err(problem) => format!("{}: {problem}", env.show(&file)),
        };
        lines.push(Line {
            label: "Keybinding",
            text,
        });
    }
    Status { lines, installed }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        desktop::keybinding::{self, Setup},
        test_executable::write_executable,
    };
    use std::{ffi::OsString, fs};

    /// Omarchy on Hyprland in a temporary home, with fake programs.
    fn omarchy() -> (tempfile::TempDir, DesktopEnv) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let home = root.join("home");
        let bin = root.join("bin");
        fs::create_dir_all(home.join(".config/omarchy/plugins")).unwrap();
        fs::create_dir_all(home.join(".config/hypr")).unwrap();
        fs::create_dir_all(&bin).unwrap();
        fs::write(home.join(".config/hypr/bindings.lua"), "-- mine\n").unwrap();
        write_executable(bin.join("omarchy-shell"), "#!/bin/sh\nexit 0\n");
        write_executable(
            bin.join("omarchy"),
            "#!/bin/sh\n[ \"$2\" = list ] && echo '[{\"id\":\"portico\",\"enabled\":false}]'\nexit 0\n",
        );
        let path = OsString::from(&bin);
        let env = DesktopEnv::new(
            home,
            move |name| match name {
                "PATH" => Some(path.clone()),
                "HYPRLAND_INSTANCE_SIGNATURE" => Some("x".into()),
                _ => None,
            },
            Some("/usr/bin/portico".into()),
        );
        (dir, env)
    }

    fn add_binding(env: &DesktopEnv) {
        let file = env.config_home.join("hypr/bindings.lua");
        let line = "o.bind(\"SUPER + ALT + P\", \"Portico\", \"x\")";
        let block = keybinding::block_lines(Setup::Omarchy, line);
        let text = fs::read_to_string(&file).unwrap();
        fs::write(&file, keybinding::append_block(&text, &block)).unwrap();
    }

    #[test]
    fn nothing_installed() {
        let (_dir, env) = omarchy();
        let s = status(&env);
        assert!(!s.installed);
        assert_eq!(s.lines[0].label, "Panel");
        assert_eq!(s.lines[0].text, "not installed");
    }

    #[cfg(unix)]
    #[test]
    fn after_install_everything_is_installed() {
        let (_dir, env) = omarchy();
        panel::write(&env.panel_dir()).unwrap();
        let (desktop, icon) = launcher::paths(&env);
        fs::create_dir_all(desktop.parent().unwrap()).unwrap();
        fs::create_dir_all(icon.parent().unwrap()).unwrap();
        fs::write(
            &desktop,
            launcher::desktop_file("/usr/bin/portico".as_ref(), true),
        )
        .unwrap();
        fs::write(&icon, launcher::icon_file()).unwrap();
        add_binding(&env);
        std::os::unix::fs::symlink(env.panel_dir(), Target::Omarchy.link(&env)).unwrap();
        let s = status(&env);
        assert!(s.installed);
        assert!(
            s.lines[0].text.starts_with("installed in "),
            "{:?}",
            s.lines
        );
        assert!(
            s.lines
                .iter()
                .any(|l| l.label == "Launcher" && l.text.contains("runs ")),
            "{:?}",
            s.lines
        );
        assert!(
            s.lines.iter().any(|l| l.label.is_empty()
                && l.text.starts_with("  Omarchy")
                && l.text.contains("->")),
            "{:?}",
            s.lines
        );
        assert!(
            s.lines
                .iter()
                .any(|l| l.label == "Keybinding" && l.text.contains(", line "))
        );
    }

    #[test]
    fn only_a_binding_counts_as_installed() {
        let (_dir, env) = omarchy();
        add_binding(&env);
        let s = status(&env);
        assert!(s.installed);
        assert_eq!(s.lines[0].text, "not installed");
    }

    #[test]
    fn rows_render_as_the_cli_prints_them() {
        let target = Line {
            label: "",
            text: "  omarchy    not linked".into(),
        };
        assert_eq!(target.render(), "  omarchy    not linked");
        let more = Line {
            label: "",
            text: "/x".into(),
        };
        assert_eq!(more.render(), format!("{:<12} /x", ""));
        let first = Line {
            label: "Panel",
            text: "not installed".into(),
        };
        assert_eq!(first.render(), "Panel        not installed");
    }
}
