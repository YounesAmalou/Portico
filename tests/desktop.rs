//! `portico desktop` against fake `omarchy-shell`, `omarchy`, `qs`, `hyprctl` and `swaymsg`, in a
//! temporary home. Nothing here reads or writes the real `~/.config`.

#[path = "support/executable.rs"]
mod executable;

use std::{
    fs,
    io::Write,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use executable::write_executable;

/// The maintainer's three `P` bindings, from `hyprctl binds -j`.
const BINDS: &str = r#"[
  {"mouse": false, "modmask": 64, "submap": "", "key": "P", "description": "Pseudo window", "dispatcher": "__lua", "arg": "19"},
  {"mouse": false, "modmask": 68, "submap": "", "key": "P", "description": "Power", "dispatcher": "__lua", "arg": "243"},
  {"mouse": false, "modmask": 65, "submap": "", "key": "P", "description": "Google Photos", "dispatcher": "__lua", "arg": "323"}
]"#;

const BINDINGS_LUA: &str = "-- Keep only your personal keybinding overrides here.\no.bind(\"SUPER + CTRL + PRINT\", \"OCR\", \"omarchy-ocr-text\")\n";

struct Desktop {
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    bin: PathBuf,
    hyprland: bool,
    sway: bool,
}

impl Desktop {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let home = root.join("home");
        let bin = root.join("bin");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&bin).unwrap();
        fs::write(root.join("binds.json"), BINDS).unwrap();
        Self {
            _dir: dir,
            root,
            home,
            bin,
            hyprland: false,
            sway: false,
        }
    }

    /// Omarchy on Hyprland: its programs, plugins directory and `bindings.lua`.
    fn omarchy() -> Self {
        let mut desktop = Self::new();
        desktop.hyprland = true;
        fs::create_dir_all(desktop.home.join(".config/omarchy/plugins")).unwrap();
        fs::create_dir_all(desktop.home.join(".config/hypr")).unwrap();
        fs::write(desktop.home.join(".config/hypr/bindings.lua"), BINDINGS_LUA).unwrap();
        desktop.program("omarchy-shell", "exit 0");
        desktop.program(
            "omarchy",
            &format!(
                "case \"$2\" in\n  list) if [ -f '{0}/enabled' ]; then printf '[{{\"id\":\"portico\",\"enabled\":true}}]\\n'; else printf '[]\\n'; fi ;;\n  enable) [ -f '{0}/fail-enable' ] && {{ echo 'omarchy-plugin-enable: plugin is not known' >&2; exit 1; }}; : > '{0}/enabled'; echo \"Enabled $3\" ;;\n  disable) rm -f '{0}/enabled'; echo \"Disabled $3\" ;;\nesac",
                desktop.root.display()
            ),
        );
        desktop.hyprctl();
        desktop
    }

    /// Plain Quickshell on Hyprland, whose `hyprland.conf` sources `bindings.conf`.
    fn quickshell_hyprland() -> Self {
        let mut desktop = Self::new();
        desktop.hyprland = true;
        let hypr = desktop.home.join(".config/hypr");
        fs::create_dir_all(&hypr).unwrap();
        fs::write(
            hypr.join("hyprland.conf"),
            "source = ~/.config/hypr/bindings.conf\nexec-once = waybar\n",
        )
        .unwrap();
        fs::write(hypr.join("bindings.conf"), "bind = SUPER, Q, exec, foot\n").unwrap();
        desktop.qs();
        desktop.hyprctl();
        desktop
    }

    fn qs(&self) {
        self.program(
            "qs",
            &format!(
                "case \"$1\" in\n  list) if [ -f '{0}/running' ]; then echo '[{{\"id\":\"x\"}}]'; else echo '[]'; fi ;;\n  kill) rm -f '{0}/running' ;;\n  *) : > '{0}/running' ;;\nesac",
                self.root.display()
            ),
        );
    }

    fn hyprctl(&self) {
        self.program(
            "hyprctl",
            &format!("exec /bin/cat '{}/binds.json'", self.root.display()),
        );
    }

    /// A fake program that logs its arguments, then runs `body`.
    fn program(&self, name: &str, body: &str) {
        write_executable(
            self.bin.join(name),
            &format!(
                "#!/bin/sh\nprintf '%s %s\\n' '{name}' \"$*\" >> '{}/calls'\n{body}\n",
                self.root.display()
            ),
        );
    }

    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.root.join("calls"))
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.contains(" list"))
            .filter(|line| !line.starts_with("hyprctl"))
            .map(str::to_string)
            .collect()
    }

    fn command(&self, args: &[&str]) -> Command {
        self.command_with(Path::new(env!("CARGO_BIN_EXE_portico")), args)
    }

    /// A copy of `portico` outside cargo's target directory, as `cargo install` leaves it: only
    /// such a binary refreshes an installed panel.
    fn installed_binary(&self) -> PathBuf {
        let dir = self.root.join("installed");
        let program = dir.join("portico");
        if !program.exists() {
            fs::create_dir_all(&dir).unwrap();
            fs::copy(env!("CARGO_BIN_EXE_portico"), &program).unwrap();
        }
        program
    }

    /// Runs the installed copy, retrying while another test's fork still holds it open.
    fn run_installed(&self, args: &[&str]) -> Output {
        let program = self.installed_binary();
        for _ in 0..100 {
            match self
                .command_with(&program, args)
                .stdin(Stdio::null())
                .output()
            {
                Err(error) if error.raw_os_error() == Some(26) => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                result => return result.unwrap(),
            }
        }
        panic!("{} stayed busy", program.display());
    }

    fn command_with(&self, program: &Path, args: &[&str]) -> Command {
        let mut command = Command::new(program);
        command
            .args(args)
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", &self.bin)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if self.hyprland {
            command.env("HYPRLAND_INSTANCE_SIGNATURE", "test");
        }
        if self.sway {
            command.env("SWAYSOCK", "/run/sway.sock");
        }
        command
    }

    /// Runs `portico` with `stdin` as its input (not a terminal).
    fn run(&self, args: &[&str], stdin: &str) -> Output {
        let mut child = self.command(args).spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn panel(&self) -> PathBuf {
        self.home.join(".local/share/portico/panel")
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.home.join(relative)
    }
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

#[test]
fn installs_everything_on_omarchy_with_yes() {
    let desktop = Desktop::omarchy();
    let output = desktop.run(&["desktop", "install", "--yes"], "");
    let out = text(&output);
    assert!(output.status.success(), "{out}");
    assert!(out.contains("desktop integration for Omarchy (omarchy-shell found, ~/.config/omarchy/plugins exists)"), "{out}");
    assert!(
        out.contains("Keybinding: SUPER + ALT + P, free (checked with hyprctl binds)"),
        "{out}"
    );

    let panel = desktop.panel();
    assert!(panel.join("shell.qml").is_file());
    assert!(read(&panel.join("VERSION")).starts_with(env!("CARGO_PKG_VERSION")));
    assert_eq!(
        fs::read_link(desktop.path(".config/omarchy/plugins/portico")).unwrap(),
        panel
    );
    assert_eq!(
        desktop.calls(),
        [
            "omarchy-shell shell rescanPlugins",
            "omarchy plugin enable portico"
        ]
    );
    let entry = read(&desktop.path(".local/share/applications/portico.desktop"));
    assert!(
        entry.contains(&format!("Exec={}", env!("CARGO_BIN_EXE_portico"))),
        "{entry}"
    );
    assert!(
        desktop
            .path(".local/share/icons/hicolor/scalable/apps/portico.svg")
            .is_file()
    );
    assert!(out.contains("is a cargo build directory's binary"), "{out}");
    assert_eq!(
        read(&desktop.path(".config/hypr/bindings.lua")),
        format!(
            "{BINDINGS_LUA}-- portico desktop install: begin\no.bind(\"SUPER + ALT + P\", \"Portico\", \"omarchy-shell shell toggle portico\")\n-- portico desktop install: end\n"
        )
    );
    let backups: Vec<_> = fs::read_dir(desktop.path(".config/hypr"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("bindings.lua.portico-bak."))
        .collect();
    assert_eq!(backups.len(), 1, "{backups:?}");

    // A second run finds everything in place.
    let again = desktop.run(&["desktop", "install", "--yes"], "");
    let again_out = text(&again);
    assert!(again.status.success(), "{again_out}");
    assert!(again_out.contains("Nothing to install."), "{again_out}");
    assert!(
        again_out.contains("Keybinding: already bound in ~/.config/hypr/bindings.lua, line 4"),
        "{again_out}"
    );
}

#[test]
fn refuses_to_ask_without_a_terminal() {
    let desktop = Desktop::omarchy();
    // The checkout link of the manual steps is replaced.
    symlink(
        "/checkout/desktop/quickshell",
        desktop.path(".config/omarchy/plugins/portico"),
    )
    .unwrap();
    // Not a terminal: the prompts need --yes (the questions are tested in `cli::desktop`).
    let output = desktop.run(&["desktop", "install"], "y\n");
    assert!(!output.status.success());
    assert!(text(&output).contains("desktop install requires --yes in non-interactive mode"));
    assert!(text(&output).contains(
        "replace  ~/.config/omarchy/plugins/portico, a link to /checkout/desktop/quickshell"
    ));
    assert!(!desktop.panel().exists(), "nothing changed");
    assert_eq!(
        read(&desktop.path(".config/hypr/bindings.lua")),
        BINDINGS_LUA
    );
}

#[test]
fn refuses_a_taken_combination_with_yes_before_any_change() {
    let desktop = Desktop::omarchy();
    let output = desktop.run(&["desktop", "install", "--yes", "--bind", "super+p"], "");
    let out = text(&output);
    assert_eq!(output.status.code(), Some(1), "{out}");
    assert!(
        out.contains("Keybinding: SUPER + P is taken (Pseudo window)"),
        "{out}"
    );
    assert!(out.contains("error: SUPER + P is taken (Pseudo window). Choose another with --bind, or skip the keybinding with --no-bind."), "{out}");
    assert!(!desktop.panel().exists());
    assert!(desktop.calls().is_empty());
    assert_eq!(
        read(&desktop.path(".config/hypr/bindings.lua")),
        BINDINGS_LUA
    );
}

#[test]
fn no_bind_and_no_launcher_leave_those_out() {
    let desktop = Desktop::omarchy();
    let output = desktop.run(
        &["desktop", "install", "--yes", "--no-bind", "--no-launcher"],
        "",
    );
    assert!(output.status.success(), "{}", text(&output));
    assert!(desktop.panel().is_dir());
    assert!(!desktop.path(".local/share/applications").exists());
    assert_eq!(
        read(&desktop.path(".config/hypr/bindings.lua")),
        BINDINGS_LUA
    );
}

#[test]
fn a_failed_command_is_reported_and_later_components_still_install() {
    let desktop = Desktop::omarchy();
    fs::write(desktop.root.join("fail-enable"), "").unwrap();
    let output = desktop.run(&["desktop", "install", "--yes"], "");
    let out = text(&output);
    assert_eq!(output.status.code(), Some(1), "{out}");
    assert!(
        out.contains("error: omarchy plugin enable portico failed"),
        "{out}"
    );
    assert!(out.contains("plugin is not known"), "{out}");
    assert!(out.contains("Some steps failed"), "{out}");
    assert!(
        desktop
            .path(".local/share/applications/portico.desktop")
            .is_file()
    );
    assert!(
        read(&desktop.path(".config/hypr/bindings.lua")).contains("portico desktop install: begin")
    );
}

#[test]
fn installs_for_plain_quickshell_on_hyprland() {
    let desktop = Desktop::quickshell_hyprland();
    let output = desktop.run(&["desktop", "install", "--yes"], "");
    let out = text(&output);
    assert!(output.status.success(), "{out}");
    assert!(
        out.contains("desktop integration for Quickshell (qs found; no Omarchy)"),
        "{out}"
    );
    assert!(
        out.contains(
            "append   to ~/.config/hypr/bindings.conf (sourced by hyprland.conf), after line 1:"
        ),
        "{out}"
    );
    assert!(out.contains("exec-once = qs -c portico"), "{out}");
    assert_eq!(
        fs::read_link(desktop.path(".config/quickshell/portico")).unwrap(),
        desktop.panel()
    );
    assert_eq!(desktop.calls(), ["qs -c portico -n -d"]);
    assert_eq!(
        read(&desktop.path(".config/hypr/bindings.conf")),
        "bind = SUPER, Q, exec, foot\n# portico desktop install: begin\nbind = SUPER ALT, P, exec, qs -c portico ipc call portico toggle\n# portico desktop install: end\n"
    );
}

#[test]
fn binds_in_sway_and_reloads_it() {
    let mut desktop = Desktop::new();
    desktop.sway = true;
    desktop.qs();
    desktop.program("swaymsg", "exit 0");
    fs::create_dir_all(desktop.path(".config/sway")).unwrap();
    fs::write(
        desktop.path(".config/sway/config"),
        "set $mod Mod4\nbindsym $mod+p exec x\n",
    )
    .unwrap();
    let taken = desktop.run(&["desktop", "install", "--yes", "--bind", "SUPER+P"], "");
    assert!(
        text(&taken).contains("SUPER + P is taken (bindsym $mod+p exec x)"),
        "{}",
        text(&taken)
    );
    let output = desktop.run(&["desktop", "install", "--yes"], "");
    assert!(output.status.success(), "{}", text(&output));
    assert!(
        read(&desktop.path(".config/sway/config"))
            .contains("\nbindsym $mod+Alt+p exec qs -c portico ipc call portico toggle\n")
    );
    assert!(desktop.calls().contains(&"swaymsg reload".to_string()));
}

#[test]
fn prints_the_command_for_another_compositor() {
    let desktop = Desktop::new();
    desktop.qs();
    let output = desktop.run(&["desktop", "install", "--yes"], "");
    let out = text(&output);
    assert!(output.status.success(), "{out}");
    assert!(
        out.contains("bind this command yourself:\n  qs -c portico ipc call portico toggle"),
        "{out}"
    );
}

#[test]
fn says_what_is_missing_when_no_target_is_found() {
    let desktop = Desktop::new();
    let output = desktop.run(&["desktop", "install"], "");
    let out = text(&output);
    assert_eq!(output.status.code(), Some(1), "{out}");
    assert!(
        out.contains("Portico found no desktop shell to install the panel into"),
        "{out}"
    );
    assert!(
        out.contains("portico desktop install --quickshell"),
        "{out}"
    );
    assert!(out.contains("portico desktop install --no-panel"), "{out}");

    let forced = desktop.run(&["desktop", "install", "--omarchy"], "");
    assert!(text(&forced).contains("--omarchy needs omarchy-shell on PATH"));

    let launcher = desktop.run(&["desktop", "install", "--no-panel", "--yes"], "");
    assert!(launcher.status.success(), "{}", text(&launcher));
    assert!(
        desktop
            .path(".local/share/applications/portico.desktop")
            .is_file()
    );
}

#[test]
fn never_touches_a_real_directory_at_the_link_location() {
    let desktop = Desktop::omarchy();
    fs::create_dir_all(desktop.path(".config/omarchy/plugins/portico")).unwrap();
    let output = desktop.run(&["desktop", "install", "--yes"], "");
    let out = text(&output);
    assert!(
        out.contains(
            "Panel: unavailable, ~/.config/omarchy/plugins/portico exists and is not a link"
        ),
        "{out}"
    );
    assert!(
        out.contains("Keybinding: unavailable, it toggles the panel, which cannot be installed"),
        "{out}"
    );
    assert!(!desktop.panel().exists());
}

#[test]
fn status_reports_each_component() {
    let desktop = Desktop::omarchy();
    let before = text(&desktop.run(&["desktop", "status"], ""));
    assert!(before.contains("Panel        not installed"), "{before}");
    assert!(before.contains("Keybinding   not installed"), "{before}");

    desktop.run(&["desktop", "install", "--yes"], "");
    let after = text(&desktop.run(&["desktop", "status"], ""));
    assert!(
        after.contains(&format!(
            "Panel        installed in ~/.local/share/portico/panel, {} (matches this portico)",
            env!("CARGO_PKG_VERSION")
        )),
        "{after}"
    );
    assert!(
        after.contains(
            "Omarchy    ~/.config/omarchy/plugins/portico -> ~/.local/share/portico/panel, enabled"
        ),
        "{after}"
    );
    assert!(after.contains("Quickshell not linked"), "{after}");
    assert!(
        after.contains("Launcher     ~/.local/share/applications/portico.desktop (runs "),
        "{after}"
    );
    assert!(
        after.contains(
            "Keybinding   ~/.config/hypr/bindings.lua, line 4: o.bind(\"SUPER + ALT + P\""
        ),
        "{after}"
    );

    fs::write(desktop.panel().join("VERSION"), "0.0.1+0000000000000000\n").unwrap();
    // `desktop status` itself never refreshes.
    let stale = text(&desktop.run(&["desktop", "status"], ""));
    assert!(
        stale.contains("written by 0.0.1; this portico is"),
        "{stale}"
    );
}

#[test]
fn uninstall_removes_exactly_what_install_created() {
    let desktop = Desktop::omarchy();
    let mine = desktop.path(".local/share/applications/other.desktop");
    fs::create_dir_all(mine.parent().unwrap()).unwrap();
    fs::write(&mine, "[Desktop Entry]\n").unwrap();
    desktop.run(&["desktop", "install", "--yes"], "");
    fs::remove_file(desktop.root.join("calls")).unwrap();

    let refused = desktop.run(&["desktop", "uninstall"], "");
    assert!(text(&refused).contains("desktop uninstall requires --yes"));
    assert!(desktop.panel().exists());

    let output = desktop.run(&["desktop", "uninstall", "--yes"], "");
    let out = text(&output);
    assert!(output.status.success(), "{out}");
    assert!(
        out.contains("remove   lines 3-5 of ~/.config/hypr/bindings.lua:"),
        "{out}"
    );
    assert_eq!(
        read(&desktop.path(".config/hypr/bindings.lua")),
        BINDINGS_LUA
    );
    assert!(!desktop.path(".local/share/portico").exists());
    assert!(fs::symlink_metadata(desktop.path(".config/omarchy/plugins/portico")).is_err());
    assert!(
        !desktop
            .path(".local/share/applications/portico.desktop")
            .exists()
    );
    assert!(
        !desktop
            .path(".local/share/icons/hicolor/scalable/apps/portico.svg")
            .exists()
    );
    assert!(mine.exists(), "the user's own file stays");
    assert_eq!(
        desktop.calls(),
        [
            "omarchy plugin disable portico",
            "omarchy-shell shell rescanPlugins"
        ]
    );

    let nothing = desktop.run(&["desktop", "uninstall", "--yes"], "");
    assert!(text(&nothing).contains("Portico's desktop integration is not installed."));
}

#[test]
fn uninstall_leaves_a_link_to_elsewhere() {
    let desktop = Desktop::quickshell_hyprland();
    fs::create_dir_all(desktop.path(".config/quickshell")).unwrap();
    symlink("/checkout", desktop.path(".config/quickshell/portico")).unwrap();
    let output = desktop.run(&["desktop", "uninstall", "--yes"], "");
    assert!(text(&output).contains("not installed"), "{}", text(&output));
    assert!(fs::symlink_metadata(desktop.path(".config/quickshell/portico")).is_ok());
}

#[test]
fn ordinary_commands_refresh_a_panel_from_an_older_portico_silently() {
    let desktop = Desktop::omarchy();
    let clean = desktop.run_installed(&["hosts", "--json"]);

    desktop.run(
        &["desktop", "install", "--yes", "--no-bind", "--no-launcher"],
        "",
    );
    let panel = desktop.panel();
    fs::write(panel.join("VERSION"), "0.0.1+0000000000000000\n").unwrap();
    fs::write(panel.join("shell.qml"), "old").unwrap();
    fs::write(panel.join("Gone.qml"), "old").unwrap();

    // A cargo build never refreshes: it would rewrite the panel on every run.
    desktop.run(&["hosts", "--json"], "");
    assert_eq!(read(&panel.join("VERSION")), "0.0.1+0000000000000000\n");

    let refreshed = desktop.run_installed(&["hosts", "--json"]);
    assert_eq!(refreshed.stdout, clean.stdout, "the document is unchanged");
    assert_eq!(refreshed.stderr, clean.stderr);
    assert_eq!(refreshed.status.code(), clean.status.code());
    assert!(read(&panel.join("VERSION")).starts_with(env!("CARGO_PKG_VERSION")));
    assert_ne!(read(&panel.join("shell.qml")), "old");
    assert!(!panel.join("Gone.qml").exists());

    // A panel from a newer Portico is left alone, so two binaries never take turns.
    fs::write(panel.join("VERSION"), "999.0.0+0000000000000000\n").unwrap();
    desktop.run_installed(&["hosts", "--json"]);
    assert_eq!(read(&panel.join("VERSION")), "999.0.0+0000000000000000\n");
}

#[test]
fn a_failed_refresh_changes_nothing_about_the_command() {
    use std::os::unix::fs::PermissionsExt;
    let desktop = Desktop::omarchy();
    let clean = desktop.run_installed(&["hosts", "--json"]);
    let panel = desktop.panel();
    fs::create_dir_all(&panel).unwrap();
    fs::write(panel.join("VERSION"), "0.0.1+0000000000000000\n").unwrap();
    fs::set_permissions(&panel, fs::Permissions::from_mode(0o555)).unwrap();
    let output = desktop.run_installed(&["hosts", "--json"]);
    fs::set_permissions(&panel, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(output.stdout, clean.stdout);
    assert_eq!(output.stderr, clean.stderr);
    assert_eq!(output.status.code(), clean.status.code());
    assert_eq!(read(&panel.join("VERSION")), "0.0.1+0000000000000000\n");
}

#[test]
fn a_linked_panel_directory_is_never_written_or_removed() {
    let desktop = Desktop::omarchy();
    let checkout = desktop.root.join("checkout");
    fs::create_dir_all(&checkout).unwrap();
    fs::write(checkout.join("preview.png"), "image").unwrap();
    fs::write(checkout.join("VERSION"), "0.0.1+0000000000000000\n").unwrap();
    fs::create_dir_all(desktop.panel().parent().unwrap()).unwrap();
    symlink(&checkout, desktop.panel()).unwrap();

    desktop.run_installed(&["hosts", "--json"]);
    let install = desktop.run(&["desktop", "install", "--yes"], "");
    let out = text(&install);
    assert!(
        out.contains("Panel: unavailable, ~/.local/share/portico/panel is a link or a file"),
        "{out}"
    );
    let status = text(&desktop.run(&["desktop", "status"], ""));
    assert!(
        status.contains("Panel        blocked: ~/.local/share/portico/panel is a link or a file"),
        "{status}"
    );
    let uninstall = text(&desktop.run(&["desktop", "uninstall", "--yes"], ""));
    assert!(
        uninstall.contains("is a link or a file, not Portico's directory; it is left alone"),
        "{uninstall}"
    );

    assert!(checkout.join("preview.png").exists());
    assert!(!checkout.join("shell.qml").exists());
    assert_eq!(read(&checkout.join("VERSION")), "0.0.1+0000000000000000\n");
    assert!(
        fs::symlink_metadata(desktop.panel())
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn a_linked_bindings_file_keeps_its_link() {
    let desktop = Desktop::omarchy();
    let dotfiles = desktop.root.join("dotfiles/hypr");
    fs::create_dir_all(&dotfiles).unwrap();
    let bindings = desktop.path(".config/hypr/bindings.lua");
    fs::rename(&bindings, dotfiles.join("bindings.lua")).unwrap();
    symlink(dotfiles.join("bindings.lua"), &bindings).unwrap();

    let install = desktop.run(&["desktop", "install", "--yes"], "");
    let out = text(&install);
    assert!(install.status.success(), "{out}");
    assert!(
        out.contains("(linked from ~/.config/hypr/bindings.lua)"),
        "{out}"
    );
    assert!(
        fs::symlink_metadata(&bindings)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(read(&dotfiles.join("bindings.lua")).contains("portico desktop install: begin"));
    assert!(
        fs::read_dir(&dotfiles).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("bindings.lua.portico-bak.")),
        "the backup sits next to the file it copies"
    );

    let uninstall = desktop.run(&["desktop", "uninstall", "--yes"], "");
    assert!(uninstall.status.success(), "{}", text(&uninstall));
    assert!(
        fs::symlink_metadata(&bindings)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(read(&dotfiles.join("bindings.lua")), BINDINGS_LUA);
}

#[test]
fn backups_in_the_same_second_never_overwrite_each_other() {
    let desktop = Desktop::omarchy();
    for _ in 0..3 {
        desktop.run(&["desktop", "install", "--yes"], "");
        desktop.run(&["desktop", "uninstall", "--yes"], "");
    }
    let backups: Vec<String> = fs::read_dir(desktop.path(".config/hypr"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("bindings.lua.portico-bak."))
        .collect();
    assert_eq!(backups.len(), 6, "{backups:?}");
}

#[test]
fn a_link_at_a_launcher_path_is_not_portico_s() {
    let desktop = Desktop::omarchy();
    let entry = desktop.path(".local/share/applications/portico.desktop");
    fs::create_dir_all(entry.parent().unwrap()).unwrap();
    let outside = desktop.root.join("outside.desktop");
    symlink(&outside, &entry).unwrap();
    let output = desktop.run(&["desktop", "install", "--yes"], "");
    let out = text(&output);
    assert!(
        out.contains("Launcher entry: unavailable, ~/.local/share/applications/portico.desktop is a link, not Portico's"),
        "{out}"
    );
    assert!(!outside.exists(), "nothing written through the link");
    desktop.run(&["desktop", "uninstall", "--yes"], "");
    assert!(
        fs::symlink_metadata(&entry).is_ok(),
        "the user's link stays"
    );
}

#[test]
fn omarchy_lives_in_home_config_whatever_xdg_config_home_says() {
    let desktop = Desktop::omarchy();
    let elsewhere = desktop.root.join("xdg-config");
    fs::create_dir_all(&elsewhere).unwrap();
    let output = desktop
        .command(&["desktop", "install", "--yes"])
        .env("XDG_CONFIG_HOME", &elsewhere)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", text(&output));
    assert!(fs::symlink_metadata(desktop.path(".config/omarchy/plugins/portico")).is_ok());
    assert!(
        read(&desktop.path(".config/hypr/bindings.lua")).contains("portico desktop install: begin")
    );
    assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
}

#[test]
fn ordinary_commands_create_nothing_without_a_panel() {
    let desktop = Desktop::omarchy();
    let data = desktop.root.join("data");
    let output = desktop
        .command(&["hosts", "--json"])
        .env("XDG_DATA_HOME", &data)
        .output()
        .unwrap();
    assert!(!output.stdout.is_empty());
    assert!(!data.exists());
    assert!(!desktop.path(".local/share").exists());
}
