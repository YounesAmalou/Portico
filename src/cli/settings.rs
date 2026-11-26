//! `portico settings` and `portico settings set`.

use std::{io::Write, process::ExitCode};

use crate::{
    app::App,
    backend::{BackendId, Target},
    domain::settings::{Key, UserKey, parse_bool},
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
    /// Forget the user remembered for a container, so its shells use the image's user again.
    Forget {
        /// The host the container runs on; this machine when left out.
        #[arg(long, value_name = "ALIAS")]
        on: Option<String>,
        /// The container's backend.
        #[arg(long, value_parser = backend_argument, default_value = "docker")]
        backend: BackendId,
        /// The container as `portico settings` lists it: `project/service`, or its name outside Compose.
        container: String,
    },
}

fn backend_argument(text: &str) -> Result<BackendId, String> {
    match text {
        "docker" => Ok(BackendId::Docker),
        "podman" => Ok(BackendId::Podman),
        "proxmox" => {
            Err("a Proxmox container's shell runs as root, so no user is remembered".to_owned())
        }
        _ => Err("the backends are docker and podman".to_owned()),
    }
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
            if !settings.users.is_empty() {
                let rows: Vec<[String; 4]> = settings
                    .users
                    .iter()
                    .map(|(key, user)| {
                        [
                            key.target.describe(),
                            key.backend.as_str().to_owned(),
                            key.container.clone(),
                            user.clone(),
                        ]
                    })
                    .collect();
                let width =
                    |column: usize| rows.iter().map(|row| row[column].chars().count()).max();
                let (a, b, c) = (width(0), width(1), width(2));
                writeln!(output, "\nRemembered container users:").map_err(io)?;
                for row in &rows {
                    writeln!(
                        output,
                        "  {:<a$}  {:<b$}  {:<c$}  {}",
                        row[0],
                        row[1],
                        row[2],
                        row[3],
                        a = a.unwrap_or(0),
                        b = b.unwrap_or(0),
                        c = c.unwrap_or(0),
                    )
                    .map_err(io)?;
                }
            }
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
        Some(SettingsCommand::Forget {
            on,
            backend,
            container,
        }) => {
            let target = on.map_or(Target::Local, Target::Host);
            let key = UserKey {
                target,
                backend,
                container,
            };
            let changed = app
                .remember_container_user(&key, None)
                .map_err(|error| error.to_string())?;
            let place = format!(
                "{} ({} on {})",
                key.container,
                backend.as_str(),
                key.target.describe()
            );
            if changed {
                writeln!(output, "forgot the user of {place}").map_err(io)?;
            } else {
                writeln!(output, "nothing remembered for {place}").map_err(io)?;
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}
