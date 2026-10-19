//! The keybinding that toggles the panel: where it goes, what it says, whether its combination is
//! taken, and the marked block that holds it.

use std::path::{Path, PathBuf};

use super::{
    DesktopEnv, Target,
    combo::{Combo, modifier},
};

/// The command that toggles the panel on Omarchy.
pub const OMARCHY_TOGGLE: &str = "omarchy-shell shell toggle portico";
/// The command that toggles the panel in a plain Quickshell setup.
pub const QUICKSHELL_TOGGLE: &str = "qs -c portico ipc call portico toggle";

/// The text after the comment prefix on the block's first and last lines.
const BEGIN: &str = "portico desktop install: begin";
const END: &str = "portico desktop install: end";

/// The command that toggles the panel for `target`.
pub fn toggle_command(target: Target) -> &'static str {
    match target {
        Target::Omarchy => OMARCHY_TOGGLE,
        Target::Quickshell => QUICKSHELL_TOGGLE,
    }
}

/// The kind of configuration file the binding goes into.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Setup {
    /// Omarchy's `~/.config/hypr/bindings.lua`, with its `o.bind` helper.
    Omarchy,
    /// A plain Hyprland Lua configuration (`hyprland.lua`).
    HyprlandLua,
    /// A Hyprland `hyprland.conf`, or the bindings file it sources.
    HyprlandConf,
    Sway,
}

impl Setup {
    pub fn comment(self) -> &'static str {
        match self {
            Setup::Omarchy | Setup::HyprlandLua => "--",
            Setup::HyprlandConf | Setup::Sway => "#",
        }
    }

    pub fn is_hyprland(self) -> bool {
        self != Setup::Sway
    }
}

/// The file a binding goes into.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Location {
    pub setup: Setup,
    pub file: PathBuf,
    /// How the file was chosen, when it is not the obvious one: `sourced by hyprland.conf`.
    pub note: Option<String>,
    /// Sway only: whether `$mod` is the SUPER key.
    pub mod_is_super: bool,
}

/// Why no binding can be written, and what to do instead.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Unavailable {
    /// Not Hyprland or Sway: print the command to bind by hand.
    OtherCompositor,
    /// A reason, such as a missing file.
    Reason(String),
}

/// Where the binding for `target` goes in this session.
pub fn locate(env: &DesktopEnv, target: Target) -> Result<Location, Unavailable> {
    let hypr = env.config_home.join("hypr");
    if env.hyprland {
        let (setup, file, note) = match target {
            Target::Omarchy => {
                let file = env.omarchy_config().join("hypr/bindings.lua");
                if !file.is_file() {
                    return Err(Unavailable::Reason(format!(
                        "{} does not exist",
                        env.show(&file)
                    )));
                }
                (Setup::Omarchy, file, None)
            }
            Target::Quickshell if hypr.join("hyprland.lua").is_file() => {
                (Setup::HyprlandLua, hypr.join("hyprland.lua"), None)
            }
            Target::Quickshell => {
                let main = hypr.join("hyprland.conf");
                let Ok(text) = std::fs::read_to_string(&main) else {
                    return Err(Unavailable::Reason(format!(
                        "found neither {} nor {}",
                        env.show(&hypr.join("hyprland.lua")),
                        env.show(&main)
                    )));
                };
                match bindings_source(&text, &env.home, &hypr) {
                    Some(file) => (
                        Setup::HyprlandConf,
                        file,
                        Some("sourced by hyprland.conf".into()),
                    ),
                    None => (Setup::HyprlandConf, main, None),
                }
            }
        };
        return Ok(Location {
            setup,
            file,
            note,
            mod_is_super: false,
        });
    }
    if target == Target::Omarchy {
        return Err(Unavailable::Reason(
            "Hyprland is not running here; run portico desktop install from your Hyprland session"
                .into(),
        ));
    }
    if env.sway {
        let file = sway_config(env).ok_or_else(|| {
            Unavailable::Reason(format!(
                "found no Sway configuration at {}",
                env.show(&env.config_home.join("sway/config"))
            ))
        })?;
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        let mod_is_super = sway_variables(&text)
            .get("$mod")
            .is_some_and(|value| modifier(value) == Some(64));
        return Ok(Location {
            setup: Setup::Sway,
            file,
            note: None,
            mod_is_super,
        });
    }
    Err(Unavailable::OtherCompositor)
}

/// The line that binds `combo` to `command` in `location`'s file.
pub fn line(location: &Location, combo: &Combo, command: &str) -> String {
    match location.setup {
        Setup::Omarchy => format!("o.bind(\"{combo}\", \"Portico\", \"{command}\")"),
        Setup::HyprlandLua => format!(
            "hl.bind(\"{combo}\", hl.dsp.exec_cmd(\"{command}\"), {{ description = \"Portico\" }})"
        ),
        Setup::HyprlandConf => format!(
            "bind = {}, {}, exec, {command}",
            combo.hyprland_mods(),
            combo.key()
        ),
        Setup::Sway => format!(
            "bindsym {} exec {command}",
            combo.sway(location.mod_is_super)
        ),
    }
}

/// The block Portico writes: the line between its markers.
pub fn block_lines(setup: Setup, line: &str) -> [String; 3] {
    let comment = setup.comment();
    [
        format!("{comment} {BEGIN}"),
        line.to_string(),
        format!("{comment} {END}"),
    ]
}

/// The line that starts the panel with the session, for a plain Quickshell setup.
pub fn autostart_hint(env: &DesktopEnv) -> String {
    let hypr = env.config_home.join("hypr");
    if env.hyprland && hypr.join("hyprland.lua").is_file() {
        format!(
            "Portico does not start the panel with your session. Add this line to {}:\n  hl.on(\"hyprland.start\", function() hl.exec_cmd(\"qs -c portico\") end)",
            env.show(&hypr.join("hyprland.lua"))
        )
    } else if env.hyprland {
        format!(
            "Portico does not start the panel with your session. Add this line to {}:\n  exec-once = qs -c portico",
            env.show(&hypr.join("hyprland.conf"))
        )
    } else if env.sway {
        "Portico does not start the panel with your session. Add this line to your Sway configuration:\n  exec qs -c portico".into()
    } else {
        "Portico does not start the panel with your session: start `qs -c portico` with it.".into()
    }
}

/// Portico's block in a file: its line range (0-based, inclusive) and the binding line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Block {
    pub first: usize,
    pub last: usize,
    pub line: String,
}

/// Portico's block in `text`, if any. Two blocks, or a begin without an end, are errors: the
/// file was edited by hand and Portico does not guess.
pub fn find_block(text: &str) -> Result<Option<Block>, String> {
    let lines: Vec<&str> = text.lines().collect();
    let is = |line: &str, marker: &str| {
        let line = line.trim();
        ["--", "#"]
            .iter()
            .any(|prefix| line.strip_prefix(prefix).map(str::trim) == Some(marker))
    };
    let mut found: Option<Block> = None;
    let mut index = 0;
    while index < lines.len() {
        if is(lines[index], BEGIN) {
            if found.is_some() {
                return Err("it holds more than one Portico block".into());
            }
            let Some(end) = (index + 1..lines.len()).find(|&i| is(lines[i], END)) else {
                return Err(format!(
                    "its Portico block at line {} has no end",
                    index + 1
                ));
            };
            let line = lines[index + 1..end]
                .iter()
                .map(|line| line.trim())
                .find(|line| !line.is_empty())
                .unwrap_or_default()
                .to_string();
            found = Some(Block {
                first: index,
                last: end,
                line,
            });
            index = end;
        } else if is(lines[index], END) {
            return Err(format!(
                "its Portico block end at line {} has no begin",
                index + 1
            ));
        }
        index += 1;
    }
    Ok(found)
}

/// `text` with `block` appended, after a newline when the text lacks a final one.
pub fn append_block(text: &str, block: &[String; 3]) -> String {
    let mut out = text.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    for line in block {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// `text` with `old`'s lines replaced by `block`, or removed when `block` is `None`.
pub fn replace_block(text: &str, old: &Block, block: Option<&[String; 3]>) -> String {
    let mut out = String::new();
    for (index, line) in text.split_inclusive('\n').enumerate() {
        if index == old.first
            && let Some(block) = block
        {
            for new in block {
                out.push_str(new);
                out.push('\n');
            }
        }
        if index < old.first || index > old.last {
            out.push_str(line);
        }
    }
    out
}

/// A live Hyprland binding that already uses `combo`, named by its description (or dispatcher
/// and argument). Bindings in a submap, mouse bindings and Portico's own are not clashes.
pub fn hyprland_clash(binds_json: &str, combo: &Combo) -> Result<Option<String>, String> {
    let binds: serde_json::Value = serde_json::from_str(binds_json)
        .map_err(|error| format!("hyprctl binds -j printed no JSON list ({error})"))?;
    let Some(binds) = binds.as_array() else {
        return Err("hyprctl binds -j printed no JSON list".into());
    };
    let text = |bind: &serde_json::Value, key: &str| {
        bind.get(key)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    for bind in binds {
        if !text(bind, "submap").is_empty()
            || bind.get("mouse").and_then(serde_json::Value::as_bool) == Some(true)
            || bind.get("modmask").and_then(serde_json::Value::as_u64)
                != Some(u64::from(combo.modmask()))
            || !text(bind, "key").eq_ignore_ascii_case(combo.key())
        {
            continue;
        }
        let description = text(bind, "description");
        let arg = text(bind, "arg");
        if description == "Portico" || arg == OMARCHY_TOGGLE || arg == QUICKSHELL_TOGGLE {
            continue;
        }
        return Ok(Some(if description.is_empty() {
            format!("{} {arg}", text(bind, "dispatcher"))
                .trim()
                .to_string()
        } else {
            description
        }));
    }
    Ok(None)
}

/// A Sway binding in `file` (or a file it includes) that already uses `combo`: its line.
/// Bindings inside a `mode` block and Portico's own block are not clashes.
pub fn sway_clash(file: &Path, home: &Path, combo: &Combo) -> Option<String> {
    let text = std::fs::read_to_string(file).ok()?;
    let mut texts = vec![text.clone()];
    let dir = file.parent().unwrap_or(Path::new("/"));
    for line in text.lines() {
        let mut words = line.split_whitespace();
        if words.next() != Some("include") {
            continue;
        }
        let pattern = words.collect::<Vec<_>>().join(" ");
        let pattern = expand(&pattern, home);
        let pattern = if Path::new(&pattern).is_absolute() {
            pattern
        } else {
            dir.join(pattern).to_string_lossy().into_owned()
        };
        for path in glob::glob(&pattern).into_iter().flatten().flatten() {
            if let Ok(included) = std::fs::read_to_string(path) {
                texts.push(included);
            }
        }
    }
    let variables = sway_variables(&texts.join("\n"));
    for text in &texts {
        let ours = find_block(text).ok().flatten();
        let mut depth = 0usize;
        let mut in_bindsym_block = false;
        for (index, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            if ours
                .as_ref()
                .is_some_and(|block| (block.first..=block.last).contains(&index))
            {
                continue;
            }
            let first_keys = |words: &mut std::str::SplitWhitespace<'_>| {
                words
                    .find(|word| !word.starts_with("--"))
                    .map(str::to_string)
            };
            if in_bindsym_block {
                // `bindsym { <keys> <command> … }`: each inner line starts with its keys.
                if trimmed == "}" {
                    in_bindsym_block = false;
                } else if first_keys(&mut trimmed.split_whitespace())
                    .is_some_and(|keys| sway_keys_match(&keys, &variables, combo))
                {
                    return Some(trimmed.to_string());
                }
                continue;
            }
            if depth == 0 && trimmed.starts_with("bindsym") {
                if trimmed.ends_with('{') {
                    in_bindsym_block = true;
                    continue;
                }
                let mut words = trimmed.split_whitespace();
                words.next();
                if first_keys(&mut words)
                    .is_some_and(|keys| sway_keys_match(&keys, &variables, combo))
                {
                    return Some(trimmed.to_string());
                }
            }
            let opens_mode = trimmed.starts_with("mode") || depth > 0;
            if opens_mode && trimmed.ends_with('{') {
                depth += 1;
            } else if depth > 0 && trimmed == "}" {
                depth -= 1;
            }
        }
    }
    None
}

fn sway_keys_match(
    keys: &str,
    variables: &std::collections::HashMap<String, String>,
    combo: &Combo,
) -> bool {
    let mut mods = 0;
    let mut key = None;
    for part in keys.split('+') {
        let part = variables.get(part).map_or(part, String::as_str);
        match modifier(part) {
            Some(bit) => mods |= bit,
            None => key = Some(part),
        }
    }
    mods == combo.modmask() && key.is_some_and(|key| key.eq_ignore_ascii_case(combo.key()))
}

/// Sway's `set $name value` variables.
fn sway_variables(text: &str) -> std::collections::HashMap<String, String> {
    text.lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            (words.next() == Some("set")).then_some(())?;
            Some((words.next()?.to_string(), words.next()?.to_string()))
        })
        .collect()
}

fn sway_config(env: &DesktopEnv) -> Option<PathBuf> {
    [
        env.config_home.join("sway/config"),
        env.home.join(".sway/config"),
    ]
    .into_iter()
    .find(|path| path.is_file())
}

/// The file `hyprland.conf` sources whose name contains `bind`.
fn bindings_source(text: &str, home: &Path, hypr: &Path) -> Option<PathBuf> {
    text.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        if key.trim() != "source" {
            return None;
        }
        let value = value.split('#').next()?.trim();
        let path = PathBuf::from(expand(value, home));
        let path = if path.is_absolute() {
            path
        } else {
            hypr.join(path)
        };
        path.file_name()?
            .to_string_lossy()
            .contains("bind")
            .then_some(path)
    })
}

fn expand(value: &str, home: &Path) -> String {
    let home = home.to_string_lossy();
    let value = value
        .strip_prefix("$HOME")
        .map_or_else(|| value.to_string(), |rest| format!("~{rest}"));
    crate::home::expand(&value, Path::new(home.as_ref()))
}

/// Candidate files that may hold Portico's block, for `status` and `uninstall`.
pub fn candidate_files(env: &DesktopEnv) -> Vec<(Setup, PathBuf)> {
    let hypr = env.config_home.join("hypr");
    let mut files = vec![
        (
            Setup::Omarchy,
            env.omarchy_config().join("hypr/bindings.lua"),
        ),
        (Setup::HyprlandLua, hypr.join("hyprland.lua")),
        (Setup::HyprlandConf, hypr.join("hyprland.conf")),
    ];
    if let Ok(text) = std::fs::read_to_string(hypr.join("hyprland.conf"))
        && let Some(file) = bindings_source(&text, &env.home, &hypr)
    {
        files.push((Setup::HyprlandConf, file));
    }
    if let Some(file) = sway_config(env) {
        files.push((Setup::Sway, file));
    }
    files.retain(|(_, file)| file.is_file());
    let mut seen = std::collections::HashSet::new();
    files.retain(|(_, file)| seen.insert(file.clone()));
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    fn location(setup: Setup) -> Location {
        Location {
            setup,
            file: PathBuf::from("/x"),
            note: None,
            mod_is_super: true,
        }
    }

    #[test]
    fn writes_each_setup_s_line() {
        let combo = Combo::default_proposal();
        assert_eq!(
            line(&location(Setup::Omarchy), &combo, OMARCHY_TOGGLE),
            r#"o.bind("SUPER + ALT + P", "Portico", "omarchy-shell shell toggle portico")"#
        );
        assert_eq!(
            line(&location(Setup::HyprlandLua), &combo, QUICKSHELL_TOGGLE),
            r#"hl.bind("SUPER + ALT + P", hl.dsp.exec_cmd("qs -c portico ipc call portico toggle"), { description = "Portico" })"#
        );
        assert_eq!(
            line(&location(Setup::HyprlandConf), &combo, QUICKSHELL_TOGGLE),
            "bind = SUPER ALT, P, exec, qs -c portico ipc call portico toggle"
        );
        assert_eq!(
            line(&location(Setup::Sway), &combo, QUICKSHELL_TOGGLE),
            "bindsym $mod+Alt+p exec qs -c portico ipc call portico toggle"
        );
    }

    #[test]
    fn appends_and_removes_the_block_exactly() {
        let original = "-- mine\no.bind(\"SUPER + H\", nil, \"x\")\n";
        let block = block_lines(Setup::Omarchy, "o.bind(...)");
        let added = append_block(original, &block);
        assert_eq!(
            added,
            "-- mine\no.bind(\"SUPER + H\", nil, \"x\")\n-- portico desktop install: begin\no.bind(...)\n-- portico desktop install: end\n"
        );
        let found = find_block(&added).unwrap().unwrap();
        assert_eq!((found.first, found.last), (2, 4));
        assert_eq!(found.line, "o.bind(...)");
        assert_eq!(replace_block(&added, &found, None), original);
        let other = block_lines(Setup::Omarchy, "o.bind(other)");
        let replaced = replace_block(&added, &found, Some(&other));
        assert_eq!(replaced, append_block(original, &other));
        assert_eq!(find_block(original).unwrap(), None);
    }

    #[test]
    fn appends_after_a_missing_final_newline_and_keeps_crlf_lines() {
        let block = block_lines(Setup::Sway, "bindsym x");
        assert!(append_block("a", &block).starts_with("a\n# portico"));
        let text = format!("a\r\n{}", block.map(|line| line + "\r\n").concat());
        let found = find_block(&text).unwrap().unwrap();
        assert_eq!(replace_block(&text, &found, None), "a\r\n");
    }

    #[test]
    fn refuses_hand_edited_blocks() {
        let block = block_lines(Setup::Omarchy, "x").join("\n");
        assert!(
            find_block(&format!("{block}\n{block}\n"))
                .unwrap_err()
                .contains("more than one")
        );
        assert!(
            find_block("-- portico desktop install: begin\nx\n")
                .unwrap_err()
                .contains("no end")
        );
        assert!(
            find_block("# portico desktop install: end\n")
                .unwrap_err()
                .contains("no begin")
        );
    }

    const BINDS: &str = r#"[
      {"mouse": false, "modmask": 64, "submap": "", "key": "P", "description": "Pseudo window", "dispatcher": "__lua", "arg": "19"},
      {"mouse": false, "modmask": 68, "submap": "", "key": "P", "description": "Power", "dispatcher": "__lua", "arg": "243"},
      {"mouse": false, "modmask": 65, "submap": "", "key": "P", "description": "Google Photos", "dispatcher": "__lua", "arg": "323"},
      {"mouse": false, "modmask": 72, "submap": "resize", "key": "P", "description": "In a submap", "dispatcher": "exec", "arg": "x"},
      {"mouse": true, "modmask": 72, "submap": "", "key": "p", "description": "", "dispatcher": "movewindow", "arg": ""},
      {"mouse": false, "modmask": 9, "submap": "", "key": "q", "description": "", "dispatcher": "exec", "arg": "foot"},
      {"mouse": false, "modmask": 72, "submap": "", "key": "T", "description": "Portico", "dispatcher": "__lua", "arg": "400"}
    ]"#;

    #[test]
    fn finds_live_hyprland_clashes() {
        let clash = |text: &str| hyprland_clash(BINDS, &Combo::parse(text).unwrap()).unwrap();
        assert_eq!(clash("SUPER + ALT + P"), None);
        assert_eq!(clash("SUPER + P").as_deref(), Some("Pseudo window"));
        assert_eq!(clash("SUPER + CTRL + P").as_deref(), Some("Power"));
        assert_eq!(clash("SUPER + SHIFT + P").as_deref(), Some("Google Photos"));
        assert_eq!(clash("ALT + SHIFT + Q").as_deref(), Some("exec foot"));
        assert_eq!(clash("SUPER + ALT + T"), None, "Portico's own binding");
        assert!(hyprland_clash("not json", &Combo::default_proposal()).is_err());
    }

    #[test]
    fn finds_sway_clashes_outside_modes_and_portico_s_block() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        std::fs::create_dir(dir.path().join("config.d")).unwrap();
        std::fs::write(
            dir.path().join("config.d/keys"),
            "bindsym --to-code $mod+Shift+p exec grim\n",
        )
        .unwrap();
        let block = block_lines(Setup::Sway, "bindsym $mod+Alt+t exec qs").join("\n");
        std::fs::write(
            &config,
            format!(
                "set $mod Mod4\nbindsym $mod+p exec x\nmode \"resize\" {{\n  bindsym $mod+Ctrl+p exec y\n  bindsym {{\n    $mod+Alt+Ctrl+p exec z\n  }}\n}}\nbindsym --to-code {{\n  $mod+Alt+Shift+p exec grim\n  $mod+Alt+q kill\n}}\ninclude config.d/*\n{block}\n"
            ),
        )
        .unwrap();
        let clash = |text: &str| sway_clash(&config, dir.path(), &Combo::parse(text).unwrap());
        assert_eq!(clash("SUPER + P").as_deref(), Some("bindsym $mod+p exec x"));
        assert_eq!(clash("SUPER + CTRL + P"), None, "inside a mode");
        assert_eq!(
            clash("SUPER + SHIFT + P").as_deref(),
            Some("bindsym --to-code $mod+Shift+p exec grim")
        );
        assert_eq!(clash("SUPER + ALT + T"), None, "Portico's own block");
        assert_eq!(
            clash("SUPER + ALT + SHIFT + P").as_deref(),
            Some("$mod+Alt+Shift+p exec grim"),
            "a block-form bindsym"
        );
        assert_eq!(clash("SUPER + ALT + Q").as_deref(), Some("$mod+Alt+q kill"));
        assert_eq!(
            clash("SUPER + CTRL + ALT + P"),
            None,
            "a block inside a mode"
        );
        assert_eq!(clash("SUPER + ALT + P"), None);
    }

    #[test]
    fn picks_the_bindings_file_hyprland_conf_sources() {
        let home = Path::new("/home/me");
        let hypr = Path::new("/home/me/.config/hypr");
        assert_eq!(
            bindings_source(
                "source = ~/.config/hypr/monitors.conf\nsource = ~/.config/hypr/bindings.conf # keys\n",
                home,
                hypr
            ),
            Some(PathBuf::from("/home/me/.config/hypr/bindings.conf"))
        );
        assert_eq!(
            bindings_source("source = $HOME/keybinds.conf\n", home, hypr),
            Some(PathBuf::from("/home/me/keybinds.conf"))
        );
        assert_eq!(
            bindings_source("source = binds.conf\n", home, hypr),
            Some(PathBuf::from("/home/me/.config/hypr/binds.conf"))
        );
        assert_eq!(bindings_source("source = looks.conf\n", home, hypr), None);
    }
}
