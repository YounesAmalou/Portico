use std::io::{BufRead, Write};

use crate::{
    app::{App, ContainerError},
    backend::{
        AUTOMATIC, BackendId, BackendIssue, ContainerList, Problem, ResolveError, Target, Via,
        display_name, group, remote_command, resolve,
    },
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
            let project = group.project.unwrap_or(list.backend.ungrouped_heading());
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

/// How one backend's listing ended, when it did not stop the command.
enum Attempt {
    Listed(ContainerList),
    /// The backend is not installed; nothing was printed.
    Missing(Box<BackendIssue>),
    /// The backend refused; its issue was printed.
    Failed,
}

/// Lists the containers of `target` with `backend`, or the first installed one without it.
/// When the backend refuses with a fix Portico can run, an interactive session is asked
/// whether to run it, and on yes the listing runs once more. With `quiet_missing`, a backend
/// that is not installed is returned unprinted. An `Err` stops the command: an ssh failure,
/// or a fix that failed.
#[allow(clippy::too_many_arguments)]
fn attempt<I: BufRead, O: Write, E: Write>(
    app: &App,
    target: &Target,
    backend: Option<BackendId>,
    quiet_missing: bool,
    input: &mut I,
    output: &mut O,
    error: &mut E,
    interactive: bool,
) -> Result<Attempt, String> {
    let mut offered = false;
    loop {
        let issue = match app.containers(target, backend, false, None) {
            Ok(list) => return Ok(Attempt::Listed(list)),
            Err(ContainerError::Backend(issue)) => issue,
            Err(failure) => return Err(report_container_failure(error, failure)?),
        };
        if quiet_missing && issue.problem == Problem::NotInstalled {
            return Ok(Attempt::Missing(issue));
        }
        let offer = match &issue.offer {
            Some(offer) if interactive && !offered => offer,
            _ => {
                print_backend_issue(error, &issue, true)?;
                return Ok(Attempt::Failed);
            }
        };
        print_backend_issue(error, &issue, false)?;
        let prompt = format!("{} [y/N] ", offer.prompt);
        if !confirm(input, output, &prompt)? {
            print_backend_issue_next(error, &issue)?;
            return Ok(Attempt::Failed);
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

/// Lists the containers of `target` with `backend`, or the first installed one without it,
/// offering a fix as [`attempt`] does. A refusal is printed and returned as an empty `Err`.
pub(super) fn list_with_offers<I: BufRead, O: Write, E: Write>(
    app: &App,
    target: &Target,
    backend: Option<BackendId>,
    input: &mut I,
    output: &mut O,
    error: &mut E,
    interactive: bool,
) -> Result<ContainerList, String> {
    match attempt(
        app,
        target,
        backend,
        false,
        input,
        output,
        error,
        interactive,
    )? {
        Attempt::Listed(list) => Ok(list),
        Attempt::Missing(_) | Attempt::Failed => Err(String::new()),
    }
}

/// The listings of every backend installed on `target`.
pub(super) struct Every {
    /// In the order of [`AUTOMATIC`].
    pub lists: Vec<ContainerList>,
    /// Whether an installed backend refused (its issue was printed).
    pub failed: bool,
}

/// Lists `target` with each backend of [`AUTOMATIC`] in turn, offering fixes as
/// [`list_with_offers`] does. A backend that is not installed is skipped; one that refuses is
/// reported and the next is listed. When no backend listed, the issues are printed and the
/// result is an empty `Err`: the first backend's when none is installed.
pub(super) fn list_every<I: BufRead, O: Write, E: Write>(
    app: &App,
    target: &Target,
    input: &mut I,
    output: &mut O,
    error: &mut E,
    interactive: bool,
) -> Result<Every, String> {
    let mut every = Every {
        lists: Vec::new(),
        failed: false,
    };
    let mut first_missing = None;
    for backend in AUTOMATIC {
        match attempt(
            app,
            target,
            Some(backend),
            true,
            input,
            output,
            error,
            interactive,
        )? {
            Attempt::Listed(list) => every.lists.push(list),
            Attempt::Missing(issue) => {
                first_missing.get_or_insert(issue);
            }
            Attempt::Failed => every.failed = true,
        }
    }
    if every.lists.is_empty() {
        if !every.failed
            && let Some(issue) = first_missing
        {
            print_backend_issue(error, &issue, true)?;
        }
        return Err(String::new());
    }
    Ok(every)
}

/// One container a reference or the picker can choose, with what its shell needs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Chosen {
    pub backend: BackendId,
    pub id: String,
    pub via: Option<Via>,
}

impl Chosen {
    fn from(list: &ContainerList, id: &str) -> Self {
        Self {
            backend: list.backend,
            id: id.to_owned(),
            via: list.via,
        }
    }
}

/// Resolves `reference` in every listing: one match is chosen; matches in several listings
/// are ambiguous across backends, and must be told apart with `--backend`.
pub(super) fn resolve_in(
    target: &Target,
    reference: &str,
    lists: &[ContainerList],
) -> Result<Chosen, String> {
    let mut found = Vec::new();
    let mut ambiguous = Vec::new();
    for list in lists {
        match resolve(reference, &list.listing.containers) {
            Ok(container) => found.push((Chosen::from(list, &container.id), &container.name)),
            Err(ResolveError::NotFound) => {}
            Err(ResolveError::Ambiguous(names)) => ambiguous.push((list.backend, names)),
        }
    }
    let error = |candidates| ContainerError::Ambiguous {
        target: target.clone(),
        reference: reference.to_owned(),
        candidates,
    };
    match (found.len(), ambiguous.len()) {
        (0, 0) => Err(ContainerError::NoMatch {
            target: target.clone(),
            reference: reference.to_owned(),
        }
        .to_string()),
        (1, 0) => Ok(found.remove(0).0),
        (0, 1) => Err(error(ambiguous.remove(0).1).to_string()),
        _ => {
            let named = |backend: BackendId, name: &str| format!("{name} ({})", backend.as_str());
            let mut candidates: Vec<String> = found
                .iter()
                .map(|(chosen, name)| named(chosen.backend, name))
                .collect();
            for (backend, names) in ambiguous {
                candidates.extend(names.iter().map(|name| named(backend, name)));
            }
            Err(format!("{}; choose one with --backend", error(candidates)))
        }
    }
}

/// The picker's rows for the containers of every listing, each with its container. With
/// more than one listing, each group label names its backend (`Podman · shop`).
pub(super) fn every_container_row(lists: &[ContainerList]) -> Vec<(Chosen, Row)> {
    let several = lists.len() > 1;
    lists
        .iter()
        .flat_map(|list| {
            container_rows(list).into_iter().map(move |(id, mut row)| {
                if several && let Some(label) = row.extra.first_mut() {
                    *label = format!("{} \u{b7} {label}", list.backend.title());
                }
                (Chosen::from(list, &id), row)
            })
        })
        .collect()
}

/// Prints each listing as [`print_listing`] does; with more than one, each under its
/// backend's name, separated by a blank line.
pub(super) fn print_listings(
    output: &mut impl Write,
    lists: &[ContainerList],
) -> Result<(), String> {
    if let [list] = lists {
        return print_listing(output, list);
    }
    for (index, list) in lists.iter().enumerate() {
        if index > 0 {
            writeln!(output).map_err(|error| error.to_string())?;
        }
        writeln!(output, "{}", list.backend.title()).map_err(|error| error.to_string())?;
        print_listing(output, list)?;
    }
    Ok(())
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
        text.push_str(group.project.unwrap_or(list.backend.ungrouped_heading()));
        text.push('\n');
        let rows: Vec<Vec<String>> = group
            .containers
            .iter()
            .map(|container| match list.backend {
                BackendId::Docker | BackendId::Podman => vec![
                    display_name(container, containers),
                    container.name.clone(),
                    container.image.clone(),
                    container.status.clone(),
                ],
                // A CT has no image and its display name is its name: its VMID instead.
                BackendId::Proxmox => vec![
                    container.id.clone(),
                    container.name.clone(),
                    container.status.clone(),
                ],
            })
            .collect();
        let width = |column: usize| {
            rows.iter()
                .map(|row| row[column].chars().count())
                .max()
                .unwrap_or(0)
        };
        let columns = rows.first().map_or(0, Vec::len);
        let widths: Vec<usize> = (0..columns).map(width).collect();
        for row in &rows {
            let mut line = String::from(" ");
            for (column, cell) in row.iter().enumerate() {
                if column + 1 == columns {
                    line.push_str(&format!(" {cell}"));
                } else {
                    line.push_str(&format!(" {cell:<w$} ", w = widths[column]));
                }
            }
            text.push_str(&line);
            text.push('\n');
        }
    }
    if let Some(note) = &list.note {
        text.push_str(note);
        text.push('\n');
    }
    if list.listing.skipped > 0 {
        text.push_str(&format!(
            "Skipped {} lines {} printed that Portico could not read.\n",
            list.listing.skipped,
            list.backend.program()
        ));
    }
    write!(output, "{text}").map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{Container, Listing};

    fn list(backend: BackendId, names: &[&str]) -> ContainerList {
        let containers = names
            .iter()
            .enumerate()
            .map(|(index, name)| Container {
                id: format!("{}{index}", backend.as_str()),
                name: (*name).to_owned(),
                image: "img".into(),
                command: String::new(),
                running_for: String::new(),
                status: "Up".into(),
                ports: String::new(),
                networks: String::new(),
                mounts: String::new(),
                project: None,
                // `web-1` and `web-2` are two replicas of the service `web`.
                service: name.split_once('-').map(|(service, _)| service.to_owned()),
                number: None,
                working_dir: None,
            })
            .collect();
        ContainerList {
            backend,
            target: Target::Local,
            listing: Listing {
                containers,
                skipped: 0,
            },
            via: (backend == BackendId::Proxmox).then_some(Via::Sudo),
            note: None,
        }
    }

    #[test]
    fn picker_rows_name_their_backend_only_when_there_are_several() {
        let docker = list(BackendId::Docker, &["web"]);
        let proxmox = list(BackendId::Proxmox, &["ct"]);

        let one = every_container_row(std::slice::from_ref(&docker));
        assert_eq!(one[0].1.extra[0], "Other");

        let both = every_container_row(&[docker, proxmox]);
        let labels: Vec<&str> = both.iter().map(|(_, row)| row.extra[0].as_str()).collect();
        assert_eq!(
            labels,
            ["Docker \u{b7} Other", "Proxmox \u{b7} Proxmox containers"]
        );
        assert_eq!(
            both[1].0,
            Chosen {
                backend: BackendId::Proxmox,
                id: "proxmox0".into(),
                via: Some(Via::Sudo),
            }
        );
    }

    #[test]
    fn a_reference_ambiguous_in_one_backend_reads_as_before() {
        let lists = [
            list(BackendId::Docker, &["web-1", "web-2"]),
            list(BackendId::Podman, &["db"]),
        ];
        assert_eq!(
            resolve_in(&Target::Local, "web", &lists),
            Err("web matches several containers on this machine: web-1, web-2".into())
        );
        assert_eq!(
            resolve_in(&Target::Local, "db", &lists).map(|chosen| chosen.backend),
            Ok(BackendId::Podman)
        );
        assert_eq!(
            resolve_in(&Target::Local, "nope", &lists),
            Err("no container matches nope on this machine".into())
        );
    }
}
