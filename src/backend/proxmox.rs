//! The Proxmox VE adapter: the LXC containers (CTs) of a node, listed with `pct list` and
//! opened with `pct exec`, as root or through `sudo -n`.

use super::{
    Backend, BackendId, BackendIssue, Container, ContainerList, ListFailure, Listing, PortOffer,
    Problem, Reach, RunFailure, RunResult, SHELL_SCRIPT, Target, Via,
};
use crate::domain::tunnel::ports::{parse_ss, suggested_ports};

/// Lists the CTs as root: directly when the user is root, otherwise through `sudo -n`, which
/// never prompts. `pct` lives in `/usr/sbin`, which Debian leaves out of a non-root `PATH`.
/// Its own markers say what it found: `via=` and `user=` on stdout, `portico: no pct` and
/// `portico: no sudo` on stderr.
pub const LIST_SCRIPT: &str = r#"PATH=$PATH:/usr/sbin:/sbin
if ! command -v pct >/dev/null 2>&1; then echo 'portico: no pct' >&2; exit 127; fi
if [ "$(id -u)" = 0 ]; then echo via=none; exec pct list; fi
echo "user=$(id -un)"
if ! command -v sudo >/dev/null 2>&1; then echo 'portico: no sudo' >&2; exit 1; fi
echo via=sudo
exec sudo -n pct list
"#;

/// The words of the listing command, to be run locally or quoted for ssh.
pub fn list_words() -> Vec<String> {
    ["sh", "-c", LIST_SCRIPT].map(String::from).to_vec()
}

/// The words that open a shell in CT `vmid`, as root inside it, through `via` when the
/// listing needed sudo.
pub fn exec_words(vmid: &str, via: Option<Via>) -> Vec<String> {
    let mut words = Vec::new();
    if via == Some(Via::Sudo) {
        words.extend(["sudo", "-n"]);
    }
    words.extend(["pct", "exec", vmid, "--", "sh", "-c", SHELL_SCRIPT]);
    words.into_iter().map(String::from).collect()
}

/// Run inside a CT: its global IPv4 addresses, a `---` line, then the tcp ports it listens on.
pub const REACH_SCRIPT: &str =
    "ip -4 -o addr show scope global; echo ---; ss -Hltn 2>/dev/null || true";

/// The words that run [`REACH_SCRIPT`] in CT `vmid`, through `via` as [`exec_words`] has it.
pub fn reach_words(vmid: &str, via: Option<Via>) -> Vec<String> {
    let mut words = Vec::new();
    if via == Some(Via::Sudo) {
        words.extend(["sudo", "-n"]);
    }
    words.extend(["pct", "exec", vmid, "--", "sh", "-c", REACH_SCRIPT]);
    words.into_iter().map(String::from).collect()
}

/// Reads the output of [`REACH_SCRIPT`]: the first address, without its prefix length, and the
/// suggested ports.
pub fn parse_reach(stdout: &str) -> Reach {
    let mut addresses = String::new();
    let mut listeners = String::new();
    let mut past_marker = false;
    for line in stdout.lines() {
        if !past_marker && line.trim() == "---" {
            past_marker = true;
            continue;
        }
        let side = if past_marker {
            &mut listeners
        } else {
            &mut addresses
        };
        side.push_str(line);
        side.push('\n');
    }
    let address = addresses.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        words.find(|word| *word == "inet")?;
        let cidr = words.next()?;
        Some(cidr.split('/').next().unwrap_or(cidr).to_string())
    });
    Reach {
        address,
        host_network: false,
        ports: suggested_ports(&parse_ss(&listeners))
            .into_iter()
            .map(|(port, loopback_only)| PortOffer {
                port,
                published: None,
                loopback_only,
            })
            .collect(),
    }
}

/// What the listing script printed besides the table.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Markers {
    /// `pct` ran through `sudo -n`.
    pub sudo: bool,
    /// The user, when the script was not root.
    pub user: Option<String>,
}

/// Reads the script's markers from its stdout.
pub fn parse_markers(stdout: &str) -> Markers {
    let mut markers = Markers::default();
    for line in stdout.lines().map(str::trim) {
        if line == "via=sudo" {
            markers.sudo = true;
        } else if let Some(user) = line.strip_prefix("user=")
            && !user.is_empty()
        {
            markers.user = Some(user.to_string());
        }
    }
    markers
}

/// The byte offsets of the `Status`, `Lock` and `Name` columns in `pct list`'s header.
#[derive(Clone, Copy, Debug)]
struct Columns {
    status: usize,
    lock: usize,
    name: usize,
}

fn columns(header: &str) -> Option<Columns> {
    let at = |word: &str| header.find(word);
    let (status, lock, name) = (at("Status")?, at("Lock")?, at("Name")?);
    (status < lock && lock < name).then_some(Columns { status, lock, name })
}

/// The cell between two byte offsets, trimmed; `None` when an offset splits a character.
fn cell(line: &str, start: usize, end: Option<usize>) -> Option<&str> {
    let start = start.min(line.len());
    let end = end.map_or(line.len(), |end| end.min(line.len()));
    line.get(start..end.max(start)).map(str::trim)
}

/// One row as (VMID, status, name), cut at the header's columns when there is one.
fn row(line: &str, columns: Option<Columns>) -> (&str, &str, &str) {
    if let Some(Columns { status, lock, name }) = columns
        && let (Some(vmid), Some(state), Some(named)) = (
            cell(line, 0, Some(status)),
            cell(line, status, Some(lock)),
            cell(line, name, None),
        )
        && !vmid.contains(char::is_whitespace)
    {
        return (vmid, state, named);
    }
    let fields: Vec<&str> = line.split_whitespace().collect();
    let name = match fields.len() {
        0..=2 => "",
        _ => fields[fields.len() - 1],
    };
    (
        fields.first().copied().unwrap_or_default(),
        fields.get(1).copied().unwrap_or_default(),
        name,
    )
}

/// Reads `pct list`: the running CTs. Lines whose first word is not a number (the header, a
/// login banner, the script's markers) are ignored; a row with no status is counted in
/// `skipped`.
pub fn parse_listing(stdout: &str) -> Listing {
    let mut listing = Listing::default();
    let mut columns = None;
    for line in stdout.lines() {
        let line = line.trim_end();
        let first = line.split_whitespace().next().unwrap_or_default();
        if first == "VMID" {
            columns = self::columns(line);
            continue;
        }
        if first.is_empty() || !first.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let (vmid, status, name) = row(line, columns);
        if status.is_empty() {
            listing.skipped += 1;
            continue;
        }
        if status != "running" {
            continue;
        }
        let name = if name.is_empty() { vmid } else { name };
        listing.containers.push(Container {
            id: vmid.to_string(),
            name: name.to_string(),
            image: String::new(),
            command: String::new(),
            running_for: String::new(),
            status: status.to_string(),
            ports: String::new(),
            networks: String::new(),
            mounts: String::new(),
            project: None,
            service: None,
            number: None,
            working_dir: None,
        });
    }
    listing
}

/// Recognises a failure of the listing script from its exit status, markers and stderr.
pub fn classify(code: Option<i32>, markers: &Markers, stderr: &str) -> Problem {
    if code == Some(127) && stderr.contains("portico: no pct") {
        Problem::NotInstalled
    } else if stderr.contains("portico: no sudo") || markers.sudo && sudo_message(stderr).is_some()
    {
        Problem::PermissionDenied
    } else {
        Problem::Other
    }
}

/// sudo's own refusal, the last line it wrote that starts with `sudo:`.
fn sudo_message(stderr: &str) -> Option<&str> {
    stderr
        .lines()
        .map(str::trim)
        .rfind(|line| line.starts_with("sudo:"))
}

/// The last non-empty line on stderr, which carries the message.
fn last_line(stderr: &str) -> Option<&str> {
    stderr.lines().map(str::trim).rfind(|line| !line.is_empty())
}

/// The Proxmox adapter.
pub struct Proxmox;

impl Backend for Proxmox {
    fn id(&self) -> BackendId {
        BackendId::Proxmox
    }

    /// Runs [`LIST_SCRIPT`] once: it is root or uses `sudo -n`, and its markers tell which.
    fn list<E>(
        &self,
        target: &Target,
        run: &mut dyn FnMut(&[String]) -> RunResult<E>,
    ) -> Result<ContainerList, ListFailure<E>> {
        let output = run(&list_words()).map_err(|failure| match failure {
            RunFailure::Cancelled(error) | RunFailure::Failed(error) => ListFailure::Run(error),
        })?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let markers = parse_markers(&stdout);
        if output.status.success() {
            return Ok(ContainerList {
                backend: BackendId::Proxmox,
                target: target.clone(),
                listing: parse_listing(&stdout),
                via: markers.sudo.then_some(Via::Sudo),
                note: None,
            });
        }
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let problem = classify(output.status.code(), &markers, &stderr);
        Err(ListFailure::Issue(Box::new(issue(
            target,
            problem,
            markers.user.as_deref(),
            stderr,
        ))))
    }

    /// Runs [`REACH_SCRIPT`] in the CT through `pct exec`, as the listing reached `pct`.
    fn reach<E>(
        &self,
        target: &Target,
        container: &Container,
        via: Option<Via>,
        run: &mut dyn FnMut(&[String]) -> RunResult<E>,
    ) -> Result<Reach, ListFailure<E>> {
        let output = run(&reach_words(&container.id, via)).map_err(|failure| match failure {
            RunFailure::Cancelled(error) | RunFailure::Failed(error) => ListFailure::Run(error),
        })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            return Err(ListFailure::Issue(Box::new(issue(
                target,
                Problem::Other,
                None,
                stderr,
            ))));
        }
        Ok(parse_reach(&String::from_utf8_lossy(&output.stdout)))
    }

    /// `pct exec` into CT `id` as root; `user` does not apply (the app refuses it first).
    fn shell_words(&self, id: &str, _user: Option<&str>, via: Option<Via>) -> Vec<String> {
        exec_words(id, via)
    }

    fn plain_issue(&self, target: &Target, problem: Problem, stderr: String) -> BackendIssue {
        issue(target, problem, None, stderr)
    }
}

/// `problem` explained, naming `user` when the script reported it.
fn issue(target: &Target, problem: Problem, user: Option<&str>, stderr: String) -> BackendIssue {
    let who = user.unwrap_or("your user");
    let (headline, next) = match problem {
        Problem::NotInstalled => (
            format!("pct is not available on {target}: it is not a Proxmox VE node."),
            Vec::new(),
        ),
        Problem::PermissionDenied => {
            let headline = if stderr.contains("portico: no sudo") {
                format!(
                    "pct needs root on {target}, and {who} is not root and sudo is not installed."
                )
            } else {
                match sudo_message(&stderr) {
                    Some(message) if message.contains("password is required") => format!(
                        "pct needs root on {target}, and sudo would ask {who} for a password."
                    ),
                    Some(message) => {
                        format!("pct needs root on {target}, and sudo refused: {message}")
                    }
                    None => format!("pct needs root on {target}."),
                }
            };
            (headline, root_steps(target, who))
        }
        Problem::ServiceDown | Problem::Other => (
            match last_line(&stderr) {
                Some(message) => format!("pct failed on {target}: {message}"),
                None => format!("pct failed on {target}."),
            },
            Vec::new(),
        ),
    };
    BackendIssue {
        backend: BackendId::Proxmox,
        target: target.clone(),
        problem,
        headline,
        next,
        offer: None,
        stderr,
    }
}

/// How the user can give Portico root for `pct`; Portico never changes sudoers itself.
fn root_steps(target: &Target, who: &str) -> Vec<String> {
    let root = match target {
        Target::Local => "Run Portico as root on this machine.".to_string(),
        Target::Host(alias) => {
            format!("Connect to {alias} as root: portico edit {alias} --user root")
        }
    };
    vec![
        root,
        format!(
            "Or let {who} run pct without a password: add \"{who} ALL=(root) NOPASSWD: /usr/sbin/pct\" with visudo on {target}."
        ),
    ]
}

#[cfg(test)]
mod tests {
    use std::process::Output;

    use super::*;
    use crate::platform::process::exit_status;

    const TABLE: &str = "\
VMID       Status     Lock         Name                \n\
100        running                 web                 \n\
101        stopped                 db                  \n\
102        running    backup       mail                \n\
103        running                                     \n";

    fn ids(listing: &Listing) -> Vec<(&str, &str)> {
        listing
            .containers
            .iter()
            .map(|c| (c.id.as_str(), c.name.as_str()))
            .collect()
    }

    #[test]
    fn lists_running_cts_with_or_without_a_lock() {
        let listing = parse_listing(TABLE);
        assert_eq!(
            ids(&listing),
            [("100", "web"), ("102", "mail"), ("103", "103")]
        );
        assert_eq!(listing.skipped, 0);
        let web = &listing.containers[0];
        assert_eq!(web.status, "running");
        assert!(web.image.is_empty() && web.project.is_none() && web.service.is_none());
    }

    #[test]
    fn ignores_banners_and_markers_and_counts_rows_without_status() {
        let stdout = format!("Welcome to pve\nuser=deploy\nvia=sudo\n{TABLE}104\n");
        let listing = parse_listing(&stdout);
        assert_eq!(listing.containers.len(), 3);
        assert_eq!(listing.skipped, 1);
    }

    #[test]
    fn falls_back_to_fields_without_a_header() {
        let listing = parse_listing("200 running web\n201 running migrate box\n202 stopped x\n");
        assert_eq!(ids(&listing), [("200", "web"), ("201", "box")]);
    }

    #[test]
    fn an_empty_table_lists_nothing() {
        let listing = parse_listing("via=none\n");
        assert!(listing.containers.is_empty());
        assert_eq!(listing.skipped, 0);
    }

    #[test]
    fn reads_the_markers() {
        assert_eq!(
            parse_markers("user=deploy\nvia=sudo\n"),
            Markers {
                sudo: true,
                user: Some("deploy".into())
            }
        );
        assert_eq!(parse_markers("via=none\n"), Markers::default());
    }

    #[test]
    fn classifies_the_scripts_failures() {
        let sudo = Markers {
            sudo: true,
            user: Some("deploy".into()),
        };
        let plain = Markers::default();
        assert_eq!(
            classify(Some(127), &plain, "portico: no pct\n"),
            Problem::NotInstalled
        );
        assert_eq!(
            classify(Some(1), &plain, "portico: no sudo\n"),
            Problem::PermissionDenied
        );
        assert_eq!(
            classify(Some(1), &sudo, "sudo: a password is required\n"),
            Problem::PermissionDenied
        );
        assert_eq!(
            classify(
                Some(2),
                &plain,
                "ipcc_send_rec[1] failed: Unknown error -1\n"
            ),
            Problem::Other
        );
        // Without the sudo marker, a sudo line is not the script's sudo.
        assert_eq!(
            classify(Some(1), &plain, "sudo: a password is required\n"),
            Problem::Other
        );
    }

    #[test]
    fn shell_words_run_as_root_with_or_without_sudo() {
        let words = |via| Proxmox.shell_words("101", None, via);
        assert_eq!(
            words(None),
            ["pct", "exec", "101", "--", "sh", "-c", SHELL_SCRIPT]
        );
        assert_eq!(
            words(Some(Via::Sudo)),
            [
                "sudo",
                "-n",
                "pct",
                "exec",
                "101",
                "--",
                "sh",
                "-c",
                SHELL_SCRIPT
            ]
        );
    }

    fn output(code: i32, stdout: &str, stderr: &str) -> Output {
        Output {
            status: exit_status(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    fn list(result: Output) -> Result<ContainerList, ListFailure<()>> {
        let mut calls = Vec::new();
        let listed = Proxmox.list(&Target::Host("pve".into()), &mut |words| {
            calls.push(words.to_vec());
            Ok(result.clone())
        });
        assert_eq!(calls, [list_words()]);
        listed
    }

    fn failure(result: Result<ContainerList, ListFailure<()>>) -> BackendIssue {
        match result {
            Err(ListFailure::Issue(issue)) => *issue,
            other => panic!("expected an issue, got {other:?}"),
        }
    }

    #[test]
    fn a_root_listing_runs_pct_directly() {
        let list = list(output(0, &format!("via=none\n{TABLE}"), "")).unwrap();
        assert_eq!(list.backend, BackendId::Proxmox);
        assert_eq!(list.via, None);
        assert_eq!(list.note, None);
        assert_eq!(list.listing.containers.len(), 3);
    }

    #[test]
    fn a_sudo_listing_remembers_the_sudo() {
        let list = list(output(0, &format!("user=deploy\nvia=sudo\n{TABLE}"), "")).unwrap();
        assert_eq!(list.via, Some(Via::Sudo));
    }

    #[test]
    fn a_node_without_pct_is_not_installed() {
        let issue = failure(list(output(127, "", "portico: no pct\n")));
        assert_eq!(issue.problem, Problem::NotInstalled);
        assert_eq!(
            issue.headline,
            "pct is not available on pve: it is not a Proxmox VE node."
        );
        assert!(issue.next.is_empty());
    }

    #[test]
    fn a_password_for_sudo_is_permission_denied_with_both_ways_out() {
        let issue = failure(list(output(
            1,
            "user=deploy\nvia=sudo\n",
            "sudo: a password is required\n",
        )));
        assert_eq!(issue.backend, BackendId::Proxmox);
        assert_eq!(issue.problem, Problem::PermissionDenied);
        assert_eq!(
            issue.headline,
            "pct needs root on pve, and sudo would ask deploy for a password."
        );
        assert_eq!(
            issue.next,
            [
                "Connect to pve as root: portico edit pve --user root",
                "Or let deploy run pct without a password: add \"deploy ALL=(root) NOPASSWD: /usr/sbin/pct\" with visudo on pve.",
            ]
        );
        assert_eq!(issue.offer, None);
    }

    #[test]
    fn another_sudo_refusal_is_quoted() {
        let issue = failure(list(output(
            1,
            "user=deploy\nvia=sudo\n",
            "sudo: deploy is not in the sudoers file.\n",
        )));
        assert_eq!(
            issue.headline,
            "pct needs root on pve, and sudo refused: sudo: deploy is not in the sudoers file."
        );
    }

    #[test]
    fn no_sudo_is_permission_denied() {
        let issue = failure(list(output(1, "user=deploy\n", "portico: no sudo\n")));
        assert_eq!(issue.problem, Problem::PermissionDenied);
        assert_eq!(
            issue.headline,
            "pct needs root on pve, and deploy is not root and sudo is not installed."
        );
    }

    #[test]
    fn any_other_failure_quotes_pct() {
        let issue = failure(list(output(
            2,
            "via=none\n",
            "ipcc_send_rec[1] failed: Unknown error -1\n",
        )));
        assert_eq!(issue.problem, Problem::Other);
        assert_eq!(
            issue.headline,
            "pct failed on pve: ipcc_send_rec[1] failed: Unknown error -1"
        );
    }

    #[test]
    fn local_steps_name_this_machine() {
        let issue = Proxmox.plain_issue(&Target::Local, Problem::PermissionDenied, String::new());
        assert_eq!(
            issue.next,
            [
                "Run Portico as root on this machine.",
                "Or let your user run pct without a password: add \"your user ALL=(root) NOPASSWD: /usr/sbin/pct\" with visudo on this machine.",
            ]
        );
    }

    #[test]
    fn a_cancelled_listing_stops() {
        let result = Proxmox.list(&Target::Local, &mut |_| Err(RunFailure::Cancelled(())));
        assert!(matches!(result, Err(ListFailure::Run(()))));
    }

    #[test]
    fn the_script_lists_as_root_or_through_sudo() {
        // Run the real script with fake `id`, `pct` and `sudo` on PATH.
        let dir = tempfile::tempdir().unwrap();
        let tool = |name: &str, body: &str| {
            crate::test_executable::write_executable(
                dir.path().join(name),
                &format!("#!/bin/sh\n{body}\n"),
            );
        };
        tool("pct", "echo \"pct $*\"");
        tool("sudo", "echo \"sudo $*\"");
        let run = |uid: &str| {
            tool(
                "id",
                &format!("case \"$1\" in -u) echo {uid} ;; -un) echo deploy ;; esac"),
            );
            let output = std::process::Command::new(crate::test_executable::posix_sh())
                .args(["-c", LIST_SCRIPT])
                .env("PATH", dir.path())
                .output()
                .unwrap();
            String::from_utf8(output.stdout).unwrap()
        };
        assert_eq!(run("0"), "via=none\npct list\n");
        assert_eq!(run("1000"), "user=deploy\nvia=sudo\nsudo -n pct list\n");
    }

    #[test]
    fn reach_reads_the_address_and_listeners_of_a_ct() {
        let stdout = "2: eth0    inet 192.168.1.50/24 brd 192.168.1.255 scope global eth0\n---\nLISTEN 0 4096 127.0.0.1:5432 0.0.0.0:*\nLISTEN 0 511 0.0.0.0:80 0.0.0.0:*\n";
        let mut calls = Vec::new();
        let container = Container {
            id: "101".into(),
            name: "db".into(),
            image: String::new(),
            command: String::new(),
            running_for: String::new(),
            status: "running".into(),
            ports: String::new(),
            networks: String::new(),
            mounts: String::new(),
            project: None,
            service: None,
            number: None,
            working_dir: None,
        };
        let reach = Proxmox
            .reach::<()>(&Target::Local, &container, Some(Via::Sudo), &mut |words| {
                calls.push(words.to_vec());
                Ok(Output {
                    status: exit_status(0),
                    stdout: stdout.as_bytes().to_vec(),
                    stderr: Vec::new(),
                })
            })
            .unwrap();
        assert_eq!(calls, [reach_words("101", Some(Via::Sudo))]);
        assert_eq!(
            calls[0][..4],
            ["sudo", "-n", "pct", "exec"].map(String::from)
        );
        assert_eq!(reach.address.as_deref(), Some("192.168.1.50"));
        let ports: Vec<_> = reach
            .ports
            .iter()
            .map(|o| (o.port, o.loopback_only))
            .collect();
        assert_eq!(ports, [(80, false), (5432, true)]);
        assert!(reach.ports.iter().all(|o| o.published.is_none()));
        let failed = Proxmox.reach::<()>(&Target::Local, &container, None, &mut |_| {
            Ok(Output {
                status: exit_status(255),
                stdout: Vec::new(),
                stderr: b"no".to_vec(),
            })
        });
        assert!(matches!(failed, Err(ListFailure::Issue(_))));
    }

    #[test]
    fn reach_keeps_the_ports_of_a_ct_with_no_address() {
        let reach = parse_reach("---\nLISTEN 0 511 0.0.0.0:80 0.0.0.0:*\n");
        assert_eq!(reach.address, None);
        assert_eq!(reach.ports.len(), 1);
        assert_eq!(reach.ports[0].port, 80);
    }
}
