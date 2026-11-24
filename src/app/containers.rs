//! Containers on this machine or on a host: listing them, opening a shell in one, and the
//! fixes Portico offers when docker refuses.

use std::{
    ffi::{OsStr, OsString},
    io,
    path::PathBuf,
    process::{Command, ExitStatus, Output, Stdio},
    sync::atomic::AtomicBool,
};

use thiserror::Error;

use super::{App, AppError};
use crate::{
    backend::{
        self, BackendId, BackendIssue, ContainerList, GroupTool, ListFailure, Offer, Problem,
        RunFailure, Target, remote_command,
    },
    domain::explain::Explanation,
    openssh::{ConnectionOutcome, OpenSshError},
};

/// Why a container command did not run.
#[derive(Debug, Error)]
pub enum ContainerError {
    #[error(transparent)]
    App(#[from] AppError),
    #[error("ssh to {alias} failed")]
    Ssh {
        alias: String,
        /// Boxed, like the issue, to keep the error small.
        explanation: Option<Box<Explanation>>,
        stderr: String,
    },
    /// Boxed: an issue is much larger than the other variants.
    #[error("{}", .0.headline)]
    Backend(Box<BackendIssue>),
    #[error("no container matches {reference} on {target}")]
    NoMatch { target: Target, reference: String },
    #[error("{reference} matches several containers on {target}: {}", .candidates.join(", "))]
    Ambiguous {
        target: Target,
        reference: String,
        candidates: Vec<String>,
    },
}

impl From<BackendIssue> for ContainerError {
    fn from(issue: BackendIssue) -> Self {
        ContainerError::Backend(Box::new(issue))
    }
}

/// The programs container commands run on this machine. On a host the bare names are used.
///
/// Only the first word of a local command is mapped, so the `docker` inside an `sg`
/// rerun (`sg docker -c 'docker ps …'`) is the bare name, found on PATH.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContainerPrograms {
    pub docker: PathBuf,
    pub sudo: PathBuf,
    pub sg: PathBuf,
    pub sh: PathBuf,
}

impl Default for ContainerPrograms {
    fn default() -> Self {
        Self {
            docker: "docker".into(),
            sudo: "sudo".into(),
            sg: "sg".into(),
            sh: "sh".into(),
        }
    }
}

impl App {
    pub fn container_programs(&self) -> &ContainerPrograms {
        &self.container_programs
    }

    /// Lists the running containers of `target`. On a host, `batch` keeps ssh from prompting
    /// and lets `cancelled` stop it, and an ssh failure is explained as in the TUI. When the
    /// socket is refused because this session predates the user's docker group, the listing
    /// is rerun inside the group.
    pub fn containers(
        &self,
        target: &Target,
        batch: bool,
        cancelled: Option<&AtomicBool>,
    ) -> Result<ContainerList, ContainerError> {
        let target = self.container_target(target)?;
        backend::list(BackendId::Docker, &target, &mut |words| {
            self.capture(&target, words, batch, cancelled)
                .map_err(|error| match error {
                    ContainerError::App(AppError::OpenSsh(OpenSshError::Cancelled)) => {
                        RunFailure::Cancelled(error)
                    }
                    error => RunFailure::Failed(error),
                })
        })
        .map_err(|failure| match failure {
            ListFailure::Run(error) => error,
            ListFailure::Issue(issue) => ContainerError::Backend(issue),
        })
    }

    /// Opens a shell in container `id` of `target`, in this terminal or a new one, running
    /// docker through `via` when the listing needed it. The shell's exit status is passed
    /// through unclassified; over ssh a 255 is explained as a failed connection, worded for
    /// the TUI when `in_tui`.
    #[expect(
        clippy::too_many_arguments,
        reason = "the CLI and the TUI each pass every choice"
    )]
    pub fn container_shell(
        &self,
        target: &Target,
        id: &str,
        user: Option<&str>,
        via: Option<GroupTool>,
        new_terminal: bool,
        cancelled: Option<&AtomicBool>,
        in_tui: bool,
    ) -> Result<ConnectionOutcome, ContainerError> {
        let target = self.container_target(target)?;
        let words = backend::shell_words(BackendId::Docker, id, user, via);
        if new_terminal {
            let mut args: Vec<OsString> = vec!["shell".into(), "--hold".into()];
            if let Target::Host(alias) = &target {
                args.extend(["--on".into(), alias.into()]);
            }
            if let Some(user) = user {
                args.extend(["-u".into(), user.into()]);
            }
            if let Some(tool) = via {
                args.extend(["--via".into(), tool.as_str().into()]);
            }
            args.extend(["--".into(), id.into()]);
            let fallback = self.direct_words(&target, &words);
            return Ok(self
                .openssh
                .launch_new_terminal_with(&args, &fallback)
                .map_err(AppError::from)?);
        }
        match &target {
            Target::Local => {
                let status = self.run_local(&words)?;
                Ok(ConnectionOutcome::Exited {
                    alias: target.describe(),
                    status,
                    explanation: None,
                })
            }
            Target::Host(alias) => {
                let mirrored = self
                    .openssh
                    .connect_mirrored_command(
                        alias,
                        Some(&remote_command(&words)),
                        Box::new(io::stderr()),
                        cancelled,
                    )
                    .map_err(AppError::from)?;
                // Only 255 is ssh's own failure; any other code is docker's or the shell's.
                let explanation = if mirrored.status.code() == Some(255) {
                    self.explain_connection(alias, &mirrored.tail, in_tui)
                } else {
                    None
                };
                Ok(ConnectionOutcome::Exited {
                    alias: alias.clone(),
                    status: mirrored.status,
                    explanation,
                })
            }
        }
    }

    /// Runs `offer` on `target` in the user's terminal, through `ssh -t` on a host, and
    /// returns its exit status.
    pub fn run_offer(&self, target: &Target, offer: &Offer) -> Result<ExitStatus, ContainerError> {
        let target = self.container_target(target)?;
        let words = &offer.words;
        match &target {
            Target::Local => self.run_local(words),
            Target::Host(alias) => Ok(self
                .openssh
                .connect_mirrored_command(
                    alias,
                    Some(&remote_command(words)),
                    Box::new(io::stderr()),
                    None,
                )
                .map_err(AppError::from)?
                .status),
        }
    }

    /// The target with a host's alias spelled as the inventory spells it.
    fn container_target(&self, target: &Target) -> Result<Target, ContainerError> {
        Ok(match target {
            Target::Local => Target::Local,
            Target::Host(alias) => Target::Host(self.connectable_alias(alias)?),
        })
    }

    /// Runs `words` on `target` with stdout and stderr captured. On a host, ssh's own failure
    /// (255 with nothing on stdout) is an `Ssh` error.
    fn capture(
        &self,
        target: &Target,
        words: &[String],
        batch: bool,
        cancelled: Option<&AtomicBool>,
    ) -> Result<Output, ContainerError> {
        let alias = match target {
            Target::Local => {
                let mut command = self.local_command(words);
                command.stdin(if batch {
                    Stdio::null()
                } else {
                    Stdio::inherit()
                });
                let output = match cancelled {
                    Some(cancelled) if batch => {
                        self.openssh.cancellable_output(&mut command, cancelled)
                    }
                    _ => command.output().map_err(|source| OpenSshError::Io {
                        program: command.get_program().into(),
                        source,
                    }),
                };
                return output.map_err(|error| local_failure(words, error));
            }
            Target::Host(alias) => alias,
        };
        let output = self
            .openssh
            .run_remote(alias, &remote_command(words), batch, cancelled)
            .map_err(AppError::from)?;
        if output.status.code() == Some(255) && output.stdout.is_empty() {
            return Err(ContainerError::Ssh {
                alias: alias.clone(),
                explanation: self
                    .explain_connection(alias, &output.stderr, batch)
                    .map(Box::new),
                stderr: lossy(&output.stderr),
            });
        }
        Ok(output)
    }

    /// Runs `words` on this machine with the terminal inherited.
    fn run_local(&self, words: &[String]) -> Result<ExitStatus, ContainerError> {
        let mut command = self.local_command(words);
        command.status().map_err(|source| {
            local_failure(
                words,
                OpenSshError::Io {
                    program: command.get_program().into(),
                    source,
                },
            )
        })
    }

    /// `words` as a command on this machine, its first word replaced by the configured program.
    fn local_command(&self, words: &[String]) -> Command {
        let mut command = Command::new(self.local_program(&words[0]));
        command.args(&words[1..]);
        command
    }

    fn local_program<'a>(&'a self, word: &'a str) -> &'a OsStr {
        let programs = &self.container_programs;
        match word {
            "docker" => programs.docker.as_os_str(),
            "sudo" => programs.sudo.as_os_str(),
            "sg" => programs.sg.as_os_str(),
            "sh" => programs.sh.as_os_str(),
            other => OsStr::new(other),
        }
    }

    /// The command a terminal runs to do `words` on `target` without Portico.
    fn direct_words(&self, target: &Target, words: &[String]) -> Vec<OsString> {
        match target {
            Target::Local => std::iter::once(self.local_program(&words[0]).to_os_string())
                .chain(words[1..].iter().map(OsString::from))
                .collect(),
            Target::Host(alias) => {
                let mut command: Vec<OsString> = vec![self.openssh.ssh_program().into()];
                command.extend(
                    [
                        "-t",
                        "-o",
                        "RemoteCommand=none",
                        "--",
                        alias,
                        &remote_command(words),
                    ]
                    .map(OsString::from),
                );
                command
            }
        }
    }
}

/// A local spawn failure; a `docker` that cannot be found is not installed.
fn local_failure(words: &[String], error: OpenSshError) -> ContainerError {
    if let OpenSshError::Io { source, .. } = &error
        && source.kind() == io::ErrorKind::NotFound
        && words[0] == "docker"
    {
        return ContainerError::from(backend::plain_issue(
            BackendId::Docker,
            &Target::Local,
            Problem::NotInstalled,
            error.to_string(),
        ));
    }
    AppError::from(error).into()
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use tempfile::TempDir;

    use super::*;
    use crate::{
        backend::docker::{
            SHELL_SCRIPT, exec_words, in_group, join_group_offer, join_group_words, ps_words,
            start_daemon_offer, start_daemon_words,
        },
        domain::host::Host,
        domain::managed::{ConfigPaths, ManagedStore},
        openssh::OpenSsh,
        test_executable::write_executable,
    };

    const LISTING: &str = r#"{"id":"abc123","name":"web","image":"nginx","status":"Up"}"#;
    const DENIED: &str = "permission denied while trying to connect to the Docker daemon socket at unix:///var/run/docker.sock: dial unix /var/run/docker.sock: connect: permission denied";
    const DAEMON_DOWN: &str = "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?";
    const SOCKET: &str = "/var/run/docker.sock";

    /// An initialised app with the host `dev`, whose ssh, terminal and local programs are
    /// fakes in a temporary directory. Each fake appends its arguments to `<name>.calls`, one
    /// call per line as `[arg][arg]…`, then sources `<name>.behaviour` when it exists.
    struct Fixture {
        dir: TempDir,
        app: App,
    }

    const PROGRAMS: [&str; 6] = ["ssh", "terminal", "docker", "sudo", "sg", "sh"];

    fn fixture() -> Fixture {
        fixture_with_portico(None)
    }

    fn fixture_with_portico(portico: Option<&str>) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        for name in PROGRAMS {
            let resolve = if name == "ssh" {
                "case \"$1\" in -G) if [ \"$2\" != '-F' ]; then printf 'hostname h\\nuser u\\nport 22\\n'; fi; exit 0 ;; esac\n"
            } else {
                ""
            };
            let base = dir.path().join(name);
            write_executable(
                &base,
                &format!(
                    "#!/bin/sh\n{resolve}printf '[%s]' \"$@\" >> '{base}.calls'\necho >> '{base}.calls'\nfor a; do last=$a; done\nif [ -f '{base}.behaviour' ]; then . '{base}.behaviour'; fi\nexit 0\n",
                    base = base.display()
                ),
            );
        }
        let program = |name: &str| dir.path().join(name);
        let store = ManagedStore::new(ConfigPaths::from_home(dir.path()));
        let openssh = OpenSsh::new(program("ssh"), program("terminal"))
            .with_portico(portico.map(PathBuf::from));
        let app = App::new(store, openssh).with_container_programs(ContainerPrograms {
            docker: program("docker"),
            sudo: program("sudo"),
            sg: program("sg"),
            sh: program("sh"),
        });
        app.initialize(true).unwrap();
        app.add(Host::new("dev", "h.example", None, None, None, None).unwrap())
            .unwrap();
        // Adding the host validated it through ssh.
        let _ = fs::remove_file(dir.path().join("ssh.calls"));
        Fixture { dir, app }
    }

    impl Fixture {
        fn behave(&self, name: &str, script: &str) {
            fs::write(self.dir.path().join(format!("{name}.behaviour")), script).unwrap();
        }

        fn calls(&self, name: &str) -> String {
            fs::read_to_string(self.dir.path().join(format!("{name}.calls"))).unwrap_or_default()
        }

        fn path(&self, name: &str) -> PathBuf {
            self.dir.path().join(name)
        }
    }

    /// One call as the fakes log it.
    fn call<S: AsRef<str>>(words: &[S]) -> String {
        let mut line: String = words
            .iter()
            .map(|word| format!("[{}]", word.as_ref()))
            .collect();
        line.push('\n');
        line
    }

    fn probe_output(
        database: &str,
        group: &str,
        socket: &str,
        socket_group: &str,
        tools: &str,
    ) -> String {
        format!(
            "printf 'user=me\\nprocess=me\\ndatabase={database}\\ngroup={group}\\nsocket={socket}\\nsocket_group={socket_group}\\n{tools}'\n"
        )
    }

    const ALL_TOOLS: &str = "sg=yes\\nnewgrp=yes\\nsystemctl=yes\\n";
    const NO_TOOLS: &str = "sg=no\\nnewgrp=no\\nsystemctl=no\\n";

    fn fails_with(message: &str) -> String {
        format!("echo '{message}' >&2\nexit 1\n")
    }

    fn docker_issue(result: Result<ContainerList, ContainerError>) -> BackendIssue {
        match result {
            Err(ContainerError::Backend(issue)) => *issue,
            other => panic!("expected a docker issue, got {other:?}"),
        }
    }

    fn host() -> Target {
        Target::Host("dev".into())
    }

    #[test]
    fn target_describe() {
        assert_eq!(Target::Local.describe(), "this machine");
        assert_eq!(Target::Host("dev".into()).describe(), "dev");
    }

    #[test]
    fn offers_run_the_container_builders() {
        assert_eq!(
            join_group_offer(&Target::Local, "me", SOCKET).words,
            join_group_words("me", SOCKET)
        );
        assert_eq!(
            start_daemon_offer(&Target::Local).words,
            start_daemon_words()
        );
    }

    #[test]
    fn resolution_errors_name_the_target() {
        let none = ContainerError::NoMatch {
            target: host(),
            reference: "db".into(),
        };
        assert_eq!(none.to_string(), "no container matches db on dev");
        let several = ContainerError::Ambiguous {
            target: Target::Local,
            reference: "web".into(),
            candidates: vec!["shop/web#1".into(), "shop/web#2".into()],
        };
        assert_eq!(
            several.to_string(),
            "web matches several containers on this machine: shop/web#1, shop/web#2"
        );
    }

    #[test]
    fn a_local_listing_runs_the_configured_docker() {
        let fixture = fixture();
        fixture.behave("docker", &format!("echo '{LISTING}'\n"));

        let list = fixture.app.containers(&Target::Local, false, None).unwrap();

        assert_eq!(list.target, Target::Local);
        assert_eq!(list.listing.containers[0].id, "abc123");
        assert_eq!((list.via, list.note), (None, None));
        assert_eq!(fixture.calls("docker"), call(&ps_words()[1..]));
    }

    #[test]
    fn a_local_listing_can_be_cancelled_in_batch_mode() {
        let fixture = fixture();
        let cancelled = AtomicBool::new(true);
        assert!(matches!(
            fixture
                .app
                .containers(&Target::Local, true, Some(&cancelled)),
            Err(ContainerError::App(AppError::OpenSsh(
                crate::openssh::OpenSshError::Cancelled
            )))
        ));
    }

    #[test]
    fn a_local_docker_that_cannot_be_spawned_is_not_installed() {
        let fixture = fixture();
        let app = App::new(
            ManagedStore::new(ConfigPaths::from_home(fixture.dir.path())),
            OpenSsh::new(fixture.path("ssh"), fixture.path("terminal")),
        )
        .with_container_programs(ContainerPrograms {
            docker: fixture.path("missing-docker"),
            ..ContainerPrograms::default()
        });

        let issue = docker_issue(app.containers(&Target::Local, false, None));

        assert_eq!(issue.problem, Problem::NotInstalled);
        assert_eq!(
            issue.headline,
            "Docker is not installed on this machine, or it is not on the PATH of a non-interactive shell there."
        );
        assert_eq!(
            issue.next,
            ["Install Docker from the distribution's packages, then list again."]
        );
        assert_eq!(issue.offer, None);
    }

    #[test]
    fn a_remote_listing_skips_remote_command_and_batches_only_when_asked() {
        let fixture = fixture();
        fixture.behave("ssh", &format!("echo 'Welcome!'\necho '{LISTING}'\n"));

        let list = fixture
            .app
            .containers(&Target::Host("DEV".into()), true, None)
            .unwrap();
        assert_eq!(list.target, host());
        assert_eq!(list.listing.containers[0].id, "abc123");
        let command = remote_command(&ps_words());
        assert_eq!(
            fixture.calls("ssh"),
            call(&[
                "-o",
                "RemoteCommand=none",
                "-o",
                "BatchMode=yes",
                "--",
                "dev",
                &command
            ])
        );

        fs::remove_file(fixture.path("ssh.calls")).unwrap();
        fixture.app.containers(&host(), false, None).unwrap();
        assert_eq!(
            fixture.calls("ssh"),
            call(&["-o", "RemoteCommand=none", "--", "dev", &command])
        );
    }

    #[test]
    fn a_remote_not_found_is_not_installed_on_the_host() {
        let fixture = fixture();
        fixture.behave("ssh", "echo 'sh: 1: docker: not found' >&2\nexit 127\n");

        let issue = docker_issue(fixture.app.containers(&host(), false, None));

        assert_eq!(issue.problem, Problem::NotInstalled);
        assert_eq!(
            issue.headline,
            "Docker is not installed on dev, or it is not on the PATH of a non-interactive shell there."
        );
        assert_eq!(issue.stderr, "sh: 1: docker: not found\n");
    }

    #[test]
    fn ssh_failing_with_255_and_no_output_is_an_ssh_error() {
        let fixture = fixture();
        fixture.behave(
            "ssh",
            "echo 'ssh: connect to host h port 22: Connection refused' >&2\nexit 255\n",
        );

        match fixture.app.containers(&host(), false, None) {
            Err(error @ ContainerError::Ssh { .. }) => {
                assert_eq!(error.to_string(), "ssh to dev failed");
                let ContainerError::Ssh { alias, stderr, .. } = error else {
                    unreachable!()
                };
                assert_eq!(alias, "dev");
                assert!(stderr.contains("Connection refused"), "{stderr}");
            }
            other => panic!("expected an ssh error, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_host_is_an_app_error() {
        let fixture = fixture();
        assert!(matches!(
            fixture.app.containers(&Target::Host("nope".into()), false, None),
            Err(ContainerError::App(AppError::UnknownHost(alias))) if alias == "nope"
        ));
    }

    #[test]
    fn a_session_older_than_the_group_reruns_the_listing_through_sg() {
        let fixture = fixture();
        fixture.behave("docker", &fails_with(DENIED));
        fixture.behave(
            "sh",
            &probe_output("me docker", "yes", SOCKET, "docker", ALL_TOOLS),
        );
        fixture.behave("sg", &format!("echo '{LISTING}'\n"));

        let list = fixture.app.containers(&Target::Local, false, None).unwrap();

        assert_eq!(list.via, Some(GroupTool::Sg));
        assert_eq!(
            list.note.as_deref(),
            Some(
                "Ran docker through `sg docker`: other shells need `newgrp docker` or a new login."
            )
        );
        assert_eq!(list.listing.containers[0].id, "abc123");
        assert_eq!(
            fixture.calls("sh"),
            call(&["-c", crate::backend::docker::PROBE_SCRIPT])
        );
        assert_eq!(
            fixture.calls("sg"),
            call(&in_group(GroupTool::Sg, &ps_words())[1..])
        );
    }

    #[test]
    fn a_user_outside_the_group_is_offered_to_join_it() {
        let fixture = fixture();
        fixture.behave(
            "ssh",
            &format!(
                "case \"$last\" in\n*'id -un'*) {probe};;\n*) echo '{DENIED}' >&2; exit 1 ;;\nesac\n",
                probe = probe_output("me", "yes", SOCKET, "root", ALL_TOOLS),
            ),
        );

        let issue = docker_issue(fixture.app.containers(&host(), false, None));

        assert_eq!(issue.problem, Problem::PermissionDenied);
        assert_eq!(issue.offer, Some(join_group_offer(&host(), "me", SOCKET)));
        assert_eq!(
            issue.headline,
            "me cannot use Docker on dev: permission denied on /var/run/docker.sock."
        );
        assert_eq!(
            issue.next,
            [
                format!(
                    "Add me to the docker group (root-equivalent access): {}",
                    remote_command(&join_group_words("me", SOCKET))
                ),
                "Or use rootless Docker: https://docs.docker.com/engine/security/rootless/".into(),
                "Or run sudo docker yourself in portico connect dev".into(),
            ]
        );
        assert_eq!(issue.stderr, format!("{DENIED}\n"));

        fixture.behave("docker", &fails_with(DENIED));
        fixture.behave("sh", &probe_output("me", "no", SOCKET, "root", ALL_TOOLS));
        let issue = docker_issue(fixture.app.containers(&Target::Local, false, None));
        assert_eq!(
            issue.headline,
            "me cannot use Docker on this machine: permission denied on /var/run/docker.sock."
        );
        assert_eq!(issue.next[2], "Or run sudo docker yourself");
    }

    #[test]
    fn a_session_older_than_the_group_without_sg_needs_a_login() {
        let fixture = fixture();
        fixture.behave("docker", &fails_with(DENIED));
        fixture.behave(
            "sh",
            &probe_output(
                "me docker",
                "yes",
                SOCKET,
                "docker",
                "sg=no\\nnewgrp=yes\\nsystemctl=no\\n",
            ),
        );

        let issue = docker_issue(fixture.app.containers(&Target::Local, false, None));

        assert_eq!(
            issue.headline,
            "me is in the docker group on this machine, but this session started before that and sg is not available."
        );
        assert_eq!(issue.next, ["Log out and back in, then list again."]);
        assert_eq!(issue.offer, None);
    }

    #[test]
    fn a_socket_of_another_group_gets_no_offer() {
        let fixture = fixture();
        fixture.behave("docker", &fails_with(DENIED));
        fixture.behave("sh", &probe_output("me", "yes", SOCKET, "staff", ALL_TOOLS));

        let issue = docker_issue(fixture.app.containers(&Target::Local, false, None));

        assert_eq!(
            issue.headline,
            "Permission denied on /var/run/docker.sock, which belongs to group staff."
        );
        assert_eq!(
            issue.next,
            ["Ask the administrator of this machine for access, or run sudo docker yourself."]
        );
        assert_eq!(issue.offer, None);
    }

    #[test]
    fn a_socket_that_is_not_local_gets_no_offer() {
        let fixture = fixture();
        fixture.behave("docker", &fails_with(DENIED));
        fixture.behave("sh", &probe_output("me", "no", "", "", ALL_TOOLS));

        let issue = docker_issue(fixture.app.containers(&Target::Local, false, None));

        assert_eq!(issue.offer, None);
        assert_eq!(
            issue.headline,
            "Permission denied by the Docker daemon on this machine, whose DOCKER_HOST is not a local unix socket."
        );
        assert_eq!(
            issue.next,
            ["Ask the administrator of this machine for access, or run sudo docker yourself."]
        );
    }

    #[test]
    fn a_failed_probe_explains_with_dockers_message() {
        let fixture = fixture();
        fixture.behave("docker", &fails_with(DENIED));
        fixture.behave("sh", "exit 2\n");

        let issue = docker_issue(fixture.app.containers(&Target::Local, false, None));

        assert_eq!(issue.problem, Problem::PermissionDenied);
        assert_eq!(issue.headline, DENIED);
        assert_eq!(issue.offer, None);
        assert_eq!(issue.stderr, format!("{DENIED}\n"));
    }

    #[test]
    fn a_stopped_daemon_is_offered_a_start_when_systemctl_exists() {
        let fixture = fixture();
        fixture.behave("docker", &fails_with(DAEMON_DOWN));
        fixture.behave(
            "sh",
            &probe_output("me docker", "yes", SOCKET, "docker", ALL_TOOLS),
        );

        let issue = docker_issue(fixture.app.containers(&Target::Local, false, None));

        assert_eq!(issue.problem, Problem::ServiceDown);
        assert_eq!(
            issue.headline,
            "The Docker daemon is not running on this machine."
        );
        assert_eq!(issue.next, ["Start it with: sudo systemctl start docker"]);
        assert_eq!(issue.offer, Some(start_daemon_offer(&Target::Local)));

        fixture.behave(
            "sh",
            &probe_output("me docker", "yes", SOCKET, "docker", NO_TOOLS),
        );
        let issue = docker_issue(fixture.app.containers(&Target::Local, false, None));
        assert_eq!(issue.next, ["Start the Docker daemon, then list again."]);
        assert_eq!(issue.offer, None);
    }

    #[test]
    fn another_docker_failure_shows_dockers_message() {
        let fixture = fixture();
        fixture.behave("docker", &fails_with("Error response from daemon: boom"));

        let issue = docker_issue(fixture.app.containers(&Target::Local, false, None));

        assert_eq!(issue.problem, Problem::Other);
        assert_eq!(
            issue.headline,
            "docker failed on this machine: Error response from daemon: boom"
        );
        assert_eq!(issue.offer, None);
        assert_eq!(fixture.calls("sh"), "");
    }

    #[test]
    fn a_local_shell_runs_docker_exec_and_passes_its_status_through() {
        let fixture = fixture();
        fixture.behave("docker", &fails_with(DENIED).replace("exit 1", "exit 7"));

        let outcome = fixture
            .app
            .container_shell(
                &Target::Local,
                "abc123",
                Some("root"),
                None,
                false,
                None,
                false,
            )
            .unwrap();

        let ConnectionOutcome::Exited {
            alias,
            status,
            explanation,
        } = outcome
        else {
            panic!("expected an exit, got {outcome:?}");
        };
        assert_eq!(alias, "this machine");
        assert_eq!(status.code(), Some(7));
        assert_eq!(explanation, None);
        assert_eq!(
            fixture.calls("docker"),
            call(&[
                "exec",
                "-it",
                "-u",
                "root",
                "abc123",
                "sh",
                "-c",
                SHELL_SCRIPT
            ])
        );
    }

    #[test]
    fn a_local_shell_through_sg_runs_the_configured_sg() {
        let fixture = fixture();

        fixture
            .app
            .container_shell(
                &Target::Local,
                "abc123",
                None,
                Some(GroupTool::Sg),
                false,
                None,
                false,
            )
            .unwrap();

        assert_eq!(
            fixture.calls("sg"),
            call(&in_group(GroupTool::Sg, &exec_words("abc123", None))[1..])
        );
        assert_eq!(fixture.calls("docker"), "");
    }

    #[test]
    fn a_host_shell_runs_through_ssh_with_a_terminal() {
        let fixture = fixture();
        fixture.behave("ssh", "exit 3\n");

        let outcome = fixture
            .app
            .container_shell(&host(), "abc123", None, None, false, None, false)
            .unwrap();

        assert_eq!(
            fixture.calls("ssh"),
            call(&[
                "-t",
                "-o",
                "RemoteCommand=none",
                "--",
                "dev",
                &remote_command(&exec_words("abc123", None)),
            ])
        );
        let ConnectionOutcome::Exited {
            alias,
            status,
            explanation,
        } = outcome
        else {
            panic!("expected an exit, got {outcome:?}");
        };
        assert_eq!(
            (alias.as_str(), status.code(), explanation),
            ("dev", Some(3), None)
        );
    }

    fn launched(fixture: &Fixture) -> String {
        let log = fixture.path("terminal.calls");
        for _ in 0..500 {
            if let Ok(contents) = fs::read_to_string(&log)
                && contents.ends_with('\n')
            {
                return contents;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the terminal was not launched");
    }

    #[test]
    fn a_new_terminal_reruns_portico_shell_with_the_resolved_container() {
        let fixture = fixture_with_portico(Some("/opt/portico"));
        let outcome = fixture
            .app
            .container_shell(
                &host(),
                "abc123",
                Some("root"),
                Some(GroupTool::Sg),
                true,
                None,
                false,
            )
            .unwrap();
        assert_eq!(outcome, ConnectionOutcome::LaunchRequested);
        assert_eq!(
            launched(&fixture),
            call(&[
                "/opt/portico",
                "shell",
                "--hold",
                "--on",
                "dev",
                "-u",
                "root",
                "--via",
                "sg",
                "--",
                "abc123",
            ])
        );

        let local = fixture_with_portico(Some("/opt/portico"));
        local
            .app
            .container_shell(
                &Target::Local,
                "abc123",
                None,
                Some(GroupTool::Sg),
                true,
                None,
                false,
            )
            .unwrap();
        assert_eq!(
            launched(&local),
            call(&[
                "/opt/portico",
                "shell",
                "--hold",
                "--via",
                "sg",
                "--",
                "abc123"
            ])
        );
    }

    #[test]
    fn a_new_terminal_without_portico_runs_the_command_directly() {
        let remote = fixture();
        remote
            .app
            .container_shell(&host(), "abc123", None, None, true, None, false)
            .unwrap();
        let ssh = remote.path("ssh");
        assert_eq!(
            launched(&remote),
            call(&[
                ssh.to_str().unwrap(),
                "-t",
                "-o",
                "RemoteCommand=none",
                "--",
                "dev",
                &remote_command(&exec_words("abc123", None)),
            ])
        );

        let local = fixture();
        local
            .app
            .container_shell(&Target::Local, "abc123", None, None, true, None, false)
            .unwrap();
        let docker = local.path("docker");
        let mut words = exec_words("abc123", None);
        words[0] = docker.to_str().unwrap().into();
        assert_eq!(launched(&local), call(&words));
    }

    #[test]
    fn a_local_fix_runs_the_configured_sudo_in_the_terminal() {
        let fixture = fixture();
        fixture.behave("sudo", "exit 4\n");
        let offer = join_group_offer(&Target::Local, "me", SOCKET);

        let status = fixture.app.run_offer(&Target::Local, &offer).unwrap();

        assert_eq!(status.code(), Some(4));
        assert_eq!(
            fixture.calls("sudo"),
            call(&[
                "sh",
                "-c",
                crate::backend::docker::JOIN_SCRIPT,
                "sh",
                "me",
                SOCKET
            ])
        );
    }

    #[test]
    fn a_host_fix_runs_through_ssh_with_a_terminal() {
        let fixture = fixture();

        let status = fixture
            .app
            .run_offer(&host(), &start_daemon_offer(&host()))
            .unwrap();

        assert!(status.success());
        assert_eq!(
            fixture.calls("ssh"),
            call(&[
                "-t",
                "-o",
                "RemoteCommand=none",
                "--",
                "dev",
                "sudo systemctl start docker",
            ])
        );
    }

    #[test]
    fn programs_default_to_their_bare_names() {
        let programs = ContainerPrograms::default();
        assert_eq!(programs.docker, Path::new("docker"));
        assert_eq!(programs.sudo, Path::new("sudo"));
        assert_eq!(programs.sg, Path::new("sg"));
        assert_eq!(programs.sh, Path::new("sh"));
    }
}
