//! Running containers as listed by `docker ps`: the line format, its parser,
//! and the grouping by Compose project that the CLI and the TUI share.

use std::collections::BTreeMap;

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

/// A running container and the Compose labels it carries, if any.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Container {
    pub id: String,
    pub name: String,
    pub image: String,
    pub command: String,
    pub running_for: String,
    pub status: String,
    pub ports: String,
    pub networks: String,
    pub mounts: String,
    pub project: Option<String>,
    pub service: Option<String>,
    pub number: Option<u32>,
    pub working_dir: Option<String>,
}

/// The parsed output of one listing: the containers, and how many lines
/// looked like objects but could not be read.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Listing {
    pub containers: Vec<Container>,
    pub skipped: usize,
}

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

/// The containers of one Compose project, or of no project (`None`).
#[derive(Debug, Eq, PartialEq)]
pub struct Group<'a> {
    pub project: Option<&'a str>,
    pub containers: Vec<&'a Container>,
}

/// Projects sorted by name, each sorted by service then number, followed by
/// the containers without a project sorted by name.
pub fn group(containers: &[Container]) -> Vec<Group<'_>> {
    let mut projects: BTreeMap<&str, Vec<&Container>> = BTreeMap::new();
    let mut other: Vec<&Container> = Vec::new();
    for container in containers {
        match container.project.as_deref() {
            Some(project) => projects.entry(project).or_default().push(container),
            None => other.push(container),
        }
    }
    let mut groups: Vec<Group> = projects
        .into_iter()
        .map(|(project, mut members)| {
            members.sort_by(|a, b| {
                (&a.service, a.number, &a.name).cmp(&(&b.service, b.number, &b.name))
            });
            Group {
                project: Some(project),
                containers: members,
            }
        })
        .collect();
    if !other.is_empty() {
        other.sort_by(|a, b| a.name.cmp(&b.name));
        groups.push(Group {
            project: None,
            containers: other,
        });
    }
    groups
}

/// A Compose container shows its service, numbered when the service has
/// replicas; any other container shows its name.
pub fn display_name(container: &Container, all: &[Container]) -> String {
    let Some(service) = container.service.as_deref() else {
        return container.name.clone();
    };
    let replicas = all
        .iter()
        .filter(|other| {
            other.project == container.project && other.service.as_deref() == Some(service)
        })
        .count();
    match container.number {
        Some(number) if replicas > 1 => format!("{service}#{number}"),
        _ => service.to_string(),
    }
}

/// Why a container reference did not pick exactly one container.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResolveError {
    NotFound,
    /// Several containers match; holds their names, sorted.
    Ambiguous(Vec<String>),
}

/// Finds the container a user means. The first form that matches anything
/// decides: the exact name, `project/service` or `project/service#n`, a bare
/// service name, `service#n`, then an ID prefix of at least four hex digits.
pub fn resolve<'a>(
    reference: &str,
    containers: &'a [Container],
) -> Result<&'a Container, ResolveError> {
    let by_name = matching(containers, |c| c.name == reference);
    if !by_name.is_empty() {
        return pick(by_name);
    }
    if let Some((project, rest)) = reference.split_once('/') {
        let (service, number) = match rest.split_once('#') {
            Some((service, number)) => (service, number.parse::<u32>().ok()),
            None => (rest, None),
        };
        let wants_number = rest.contains('#');
        let found = matching(containers, |c| {
            c.project.as_deref() == Some(project)
                && c.service.as_deref() == Some(service)
                && (!wants_number || (number.is_some() && c.number == number))
        });
        if !found.is_empty() {
            return pick(found);
        }
    }
    let by_service = matching(containers, |c| c.service.as_deref() == Some(reference));
    if !by_service.is_empty() {
        return pick(by_service);
    }
    // `service#n`, as `display_name` shows a replica.
    if let Some((service, number)) = reference.split_once('#')
        && let Ok(number) = number.parse::<u32>()
    {
        let found = matching(containers, |c| {
            c.service.as_deref() == Some(service) && c.number == Some(number)
        });
        if !found.is_empty() {
            return pick(found);
        }
    }
    let is_id_prefix = reference.len() >= 4 && reference.chars().all(|c| c.is_ascii_hexdigit());
    if is_id_prefix {
        let prefix = reference.to_ascii_lowercase();
        return pick(matching(containers, |c| c.id.starts_with(&prefix)));
    }
    Err(ResolveError::NotFound)
}

fn matching(containers: &[Container], test: impl Fn(&Container) -> bool) -> Vec<&Container> {
    containers.iter().filter(|c| test(c)).collect()
}

fn pick(mut found: Vec<&Container>) -> Result<&Container, ResolveError> {
    match found.len() {
        0 => Err(ResolveError::NotFound),
        1 => Ok(found[0]),
        _ => {
            found.sort_by(|a, b| a.name.cmp(&b.name));
            Err(ResolveError::Ambiguous(
                found.into_iter().map(|c| c.name.clone()).collect(),
            ))
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

/// What went wrong when a docker command failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DockerProblem {
    NotInstalled,
    DaemonDown,
    PermissionDenied,
    Other,
}

/// Recognises a docker failure from its exit status and stderr; over ssh the
/// same text arrives from the remote shell.
pub fn classify(code: Option<i32>, stderr: &str) -> DockerProblem {
    let stderr = stderr.to_lowercase();
    if code == Some(127) && stderr.contains("not found") {
        DockerProblem::NotInstalled
    } else if stderr.contains("cannot connect to the docker daemon") {
        DockerProblem::DaemonDown
    } else if stderr.contains("permission denied while trying to connect to the docker") {
        DockerProblem::PermissionDenied
    } else {
        DockerProblem::Other
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

/// The tool that runs a command inside the docker group. `newgrp` cannot: it takes no command.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum GroupTool {
    Sg,
}

/// What to do about a refused docker socket.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Access {
    /// The user is in the group, but this process predates it: rerun inside it.
    RerunInGroup(GroupTool),
    /// Same, but `sg` does not exist: a new login is needed.
    NeedsLogin,
    /// Offer to add the user to the docker group.
    OfferJoin,
    NoOffer,
}

/// Decides the fix for a permission failure from the probe.
pub fn decide(probe: &Probe) -> Access {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn container(
        name: &str,
        project: Option<&str>,
        service: Option<&str>,
        number: u32,
    ) -> Container {
        Container {
            id: format!("id-{name}"),
            name: name.into(),
            image: "img".into(),
            command: String::new(),
            running_for: String::new(),
            status: String::new(),
            ports: String::new(),
            networks: String::new(),
            mounts: String::new(),
            project: project.map(String::from),
            service: service.map(String::from),
            number: service.map(|_| number),
            working_dir: None,
        }
    }

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
    fn groups_projects_then_other() {
        let all = vec![
            container("shop-web-1", Some("shop"), Some("web"), 1),
            container("shop-db-1", Some("shop"), Some("db"), 1),
            container("alpha-api-1", Some("alpha"), Some("api"), 1),
            container("pgadmin", None, None, 0),
        ];
        let groups = group(&all);
        let shape: Vec<(Option<&str>, Vec<&str>)> = groups
            .iter()
            .map(|g| {
                (
                    g.project,
                    g.containers.iter().map(|c| c.name.as_str()).collect(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                (Some("alpha"), vec!["alpha-api-1"]),
                (Some("shop"), vec!["shop-db-1", "shop-web-1"]),
                (None, vec!["pgadmin"]),
            ]
        );
    }

    #[test]
    fn display_names_number_replicas_only() {
        let all = vec![
            container("shop-web-1", Some("shop"), Some("web"), 1),
            container("shop-web-2", Some("shop"), Some("web"), 2),
            container("shop-db-1", Some("shop"), Some("db"), 1),
            container("pgadmin", None, None, 0),
        ];
        let names: Vec<String> = all.iter().map(|c| display_name(c, &all)).collect();
        assert_eq!(names, ["web#1", "web#2", "db", "pgadmin"]);
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
    fn group_breaks_ties_by_name() {
        let all = vec![
            container("shop-web-b", Some("shop"), Some("web"), 1),
            container("shop-web-a", Some("shop"), Some("web"), 1),
        ];
        let groups = group(&all);
        let names: Vec<&str> = groups[0]
            .containers
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert_eq!(names, ["shop-web-a", "shop-web-b"]);
    }

    fn with_id(mut container: Container, id: &str) -> Container {
        container.id = id.into();
        container
    }

    fn resolved<'a>(reference: &str, all: &'a [Container]) -> Result<&'a str, ResolveError> {
        resolve(reference, all).map(|c| c.name.as_str())
    }

    fn ambiguous(names: &[&str]) -> Result<&'static str, ResolveError> {
        Err(ResolveError::Ambiguous(
            names.iter().map(|n| n.to_string()).collect(),
        ))
    }

    #[test]
    fn resolves_exact_name_first() {
        let all = vec![
            container("shop-web-1", Some("shop"), Some("web"), 1),
            container("web", None, None, 0),
        ];
        assert_eq!(resolved("web", &all), Ok("web"));
    }

    #[test]
    fn resolves_project_service_and_number() {
        let all = vec![
            container("shop-web-1", Some("shop"), Some("web"), 1),
            container("shop-db-1", Some("shop"), Some("db"), 1),
        ];
        assert_eq!(resolved("shop/web", &all), Ok("shop-web-1"));
        let replicas = vec![
            container("shop-web-1", Some("shop"), Some("web"), 1),
            container("shop-web-2", Some("shop"), Some("web"), 2),
        ];
        assert_eq!(resolved("shop/web#2", &replicas), Ok("shop-web-2"));
        assert_eq!(resolved("web#2", &replicas), Ok("shop-web-2"));
        assert_eq!(resolved("web#3", &replicas), Err(ResolveError::NotFound));
        assert_eq!(
            resolved("shop/web#3", &replicas),
            Err(ResolveError::NotFound)
        );
    }

    #[test]
    fn resolves_unique_service() {
        let all = vec![
            container("shop-web-1", Some("shop"), Some("web"), 1),
            container("shop-db-1", Some("shop"), Some("db"), 1),
        ];
        assert_eq!(resolved("db", &all), Ok("shop-db-1"));
    }

    #[test]
    fn ambiguous_service_lists_names() {
        let all = vec![
            container("shop-web-1", Some("shop"), Some("web"), 1),
            container("blog-web-1", Some("blog"), Some("web"), 1),
        ];
        assert_eq!(
            resolved("web", &all),
            ambiguous(&["blog-web-1", "shop-web-1"])
        );
        let replicas = vec![
            container("shop-web-2", Some("shop"), Some("web"), 2),
            container("shop-web-1", Some("shop"), Some("web"), 1),
        ];
        assert_eq!(
            resolved("shop/web", &replicas),
            ambiguous(&["shop-web-1", "shop-web-2"])
        );
    }

    #[test]
    fn resolves_id_prefix_of_four_or_more_hex() {
        let all = vec![
            with_id(container("a", None, None, 0), "3174abcdef"),
            with_id(container("b", None, None, 0), "9f00112233"),
            with_id(container("c", None, None, 0), "9f00998877"),
        ];
        assert_eq!(resolved("3174", &all), Ok("a"));
        assert_eq!(resolved("3174ABCD", &all), Ok("a"));
        assert_eq!(resolved("317", &all), Err(ResolveError::NotFound));
        assert_eq!(resolved("zzzz", &all), Err(ResolveError::NotFound));
        assert_eq!(resolved("9f00", &all), ambiguous(&["b", "c"]));
    }

    #[test]
    fn bare_service_with_replicas_is_ambiguous() {
        let all = vec![
            container("shop-web-1", Some("shop"), Some("web"), 1),
            container("shop-web-2", Some("shop"), Some("web"), 2),
        ];
        assert_eq!(
            resolved("web", &all),
            ambiguous(&["shop-web-1", "shop-web-2"])
        );
    }

    #[test]
    fn unknown_reference_is_not_found() {
        let all = vec![container("shop-web-1", Some("shop"), Some("web"), 1)];
        assert_eq!(resolved("nope", &all), Err(ResolveError::NotFound));
        assert_eq!(resolved("nope/web", &all), Err(ResolveError::NotFound));
        assert_eq!(resolved("web", &[]), Err(ResolveError::NotFound));
    }

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
        let not_installed = DockerProblem::NotInstalled;
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
            DockerProblem::DaemonDown
        );
        assert_eq!(
            classify(
                Some(1),
                "permission denied while trying to connect to the Docker daemon socket at unix:///var/run/docker.sock: Get \"x\""
            ),
            DockerProblem::PermissionDenied
        );
        assert_eq!(
            classify(
                Some(1),
                "permission denied while trying to connect to the docker API at unix:///var/run/docker.sock"
            ),
            DockerProblem::PermissionDenied
        );
        assert_eq!(
            classify(Some(1), "Error response from daemon: x"),
            DockerProblem::Other
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
}
