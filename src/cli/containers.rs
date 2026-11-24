use std::io::{BufRead, Write};

use crate::{
    app::{App, ContainerError},
    backend::{BackendIssue, ContainerList, Target, display_name, group, remote_command},
    picker::{Picker, Row},
};

use super::{
    confirm,
    hosts::{host_argument, print_connection_failure},
};

/// The picker's rows for the containers of `list`, grouped and ordered as they are listed.
pub(super) fn container_rows(list: &ContainerList) -> Vec<(String, Row)> {
    let containers = &list.listing.containers;
    group(containers)
        .into_iter()
        .flat_map(|group| {
            let project = group.project.unwrap_or("Other");
            group.containers.into_iter().map(move |container| {
                let row = Row {
                    fields: vec![
                        display_name(container, containers),
                        container.name.clone(),
                        container.project.clone().unwrap_or_default(),
                        container.service.clone().unwrap_or_default(),
                        container.image.clone(),
                    ],
                    extra: vec![project.to_owned(), container.status.clone()],
                };
                (container.id.clone(), row)
            })
        })
        .collect()
}

/// This machine without `--on`, the host it names, or one chosen in the picker for a bare
/// `--on`; `None` when the choice is cancelled.
pub(super) fn on_argument(
    on: Option<Option<String>>,
    app: &App,
    picker: &mut dyn Picker,
    interactive: bool,
    command: &str,
) -> Result<Option<Target>, String> {
    match on {
        None => Ok(Some(Target::Local)),
        Some(alias) => Ok(host_argument(
            alias,
            app,
            picker,
            interactive,
            &format!("{command} --on"),
            false,
        )?
        .map(Target::Host)),
    }
}

/// Lists the containers of `target`. When docker refuses with a fix Portico can run, an
/// interactive session is asked whether to run it, and on yes the listing runs once more.
pub(super) fn list_with_offers<I: BufRead, O: Write, E: Write>(
    app: &App,
    target: &Target,
    input: &mut I,
    output: &mut O,
    error: &mut E,
    interactive: bool,
) -> Result<ContainerList, String> {
    let mut offered = false;
    loop {
        let issue = match app.containers(target, false, None) {
            Ok(list) => return Ok(list),
            Err(ContainerError::Backend(issue)) => issue,
            Err(failure) => return Err(report_container_failure(error, failure)?),
        };
        let offer = match &issue.offer {
            Some(offer) if interactive && !offered => offer,
            _ => {
                print_backend_issue(error, &issue, true)?;
                return Err(String::new());
            }
        };
        print_backend_issue(error, &issue, false)?;
        let prompt = format!("{} [y/N] ", offer.prompt);
        if !confirm(input, output, &prompt)? {
            print_backend_issue_next(error, &issue)?;
            return Err(String::new());
        }
        let status = match app.run_offer(target, offer) {
            Ok(status) => status,
            Err(failure) => return Err(report_container_failure(error, failure)?),
        };
        if !status.success() {
            return Err(format!(
                "{} exited with {status}",
                remote_command(&offer.words)
            ));
        }
        offered = true;
    }
}

/// Prints the issue's headline, and with `next` what to do about it.
fn print_backend_issue(
    error: &mut impl Write,
    issue: &BackendIssue,
    next: bool,
) -> Result<(), String> {
    writeln!(error, "error: {}", issue.headline).map_err(|error| error.to_string())?;
    if next {
        print_backend_issue_next(error, issue)?;
    }
    Ok(())
}

fn print_backend_issue_next(error: &mut impl Write, issue: &BackendIssue) -> Result<(), String> {
    for line in &issue.next {
        writeln!(error, "  {line}").map_err(|error| error.to_string())?;
    }
    error.flush().map_err(|error| error.to_string())
}

/// Reports a container failure on `error` and returns what is left for `run` to print: the
/// message, or nothing when the failure was explained here.
pub(super) fn report_container_failure(
    error: &mut impl Write,
    failure: ContainerError,
) -> Result<String, String> {
    match failure {
        ContainerError::Backend(issue) => {
            print_backend_issue(error, &issue, true)?;
            Ok(String::new())
        }
        ContainerError::Ssh {
            alias,
            explanation,
            stderr,
        } => {
            // ssh's stderr was captured, so show it above the explanation.
            write!(error, "{stderr}").map_err(|error| error.to_string())?;
            match explanation {
                Some(explanation) => {
                    print_connection_failure(error, &alias, &explanation)?;
                    Ok(String::new())
                }
                None => Ok(format!("ssh to {alias} failed")),
            }
        }
        failure => Ok(failure.to_string()),
    }
}

/// Prints the containers grouped by project, each group's columns aligned, then the note
/// and the count of skipped lines.
pub(super) fn print_listing(output: &mut impl Write, list: &ContainerList) -> Result<(), String> {
    let containers = &list.listing.containers;
    let mut text = String::new();
    if containers.is_empty() {
        text.push_str(&format!("No running containers on {}.\n", list.target));
    }
    for group in group(containers) {
        text.push_str(group.project.unwrap_or("Other"));
        text.push('\n');
        let rows: Vec<[String; 4]> = group
            .containers
            .iter()
            .map(|container| {
                [
                    display_name(container, containers),
                    container.name.clone(),
                    container.image.clone(),
                    container.status.clone(),
                ]
            })
            .collect();
        let width = |column: usize| {
            rows.iter()
                .map(|row| row[column].chars().count())
                .max()
                .unwrap_or(0)
        };
        let widths = [width(0), width(1), width(2)];
        for [display, name, image, status] in &rows {
            text.push_str(&format!(
                "  {display:<w0$}  {name:<w1$}  {image:<w2$}  {status}\n",
                w0 = widths[0],
                w1 = widths[1],
                w2 = widths[2]
            ));
        }
    }
    if let Some(note) = &list.note {
        text.push_str(note);
        text.push('\n');
    }
    if list.listing.skipped > 0 {
        text.push_str(&format!(
            "Skipped {} lines docker printed that Portico could not read.\n",
            list.listing.skipped
        ));
    }
    write!(output, "{text}").map_err(|error| error.to_string())
}
