//! The Docker adapter: the line format of `docker ps`, its parser, and the
//! commands and explanations around it.

use std::collections::BTreeMap;

use super::{
    Backend, BackendId, BackendIssue, Container, ContainerList, GroupTool, ListFailure, Listing,
    Offer, Problem, RunFailure, RunResult, Target, remote_command,
};

/// The `docker ps --format` template: one flat JSON object of strings per
/// container, with the Compose labels that group and name it.
pub const PS_FORMAT: &str = concat!(
    r#"{"id":{{json .ID}},"name":{{json .Names}},"image":{{json .Image}},"#,
    r#""command":{{json .Command}},"running_for":{{json .RunningFor}},"#,
    r#""status":{{json .Status}},"ports":{{json .Ports}},"#,
    r#""networks":{{json .Networks}},"mounts":{{json .Mounts}},"#,
    r#""project":{{json (.Label "com.docker.compose.project")}},"#,
    r#""service":{{json (.Label "com.docker.compose.service")}},"#,
    r#""number":{{json (.Label "com.docker.compose.container-number")}},"#,
    r#""working_dir":{{json (.Label "com.docker.compose.project.working_dir")}}}"#,
);

/// The words of the listing command, to be run locally or quoted for ssh.
pub fn ps_words() -> Vec<String> {
    ["docker", "ps", "--no-trunc", "--format", PS_FORMAT]
        .into_iter()
        .map(String::from)
        .collect()
}

/// Reads the output of [`ps_words`]. Lines that do not start with `{` (a
/// login banner, shell noise) are ignored; lines that do but cannot be
/// parsed are counted in `skipped`.
pub fn parse_listing(stdout: &str) -> Listing {
    let mut listing = Listing::default();
    for line in stdout.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        match parse_object(line) {
            Some(fields) if fields.get("id").is_some_and(|id| !id.is_empty()) => {
                listing.containers.push(container_from(fields));
            }
            _ => listing.skipped += 1,
        }
    }
    listing
}

fn container_from(mut fields: BTreeMap<String, String>) -> Container {
    let mut take = |key: &str| fields.remove(key).unwrap_or_default();
    let label = |value: String| (!value.is_empty()).then_some(value);
    Container {
        id: take("id"),
        name: take("name"),
        image: take("image"),
        command: take("command"),
        running_for: take("running_for"),
        status: take("status"),
        ports: take("ports"),
        networks: take("networks"),
        mounts: take("mounts"),
        project: label(take("project")),
        service: label(take("service")),
        number: take("number").trim().parse().ok(),
        working_dir: label(take("working_dir")),
    }
}

/// Parses one flat JSON object whose values are all strings.
fn parse_object(line: &str) -> Option<BTreeMap<String, String>> {
    let mut chars = line.chars().peekable();
    let mut fields = BTreeMap::new();
    skip_space(&mut chars);
    if chars.next()? != '{' {
        return None;
    }
    skip_space(&mut chars);
    if chars.peek() == Some(&'}') {
        chars.next();
    } else {
        loop {
            skip_space(&mut chars);
            let key = parse_string(&mut chars)?;
            skip_space(&mut chars);
            if chars.next()? != ':' {
                return None;
            }
            skip_space(&mut chars);
            let value = parse_string(&mut chars)?;
            fields.insert(key, value);
            skip_space(&mut chars);
            match chars.next()? {
                ',' => {}
                '}' => break,
                _ => return None,
            }
        }
    }
    skip_space(&mut chars);
    chars.next().is_none().then_some(fields)
}

type Chars<'a> = std::iter::Peekable<std::str::Chars<'a>>;

fn skip_space(chars: &mut Chars) {
    while chars.peek().is_some_and(|c| c.is_whitespace()) {
        chars.next();
    }
}

fn parse_string(chars: &mut Chars) -> Option<String> {
    if chars.next()? != '"' {
        return None;
    }
    let mut out = String::new();
    loop {
        match chars.next()? {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '/' => out.push('/'),
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                'b' => out.push('\u{8}'),
                'f' => out.push('\u{c}'),
                'u' => out.push(parse_unicode(chars)?),
                _ => return None,
            },
            c => out.push(c),
        }
    }
}

/// Decodes the `XXXX` after `\u`, joining a surrogate pair into one char.
fn parse_unicode(chars: &mut Chars) -> Option<char> {
    let first = hex4(chars)?;
    if (0xD800..0xDC00).contains(&first) {
        if chars.next()? != '\\' || chars.next()? != 'u' {
            return None;
        }
        let second = hex4(chars)?;
        if !(0xDC00..0xE000).contains(&second) {
            return None;
        }
        char::from_u32(0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00))
    } else {
        char::from_u32(first)
    }
}

fn hex4(chars: &mut Chars) -> Option<u32> {
    let mut value = 0;
    for _ in 0..4 {
        value = value * 16 + chars.next()?.to_digit(16)?;
    }
    Some(value)
}

/// Runs bash when the container has it, and sh otherwise.
pub const SHELL_SCRIPT: &str = "command -v bash >/dev/null 2>&1 && exec bash || exec sh";

/// The words of the command that opens a shell in a container.
pub fn exec_words(id: &str, user: Option<&str>) -> Vec<String> {
    let mut words = vec!["docker", "exec", "-it"];
    if let Some(user) = user {
        words.extend(["-u", user]);
    }
    words.extend([id, "sh", "-c", SHELL_SCRIPT]);
    words.into_iter().map(String::from).collect()
}

/// Recognises a docker failure from its exit status and stderr; over ssh the
/// same text arrives from the remote shell.
pub fn classify(code: Option<i32>, stderr: &str) -> Problem {
    let stderr = stderr.to_lowercase();
    if code == Some(127) && stderr.contains("not found") {
        Problem::NotInstalled
    } else if stderr.contains("cannot connect to the docker daemon") {
        Problem::ServiceDown
    } else if stderr.contains("permission denied while trying to connect to the docker") {
        Problem::PermissionDenied
    } else {
        Problem::Other
    }
}

/// A read-only POSIX script that reports, as `key=value` lines, what decides
/// whether the user can reach the docker socket. Every key is always printed.
pub const PROBE_SCRIPT: &str = r#"user=$(id -un 2>/dev/null)
echo "user=$user"
echo "process=$(id -Gn 2>/dev/null)"
echo "database=$(id -Gn "$user" 2>/dev/null)"
if command -v getent >/dev/null 2>&1 && getent group docker >/dev/null 2>&1; then echo group=yes; else echo group=no; fi
host=${DOCKER_HOST:-unix:///var/run/docker.sock}
case $host in unix://*) socket=${host#unix://} ;; *) socket= ;; esac
echo "socket=$socket"
socket_group=
if [ -n "$socket" ] && command -v stat >/dev/null 2>&1; then socket_group=$(stat -c %G "$socket" 2>/dev/null); fi
echo "socket_group=$socket_group"
for tool in sg systemctl; do
  if command -v $tool >/dev/null 2>&1; then echo "$tool=yes"; else echo "$tool=no"; fi
done
"#;

/// What [`PROBE_SCRIPT`] reported about the target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Probe {
    pub user: String,
    /// Groups of the current process.
    pub process_groups: Vec<String>,
    /// Groups of the user in the group database.
    pub database_groups: Vec<String>,
    pub group_exists: bool,
    pub socket: String,
    pub socket_group: Option<String>,
    pub has_sg: bool,
    pub has_systemctl: bool,
}

/// Reads the output of [`PROBE_SCRIPT`]; `None` when it has no user.
pub fn parse_probe(stdout: &str) -> Option<Probe> {
    let values: BTreeMap<&str, &str> = stdout
        .lines()
        .filter_map(|line| line.split_once('='))
        .collect();
    let value = |key: &str| values.get(key).map_or("", |value| value.trim());
    let groups = |key: &str| value(key).split_whitespace().map(String::from).collect();
    let user = value("user");
    if user.is_empty() {
        return None;
    }
    Some(Probe {
        user: user.to_string(),
        process_groups: groups("process"),
        database_groups: groups("database"),
        group_exists: value("group") == "yes",
        socket: value("socket").to_string(),
        socket_group: Some(value("socket_group"))
            .filter(|group| !group.is_empty())
            .map(String::from),
        has_sg: value("sg") == "yes",
        has_systemctl: value("systemctl") == "yes",
    })
}

/// What to do about a refused docker socket.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Access {
    /// The user is in the group, but this process predates it: rerun inside it.
    RerunInGroup(GroupTool),
    /// Same, but `sg` does not exist: a new login is needed.
    NeedsLogin,
    /// Offer to add the user to the docker group.
    OfferJoin,
    NoOffer,
}

/// Decides the fix for a permission failure from the probe.
fn decide(probe: &Probe) -> Access {
    let has = |groups: &[String]| groups.iter().any(|group| group == "docker");
    if has(&probe.database_groups) && !has(&probe.process_groups) {
        return if probe.has_sg {
            Access::RerunInGroup(GroupTool::Sg)
        } else {
            Access::NeedsLogin
        };
    }
    let socket_is_unclaimed = matches!(probe.socket_group.as_deref(), Some("docker" | "root"));
    // Without the group, only a socket that no other group claims can be handed to it.
    let group_is_missing =
        !probe.group_exists && matches!(probe.socket_group.as_deref(), None | Some("root"));
    if !has(&probe.database_groups) && (socket_is_unclaimed || group_is_missing) {
        Access::OfferJoin
    } else {
        Access::NoOffer
    }
}

/// Wraps a command so it runs inside the docker group.
pub fn in_group(tool: GroupTool, words: &[String]) -> Vec<String> {
    let GroupTool::Sg = tool;
    vec![
        "sg".to_string(),
        "docker".to_string(),
        "-c".to_string(),
        remote_command(words),
    ]
}

/// Creates the group if needed, adds the user, and gives the socket to it
/// (`$1` is the user, `$2` the socket) so no daemon restart is needed.
pub const JOIN_SCRIPT: &str =
    "groupadd -f docker && usermod -aG docker \"$1\" && chgrp docker \"$2\"";

/// The command that adds `user` to the docker group and fixes the socket.
pub fn join_group_words(user: &str, socket: &str) -> Vec<String> {
    ["sudo", "sh", "-c", JOIN_SCRIPT, "sh", user, socket]
        .into_iter()
        .map(String::from)
        .collect()
}

/// The command that starts the docker daemon.
pub fn start_daemon_words() -> Vec<String> {
    ["sudo", "systemctl", "start", "docker"]
        .into_iter()
        .map(String::from)
        .collect()
}

/// The Docker adapter.
pub struct Docker;

impl Backend for Docker {
    fn id(&self) -> BackendId {
        BackendId::Docker
    }

    /// Runs `docker ps`. When the socket is refused or the daemon is down, the probe tells
    /// why; a session that predates the user's docker group is rerun inside the group.
    fn list<E>(
        &self,
        target: &Target,
        run: &mut dyn FnMut(&[String]) -> RunResult<E>,
    ) -> Result<ContainerList, ListFailure<E>> {
        let words = ps_words();
        let output = run(&words).map_err(stopped)?;
        if output.status.success() {
            return Ok(listed(target.clone(), &output.stdout, None));
        }
        let stderr = lossy(&output.stderr);
        let problem = classify(output.status.code(), &stderr);
        if !matches!(problem, Problem::PermissionDenied | Problem::ServiceDown) {
            return Err(failed(plain_issue(target, problem, stderr)));
        }
        // A probe that fails is no answer; only a cancellation stops the listing.
        let probe = match run(&["sh", "-c", PROBE_SCRIPT].map(String::from)) {
            Ok(output) if output.status.success() => parse_probe(&lossy(&output.stdout)),
            Err(RunFailure::Cancelled(error)) => return Err(ListFailure::Run(error)),
            _ => None,
        };
        let Some(probe) = probe else {
            return Err(failed(plain_issue(target, problem, stderr)));
        };
        if problem == Problem::ServiceDown {
            return Err(failed(daemon_down(target, probe.has_systemctl, stderr)));
        }
        let issue = match decide(&probe) {
            Access::RerunInGroup(tool) => {
                let output = run(&in_group(tool, &words)).map_err(stopped)?;
                if output.status.success() {
                    return Ok(listed(target.clone(), &output.stdout, Some(tool)));
                }
                let stderr = lossy(&output.stderr);
                plain_issue(target, classify(output.status.code(), &stderr), stderr)
            }
            Access::NeedsLogin => BackendIssue {
                headline: format!(
                    "{} is in the docker group on {target}, but this session started before that and sg is not available.",
                    probe.user
                ),
                next: vec!["Log out and back in, then list again.".into()],
                ..issue(target, problem, stderr)
            },
            // An empty socket is not a local path, so its group cannot be fixed.
            Access::OfferJoin if !probe.socket.is_empty() => join_offer(target, &probe, stderr),
            Access::OfferJoin | Access::NoOffer => no_offer(target, &probe, stderr),
        };
        Err(failed(issue))
    }

    /// `docker exec` into container `id`, through `via` when the listing needed it.
    fn shell_words(&self, id: &str, user: Option<&str>, via: Option<GroupTool>) -> Vec<String> {
        let words = exec_words(id, user);
        match via {
            Some(tool) => in_group(tool, &words),
            None => words,
        }
    }

    fn plain_issue(&self, target: &Target, problem: Problem, stderr: String) -> BackendIssue {
        plain_issue(target, problem, stderr)
    }
}

/// A command that did not run, whether cancelled or failed, stops the listing.
fn stopped<E>(failure: RunFailure<E>) -> ListFailure<E> {
    match failure {
        RunFailure::Cancelled(error) | RunFailure::Failed(error) => ListFailure::Run(error),
    }
}

fn failed<E>(issue: BackendIssue) -> ListFailure<E> {
    ListFailure::Issue(Box::new(issue))
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn listed(target: Target, stdout: &[u8], via: Option<GroupTool>) -> ContainerList {
    ContainerList {
        target,
        listing: parse_listing(&lossy(stdout)),
        via,
        note: via.map(|tool| {
            format!(
                "Ran docker through `{} docker`: other shells need `newgrp docker` or a new login.",
                tool.as_str()
            )
        }),
    }
}

/// An issue with no headline, advice or offer yet.
fn issue(target: &Target, problem: Problem, stderr: String) -> BackendIssue {
    BackendIssue {
        backend: BackendId::Docker,
        target: target.clone(),
        problem,
        headline: String::new(),
        next: Vec::new(),
        offer: None,
        stderr,
    }
}

/// The last line docker wrote to stderr, which carries its message.
fn docker_message(stderr: &str) -> Option<&str> {
    stderr.lines().map(str::trim).rfind(|line| !line.is_empty())
}

fn ask_administrator(target: &Target) -> String {
    format!("Ask the administrator of {target} for access, or run sudo docker yourself.")
}

/// A problem explained without what the probe would tell.
fn plain_issue(target: &Target, problem: Problem, stderr: String) -> BackendIssue {
    let (headline, next) = match problem {
        Problem::NotInstalled => (
            format!(
                "Docker is not installed on {target}, or it is not on the PATH of a non-interactive shell there."
            ),
            vec!["Install Docker from the distribution's packages, then list again.".into()],
        ),
        Problem::ServiceDown => return daemon_down(target, false, stderr),
        Problem::PermissionDenied => (
            docker_message(&stderr)
                .unwrap_or("Permission denied")
                .to_string(),
            vec![ask_administrator(target)],
        ),
        Problem::Other => (
            match docker_message(&stderr) {
                Some(message) => format!("docker failed on {target}: {message}"),
                None => format!("docker failed on {target}."),
            },
            Vec::new(),
        ),
    };
    BackendIssue {
        headline,
        next,
        ..issue(target, problem, stderr)
    }
}

/// The offer to add `user` to the docker group on `target` and give it `socket`.
pub fn join_group_offer(target: &Target, user: &str, socket: &str) -> Offer {
    let words = join_group_words(user, socket);
    Offer {
        key: 'g',
        hint: "add to docker group".into(),
        prompt: format!(
            "Add {user} to the docker group on {target}? Members of the docker group have root-equivalent access. This runs: {}",
            remote_command(&words)
        ),
        words,
    }
}

/// The offer to start the Docker service on `target`.
pub fn start_daemon_offer(target: &Target) -> Offer {
    let words = start_daemon_words();
    Offer {
        key: 's',
        hint: "start docker".into(),
        prompt: format!(
            "Start the Docker service on {target}? This runs: {}",
            remote_command(&words)
        ),
        words,
    }
}

fn daemon_down(target: &Target, has_systemctl: bool, stderr: String) -> BackendIssue {
    BackendIssue {
        headline: format!("The Docker daemon is not running on {target}."),
        next: vec![if has_systemctl {
            "Start it with: sudo systemctl start docker".into()
        } else {
            "Start the Docker daemon, then list again.".into()
        }],
        offer: has_systemctl.then(|| start_daemon_offer(target)),
        ..issue(target, Problem::ServiceDown, stderr)
    }
}

fn join_offer(target: &Target, probe: &Probe, stderr: String) -> BackendIssue {
    let Probe { user, socket, .. } = probe;
    let sudo_yourself = match target {
        Target::Local => "Or run sudo docker yourself".to_string(),
        Target::Host(alias) => format!("Or run sudo docker yourself in portico connect {alias}"),
    };
    BackendIssue {
        headline: format!("{user} cannot use Docker on {target}: permission denied on {socket}."),
        next: vec![
            format!(
                "Add {user} to the docker group (root-equivalent access): {}",
                remote_command(&join_group_words(user, socket))
            ),
            "Or use rootless Docker: https://docs.docker.com/engine/security/rootless/".into(),
            sudo_yourself,
        ],
        offer: Some(join_group_offer(target, user, socket)),
        ..issue(target, Problem::PermissionDenied, stderr)
    }
}

fn no_offer(target: &Target, probe: &Probe, stderr: String) -> BackendIssue {
    let headline = match (probe.socket.as_str(), &probe.socket_group) {
        ("", _) => format!(
            "Permission denied by the Docker daemon on {target}, whose DOCKER_HOST is not a local unix socket."
        ),
        (socket, Some(group)) => {
            format!("Permission denied on {socket}, which belongs to group {group}.")
        }
        (socket, None) => format!("Permission denied on {socket}."),
    };
    BackendIssue {
        headline,
        next: vec![ask_administrator(target)],
        ..issue(target, Problem::PermissionDenied, stderr)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        os::unix::process::ExitStatusExt,
        process::{ExitStatus, Output},
    };

    use super::*;

    #[test]
    fn parses_a_ps_line() {
        let line = r#"{"id":"abc123","name":"shop-web-2","image":"nginx:1","command":"\"nginx -g\"","running_for":"2 hours ago","status":"Up 2 hours","ports":"80/tcp","networks":"shop_default","mounts":"vol","project":"","service":"web","number":"2","working_dir":"/srv/shop"}"#;
        let listing = parse_listing(line);
        assert_eq!(listing.skipped, 0);
        assert_eq!(
            listing.containers,
            vec![Container {
                id: "abc123".into(),
                name: "shop-web-2".into(),
                image: "nginx:1".into(),
                command: "\"nginx -g\"".into(),
                running_for: "2 hours ago".into(),
                status: "Up 2 hours".into(),
                ports: "80/tcp".into(),
                networks: "shop_default".into(),
                mounts: "vol".into(),
                project: None,
                service: Some("web".into()),
                number: Some(2),
                working_dir: Some("/srv/shop".into()),
            }]
        );
    }

    #[test]
    fn parses_escapes() {
        let line = r#"{"id":"a","name":"q\"b\\n\nt\tcéd…e😀"}"#;
        let listing = parse_listing(line);
        assert_eq!(listing.containers[0].name, "q\"b\\n\nt\tcéd…e😀");
        let raw = "{\"id\":\"a\",\"name\":\"é…\"}";
        assert_eq!(parse_listing(raw).containers[0].name, "é…");
    }

    #[test]
    fn ignores_shell_noise_and_counts_broken_objects() {
        let input = "welcome!\n{\"id\":\"a\",\"name\":\"n\"}\n{\"id\": broken\n";
        let listing = parse_listing(input);
        assert_eq!(listing.containers.len(), 1);
        assert_eq!(listing.skipped, 1);
        assert_eq!(parse_listing(""), Listing::default());
    }

    #[test]
    fn decodes_unicode_escapes_and_skips_lone_surrogates() {
        let escaped =
            r#"{"id":"a","name":"\u00e9 \ud83d\ude00","ports":"0.0.0.0:80\u002d\u003e80/tcp"}"#;
        assert!(escaped.contains("\\ud83d"));
        let listing = parse_listing(escaped);
        assert_eq!(listing.skipped, 0);
        assert_eq!(listing.containers[0].name, "é 😀");
        assert_eq!(listing.containers[0].ports, "0.0.0.0:80->80/tcp");
        let lone = r#"{"id":"a","name":"\ud83dx"}"#;
        let listing = parse_listing(lone);
        assert!(listing.containers.is_empty());
        assert_eq!(listing.skipped, 1);
    }

    #[test]
    fn objects_without_an_id_are_skipped() {
        let listing = parse_listing("{}\n{\"id\":\"\",\"name\":\"n\"}");
        assert!(listing.containers.is_empty());
        assert_eq!(listing.skipped, 2);
    }

    #[test]
    fn exec_words_with_and_without_user() {
        assert_eq!(
            exec_words("abc", None),
            ["docker", "exec", "-it", "abc", "sh", "-c", SHELL_SCRIPT]
        );
        assert_eq!(
            exec_words("abc", Some("root")),
            [
                "docker",
                "exec",
                "-it",
                "-u",
                "root",
                "abc",
                "sh",
                "-c",
                SHELL_SCRIPT
            ]
        );
    }

    #[test]
    fn classifies_docker_messages() {
        let not_installed = Problem::NotInstalled;
        assert_eq!(
            classify(Some(127), "bash: docker: command not found"),
            not_installed
        );
        assert_eq!(
            classify(Some(127), "sh: 1: docker: not found"),
            not_installed
        );
        assert_eq!(
            classify(
                Some(1),
                "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?"
            ),
            Problem::ServiceDown
        );
        assert_eq!(
            classify(
                Some(1),
                "permission denied while trying to connect to the Docker daemon socket at unix:///var/run/docker.sock: Get \"x\""
            ),
            Problem::PermissionDenied
        );
        assert_eq!(
            classify(
                Some(1),
                "permission denied while trying to connect to the docker API at unix:///var/run/docker.sock"
            ),
            Problem::PermissionDenied
        );
        assert_eq!(
            classify(Some(1), "Error response from daemon: x"),
            Problem::Other
        );
    }

    fn probe() -> Probe {
        Probe {
            user: "ann".into(),
            process_groups: vec!["ann".into()],
            database_groups: vec!["ann".into()],
            group_exists: true,
            socket: "/var/run/docker.sock".into(),
            socket_group: Some("staff".into()),
            has_sg: true,
            has_systemctl: true,
        }
    }

    #[test]
    fn parses_probe_output() {
        let out = "motd noise\nuser=ann\nprocess=ann wheel\ndatabase=ann wheel docker\n\
                   group=yes\nsocket=/var/run/docker.sock\nsocket_group=docker\n\
                   sg=yes\nsystemctl=yes\n";
        let parsed = parse_probe(out).unwrap();
        assert_eq!(parsed.user, "ann");
        assert_eq!(parsed.process_groups, ["ann", "wheel"]);
        assert_eq!(parsed.database_groups, ["ann", "wheel", "docker"]);
        assert!(parsed.group_exists);
        assert_eq!(parsed.socket, "/var/run/docker.sock");
        assert_eq!(parsed.socket_group.as_deref(), Some("docker"));
        assert!(parsed.has_sg && parsed.has_systemctl);

        let unknown = parse_probe("user=ann\nsocket=\nsocket_group=\n").unwrap();
        assert_eq!(unknown.socket_group, None);
        assert!(!unknown.group_exists);
        assert_eq!(parse_probe("process=ann\n"), None);
    }

    #[test]
    fn decides_rerun_with_sg_then_login() {
        let mut p = probe();
        p.database_groups.push("docker".into());
        assert_eq!(decide(&p), Access::RerunInGroup(GroupTool::Sg));
        p.has_sg = false;
        assert_eq!(decide(&p), Access::NeedsLogin);
    }

    #[test]
    fn offers_join_when_socket_is_docker_or_root_or_group_missing() {
        for group in ["docker", "root"] {
            let mut p = probe();
            p.socket_group = Some(group.into());
            assert_eq!(decide(&p), Access::OfferJoin);
        }
        let mut p = probe();
        p.group_exists = false;
        p.socket_group = None;
        assert_eq!(decide(&p), Access::OfferJoin);
    }

    #[test]
    fn no_offer_for_other_socket_group_or_already_member() {
        assert_eq!(decide(&probe()), Access::NoOffer);
        let mut p = probe();
        p.socket_group = None;
        assert_eq!(decide(&p), Access::NoOffer);
        let mut p = probe();
        p.process_groups.push("docker".into());
        p.database_groups.push("docker".into());
        p.socket_group = Some("docker".into());
        assert_eq!(decide(&p), Access::NoOffer);
    }

    #[test]
    fn no_offer_when_the_group_is_missing_and_another_group_owns_the_socket() {
        let mut p = probe();
        p.group_exists = false;
        p.socket_group = Some("wheel".into());
        assert_eq!(decide(&p), Access::NoOffer);
    }

    #[test]
    fn probe_script_runs_under_sh() {
        let run = |program: &str, args: &[&str]| {
            let output = std::process::Command::new(program)
                .args(args)
                .output()
                .unwrap();
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };
        let parsed = parse_probe(&run("sh", &["-c", PROBE_SCRIPT])).unwrap();
        assert_eq!(parsed.user, run("id", &["-un"]));
        assert!(!parsed.process_groups.is_empty());
    }

    #[test]
    fn in_group_and_join_words() {
        let words = ps_words();
        assert_eq!(
            in_group(GroupTool::Sg, &words),
            ["sg", "docker", "-c", &remote_command(&words)]
        );
        assert_eq!(
            join_group_words("ann", "/var/run/docker.sock"),
            [
                "sudo",
                "sh",
                "-c",
                JOIN_SCRIPT,
                "sh",
                "ann",
                "/var/run/docker.sock"
            ]
        );
        assert_eq!(
            start_daemon_words(),
            ["sudo", "systemctl", "start", "docker"]
        );
    }

    /// A transport that answers each command with the next scripted reply.
    fn scripted(
        replies: Vec<RunResult<&'static str>>,
    ) -> impl FnMut(&[String]) -> RunResult<&'static str> {
        let mut replies = replies.into_iter();
        move |_| replies.next().expect("no reply scripted for this command")
    }

    fn exited(code: i32, stdout: &str, stderr: &str) -> Output {
        Output {
            status: ExitStatus::from_raw(code << 8),
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    const DENIED: &str = "permission denied while trying to connect to the docker daemon socket at unix:///var/run/docker.sock";

    #[test]
    fn a_cancelled_probe_stops_the_listing() {
        let mut run = scripted(vec![
            Ok(exited(1, "", DENIED)),
            Err(RunFailure::Cancelled("cancelled")),
        ]);
        assert!(matches!(
            Docker.list(&Target::Local, &mut run),
            Err(ListFailure::Run("cancelled"))
        ));
    }

    #[test]
    fn a_failed_probe_reports_the_plain_issue() {
        let mut run = scripted(vec![
            Ok(exited(1, "", DENIED)),
            Err(RunFailure::Failed("ssh hiccup")),
        ]);
        let Err(ListFailure::Issue(issue)) = Docker.list(&Target::Local, &mut run) else {
            panic!("expected an issue");
        };
        assert_eq!(issue.problem, Problem::PermissionDenied);
        assert_eq!(issue.offer, None);
    }

    #[test]
    fn docker_offer_keys_are_not_reserved_panel_keys() {
        let target = Target::Local;
        let keys = [
            join_group_offer(&target, "alice", "/var/run/docker.sock").key,
            start_daemon_offer(&target).key,
        ];
        for key in keys {
            assert!(
                !['q', 'L', 'h', 'r', '/', 'u', 'k', 'j', 'n'].contains(&key),
                "{key} is bound by the containers panel"
            );
        }
    }

    #[test]
    fn join_offer_keeps_its_key_hint_and_prompt() {
        let probe = "user=me\nprocess=me\ndatabase=me wheel\ngroup=yes\nsocket=/var/run/docker.sock\nsocket_group=docker\nsg=yes\nsystemctl=yes\n";
        let mut run = scripted(vec![Ok(exited(1, "", DENIED)), Ok(exited(0, probe, ""))]);
        let Err(ListFailure::Issue(issue)) = Docker.list(&Target::Local, &mut run) else {
            panic!("expected an issue");
        };
        let offer = issue.offer.unwrap();
        assert_eq!(
            (offer.key, offer.hint.as_str()),
            ('g', "add to docker group")
        );
        assert_eq!(offer.words, join_group_words("me", "/var/run/docker.sock"));
        assert!(offer.prompt.starts_with(
            "Add me to the docker group on this machine? Members of the docker group have root-equivalent access. This runs: "
        ));
    }

    #[test]
    fn start_offer_keeps_its_key_hint_and_prompt() {
        let stopped = "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. Is the docker daemon running?";
        let probe = "user=me\nprocess=me docker\ndatabase=me docker\ngroup=yes\nsocket=/var/run/docker.sock\nsocket_group=docker\nsg=yes\nsystemctl=yes\n";
        let mut run = scripted(vec![Ok(exited(1, "", stopped)), Ok(exited(0, probe, ""))]);
        let Err(ListFailure::Issue(issue)) = Docker.list(&Target::Local, &mut run) else {
            panic!("expected an issue");
        };
        let offer = issue.offer.unwrap();
        assert_eq!((offer.key, offer.hint.as_str()), ('s', "start docker"));
        assert_eq!(offer.words, start_daemon_words());
        assert!(
            offer
                .prompt
                .starts_with("Start the Docker service on this machine? This runs: ")
        );
    }
}
