//! Suggestions for the jump host being typed into a ProxyJump value.
//!
//! Pure functions: the TUI passes the field's text, its cursor and the known hosts, and gets back
//! the hop under the cursor, the aliases that match it, and the value after a pick.

use std::ops::Range;
use std::path::{Path, PathBuf};

/// Where a suggested alias is defined.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Origin {
    Managed,
    External(PathBuf),
}

/// A known host alias that can be suggested as a jump host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Candidate {
    pub alias: String,
    pub origin: Origin,
}

/// The alias part of the hop under the cursor: `range` is its byte range in the value (after any
/// `user@`), and `query` is its text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Hop {
    pub range: Range<usize>,
    pub query: String,
}

/// The hop the cursor is in, or `None` once a port follows its host.
pub fn current_hop(value: &str, cursor: usize) -> Option<Hop> {
    let cursor = cursor.min(value.len());
    let start = value[..cursor].rfind(',').map_or(0, |comma| comma + 1);
    let end = value[cursor..]
        .find(',')
        .map_or(value.len(), |comma| cursor + comma);
    let alias_start = value[start..end]
        .rfind('@')
        .map_or(start, |at| start + at + 1);
    let query = &value[alias_start..end];
    if query.contains(':') {
        return None;
    }
    Some(Hop {
        range: alias_start..end,
        query: query.into(),
    })
}

/// The candidates matching `query`, without duplicates or `exclude`d aliases: those starting with
/// the query first, then those containing it, each group in alphabetical order.
pub fn rank(query: &str, candidates: &[Candidate], exclude: &[&str]) -> Vec<Candidate> {
    let mut unique: Vec<Candidate> = Vec::new();
    for candidate in candidates {
        if exclude
            .iter()
            .any(|excluded| excluded.eq_ignore_ascii_case(&candidate.alias))
        {
            continue;
        }
        match unique
            .iter_mut()
            .find(|kept| kept.alias.eq_ignore_ascii_case(&candidate.alias))
        {
            Some(kept) => {
                if kept.origin != Origin::Managed && candidate.origin == Origin::Managed {
                    *kept = candidate.clone();
                }
            }
            None => unique.push(candidate.clone()),
        }
    }
    let query = query.to_lowercase();
    let mut matches: Vec<(bool, String, Candidate)> = unique
        .into_iter()
        .filter_map(|candidate| {
            let lower = candidate.alias.to_lowercase();
            lower
                .contains(&query)
                .then(|| (!lower.starts_with(&query), lower, candidate))
        })
        .collect();
    matches.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    matches
        .into_iter()
        .map(|(_, _, candidate)| candidate)
        .collect()
}

/// The value with the hop's alias replaced by `alias`, and the cursor at the end of it.
pub fn apply(value: &str, hop: &Hop, alias: &str) -> (String, usize) {
    let mut replaced = value.to_owned();
    replaced.replace_range(hop.range.clone(), alias);
    (replaced, hop.range.start + alias.len())
}

/// `managed`, or the defining file with the home directory shown as `~`.
pub fn origin_label(origin: &Origin, home: Option<&Path>) -> String {
    match origin {
        Origin::Managed => "managed".into(),
        Origin::External(path) => match home.and_then(|home| path.strip_prefix(home).ok()) {
            Some(rest) => format!("~/{}", rest.display()),
            None => path.display().to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn c(alias: &str, managed: bool) -> Candidate {
        Candidate {
            alias: alias.into(),
            origin: if managed {
                Origin::Managed
            } else {
                Origin::External("/home/me/.ssh/config".into())
            },
        }
    }

    fn aliases(list: &[Candidate]) -> Vec<&str> {
        list.iter().map(|c| c.alias.as_str()).collect()
    }

    fn hop(range: std::ops::Range<usize>, query: &str) -> Option<Hop> {
        Some(Hop {
            range,
            query: query.into(),
        })
    }

    #[test]
    fn current_hop_finds_the_hop_under_the_cursor() {
        assert_eq!(current_hop("bas", 3), hop(0..3, "bas"));
        assert_eq!(current_hop("bastion,pro", 11), hop(8..11, "pro"));
        assert_eq!(current_hop("a,bb,c", 3), hop(2..4, "bb"));
        assert_eq!(current_hop("", 0), hop(0..0, ""));
        assert_eq!(current_hop("bastion,", 8), hop(8..8, ""));
    }

    #[test]
    fn current_hop_keeps_the_user_and_stops_at_a_port() {
        assert_eq!(current_hop("deploy@bas", 10), hop(7..10, "bas"));
        assert_eq!(current_hop("bastion:22", 10), None);
        assert_eq!(current_hop("bas:22,x", 2), None);
    }

    #[test]
    fn rank_matches_contains_ignoring_case_with_prefixes_first() {
        let all = [
            c("eu-proxy", false),
            c("prod-jump", true),
            c("Proxy", false),
            c("web", true),
        ];
        assert_eq!(
            aliases(&rank("pro", &all, &[])),
            ["prod-jump", "Proxy", "eu-proxy"]
        );
        assert_eq!(
            aliases(&rank("", &all, &[])),
            ["eu-proxy", "prod-jump", "Proxy", "web"]
        );
    }

    #[test]
    fn rank_collapses_duplicates_preferring_managed() {
        let all = [
            c("alpha", false),
            c("ALPHA", true),
            c("beta", false),
            c("beta", false),
        ];
        let ranked = rank("", &all, &[]);
        assert_eq!(aliases(&ranked), ["ALPHA", "beta"]);
        assert_eq!(ranked[0].origin, Origin::Managed);
    }

    #[test]
    fn rank_drops_excluded_aliases() {
        let all = [c("bastion", true), c("dev", true), c("prod", true)];
        assert_eq!(aliases(&rank("", &all, &["DEV", "bastion"])), ["prod"]);
    }

    #[test]
    fn apply_replaces_only_the_alias_part_of_the_hop() {
        let found = current_hop("bastion,pro", 11).unwrap();
        assert_eq!(
            apply("bastion,pro", &found, "prod-jump"),
            ("bastion,prod-jump".into(), 17)
        );
        let found = current_hop("deploy@bas", 10).unwrap();
        assert_eq!(
            apply("deploy@bas", &found, "bastion"),
            ("deploy@bastion".into(), 14)
        );
    }

    #[test]
    fn apply_replaces_the_hop_under_a_mid_hop_cursor() {
        let found = current_hop("a,bas,c", 3).unwrap();
        assert_eq!(
            apply("a,bas,c", &found, "bastion"),
            ("a,bastion,c".into(), 9)
        );
    }

    #[test]
    fn ranks_and_applies_non_ascii_aliases() {
        let all = [c("café", true), c("cab", true)];
        assert_eq!(aliases(&rank("CAF", &all, &[])), ["café"]);
        let found = current_hop("x,ca", 4).unwrap();
        assert_eq!(apply("x,ca", &found, "café"), ("x,café".into(), 7));
    }

    #[test]
    fn origin_label_names_managed_or_the_file_under_home() {
        let home = Path::new("/home/me");
        assert_eq!(origin_label(&Origin::Managed, Some(home)), "managed");
        assert_eq!(
            origin_label(&Origin::External("/home/me/.ssh/config".into()), Some(home)),
            "~/.ssh/config"
        );
        assert_eq!(
            origin_label(&Origin::External("/etc/ssh/ssh_config".into()), Some(home)),
            "/etc/ssh/ssh_config"
        );
    }
}
