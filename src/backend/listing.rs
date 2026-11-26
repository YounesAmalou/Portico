//! The running containers of a target as every backend reports them, and the
//! grouping, naming and lookup that the CLI and the TUI share.

use std::collections::BTreeMap;

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

/// The key a container's remembered user is stored under: `project/service` for a Compose
/// container, whatever its replica number, and its name for any other.
pub fn remember_key(container: &Container) -> String {
    match (&container.project, &container.service) {
        (Some(project), Some(service)) => format!("{project}/{service}"),
        _ => container.name.clone(),
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
/// service name, `service#n`, the exact ID (a Proxmox VMID), then an ID prefix of at
/// least four hex digits.
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
    let by_id = matching(containers, |c| c.id == reference);
    if !by_id.is_empty() {
        return pick(by_id);
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
    fn resolves_an_exact_short_id() {
        let mut ct = container("web", None, None, 0);
        ct.id = "101".into();
        let mut other = container("db", None, None, 0);
        other.id = "1010".into();
        let all = vec![ct, other];
        assert_eq!(resolve("101", &all).unwrap().name, "web");
        assert_eq!(resolve("1010", &all).unwrap().name, "db");
        assert_eq!(resolve("10", &all), Err(ResolveError::NotFound));
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
    fn remember_key_uses_project_service() {
        let ct = container("shop-api-1", Some("shop"), Some("api"), 1);
        assert_eq!(remember_key(&ct), "shop/api");
    }

    #[test]
    fn remember_key_ignores_replica_number() {
        let first = container("shop-api-1", Some("shop"), Some("api"), 1);
        let second = container("shop-api-2", Some("shop"), Some("api"), 2);
        assert_eq!(remember_key(&first), remember_key(&second));
    }

    #[test]
    fn remember_key_falls_back_to_name() {
        assert_eq!(remember_key(&container("web", None, None, 0)), "web");
        let mut lone = container("odd", Some("shop"), None, 0);
        lone.service = None;
        assert_eq!(remember_key(&lone), "odd");
    }
}
