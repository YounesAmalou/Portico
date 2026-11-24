//! What `portico desktop status` reports, as data: the CLI prints it and the TUI shows it.

use super::{
    DesktopEnv, Link, Target, installed_bindings, launcher, link_state, omarchy_enabled, panel,
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
    /// the text; the indented target rows are printed as they are.
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

pub fn status(env: &DesktopEnv) -> Status {
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
        let s = status(&env);
        assert!(s.installed);
        assert!(
            s.lines[0].text.starts_with("installed in "),
            "{:?}",
            s.lines
        );
        assert!(s.lines.iter().any(|l| l.label == "Launcher"));
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
