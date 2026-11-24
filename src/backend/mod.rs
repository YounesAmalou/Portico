//! The interface between the app and the tools that run containers.

pub mod docker;
pub mod listing;

use std::{fmt, process::Output};

use docker::Docker;

pub use listing::{Container, Group, Listing, ResolveError, display_name, group, resolve};

/// Where the tool runs: on this machine, or through ssh on a host of the inventory.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum Target {
    Local,
    Host(String),
}

impl Target {
    /// `this machine`, or the host's alias.
    pub fn describe(&self) -> String {
        match self {
            Target::Local => "this machine".into(),
            Target::Host(alias) => alias.clone(),
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.describe())
    }
}

/// The tool that runs a command inside the docker group. `newgrp` cannot: it takes no command.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum GroupTool {
    Sg,
}

impl GroupTool {
    /// The name `--via` takes for the tool.
    pub fn as_str(self) -> &'static str {
        match self {
            GroupTool::Sg => "sg",
        }
    }
}

/// Quotes one word for a POSIX shell: words made only of safe characters
/// stay as they are, anything else is single-quoted.
pub fn quote(word: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "_./:=@%+,-".contains(c);
    if !word.is_empty() && word.chars().all(safe) {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

/// The words quoted and joined into the one string ssh runs remotely.
pub fn remote_command<S: AsRef<str>>(words: &[S]) -> String {
    words
        .iter()
        .map(|word| quote(word.as_ref()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The running containers of a target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContainerList {
    pub target: Target,
    pub listing: Listing,
    /// The tool docker had to run through to reach the socket, if any.
    pub via: Option<GroupTool>,
    /// Said once when `via` is set: other shells still lack the group.
    pub note: Option<String>,
}

/// A container backend.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackendId {
    Docker,
}

impl BackendId {
    /// The id as the JSON output spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            BackendId::Docker => "docker",
        }
    }
}

/// What went wrong, in terms every backend shares.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Problem {
    NotInstalled,
    ServiceDown,
    PermissionDenied,
    Other,
}

impl Problem {
    /// The problem as the JSON output spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Problem::NotInstalled => "not-installed",
            Problem::ServiceDown => "daemon-down",
            Problem::PermissionDenied => "permission-denied",
            Problem::Other => "other",
        }
    }
}

/// A fix Portico can run on the target after a confirmation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Offer {
    /// The TUI key that runs it. It must not be a key the containers panel already binds
    /// (`q`, `L`, `h`, `r`, `/`, `u`, `k`, `j`, `n`, the arrows, Enter, Esc): the offer arm
    /// comes after every fixed key, so a colliding key would never fire.
    pub key: char,
    /// The TUI footer hint.
    pub hint: String,
    /// The CLI question, without ` [y/N] `.
    pub prompt: String,
    /// The command, run on the target after a yes.
    pub words: Vec<String>,
}

/// A backend failure, explained.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackendIssue {
    pub backend: BackendId,
    pub target: Target,
    pub problem: Problem,
    /// One-line summary.
    pub headline: String,
    /// Steps the user can take themselves.
    pub next: Vec<String>,
    /// A fix Portico can run after a confirmation.
    pub offer: Option<Offer>,
    /// What the tool wrote to stderr.
    pub stderr: String,
}

/// How running one command failed, as the app reports it to the adapter.
#[derive(Debug)]
pub enum RunFailure<E> {
    Cancelled(E),
    Failed(E),
}

/// What the app's transport returns for one command.
pub type RunResult<E> = Result<Output, RunFailure<E>>;

/// Why a listing produced no list.
#[derive(Debug)]
pub enum ListFailure<E> {
    Run(E),
    /// Boxed: an issue is much larger than the other variant.
    Issue(Box<BackendIssue>),
}

/// A tool that runs containers. `run` is the app's transport: it runs the words on the target
/// and returns the captured output.
pub trait Backend {
    fn id(&self) -> BackendId;

    /// Lists the running containers of `target`, running each command it needs through `run`.
    /// A `RunFailure::Cancelled` always stops the listing as `ListFailure::Run`; a
    /// `RunFailure::Failed` stops it the same way, except where the adapter treats it as no
    /// answer (Docker's probe does). A tool that ran and refused is a `ListFailure::Issue`.
    fn list<E>(
        &self,
        target: &Target,
        run: &mut dyn FnMut(&[String]) -> RunResult<E>,
    ) -> Result<ContainerList, ListFailure<E>>;

    /// The command that opens a shell in container `id`, as `user` when given, through `via`
    /// when the listing needed it.
    fn shell_words(&self, id: &str, user: Option<&str>, via: Option<GroupTool>) -> Vec<String>;

    /// `problem` explained without asking the target anything more.
    fn plain_issue(&self, target: &Target, problem: Problem, stderr: String) -> BackendIssue;
}

/// Lists the running containers of `target` with `backend`; see [`Backend::list`].
pub fn list<E>(
    backend: BackendId,
    target: &Target,
    run: &mut dyn FnMut(&[String]) -> RunResult<E>,
) -> Result<ContainerList, ListFailure<E>> {
    match backend {
        BackendId::Docker => Docker.list(target, run),
    }
}

/// The command that opens a shell in a container of `backend`; see [`Backend::shell_words`].
pub fn shell_words(
    backend: BackendId,
    id: &str,
    user: Option<&str>,
    via: Option<GroupTool>,
) -> Vec<String> {
    match backend {
        BackendId::Docker => Docker.shell_words(id, user, via),
    }
}

/// `problem` explained by `backend`; see [`Backend::plain_issue`].
pub fn plain_issue(
    backend: BackendId,
    target: &Target,
    problem: Problem,
    stderr: String,
) -> BackendIssue {
    match backend {
        BackendId::Docker => Docker.plain_issue(target, problem, stderr),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_only_when_needed() {
        assert_eq!(quote("web"), "web");
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("o'brien"), "'o'\\''brien'");
        assert_eq!(quote(""), "''");
    }

    #[test]
    fn remote_command_round_trips_through_sh() {
        let words = ["printf", "%s|", "a b", "o'brien", "", "$HOME"];
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(remote_command(&words))
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "a b|o'brien||$HOME|"
        );
    }

    #[test]
    fn ids_and_problems_keep_the_json_strings() {
        assert_eq!(BackendId::Docker.as_str(), "docker");
        assert_eq!(Problem::NotInstalled.as_str(), "not-installed");
        assert_eq!(Problem::ServiceDown.as_str(), "daemon-down");
        assert_eq!(Problem::PermissionDenied.as_str(), "permission-denied");
        assert_eq!(Problem::Other.as_str(), "other");
    }
}
