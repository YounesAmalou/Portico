//! `portico settings` and `portico settings set`.

use std::{io::Write, process::ExitCode};

use crate::{
    app::App,
    domain::settings::{Key, parse_bool},
    platform::paths::tilde,
};

#[derive(clap::Subcommand, Debug)]
pub(super) enum SettingsCommand {
    /// Change one setting.
    Set {
        /// The setting's name: one of the names `portico settings` lists.
        #[arg(value_parser = key_argument)]
        key: String,
        /// `on` or `off` (also true, false, yes and no).
        #[arg(value_parser = value_argument)]
        value: String,
    },
}

fn key_argument(text: &str) -> Result<String, String> {
    if Key::parse(text).is_some() {
        return Ok(text.to_owned());
    }
    let names: Vec<&str> = Key::ALL.iter().map(|key| key.name()).collect();
    Err(format!(
        "unknown setting; the settings are {}",
        names.join(", ")
    ))
}

fn value_argument(text: &str) -> Result<String, String> {
    match parse_bool(text) {
        Some(_) => Ok(text.to_owned()),
        None => Err("the value is on or off (also true, false, yes, no)".to_owned()),
    }
}

fn state(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

pub(super) fn run(
    command: Option<SettingsCommand>,
    app: &App,
    output: &mut impl Write,
) -> Result<ExitCode, String> {
    let io = |error: std::io::Error| error.to_string();
    match command {
        None => {
            let settings = app.settings().map_err(|error| error.to_string())?;
            for key in Key::ALL {
                let on = key.get(&settings);
                let default = if on { "" } else { "  (default on)" };
                writeln!(output, "{:<14}{}{default}", key.name(), state(on)).map_err(io)?;
            }
            let missing = if app.settings_path().exists() {
                ""
            } else {
                " (not created yet)"
            };
            writeln!(
                output,
                "\nFile: {}{missing}",
                tilde(app.settings_path(), app.home())
            )
            .map_err(io)?;
        }
        Some(SettingsCommand::Set { key, value }) => {
            let key = Key::parse(&key).ok_or("unknown setting")?;
            let value = parse_bool(&value).ok_or("invalid value")?;
            let changed = app
                .set_setting(key, value)
                .map_err(|error| error.to_string())?;
            let already = if changed { "" } else { "already " };
            writeln!(output, "{}: {already}{}", key.name(), state(value)).map_err(io)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}
