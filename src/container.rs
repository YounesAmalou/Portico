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
            Some(fields) => listing.containers.push(container_from(fields)),
            None => listing.skipped += 1,
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
            members.sort_by(|a, b| (&a.service, a.number).cmp(&(&b.service, b.number)));
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
        let line = r#"{"name":"q\"b\\n\nt\tcéd…e😀"}"#;
        let listing = parse_listing(line);
        assert_eq!(listing.containers[0].name, "q\"b\\n\nt\tcéd…e😀");
        let raw = "{\"name\":\"é…\"}";
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
}
