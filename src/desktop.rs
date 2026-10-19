//! `portico desktop`: the panel, the launcher entry and the keybinding, worked out as plans of
//! steps that the CLI prints, confirms and applies.
//!
//! Nothing records what was installed: the panel directory is Portico's, links are recognised by
//! their target, launcher files by a marker comment and the keybinding by its marker lines.

pub mod combo;
pub mod keybinding;
pub mod launcher;
pub mod panel;

use std::{
    ffi::OsString,
    fs, io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

use combo::Combo;
use keybinding::{Location, Setup, Unavailable};

/// The environment `portico desktop` works in, read once by `main`.
#[derive(Clone, Debug)]
pub struct DesktopEnv {
    pub home: PathBuf,
    /// `XDG_DATA_HOME`, or `~/.local/share`.
    pub data_home: PathBuf,
    /// `XDG_CONFIG_HOME`, or `~/.config`.
    pub config_home: PathBuf,
    /// `XDG_DATA_DIRS`, or `/usr/local/share` and `/usr/share`.
    pub data_dirs: Vec<PathBuf>,
    pub path: Option<OsString>,
    /// `HYPRLAND_INSTANCE_SIGNATURE` is set.
    pub hyprland: bool,
    /// `SWAYSOCK` is set.
    pub sway: bool,
    /// The running binary, for the launcher entry's `Exec=`.
    pub exe: Option<PathBuf>,
}

impl DesktopEnv {
    /// The environment from `var`, which reads one variable, as the XDG specification says:
    /// a relative or empty value is ignored.
    pub fn new(
        home: PathBuf,
        var: impl Fn(&str) -> Option<OsString>,
        exe: Option<PathBuf>,
    ) -> Self {
        let absolute = |name: &str| {
            var(name)
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
        };
        let set = |name: &str| var(name).is_some_and(|value| !value.is_empty());
        let data_dirs = var("XDG_DATA_DIRS")
            .map(|dirs| {
                std::env::split_paths(&dirs)
                    .filter(|path| path.is_absolute())
                    .collect::<Vec<_>>()
            })
            .filter(|dirs| !dirs.is_empty())
            .unwrap_or_else(|| vec!["/usr/local/share".into(), "/usr/share".into()]);
        Self {
            data_home: absolute("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local/share")),
            config_home: absolute("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config")),
            data_dirs,
            path: var("PATH"),
            hyprland: set("HYPRLAND_INSTANCE_SIGNATURE"),
            sway: set("SWAYSOCK"),
            exe,
            home,
        }
    }

    /// Omarchy's configuration directory: always `~/.config`, since Omarchy's shell, its plugin
    /// commands and its Hyprland bootstrap ignore `XDG_CONFIG_HOME`.
    pub fn omarchy_config(&self) -> PathBuf {
        self.home.join(".config")
    }

    /// Portico's panel directory.
    pub fn panel_dir(&self) -> PathBuf {
        self.data_home.join("portico/panel")
    }

    /// A path with the home directory as `~`.
    pub fn show(&self, path: &Path) -> String {
        crate::home::tilde(path, &self.home)
    }

    /// `program` on `PATH`.
    pub fn find(&self, program: &str) -> Option<PathBuf> {
        std::env::split_paths(self.path.as_deref()?)
            .map(|dir| dir.join(program))
            .find(|path| {
                fs::metadata(path)
                    .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
            })
    }

    /// Runs `program` found on `PATH` with `args`, and returns its stdout, or a message with
    /// the last line it printed on stderr.
    pub fn run(&self, program: &str, args: &[&str]) -> Result<String, String> {
        let path = self
            .find(program)
            .ok_or_else(|| format!("{program} is not on PATH"))?;
        let output = Command::new(path)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|error| format!("could not run {program}: {error}"))?;
        if output.status.success() {
            return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let said = stderr
            .lines()
            .chain(stdout.lines())
            .rfind(|line| !line.trim().is_empty())
            .map(|line| format!(": {}", line.trim()))
            .unwrap_or_default();
        Err(format!(
            "{} failed ({}){said}",
            command_line(program, args),
            output.status
        ))
    }

    /// Whether the running binary is a cargo build's (see `is_development_binary`).
    pub fn is_development_build(&self) -> bool {
        self.exe.as_deref().is_some_and(is_development_binary)
    }
}

/// Whether `exe` is a cargo build's: under a directory holding the `CACHEDIR.TAG` cargo writes at
/// a target directory's root, or under a `target` directory next to a `Cargo.toml`.
pub fn is_development_binary(exe: &Path) -> bool {
    exe.ancestors().skip(1).any(|dir| {
        dir.join("CACHEDIR.TAG").is_file()
            || (dir.file_name().is_some_and(|name| name == "target")
                && dir
                    .parent()
                    .is_some_and(|parent| parent.join("Cargo.toml").is_file()))
    })
}

/// Where the panel is installed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Target {
    Omarchy,
    Quickshell,
}

impl Target {
    pub fn name(self) -> &'static str {
        match self {
            Target::Omarchy => "Omarchy",
            Target::Quickshell => "Quickshell",
        }
    }

    /// The link to the panel directory in the target's configuration.
    pub fn link(self, env: &DesktopEnv) -> PathBuf {
        match self {
            Target::Omarchy => env.omarchy_config().join("omarchy/plugins/portico"),
            Target::Quickshell => env.config_home.join("quickshell/portico"),
        }
    }
}

/// The target found or forced, and why; or what is missing, with the flags to use.
pub fn detect(env: &DesktopEnv, forced: Option<Target>) -> Result<(Target, String), String> {
    let plugins = env.omarchy_config().join("omarchy/plugins");
    let omarchy_shell = env.find("omarchy-shell").is_some();
    let qs = env.find("qs").is_some();
    match forced {
        Some(Target::Omarchy) if omarchy_shell => {
            return Ok((Target::Omarchy, "chosen with --omarchy".into()));
        }
        Some(Target::Omarchy) => {
            return Err("--omarchy needs omarchy-shell on PATH, and it is not there.".into());
        }
        Some(Target::Quickshell) if qs => {
            return Ok((Target::Quickshell, "chosen with --quickshell".into()));
        }
        Some(Target::Quickshell) => {
            return Err("--quickshell needs qs on PATH, and it is not there.".into());
        }
        None => {}
    }
    if omarchy_shell && plugins.is_dir() {
        return Ok((
            Target::Omarchy,
            format!("omarchy-shell found, {} exists", env.show(&plugins)),
        ));
    }
    if qs {
        return Ok((Target::Quickshell, "qs found; no Omarchy".into()));
    }
    let omarchy_missing = match (omarchy_shell, plugins.is_dir()) {
        (false, false) => "neither found",
        (false, true) => "omarchy-shell not found",
        _ => "the plugins directory not found",
    };
    Err(format!(
        "Portico found no desktop shell to install the panel into:\n  \
         Omarchy needs omarchy-shell on PATH and {} ({omarchy_missing}).\n  \
         Quickshell needs qs on PATH (not found).\n\
         Install Quickshell, or Omarchy, then run:\n  \
         portico desktop install --quickshell\n  \
         portico desktop install --omarchy\n\
         To install only the launcher entry: portico desktop install --no-panel",
        env.show(&plugins)
    ))
}

/// What is at a link location.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Link {
    Missing,
    /// A link to the panel directory.
    Ours,
    /// A link elsewhere, or a dangling one: replaced on install, left on uninstall.
    Elsewhere(PathBuf),
    /// A real file or directory: never touched.
    Blocked,
}

pub fn link_state(path: &Path, panel_dir: &Path) -> Link {
    match fs::symlink_metadata(path) {
        Err(_) => Link::Missing,
        Ok(meta) if meta.file_type().is_symlink() => match fs::read_link(path) {
            Ok(target) if target == panel_dir => Link::Ours,
            Ok(target) => Link::Elsewhere(target),
            Err(_) => Link::Elsewhere(PathBuf::new()),
        },
        Ok(_) => Link::Blocked,
    }
}

/// Whether Omarchy has the plugin enabled; `None` when it could not be asked.
pub fn omarchy_enabled(env: &DesktopEnv) -> Option<bool> {
    let listing = env.run("omarchy", &["plugin", "list", "--json"]).ok()?;
    let plugins: serde_json::Value = serde_json::from_str(&listing).ok()?;
    Some(plugins.as_array()?.iter().any(|plugin| {
        plugin.get("id").and_then(serde_json::Value::as_str) == Some("portico")
            && plugin.get("enabled").and_then(serde_json::Value::as_bool) == Some(true)
    }))
}

/// How long `install` waits for Omarchy's shell to list the plugin after a rescan, which returns
/// before the shell has registered it: Omarchy's own `omarchy-plugin-add` polls the same way.
pub const OMARCHY_DISCOVERY_ATTEMPTS: u32 = 40;
pub const OMARCHY_DISCOVERY_PAUSE: std::time::Duration = std::time::Duration::from_millis(50);

/// Polls `omarchy plugin list --json` until it lists the `portico` plugin, up to `attempts`
/// times `pause` apart. Whether it appeared.
pub fn wait_for_omarchy_plugin(
    env: &DesktopEnv,
    attempts: u32,
    pause: std::time::Duration,
) -> bool {
    for attempt in 0..attempts {
        if attempt > 0 {
            std::thread::sleep(pause);
        }
        let known = env
            .run("omarchy", &["plugin", "list", "--json"])
            .ok()
            .and_then(|listing| serde_json::from_str::<serde_json::Value>(&listing).ok())
            .and_then(|plugins| {
                plugins.as_array().map(|plugins| {
                    plugins.iter().any(|plugin| {
                        plugin.get("id").and_then(serde_json::Value::as_str) == Some("portico")
                    })
                })
            })
            .unwrap_or(false);
        if known {
            return true;
        }
    }
    false
}

/// Whether a `qs -c portico` instance runs.
pub fn quickshell_running(env: &DesktopEnv) -> bool {
    env.run("qs", &["list", "-c", "portico", "-j"])
        .ok()
        .and_then(|out| serde_json::from_str::<serde_json::Value>(&out).ok())
        .and_then(|list| list.as_array().map(|list| !list.is_empty()))
        .unwrap_or(false)
}

/// One change, as the plan shows it and as it is applied.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Step {
    /// Write the panel files and stamp.
    WritePanel,
    /// Create the link at `path` to the panel directory, replacing a link elsewhere.
    Link {
        path: PathBuf,
        replaces: Option<PathBuf>,
    },
    /// Run a program found on `PATH`.
    Run(Vec<String>),
    /// Wait until Omarchy's running shell lists the plugin, then `omarchy plugin enable portico`.
    EnableOmarchyPlugin,
    /// Write a launcher file.
    WriteFile { path: PathBuf, contents: String },
    /// Back up `file` and replace its text, as planned from `original`.
    Edit {
        file: PathBuf,
        original: String,
        text: String,
        backup: PathBuf,
        /// What the plan shows: the action and the block's lines.
        action: String,
        lines: Vec<String>,
    },
    /// Remove a link or a launcher file.
    Remove(PathBuf),
    /// Remove the panel directory, and its parent when that is then empty.
    RemovePanel,
}

/// Runs `args[0]` with the rest; its first line of output on success.
fn run_step(env: &DesktopEnv, args: &[String]) -> Result<Option<String>, String> {
    let rest: Vec<&str> = args[1..].iter().map(String::as_str).collect();
    let out = env.run(&args[0], &rest)?;
    Ok(out
        .lines()
        .find(|line| !line.trim().is_empty())
        .map(|line| line.trim().to_string()))
}

/// Applies `step`; the program's first line of output, for a `Run`.
pub fn apply(env: &DesktopEnv, step: &Step) -> Result<Option<String>, String> {
    let show = |path: &Path| env.show(path);
    let io = |what: String| move |error: io::Error| format!("{what}: {error}");
    match step {
        Step::WritePanel => {
            panel::write(&env.panel_dir())
                .map_err(io(format!("could not write {}", show(&env.panel_dir()))))?;
        }
        Step::Link { path, replaces } => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .map_err(io(format!("could not create {}", show(parent))))?;
            }
            if replaces.is_some() {
                fs::remove_file(path).map_err(io(format!("could not remove {}", show(path))))?;
            }
            std::os::unix::fs::symlink(env.panel_dir(), path)
                .map_err(io(format!("could not link {}", show(path))))?;
        }
        Step::Run(args) => return run_step(env, args),
        Step::EnableOmarchyPlugin => {
            if !wait_for_omarchy_plugin(env, OMARCHY_DISCOVERY_ATTEMPTS, OMARCHY_DISCOVERY_PAUSE) {
                return Err(format!(
                    "Omarchy did not list the portico plugin within {} seconds of the rescan; \
                     enable it with: omarchy plugin enable portico",
                    OMARCHY_DISCOVERY_PAUSE.as_millis() * u128::from(OMARCHY_DISCOVERY_ATTEMPTS)
                        / 1000
                ));
            }
            let args = ["omarchy", "plugin", "enable", "portico"].map(String::from);
            return run_step(env, &args);
        }
        Step::WriteFile { path, contents } => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .map_err(io(format!("could not create {}", show(parent))))?;
            }
            fs::write(path, contents).map_err(io(format!("could not write {}", show(path))))?;
        }
        Step::Edit {
            file,
            original,
            text,
            backup,
            ..
        } => {
            let current =
                fs::read_to_string(file).map_err(io(format!("could not read {}", show(file))))?;
            if current != *original {
                return Err(format!(
                    "{} changed since the plan was shown; run the command again",
                    show(file)
                ));
            }
            let backup = create_backup(file, backup)
                .map_err(io(format!("could not back up {}", show(file))))?;
            // `file` is the resolved path, so a configuration file that is a link into a
            // dotfiles repository keeps its link and the repository's copy gets the change.
            let mut temporary = file.clone();
            temporary
                .as_mut_os_string()
                .push(format!(".portico-tmp-{}", std::process::id()));
            let mode = fs::metadata(file)
                .map(|meta| meta.permissions())
                .map_err(io(format!("could not read {}", show(file))))?;
            fs::write(&temporary, text)
                .and_then(|()| fs::set_permissions(&temporary, mode))
                .and_then(|()| fs::rename(&temporary, file))
                .map_err(|error| {
                    let _ = fs::remove_file(&temporary);
                    format!("could not write {}: {error}", show(file))
                })?;
            return Ok(Some(show(&backup)));
        }
        Step::Remove(path) => {
            fs::remove_file(path).map_err(io(format!("could not remove {}", show(path))))?;
        }
        Step::RemovePanel => {
            let dir = env.panel_dir();
            fs::remove_dir_all(&dir).map_err(io(format!("could not remove {}", show(&dir))))?;
            if let Some(parent) = dir.parent() {
                let _ = fs::remove_dir(parent);
            }
        }
    }
    Ok(None)
}

/// A free backup path next to `file`: `<file>.portico-bak.<unix seconds>`, then `-1`, `-2`…
pub fn backup_path(file: &Path) -> PathBuf {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let mut base = file.to_path_buf();
    base.as_mut_os_string()
        .push(format!(".portico-bak.{seconds}"));
    next_free(&base, 0)
}

fn next_free(base: &Path, from: u32) -> PathBuf {
    (from..)
        .map(|n| {
            let mut path = base.to_path_buf();
            if n > 0 {
                path.as_mut_os_string().push(format!("-{n}"));
            }
            path
        })
        .find(|path| fs::symlink_metadata(path).is_err())
        .expect("some counter is free")
}

/// Copies `file` to `planned`, or to the next free name when another backup took it: an
/// existing backup is never overwritten. The path written.
fn create_backup(file: &Path, planned: &Path) -> io::Result<PathBuf> {
    let contents = fs::read(file)?;
    let mut path = planned.to_path_buf();
    for n in 1.. {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut out) => {
                io::Write::write_all(&mut out, &contents)?;
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                path = next_free(planned, n);
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("the counter is unbounded")
}

/// The file an edit of `file` writes: the link's target when `file` is a link, so the link
/// survives. How the plan names it: the target, and the link it was reached through.
pub fn edit_target(env: &DesktopEnv, file: &Path) -> (PathBuf, String) {
    let resolved = fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
    let shown = if resolved == file {
        env.show(file)
    } else {
        format!("{} (linked from {})", env.show(&resolved), env.show(file))
    };
    (resolved, shown)
}

/// The keybinding for one combination, worked out.
#[derive(Clone, Debug)]
pub enum BindingPlan {
    /// The edit that adds (or replaces) the block.
    Ready {
        combo: Combo,
        location: Location,
        step: Step,
        /// The combination was checked live with `hyprctl` (otherwise in the Sway files).
        live: bool,
    },
    /// The same line is already in its block.
    Bound { location: Location, line: usize },
    /// Another binding uses the combination.
    Taken { combo: Combo, by: String },
    /// No binding can be written: why.
    Unavailable(Unavailable),
}

/// The keybinding of `combo` for `target`.
pub fn plan_binding(env: &DesktopEnv, target: Target, combo: &Combo) -> BindingPlan {
    let location = match keybinding::locate(env, target) {
        Ok(location) => location,
        Err(unavailable) => return BindingPlan::Unavailable(unavailable),
    };
    let reason = |text: String| BindingPlan::Unavailable(Unavailable::Reason(text));
    let original = match fs::read_to_string(&location.file) {
        Ok(text) => text,
        Err(error) => {
            return reason(format!(
                "could not read {}: {error}",
                env.show(&location.file)
            ));
        }
    };
    let existing = match keybinding::find_block(&original) {
        Ok(block) => block,
        Err(problem) => {
            return reason(format!(
                "{} cannot be edited safely: {problem}",
                env.show(&location.file)
            ));
        }
    };
    let line = keybinding::line(&location, combo, keybinding::toggle_command(target));
    if let Some(block) = &existing
        && block.line == line
    {
        return BindingPlan::Bound {
            location,
            line: block.first + 2,
        };
    }
    let live = location.setup.is_hyprland();
    let clash = if live {
        match env
            .run("hyprctl", &["binds", "-j"])
            .and_then(|json| keybinding::hyprland_clash(&json, combo))
        {
            Ok(clash) => clash,
            Err(error) => return reason(format!("could not list Hyprland's bindings: {error}")),
        }
    } else {
        keybinding::sway_clash(&location.file, &env.home, combo)
    };
    if let Some(by) = clash {
        return BindingPlan::Taken {
            combo: combo.clone(),
            by,
        };
    }
    let block = keybinding::block_lines(location.setup, &line);
    let (file, shown) = edit_target(env, &location.file);
    let (text, action) = match &existing {
        Some(old) => (
            keybinding::replace_block(&original, old, Some(&block)),
            format!(
                "replace  lines {}-{} of {shown}",
                old.first + 1,
                old.last + 1,
            ),
        ),
        None => (
            keybinding::append_block(&original, &block),
            format!(
                "append   to {shown}{}, after line {}",
                location
                    .note
                    .as_ref()
                    .map(|note| format!(" ({note})"))
                    .unwrap_or_default(),
                original.lines().count()
            ),
        ),
    };
    BindingPlan::Ready {
        combo: combo.clone(),
        step: Step::Edit {
            backup: backup_path(&file),
            file,
            original,
            text,
            action,
            lines: block.to_vec(),
        },
        location,
        live,
    }
}

/// Whether Hyprland now has Portico's binding for `combo`, waiting up to a second for its
/// configuration reload.
pub fn hyprland_has_binding(env: &DesktopEnv, combo: &Combo) -> bool {
    for attempt in 0..10 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let Ok(json) = env.run("hyprctl", &["binds", "-j"]) else {
            return false;
        };
        let Ok(binds) = serde_json::from_str::<serde_json::Value>(&json) else {
            return false;
        };
        let ours = binds.as_array().into_iter().flatten().any(|bind| {
            bind.get("modmask").and_then(serde_json::Value::as_u64)
                == Some(u64::from(combo.modmask()))
                && bind
                    .get("key")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|key| key.eq_ignore_ascii_case(combo.key()))
                && keybinding::hyprland_clash(&format!("[{bind}]"), combo) == Ok(None)
        });
        if ours {
            return true;
        }
    }
    false
}

/// Portico's block in each candidate file, for `status` and `uninstall`.
pub fn installed_bindings(
    env: &DesktopEnv,
) -> Vec<(Setup, PathBuf, Result<keybinding::Block, String>)> {
    keybinding::candidate_files(env)
        .into_iter()
        .filter_map(|(setup, file)| {
            let text = fs::read_to_string(&file).ok()?;
            match keybinding::find_block(&text) {
                Ok(Some(block)) => Some((setup, file, Ok(block))),
                Ok(None) => None,
                Err(problem) => Some((setup, file, Err(problem))),
            }
        })
        .collect()
}

/// A program and its arguments as one line, for plans and messages.
pub fn command_line(program: &str, args: &[&str]) -> String {
    std::iter::once(program)
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(vars: &[(&str, &str)]) -> DesktopEnv {
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        DesktopEnv::new(
            "/home/me".into(),
            move |name| {
                vars.iter()
                    .find(|(k, _)| k == name)
                    .map(|(_, v)| OsString::from(v))
            },
            Some("/home/me/.cargo/bin/portico".into()),
        )
    }

    #[test]
    fn follows_the_xdg_base_directories() {
        let plain = env(&[]);
        assert_eq!(
            plain.panel_dir(),
            Path::new("/home/me/.local/share/portico/panel")
        );
        assert_eq!(plain.config_home, Path::new("/home/me/.config"));
        assert_eq!(
            plain.data_dirs,
            [Path::new("/usr/local/share"), Path::new("/usr/share")]
        );
        assert!(!plain.hyprland && !plain.sway);

        let set = env(&[
            ("XDG_DATA_HOME", "/data"),
            ("XDG_CONFIG_HOME", "relative"),
            ("XDG_DATA_DIRS", "/opt/share:rel"),
            ("HYPRLAND_INSTANCE_SIGNATURE", "abc"),
            ("SWAYSOCK", ""),
        ]);
        assert_eq!(set.panel_dir(), Path::new("/data/portico/panel"));
        assert_eq!(set.config_home, Path::new("/home/me/.config"));
        assert_eq!(set.data_dirs, [Path::new("/opt/share")]);
        assert!(set.hyprland && !set.sway);
    }

    #[test]
    fn recognises_a_development_build() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut env = env(&[]);
        let installed = root.join("home/.cargo/bin");
        let tagged = root.join("build/release");
        let project = root.join("project/target/x86_64-unknown-linux-gnu/debug");
        for path in [&installed, &tagged, &project] {
            fs::create_dir_all(path).unwrap();
        }
        fs::write(
            root.join("build/CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55",
        )
        .unwrap();
        fs::write(root.join("project/Cargo.toml"), "").unwrap();
        for (exe, development) in [(&installed, false), (&tagged, true), (&project, true)] {
            env.exe = Some(exe.join("portico"));
            assert_eq!(env.is_development_build(), development, "{}", exe.display());
        }
        env.exe = std::env::current_exe().ok();
        assert!(
            env.is_development_build(),
            "this test binary is a cargo build's"
        );
    }

    #[test]
    fn tells_links_apart() {
        let dir = tempfile::tempdir().unwrap();
        let panel = dir.path().join("panel");
        let link = dir.path().join("link");
        assert_eq!(link_state(&link, &panel), Link::Missing);
        std::os::unix::fs::symlink(&panel, &link).unwrap();
        assert_eq!(link_state(&link, &panel), Link::Ours);
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink("/elsewhere", &link).unwrap();
        assert_eq!(
            link_state(&link, &panel),
            Link::Elsewhere("/elsewhere".into())
        );
        fs::remove_file(&link).unwrap();
        fs::create_dir(&link).unwrap();
        assert_eq!(link_state(&link, &panel), Link::Blocked);
    }
}
