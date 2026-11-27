//! The interface between the app and the tools that run containers.

pub mod docker;
pub mod listing;
pub mod podman;
pub mod proxmox;

use std::{fmt, process::Output};

use docker::Docker;
use podman::Podman;
use proxmox::Proxmox;

pub use listing::{
    Container, Group, Listing, Published, ResolveError, display_name, group, remember_key, resolve,
};

/// Where the tool runs: on this machine, or through ssh on a host of the inventory.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
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

/// The tool a backend's command had to run through: `sg` runs docker inside the docker group
/// (`newgrp` cannot: it takes no command), `sudo -n` runs Proxmox's `pct` as root.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Via {
    Sg,
    Sudo,
}

impl Via {
    /// The name `--via` takes for the tool.
    pub fn as_str(self) -> &'static str {
        match self {
            Via::Sg => "sg",
            Via::Sudo => "sudo",
        }
    }
}

/// Runs bash when the container has it, and sh otherwise.
pub const SHELL_SCRIPT: &str = "command -v bash >/dev/null 2>&1 && exec bash || exec sh";

/// How to install `engine` (Docker, Podman) for `target`: from the distribution's packages,
/// or, on this machine where it has no Unix tools (Windows), with its desktop app.
pub(crate) fn install_hint(engine: &str, target: &Target) -> String {
    install_hint_with(engine, target, crate::platform::process::UNIX_TOOLS)
}

fn install_hint_with(engine: &str, target: &Target, unix_tools: bool) -> String {
    if unix_tools || !matches!(target, Target::Local) {
        format!("Install {engine} from the distribution's packages, then list again.")
    } else {
        format!("Install {engine} Desktop, then list again.")
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
    /// The backend that listed them.
    pub backend: BackendId,
    pub target: Target,
    pub listing: Listing,
    /// The tool the backend had to run through (docker to reach the socket, pct to be root).
    pub via: Option<Via>,
    /// Said once when `via` is set: other shells still lack the group.
    pub note: Option<String>,
}

/// One port a container listens on, as a forward can reach it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PortOffer {
    /// The port inside the container.
    pub port: u16,
    /// The port of the host it is published on, when it is.
    pub published: Option<u16>,
    /// The one host address it is published on, when that is neither a wildcard nor loopback:
    /// reached there rather than at `localhost`.
    pub published_address: Option<String>,
    /// Listens on loopback only inside the container: nothing but the container reaches it.
    pub loopback_only: bool,
}

/// Where a container can be reached from its host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Reach {
    /// The container's own address on the host, when it has one a host can use.
    pub address: Option<String>,
    /// The container shares the host's network: its ports are the host's.
    pub host_network: bool,
    pub ports: Vec<PortOffer>,
}

/// A container backend. Docker is the default: the backend of a held shell named without one.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum BackendId {
    #[default]
    Docker,
    Podman,
    Proxmox,
}

/// Every backend, in the order Portico lists them: the TUI's tabs and the CLI's sections. A
/// command that shows one backend's list without being told which (the JSON output) takes the
/// first one installed on the target.
pub const AUTOMATIC: [BackendId; 3] = [BackendId::Docker, BackendId::Podman, BackendId::Proxmox];

impl BackendId {
    /// The id as the JSON output spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            BackendId::Docker => "docker",
            BackendId::Podman => "podman",
            BackendId::Proxmox => "proxmox",
        }
    }

    /// The backend with this id (`docker`, `podman` or `proxmox`).
    pub fn from_str_name(name: &str) -> Option<Self> {
        AUTOMATIC.into_iter().find(|id| id.as_str() == name)
    }

    /// The program the backend runs, as messages name it.
    pub fn program(self) -> &'static str {
        match self {
            BackendId::Docker => "docker",
            BackendId::Podman => "podman",
            BackendId::Proxmox => "pct",
        }
    }

    /// Whether its shells can run as a user the caller chooses.
    pub fn chooses_user(self) -> bool {
        match self {
            BackendId::Docker | BackendId::Podman => true,
            BackendId::Proxmox => false,
        }
    }

    /// The backend's name, as tab titles and section headings show it.
    pub fn title(self) -> &'static str {
        match self {
            BackendId::Docker => "Docker",
            BackendId::Podman => "Podman",
            BackendId::Proxmox => "Proxmox",
        }
    }

    /// The heading of the containers that belong to no Compose project.
    pub fn ungrouped_heading(self) -> &'static str {
        match self {
            BackendId::Docker | BackendId::Podman => "Other",
            BackendId::Proxmox => "Proxmox containers",
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
    fn shell_words(&self, id: &str, user: Option<&str>, via: Option<Via>) -> Vec<String>;

    /// Where `container` can be reached from `target`: its address and the ports it offers,
    /// asked with the commands `run` runs, through `via` as `shell_words` applies it. A run that
    /// fails stops it as `ListFailure::Run`; a tool that ran and refused is a
    /// `ListFailure::Issue`.
    fn reach<E>(
        &self,
        target: &Target,
        container: &Container,
        via: Option<Via>,
        run: &mut dyn FnMut(&[String]) -> RunResult<E>,
    ) -> Result<Reach, ListFailure<E>>;

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
        BackendId::Podman => Podman.list(target, run),
        BackendId::Proxmox => Proxmox.list(target, run),
    }
}

/// The command that opens a shell in a container of `backend`; see [`Backend::shell_words`].
pub fn shell_words(
    backend: BackendId,
    id: &str,
    user: Option<&str>,
    via: Option<Via>,
) -> Vec<String> {
    match backend {
        BackendId::Docker => Docker.shell_words(id, user, via),
        BackendId::Podman => Podman.shell_words(id, user, via),
        BackendId::Proxmox => Proxmox.shell_words(id, user, via),
    }
}

/// Where a container of `backend` can be reached; see [`Backend::reach`].
pub fn reach<E>(
    backend: BackendId,
    target: &Target,
    container: &Container,
    via: Option<Via>,
    run: &mut dyn FnMut(&[String]) -> RunResult<E>,
) -> Result<Reach, ListFailure<E>> {
    match backend {
        BackendId::Docker => Docker.reach(target, container, via, run),
        BackendId::Podman => Podman.reach(target, container, via, run),
        BackendId::Proxmox => Proxmox.reach(target, container, via, run),
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
        BackendId::Podman => Podman.plain_issue(target, problem, stderr),
        BackendId::Proxmox => Proxmox.plain_issue(target, problem, stderr),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_windows_machine_is_told_to_install_the_desktop_app() {
        let host = Target::Host("dev".into());
        assert_eq!(
            install_hint_with("Docker", &Target::Local, true),
            "Install Docker from the distribution's packages, then list again."
        );
        assert_eq!(
            install_hint_with("Podman", &Target::Local, false),
            "Install Podman Desktop, then list again."
        );
        // A host is a Linux machine whatever this one is.
        assert_eq!(
            install_hint_with("Docker", &host, false),
            "Install Docker from the distribution's packages, then list again."
        );
    }

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
        assert_eq!(BackendId::Podman.as_str(), "podman");
        assert_eq!(BackendId::Proxmox.as_str(), "proxmox");
        assert_eq!(Via::Sg.as_str(), "sg");
        assert_eq!(Via::Sudo.as_str(), "sudo");
        assert_eq!(Problem::NotInstalled.as_str(), "not-installed");
        assert_eq!(Problem::ServiceDown.as_str(), "daemon-down");
        assert_eq!(Problem::PermissionDenied.as_str(), "permission-denied");
        assert_eq!(Problem::Other.as_str(), "other");
    }
}
