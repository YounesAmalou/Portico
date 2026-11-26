//! Opening a command in a new terminal window on macOS, through `open(1)`, in the terminal
//! named by `PORTICO_TERMINAL`, else the one Portico runs in (`TERM_PROGRAM`), else Terminal.

use std::{
    env,
    ffi::{OsStr, OsString},
    io,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use super::super::{
    common::launcher::{Launch, run},
    unix::fs::write_unique,
};

/// The launcher run when none is configured: `open(1)`, which Portico drives itself.
pub const DEFAULT_LAUNCHER: &str = "open";

/// A terminal application Portico knows how to hand a command to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Terminal {
    Terminal,
    ITerm2,
    Ghostty,
    WezTerm,
    Kitty,
    Alacritty,
}

impl Terminal {
    /// The terminal a `PORTICO_TERMINAL` value or a `TERM_PROGRAM` value names, ignoring case.
    pub fn named(name: &OsStr) -> Option<Self> {
        let name = name.to_str()?.to_ascii_lowercase();
        Some(match name.as_str() {
            "terminal" | "terminal.app" | "apple_terminal" => Self::Terminal,
            "iterm" | "iterm2" | "iterm.app" => Self::ITerm2,
            "ghostty" => Self::Ghostty,
            "wezterm" => Self::WezTerm,
            "kitty" => Self::Kitty,
            "alacritty" => Self::Alacritty,
            _ => return None,
        })
    }

    /// The application name `open -a` takes.
    fn application(self) -> &'static str {
        match self {
            Self::Terminal => "Terminal",
            Self::ITerm2 => "iTerm",
            Self::Ghostty => "Ghostty",
            Self::WezTerm => "WezTerm",
            Self::Kitty => "kitty",
            Self::Alacritty => "Alacritty",
        }
    }

    /// The arguments of `open` that run `command` in a new window. Terminal and iTerm2 take
    /// no command on their command line, so they are given `script` (a `.command` file) to
    /// open; the others take the command after `--args`.
    fn open_arguments(self, command: &[OsString], script: Option<&Path>) -> Vec<OsString> {
        let mut arguments: Vec<OsString> = Vec::new();
        let new_instance = |arguments: &mut Vec<OsString>, before: &[&str]| {
            arguments.extend(["-na", self.application(), "--args"].map(OsString::from));
            arguments.extend(before.iter().map(OsString::from));
        };
        match self {
            Self::Terminal | Self::ITerm2 => {
                arguments.extend(["-a", self.application()].map(OsString::from));
                arguments.extend(script.map(|script| script.as_os_str().to_owned()));
                return arguments;
            }
            Self::Ghostty | Self::Alacritty => new_instance(&mut arguments, &["-e"]),
            Self::WezTerm => new_instance(&mut arguments, &["start", "--"]),
            Self::Kitty => new_instance(&mut arguments, &[]),
        }
        arguments.extend(command.iter().cloned());
        arguments
    }

    fn needs_script(self) -> bool {
        matches!(self, Self::Terminal | Self::ITerm2)
    }
}

/// Where a new terminal window is opened.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Choice {
    Known(Terminal),
    /// A launcher program, run with the command as its arguments like `xdg-terminal-exec`.
    Program(PathBuf),
}

/// `portico_terminal` when it names a known terminal or an executable, else the terminal of
/// `term_program`, else Terminal.
pub fn choose(portico_terminal: Option<&OsStr>, term_program: Option<&OsStr>) -> Choice {
    if let Some(value) = portico_terminal.filter(|value| !value.is_empty()) {
        if let Some(terminal) = Terminal::named(value) {
            return Choice::Known(terminal);
        }
        let path = Path::new(value);
        if path.is_absolute() && is_executable(path) {
            return Choice::Program(path.to_owned());
        }
    }
    Choice::Known(
        term_program
            .and_then(Terminal::named)
            .unwrap_or(Terminal::Terminal),
    )
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

/// Opens `command` in a new terminal window. `DEFAULT_LAUNCHER` chooses the terminal (see
/// `choose`); any other launcher is run with the command as its arguments.
pub fn launch(launcher: &Path, command: &[OsString]) -> io::Result<Launch> {
    if launcher != Path::new(DEFAULT_LAUNCHER) {
        return run(launcher, command);
    }
    let portico_terminal = env::var_os("PORTICO_TERMINAL");
    let term_program = env::var_os("TERM_PROGRAM");
    let terminal = match choose(portico_terminal.as_deref(), term_program.as_deref()) {
        Choice::Program(program) => return run(&program, command),
        Choice::Known(terminal) => terminal,
    };
    let script = if terminal.needs_script() {
        Some(write_script(command)?)
    } else {
        None
    };
    let open = Path::new("/usr/bin/open");
    match run(open, &terminal.open_arguments(command, script.as_deref())) {
        Ok(Launch::Requested) => Ok(Launch::Requested),
        failed => {
            // The script deletes itself when it runs; one that was never opened is removed.
            if let Some(script) = &script {
                let _ = std::fs::remove_file(script);
            }
            failed
        }
    }
}

/// Writes a private `.command` script, in the per-user temporary directory, that deletes
/// itself and then runs `command`.
fn write_script(command: &[OsString]) -> io::Result<PathBuf> {
    let pending = write_unique(&env::temp_dir(), "portico", &script(command), 0o700)?;
    let path = pending.keep();
    let mut named = path.clone().into_os_string();
    named.push(".command");
    let named = PathBuf::from(named);
    if let Err(error) = std::fs::rename(&path, &named) {
        let _ = std::fs::remove_file(&path);
        return Err(error);
    }
    Ok(named)
}

/// The text of the `.command` script for `command`.
fn script(command: &[OsString]) -> Vec<u8> {
    let mut text = b"#!/bin/sh\nrm -f -- \"$0\"\nexec".to_vec();
    for word in command {
        text.push(b' ');
        text.extend(quote(word));
    }
    text.push(b'\n');
    text
}

/// `word` in single quotes for `sh`, each `'` written as `'\''`.
fn quote(word: &OsStr) -> Vec<u8> {
    let mut quoted = vec![b'\''];
    for byte in word.as_bytes() {
        if *byte == b'\'' {
            quoted.extend_from_slice(b"'\\''");
        } else {
            quoted.push(*byte);
        }
    }
    quoted.push(b'\'');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    fn os(value: &str) -> Option<&OsStr> {
        Some(OsStr::new(value))
    }

    #[test]
    fn the_terminal_portico_runs_in_is_chosen_unless_one_is_named() {
        assert_eq!(choose(None, None), Choice::Known(Terminal::Terminal));
        assert_eq!(
            choose(None, os("iTerm.app")),
            Choice::Known(Terminal::ITerm2)
        );
        assert_eq!(
            choose(None, os("ghostty")),
            Choice::Known(Terminal::Ghostty)
        );
        assert_eq!(
            choose(None, os("vscode")),
            Choice::Known(Terminal::Terminal)
        );
        assert_eq!(
            choose(os("WezTerm"), os("iTerm.app")),
            Choice::Known(Terminal::WezTerm)
        );
        assert_eq!(
            choose(os("ALACRITTY"), None),
            Choice::Known(Terminal::Alacritty)
        );
        assert_eq!(choose(os(""), os("kitty")), Choice::Known(Terminal::Kitty));
        assert_eq!(
            choose(os("no-such-terminal"), os("kitty")),
            Choice::Known(Terminal::Kitty)
        );
    }

    #[test]
    fn an_executable_named_by_path_is_run_as_the_launcher() {
        let dir = tempfile::tempdir().unwrap();
        let program = dir.path().join("launcher");
        std::fs::write(&program, "#!/bin/sh\n").unwrap();
        assert_eq!(
            choose(Some(program.as_os_str()), None),
            Choice::Known(Terminal::Terminal),
            "a file that is not executable is no launcher"
        );
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            choose(Some(program.as_os_str()), None),
            Choice::Program(program)
        );
    }

    #[test]
    fn each_terminal_is_opened_with_the_command() {
        let command = words(&["/opt/portico", "connect", "web"]);
        let script = Path::new("/tmp/x.command");
        let opened = |terminal: Terminal| {
            terminal
                .open_arguments(&command, Some(script))
                .into_iter()
                .map(|word| word.into_string().unwrap())
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert_eq!(opened(Terminal::Terminal), "-a Terminal /tmp/x.command");
        assert_eq!(opened(Terminal::ITerm2), "-a iTerm /tmp/x.command");
        assert_eq!(
            opened(Terminal::Ghostty),
            "-na Ghostty --args -e /opt/portico connect web"
        );
        assert_eq!(
            opened(Terminal::WezTerm),
            "-na WezTerm --args start -- /opt/portico connect web"
        );
        assert_eq!(
            opened(Terminal::Kitty),
            "-na kitty --args /opt/portico connect web"
        );
        assert_eq!(
            opened(Terminal::Alacritty),
            "-na Alacritty --args -e /opt/portico connect web"
        );
    }

    #[test]
    fn the_script_deletes_itself_and_runs_the_quoted_command() {
        assert_eq!(
            String::from_utf8(script(&words(&["/opt/my portico", "it's", "$HOME"]))).unwrap(),
            "#!/bin/sh\nrm -f -- \"$0\"\nexec '/opt/my portico' 'it'\\''s' '$HOME'\n"
        );
    }

    #[test]
    fn a_written_script_runs_the_command_and_removes_itself() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let script = write_script(&words(&[
            "/bin/sh",
            "-c",
            "echo \"$0\" > \"$1\"",
            "it's",
            out.to_str().unwrap(),
        ]))
        .unwrap();
        assert!(script.extension().is_some_and(|ext| ext == "command"));
        let status = std::process::Command::new(&script).status().unwrap();
        assert!(status.success());
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "it's\n");
        assert!(!script.exists(), "the script was left behind");
    }
}
