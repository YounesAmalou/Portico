//! The Podman adapter: the containers of the user Portico connects as (rootless, or root's
//! when that user is root), listed with `podman ps --format json` and opened with
//! `podman exec`.

use serde_json::Value;

use crate::platform::process::UNIX_TOOLS;

use super::{
    Backend, BackendId, BackendIssue, Container, ContainerList, ListFailure, Listing, Problem,
    Reach, RunFailure, RunResult, SHELL_SCRIPT, Target, Via,
};

/// Names the user whose containers these are, then lists them. Without `podman`, `exec`
/// fails with the shell's `not found` and status 127.
pub const LIST_SCRIPT: &str = r#"echo "portico-user=$(id -un 2>/dev/null)"
exec podman ps --format json
"#;

/// The marker [`LIST_SCRIPT`] prints before the listing.
const USER_MARKER: &str = "portico-user=";

/// The words of the listing command, to be run locally or quoted for ssh.
pub fn list_words() -> Vec<String> {
    ["sh", "-c", LIST_SCRIPT].map(String::from).to_vec()
}

/// The words that list `target`'s containers: [`list_words`], or on a local machine without
/// Unix tools (`unix_tools` false: Windows, with no `sh`) `podman ps` itself, which names no
/// user.
fn listing_words(target: &Target, unix_tools: bool) -> Vec<String> {
    if unix_tools || !matches!(target, Target::Local) {
        list_words()
    } else {
        ["podman", "ps", "--format", "json"]
            .map(String::from)
            .to_vec()
    }
}

/// The words that open a shell in container `id`, as `user` when given.
pub fn exec_words(id: &str, user: Option<&str>) -> Vec<String> {
    let mut words = vec!["podman", "exec", "-it"];
    if let Some(user) = user {
        words.extend(["-u", user]);
    }
    words.extend([id, "sh", "-c", SHELL_SCRIPT]);
    words.into_iter().map(String::from).collect()
}

/// The user [`LIST_SCRIPT`] named, when it did.
pub fn parse_user(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix(USER_MARKER))
        .filter(|user| !user.is_empty())
        .map(String::from)
}

/// Reads the JSON array `podman ps --format json` prints, starting at the first line that
/// starts with `[` (the marker or a login banner may come before it). `None` when there is no
/// array or it cannot be parsed. A pod's infra container is left out; an object without an
/// `Id` is counted in `skipped`.
pub fn parse_listing(stdout: &str) -> Option<Listing> {
    let start = stdout
        .lines()
        .scan(0, |offset, line| {
            let at = *offset;
            *offset += line.len() + 1;
            Some((at, line))
        })
        .find(|(_, line)| line.trim_start().starts_with('['))
        .map(|(at, _)| at)?;
    let Value::Array(objects) = serde_json::from_str(&stdout[start..]).ok()? else {
        return None;
    };
    let mut listing = Listing::default();
    for object in &objects {
        if object.get("IsInfra").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        match container_from(object) {
            Some(container) => listing.containers.push(container),
            None => listing.skipped += 1,
        }
    }
    Some(listing)
}

fn container_from(object: &Value) -> Option<Container> {
    let text = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let words = |key: &str, separator: &str| {
        object
            .get(key)
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(separator)
            })
            .unwrap_or_default()
    };
    let label = |name: &str| {
        object
            .get("Labels")
            .and_then(|labels| labels.get(name))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(String::from)
    };
    let id = object.get("Id").and_then(Value::as_str)?;
    if id.is_empty() {
        return None;
    }
    let name = object
        .get("Names")
        .and_then(Value::as_array)
        .and_then(|names| names.first())
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let status = Some(text("Status"))
        .filter(|status| !status.is_empty())
        .unwrap_or_else(|| text("State"));
    Some(Container {
        id: id.to_string(),
        name,
        image: text("Image"),
        command: words("Command", " "),
        running_for: String::new(),
        status,
        ports: ports(object.get("Ports")),
        networks: words("Networks", ","),
        mounts: words("Mounts", ","),
        project: label("com.docker.compose.project"),
        service: label("com.docker.compose.service"),
        number: label("com.docker.compose.container-number").and_then(|n| n.parse().ok()),
        working_dir: label("com.docker.compose.project.working_dir"),
    })
}

/// Podman's port mappings as Docker prints them: `0.0.0.0:8080->80/tcp`, joined by `, `. A
/// mapping with a `range` above 1 covers that many consecutive ports.
fn ports(ports: Option<&Value>) -> String {
    let Some(ports) = ports.and_then(Value::as_array) else {
        return String::new();
    };
    ports
        .iter()
        .filter_map(|port| {
            let number = |key: &str| port.get(key).and_then(Value::as_u64);
            let container = number("container_port")?;
            let protocol = port
                .get("protocol")
                .and_then(Value::as_str)
                .unwrap_or("tcp");
            let range = number("range").unwrap_or(1).max(1);
            let span = |first: u64| {
                if range > 1 {
                    format!("{first}-{}", first + range - 1)
                } else {
                    first.to_string()
                }
            };
            Some(match number("host_port").filter(|port| *port != 0) {
                Some(host) => {
                    let ip = port
                        .get("host_ip")
                        .and_then(Value::as_str)
                        .filter(|ip| !ip.is_empty())
                        .unwrap_or("0.0.0.0");
                    format!("{ip}:{}->{}/{protocol}", span(host), span(container))
                }
                None => format!("{}/{protocol}", span(container)),
            })
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Recognises a failure of the listing script from its exit status and stderr.
pub fn classify(code: Option<i32>, stderr: &str) -> Problem {
    let lower = stderr.to_lowercase();
    if code == Some(127) && lower.contains("not found") {
        Problem::NotInstalled
    } else if !needs_subordinate_ids(stderr) && lower.contains("permission denied") {
        Problem::PermissionDenied
    } else {
        Problem::Other
    }
}

/// Whether rootless Podman failed for want of subordinate user or group IDs.
fn needs_subordinate_ids(stderr: &str) -> bool {
    ["subuid", "subgid", "newuidmap", "newgidmap"]
        .iter()
        .any(|word| stderr.contains(word))
}

/// The last non-empty line on stderr, which carries Podman's message.
fn last_line(stderr: &str) -> Option<&str> {
    stderr.lines().map(str::trim).rfind(|line| !line.is_empty())
}

/// The Podman adapter.
pub struct Podman;

impl Backend for Podman {
    fn id(&self) -> BackendId {
        BackendId::Podman
    }

    /// Runs [`LIST_SCRIPT`] once, as the user Portico connects as (`podman ps` itself on a
    /// Windows machine).
    fn list<E>(
        &self,
        target: &Target,
        run: &mut dyn FnMut(&[String]) -> RunResult<E>,
    ) -> Result<ContainerList, ListFailure<E>> {
        list(target, run, UNIX_TOOLS)
    }

    /// `podman exec` into container `id`, as `user` when given. Podman needs no `via`.
    fn shell_words(&self, id: &str, user: Option<&str>, _via: Option<Via>) -> Vec<String> {
        exec_words(id, user)
    }

    /// `podman inspect` of the container, as docker's but reading the exposed ports from
    /// `.NetworkSettings.Ports`, which Podman before 5.3 has. Podman needs no `via`.
    fn reach<E>(
        &self,
        target: &Target,
        container: &Container,
        _via: Option<Via>,
        run: &mut dyn FnMut(&[String]) -> RunResult<E>,
    ) -> Result<Reach, ListFailure<E>> {
        super::docker::reach_with(
            "podman",
            super::docker::PODMAN_INSPECT_FORMAT,
            container,
            None::<fn(&[String]) -> Vec<String>>,
            run,
            |stderr| issue(target, Problem::Other, None, stderr),
        )
    }

    fn plain_issue(&self, target: &Target, problem: Problem, stderr: String) -> BackendIssue {
        issue(target, problem, None, stderr)
    }
}

/// `Podman::list`, where this machine has Unix tools when `unix_tools`.
fn list<E>(
    target: &Target,
    run: &mut dyn FnMut(&[String]) -> RunResult<E>,
    unix_tools: bool,
) -> Result<ContainerList, ListFailure<E>> {
    {
        let output = run(&listing_words(target, unix_tools)).map_err(|failure| match failure {
            RunFailure::Cancelled(error) | RunFailure::Failed(error) => ListFailure::Run(error),
        })?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let user = parse_user(&stdout);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if !output.status.success() {
            let problem = classify(output.status.code(), &stderr);
            return Err(failed(issue(target, problem, user.as_deref(), stderr)));
        }
        let Some(listing) = parse_listing(&stdout) else {
            return Err(failed(BackendIssue {
                headline: format!("podman printed a listing Portico could not read on {target}."),
                next: Vec::new(),
                ..issue(target, Problem::Other, user.as_deref(), stderr)
            }));
        };
        let note = match user.as_deref() {
            Some(user) if listing.containers.is_empty() && user != "root" => Some(format!(
                "Podman lists {user}'s own containers; connect as root to list root's."
            )),
            _ => None,
        };
        Ok(ContainerList {
            backend: BackendId::Podman,
            target: target.clone(),
            listing,
            via: None,
            note,
        })
    }
}

fn failed<E>(issue: BackendIssue) -> ListFailure<E> {
    ListFailure::Issue(Box::new(issue))
}

/// `problem` explained, naming `user` when the script reported it.
fn issue(target: &Target, problem: Problem, user: Option<&str>, stderr: String) -> BackendIssue {
    let who = user.unwrap_or("your user");
    let message = last_line(&stderr).map(String::from);
    let (headline, next) = match problem {
        Problem::NotInstalled => (
            format!(
                "Podman is not installed on {target}, or it is not on the PATH of a non-interactive shell there."
            ),
            vec![super::install_hint("Podman", target)],
        ),
        Problem::PermissionDenied => (
            message.unwrap_or_else(|| "Permission denied".into()),
            vec![format!(
                "Check that {who} can run podman on {target}: podman info says why it cannot."
            )],
        ),
        Problem::Other if needs_subordinate_ids(&stderr) => (
            message.unwrap_or_else(|| format!("podman failed on {target}.")),
            vec![format!(
                "Give {who} subordinate IDs on {target}: add a range for them to /etc/subuid and /etc/subgid, then run podman system migrate."
            )],
        ),
        Problem::ServiceDown | Problem::Other => (
            match message {
                Some(message) => format!("podman failed on {target}: {message}"),
                None => format!("podman failed on {target}."),
            },
            Vec::new(),
        ),
    };
    BackendIssue {
        backend: BackendId::Podman,
        target: target.clone(),
        problem,
        headline,
        next,
        offer: None,
        stderr,
    }
}

#[cfg(test)]
mod tests {
    use std::process::Output;

    use super::*;
    use crate::platform::process::exit_status;

    /// A listing recorded from `podman ps --format json` (Podman 5), trimmed: a Compose
    /// service with a port, a lone container, and a pod's infra container.
    const LISTING: &str = r#"[
  {
    "AutoRemove": false,
    "Command": ["nginx", "-g", "daemon off;"],
    "Id": "6f1c0a3c9e2b4d5f8a7b6c5d4e3f2a1b0c9d8e7f6a5b4c3d2e1f0a9b8c7d6e5f",
    "Image": "docker.io/library/nginx:latest",
    "IsInfra": false,
    "Labels": {
      "com.docker.compose.project": "shop",
      "com.docker.compose.service": "web",
      "com.docker.compose.container-number": "1",
      "com.docker.compose.project.working_dir": "/srv/shop"
    },
    "Mounts": ["/srv/shop/html"],
    "Names": ["shop_web_1"],
    "Networks": ["shop_default"],
    "Ports": [{"host_ip": "", "container_port": 80, "host_port": 8080, "range": 1, "protocol": "tcp"}],
    "State": "running",
    "Status": "Up 2 hours"
  },
  {
    "Command": null,
    "Id": "1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a2b",
    "Image": "registry.fedoraproject.org/fedora:41",
    "IsInfra": false,
    "Labels": null,
    "Mounts": [],
    "Names": ["toolbox"],
    "Networks": [],
    "Ports": null,
    "State": "running",
    "Status": ""
  },
  {
    "Id": "9f8e7d6c5b4a39281706f5e4d3c2b1a09f8e7d6c5b4a39281706f5e4d3c2b1a0",
    "Image": "localhost/podman-pause:5.2.0",
    "IsInfra": true,
    "Names": ["a1b2c3d4-infra"],
    "State": "running",
    "Status": "Up 3 hours"
  }
]
"#;

    fn output(code: i32, stdout: &str, stderr: &str) -> Output {
        Output {
            status: exit_status(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    fn list(target: &Target, output: Output) -> Result<ContainerList, ListFailure<()>> {
        let mut calls = Vec::new();
        let mut output = Some(output);
        // With Unix tools, so that the script's tests run on every system.
        let result = super::list(
            target,
            &mut |words| {
                calls.push(words.to_vec());
                Ok(output.take().expect("one command"))
            },
            true,
        );
        assert_eq!(calls, [list_words()]);
        result
    }

    #[test]
    fn a_local_machine_without_unix_tools_runs_podman_itself() {
        let mut calls = Vec::new();
        let mut run = |words: &[String]| {
            calls.push(words.to_vec());
            Ok::<_, RunFailure<()>>(output(0, LISTING, ""))
        };
        let listed = super::list(&Target::Local, &mut run, false).unwrap();
        assert_eq!(calls, [["podman", "ps", "--format", "json"]]);
        assert!(!listed.listing.containers.is_empty());
        assert_eq!(listed.note, None);
        // A host is a Linux machine with sh whatever this one is.
        let mut calls = Vec::new();
        let mut run = |words: &[String]| {
            calls.push(words.to_vec());
            Ok::<_, RunFailure<()>>(output(0, LISTING, ""))
        };
        super::list(&Target::Host("dev".into()), &mut run, false).unwrap();
        assert_eq!(calls, [list_words()]);
    }

    fn issue_of(result: Result<ContainerList, ListFailure<()>>) -> BackendIssue {
        match result {
            Err(ListFailure::Issue(issue)) => *issue,
            other => panic!("expected an issue, got {other:?}"),
        }
    }

    fn host() -> Target {
        Target::Host("dev".into())
    }

    #[test]
    fn parses_a_listing_after_the_marker() {
        let stdout = format!("portico-user=deploy\n{LISTING}");
        let listing = parse_listing(&stdout).unwrap();
        assert_eq!(listing.skipped, 0);
        assert_eq!(
            listing.containers.len(),
            2,
            "the infra container is left out"
        );
        let web = &listing.containers[0];
        assert_eq!(web.name, "shop_web_1");
        assert_eq!(web.image, "docker.io/library/nginx:latest");
        assert_eq!(web.command, "nginx -g daemon off;");
        assert_eq!(web.status, "Up 2 hours");
        assert_eq!(web.ports, "0.0.0.0:8080->80/tcp");
        assert_eq!(web.networks, "shop_default");
        assert_eq!(web.mounts, "/srv/shop/html");
        assert_eq!(web.project.as_deref(), Some("shop"));
        assert_eq!(web.service.as_deref(), Some("web"));
        assert_eq!(web.number, Some(1));
        assert_eq!(web.working_dir.as_deref(), Some("/srv/shop"));
        let toolbox = &listing.containers[1];
        assert_eq!(toolbox.name, "toolbox");
        assert_eq!(
            toolbox.status, "running",
            "an empty Status falls back to State"
        );
        assert_eq!((toolbox.command.as_str(), toolbox.ports.as_str()), ("", ""));
        assert_eq!(toolbox.project, None);
    }

    #[test]
    fn counts_objects_without_an_id_and_rejects_unreadable_output() {
        let listing = parse_listing("banner\n[{\"Names\":[\"x\"]},{\"Id\":\"\"}]").unwrap();
        assert_eq!((listing.containers.len(), listing.skipped), (0, 2));
        assert_eq!(parse_listing("[]").unwrap(), Listing::default());
        assert_eq!(parse_listing("portico-user=me\n"), None);
        assert_eq!(parse_listing("[{\"Id\":"), None);
        assert_eq!(parse_listing("{}"), None);
    }

    #[test]
    fn formats_ports_like_docker() {
        let ports = serde_json::json!([
            {"host_ip": "127.0.0.1", "container_port": 5432, "host_port": 5433, "range": 1, "protocol": "tcp"},
            {"host_ip": "", "container_port": 53, "host_port": 1053, "range": 2, "protocol": "udp"},
            {"container_port": 9000, "host_port": 0, "protocol": "tcp"},
        ]);
        assert_eq!(
            super::ports(Some(&ports)),
            "127.0.0.1:5433->5432/tcp, 0.0.0.0:1053-1054->53-54/udp, 9000/tcp"
        );
    }

    #[test]
    fn lists_with_one_script_and_notes_an_empty_rootless_listing() {
        let list = list(
            &host(),
            output(0, &format!("portico-user=deploy\n{LISTING}"), ""),
        )
        .unwrap();
        assert_eq!(list.backend, BackendId::Podman);
        assert_eq!(list.listing.containers.len(), 2);
        assert_eq!((list.via, list.note), (None, None));

        let empty = list_empty("deploy");
        assert_eq!(
            empty.note.as_deref(),
            Some("Podman lists deploy's own containers; connect as root to list root's.")
        );
        assert_eq!(
            list_empty("root").note,
            None,
            "root's listing needs no note"
        );
    }

    fn list_empty(user: &str) -> ContainerList {
        list(
            &host(),
            output(0, &format!("portico-user={user}\n[]\n"), ""),
        )
        .unwrap()
    }

    #[test]
    fn a_missing_podman_is_not_installed() {
        let issue = issue_of(list(
            &host(),
            output(
                127,
                "portico-user=deploy\n",
                "sh: 2: exec: podman: not found\n",
            ),
        ));
        assert_eq!(issue.backend, BackendId::Podman);
        assert_eq!(issue.problem, Problem::NotInstalled);
        assert_eq!(
            issue.headline,
            "Podman is not installed on dev, or it is not on the PATH of a non-interactive shell there."
        );
        assert_eq!(
            issue.next,
            ["Install Podman from the distribution's packages, then list again."]
        );
        assert_eq!(issue.offer, None);
    }

    #[test]
    fn missing_subordinate_ids_say_how_to_add_them() {
        let stderr = "Error: cannot set up namespace using \"/usr/bin/newuidmap\": exit status 1\n";
        let issue = issue_of(list(&host(), output(125, "portico-user=deploy\n", stderr)));
        assert_eq!(issue.problem, Problem::Other);
        assert_eq!(
            issue.headline,
            "Error: cannot set up namespace using \"/usr/bin/newuidmap\": exit status 1"
        );
        assert_eq!(
            issue.next,
            [
                "Give deploy subordinate IDs on dev: add a range for them to /etc/subuid and /etc/subgid, then run podman system migrate."
            ]
        );
    }

    #[test]
    fn a_refusal_is_permission_denied() {
        let stderr = "Error: mkdir /run/user/1000/libpod: permission denied\n";
        let issue = issue_of(list(&Target::Local, output(125, "", stderr)));
        assert_eq!(issue.problem, Problem::PermissionDenied);
        assert_eq!(
            issue.headline,
            "Error: mkdir /run/user/1000/libpod: permission denied"
        );
        assert_eq!(
            issue.next,
            [
                "Check that your user can run podman on this machine: podman info says why it cannot."
            ]
        );
    }

    #[test]
    fn anything_else_quotes_podman() {
        let issue = issue_of(list(&host(), output(125, "", "Error: something broke\n")));
        assert_eq!(issue.problem, Problem::Other);
        assert_eq!(
            issue.headline,
            "podman failed on dev: Error: something broke"
        );
        assert!(issue.next.is_empty());

        let unreadable = issue_of(list(&host(), output(0, "portico-user=me\nnope\n", "")));
        assert_eq!(unreadable.problem, Problem::Other);
        assert_eq!(
            unreadable.headline,
            "podman printed a listing Portico could not read on dev."
        );
    }

    #[test]
    fn a_cancelled_listing_stops() {
        let result: Result<ContainerList, ListFailure<&str>> =
            Podman.list(&host(), &mut |_| Err(RunFailure::Cancelled("stop")));
        assert!(matches!(result, Err(ListFailure::Run("stop"))));
    }

    #[test]
    fn shell_words_take_a_user_and_no_via() {
        assert_eq!(
            Podman.shell_words("abc", None, None),
            ["podman", "exec", "-it", "abc", "sh", "-c", SHELL_SCRIPT]
        );
        assert_eq!(
            Podman.shell_words("abc", Some("app"), None),
            [
                "podman",
                "exec",
                "-it",
                "-u",
                "app",
                "abc",
                "sh",
                "-c",
                SHELL_SCRIPT
            ]
        );
    }

    #[test]
    fn reach_works_on_podman_before_5_3() {
        // Podman before 5.3 has no `.Config.ExposedPorts`: a template naming it fails.
        let container = Container {
            id: "abc".into(),
            name: "web".into(),
            image: String::new(),
            command: String::new(),
            running_for: String::new(),
            status: String::new(),
            ports: "0.0.0.0:8080->80/tcp".into(),
            networks: String::new(),
            mounts: String::new(),
            project: None,
            service: None,
            number: None,
            working_dir: None,
        };
        let reach = Podman
            .reach::<()>(&host(), &container, None, &mut |words| {
                let format = &words[3];
                if format.contains(".Config.ExposedPorts") {
                    return Ok(output(
                        125,
                        "",
                        "Error: template: inspect:1:52: executing \"inspect\" at <.Config.ExposedPorts>: can't evaluate field ExposedPorts in type *define.InspectContainerConfig",
                    ));
                }
                assert!(format.contains(".NetworkSettings.Ports"));
                Ok(output(
                    0,
                    r#"{"podman":{"IPAddress":"10.88.0.5"}}|{"80/tcp":[{"HostIp":"","HostPort":"8080"}],"443/tcp":null}|bridge"#,
                    "",
                ))
            })
            .unwrap();
        assert_eq!(reach.address.as_deref(), Some("10.88.0.5"));
        let ports: Vec<_> = reach.ports.iter().map(|o| (o.port, o.published)).collect();
        assert_eq!(ports, [(80, Some(8080)), (443, None)]);
    }

    #[test]
    fn a_rootless_container_with_no_address_has_none() {
        let mut calls = Vec::new();
        let container = Container {
            id: "abc".into(),
            name: "web".into(),
            image: String::new(),
            command: String::new(),
            running_for: String::new(),
            status: String::new(),
            ports: "0.0.0.0:8080->80/tcp".into(),
            networks: String::new(),
            mounts: String::new(),
            project: None,
            service: None,
            number: None,
            working_dir: None,
        };
        let reach = Podman
            .reach::<()>(&host(), &container, Some(Via::Sg), &mut |words| {
                calls.push(words.to_vec());
                Ok(output(
                    0,
                    r#"{"podman":{"IPAddress":""}}|{"80/tcp":{}}|pasta"#,
                    "",
                ))
            })
            .unwrap();
        assert_eq!(calls[0][..2], ["podman", "inspect"].map(String::from));
        assert_eq!(reach.address, None);
        assert_eq!(reach.ports.len(), 1);
        let failed = Podman.reach::<()>(&host(), &container, None, &mut |_| {
            Ok(output(1, "", "boom"))
        });
        assert!(matches!(failed, Err(ListFailure::Issue(_))));
    }
}
