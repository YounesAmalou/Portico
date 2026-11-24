use std::{
    collections::BTreeSet,
    io::{self, BufRead, Write},
    path::Path,
    process::ExitCode,
};

use crate::{
    app::App,
    domain::adopt::{Candidate, Direction},
    picker::{Picker, Row},
};

use super::{CANCELLED, Interrupts, Step, confirm};

/// What `portico adopt` or `portico export` was asked to do.
pub(super) struct Migration {
    pub(super) direction: Direction,
    pub(super) aliases: Vec<String>,
    pub(super) all: bool,
    pub(super) yes: bool,
}

impl Migration {
    fn verb(&self) -> &'static str {
        match self.direction {
            Direction::Adopt => "adopt",
            Direction::Export => "export",
        }
    }

    fn participle(&self) -> &'static str {
        match self.direction {
            Direction::Adopt => "adopted",
            Direction::Export => "exported",
        }
    }

    fn past(&self) -> &'static str {
        match self.direction {
            Direction::Adopt => "Adopted",
            Direction::Export => "Exported",
        }
    }

    fn cancelled(&self) -> &'static str {
        match self.direction {
            Direction::Adopt => "Adoption cancelled.",
            Direction::Export => "Export cancelled.",
        }
    }
}

/// `portico adopt` and `portico export`: choose the hosts, show the exact preview, ask, move.
pub(super) fn run_migration<I: BufRead, O: Write>(
    migration: &Migration,
    app: &App,
    picker: &mut dyn Picker,
    interrupts: &Interrupts,
    input: &mut I,
    output: &mut O,
    interactive: bool,
) -> Result<ExitCode, String> {
    let verb = migration.verb();
    let named = !migration.aliases.is_empty() || migration.all;
    if !named && !(interactive && picker.available()) {
        return Err(format!(
            "{verb} requires host aliases or --all in non-interactive mode"
        ));
    }
    let plan = match migration.direction {
        Direction::Adopt => app.adoption_plan(),
        Direction::Export => app.export_plan(),
    }
    .map_err(|error| error.to_string())?;
    let home = app.home();
    let eligible = plan.eligible().collect::<Vec<_>>();
    let ineligible = plan
        .candidates
        .iter()
        .filter(|candidate| candidate.reason.is_some())
        .collect::<Vec<_>>();
    let write_error = |error: io::Error| error.to_string();

    let selection = if !migration.aliases.is_empty() {
        migration.aliases.clone()
    } else if migration.all {
        for candidate in &ineligible {
            let reason = candidate.reason.as_deref().unwrap_or_default();
            writeln!(output, "Skipped {}: {reason}", candidate.alias).map_err(write_error)?;
        }
        if eligible.is_empty() {
            writeln!(output, "No host can be {}.", migration.participle()).map_err(write_error)?;
            return Ok(ExitCode::SUCCESS);
        }
        eligible
            .iter()
            .map(|candidate| candidate.alias.clone())
            .collect()
    } else {
        if eligible.is_empty() {
            writeln!(output, "No host can be {}.", migration.participle()).map_err(write_error)?;
        }
        if !ineligible.is_empty() {
            writeln!(output, "Not eligible:").map_err(write_error)?;
            for candidate in &ineligible {
                writeln!(
                    output,
                    "  {}  {}: {}",
                    candidate.alias,
                    describe_source(migration.direction, candidate, home),
                    candidate.reason.as_deref().unwrap_or_default()
                )
                .map_err(write_error)?;
            }
        }
        if eligible.is_empty() {
            return Ok(ExitCode::SUCCESS);
        }
        output.flush().map_err(write_error)?;
        let rows = eligible
            .iter()
            .map(|candidate| Row {
                fields: vec![candidate.alias.clone()],
                extra: vec![describe_source(migration.direction, candidate, home)],
            })
            .collect();
        match picker.pick_many(verb, rows)? {
            Some(picked) => picked
                .into_iter()
                .map(|index| eligible[index].alias.clone())
                .collect(),
            None => return Ok(ExitCode::from(CANCELLED)),
        }
    };

    if selection.is_empty() {
        writeln!(output, "Nothing to {verb}.").map_err(write_error)?;
        return Ok(ExitCode::SUCCESS);
    }
    let preview = plan
        .preview(&selection)
        .map_err(|error| error.to_string())?;
    write!(output, "{}", preview.render(Some(home))).map_err(write_error)?;
    if !migration.yes {
        if !interactive {
            return Err(format!("{verb} requires --yes in non-interactive mode"));
        }
        let count = selection
            .iter()
            .map(|alias| alias.to_ascii_lowercase())
            .collect::<BTreeSet<_>>()
            .len();
        let noun = if count == 1 { "host" } else { "hosts" };
        if !confirm(
            input,
            output,
            &format!("{} {count} {noun}? [y/N] ", capitalized(verb)),
        )? {
            writeln!(output, "{}", migration.cancelled()).map_err(write_error)?;
            return Ok(ExitCode::SUCCESS);
        }
    }
    // A signal after the migration finished leaves its result standing.
    let (Step::Done(moved) | Step::Late(moved)) =
        interrupts.during(|cancelled| app.migrate_cancellable(&plan, &selection, cancelled));
    let moved = moved.map_err(|error| error.to_string())?;
    writeln!(output, "{}: {}", migration.past(), moved.join(", ")).map_err(write_error)?;
    Ok(ExitCode::SUCCESS)
}

/// The file a candidate comes from, with the home directory as `~`; `managed` for export.
fn describe_source(direction: Direction, candidate: &Candidate, home: &Path) -> String {
    match direction {
        Direction::Export => "managed".into(),
        Direction::Adopt => crate::platform::paths::tilde(&candidate.source, home),
    }
}

fn capitalized(word: &str) -> String {
    let mut characters = word.chars();
    characters
        .next()
        .map(|first| first.to_ascii_uppercase().to_string() + characters.as_str())
        .unwrap_or_default()
}
