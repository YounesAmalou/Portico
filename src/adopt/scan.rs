use std::collections::BTreeMap;
use std::ops::Range;

use crate::discovery::{ConfigDirective, parse_directive, split_comment};
use crate::host::{Host, HostField, parse_port};
use crate::managed::{HostComments, ManagedError, parse_value};

/// One `Host` block of an external file and what adopting it would move.
#[derive(Clone, Debug)]
pub(crate) struct Block {
    /// 1-based line of the `Host` line.
    pub host_line: usize,
    pub aliases: Vec<String>,
    /// 0-based half-open line indices: leading comments through the last directive, plus one
    /// directly following blank line.
    pub removed: Range<usize>,
    /// The block as a managed host with its comments, or the first reason it cannot be one.
    pub parsed: Result<(Host, HostComments), String>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Kind {
    Blank,
    Comment,
    Host,
    Match,
    Directive,
}

fn lines_of(text: &str) -> Vec<&str> {
    let mut lines = text.split('\n').collect::<Vec<_>>();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    lines
        .into_iter()
        .map(|line| line.strip_suffix('\r').unwrap_or(line).trim())
        .collect()
}

fn active(line: &str) -> &str {
    let (active, _) = split_comment(line.as_bytes());
    // `split_comment` cuts at an ASCII `#`, so the prefix is valid UTF-8.
    &line[..active.len()]
}

fn classify(line: &str) -> (Kind, Vec<String>) {
    if line.is_empty() {
        return (Kind::Blank, Vec::new());
    }
    if line.starts_with('#') {
        return (Kind::Comment, Vec::new());
    }
    match parse_directive(active(line)) {
        Ok(Some(ConfigDirective::Host(aliases))) => (Kind::Host, aliases),
        Ok(Some(ConfigDirective::Match(_))) => (Kind::Match, Vec::new()),
        _ => (Kind::Directive, Vec::new()),
    }
}

fn trailing_comment(line: &str) -> Option<String> {
    let comment = line[active(line).len()..].trim();
    (!comment.is_empty()).then(|| comment.to_owned())
}

/// Splits a directive line into the key as written and its value.
fn split_key_value(line: &str) -> (&str, &str) {
    let end = line
        .find(|character: char| character.is_whitespace() || character == '=')
        .unwrap_or(line.len());
    let (key, rest) = line.split_at(end);
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('=').unwrap_or(rest);
    (key, rest.trim())
}

pub(crate) fn scan_blocks(text: &str) -> Vec<Block> {
    let lines = lines_of(text);
    let kinds = lines.iter().map(|line| classify(line)).collect::<Vec<_>>();
    let mut blocks = Vec::new();
    for (start, (kind, aliases)) in kinds.iter().enumerate() {
        if *kind != Kind::Host {
            continue;
        }
        let mut last = start;
        for (index, (kind, _)) in kinds.iter().enumerate().skip(start + 1) {
            match kind {
                Kind::Host | Kind::Match => break,
                Kind::Directive => last = index,
                Kind::Blank | Kind::Comment => {}
            }
        }
        let mut first = start;
        while first > 0 && kinds[first - 1].0 == Kind::Comment {
            first -= 1;
        }
        let mut end = last + 1;
        if kinds.get(end).is_some_and(|(kind, _)| *kind == Kind::Blank) {
            end += 1;
        }
        blocks.push(Block {
            host_line: start + 1,
            aliases: aliases.clone(),
            removed: first..end,
            parsed: convert(&lines, first, start, last),
        });
    }
    blocks
}

fn convert(
    lines: &[&str],
    first: usize,
    start: usize,
    last: usize,
) -> Result<(Host, HostComments), String> {
    let (_, aliases) = classify(lines[start]);
    if aliases.len() != 1 {
        return Err(format!("Host line declares {} aliases", aliases.len()));
    }
    let mut comments = HostComments {
        leading: lines[first..start]
            .iter()
            .map(|line| (*line).to_owned())
            .collect(),
        fields: BTreeMap::new(),
    };
    comments.leading.extend(trailing_comment(lines[start]));

    let mut hostname = None;
    let mut user = None;
    let mut port = None;
    let mut identity_file = None;
    let mut proxy_jump = None;
    let mut pending: Vec<String> = Vec::new();
    for (index, line) in lines.iter().enumerate().take(last + 1).skip(start + 1) {
        let number = index + 1;
        if line.is_empty() {
            continue;
        }
        if line.starts_with('#') {
            pending.push((*line).to_owned());
            continue;
        }
        let (key, raw) = split_key_value(active(line));
        let field = [
            ("HostName", HostField::Hostname),
            ("User", HostField::User),
            ("Port", HostField::Port),
            ("IdentityFile", HostField::IdentityFile),
            ("ProxyJump", HostField::ProxyJump),
        ]
        .into_iter()
        .find(|(name, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, field)| field)
        .ok_or_else(|| format!("{key} is not supported by Portico"))?;
        let seen = match field {
            HostField::Hostname => hostname.is_some(),
            HostField::User => user.is_some(),
            HostField::Port => port.is_some(),
            HostField::IdentityFile => identity_file.is_some(),
            _ => proxy_jump.is_some(),
        };
        if seen {
            return Err(format!("{key} is repeated on line {number}"));
        }
        if raw.is_empty() {
            return Err(format!("missing value for {key} on line {number}"));
        }
        let value = if field == HostField::ProxyJump {
            raw.to_owned()
        } else {
            parse_value(raw, number).map_err(|error| match error {
                ManagedError::Parse { message, .. } => format!("invalid {field}: {message}"),
                other => other.to_string(),
            })?
        };
        match field {
            HostField::Hostname => hostname = Some(value),
            HostField::User => user = Some(value),
            HostField::Port => port = Some(parse_port(&value).map_err(|error| error.to_string())?),
            HostField::IdentityFile => identity_file = Some(value),
            _ => proxy_jump = Some(value),
        }
        pending.extend(trailing_comment(line));
        if !pending.is_empty() {
            comments.fields.insert(field, std::mem::take(&mut pending));
        }
    }

    let alias = aliases[0].clone();
    let hostname = hostname.unwrap_or_else(|| alias.clone());
    let host = Host::new(alias, hostname, user, port, identity_file, proxy_jump)
        .map_err(|error| error.to_string())?;
    Ok((host, comments))
}

/// Removes whole lines, with their line endings, for every range.
pub(crate) fn remove_lines(contents: &[u8], ranges: &[Range<usize>]) -> Vec<u8> {
    contents
        .split_inclusive(|&byte| byte == b'\n')
        .enumerate()
        .filter(|(index, _)| !ranges.iter().any(|range| range.contains(index)))
        .flat_map(|(_, line)| line.iter().copied())
        .collect()
}

/// Inserts `text` after the first line, giving a first line without a line ending one.
pub(crate) fn insert_after_first_line(contents: &[u8], text: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(contents.len() + text.len() + 1);
    match contents.iter().position(|&byte| byte == b'\n') {
        Some(position) => {
            output.extend_from_slice(&contents[..=position]);
            output.extend_from_slice(text);
            output.extend_from_slice(&contents[position + 1..]);
        }
        None => {
            output.extend_from_slice(contents);
            if !contents.is_empty() {
                output.push(b'\n');
            }
            output.extend_from_slice(text);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_with_leading_inner_and_trailing_comments() {
        let text = "Host other\n  HostName o\n\n# work box\nHost dev\n  # the real name\n  HostName dev.example\n\n  User bob  # old account\n# next section\nHost z\n  HostName z\n";
        let blocks = scan_blocks(text);
        let dev = &blocks[1];
        assert_eq!(dev.host_line, 5);
        assert_eq!(dev.removed, 3..9);
        let (host, comments) = dev.parsed.clone().unwrap();
        assert_eq!(
            (host.alias(), host.hostname(), host.user()),
            ("dev", "dev.example", Some("bob"))
        );
        assert_eq!(comments.leading, vec!["# work box"]);
        assert_eq!(
            comments.fields[&HostField::Hostname],
            vec!["# the real name"]
        );
        assert_eq!(comments.fields[&HostField::User], vec!["# old account"]);
    }

    #[test]
    fn removed_range_takes_one_following_blank_line() {
        let blocks = scan_blocks("Host a\n  HostName x\n\n\nHost b\n");
        assert_eq!(blocks[0].removed, 0..3);
    }

    #[test]
    fn comment_separated_by_blank_line_is_not_leading() {
        let blocks = scan_blocks("# header\n\nHost a\n  HostName x\n");
        assert_eq!(blocks[0].removed, 2..4);
        assert!(blocks[0].parsed.clone().unwrap().1.leading.is_empty());
    }

    #[test]
    fn block_ends_at_match_and_at_end_of_file() {
        let blocks = scan_blocks("Host a\n  User u\nMatch all\n  User v\n");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].removed, 0..2);
        assert_eq!(blocks[0].parsed.clone().unwrap().0.hostname(), "a");
    }

    #[test]
    fn host_line_comment_is_a_leading_comment() {
        let blocks = scan_blocks("# above\nHost dev  # box\n  HostName d\n");
        let (_, comments) = blocks[0].parsed.clone().unwrap();
        assert_eq!(comments.leading, vec!["# above", "# box"]);
    }

    #[test]
    fn key_equals_value_forms_and_case() {
        let blocks = scan_blocks("host a\n  hostname=x\n  USER = bob\n  Port=2222\n");
        let (host, _) = blocks[0].parsed.clone().unwrap();
        assert_eq!(
            (host.hostname(), host.user(), host.port()),
            ("x", Some("bob"), Some(2222))
        );
    }

    #[test]
    fn refusal_reasons() {
        let reason = |text: &str| scan_blocks(text)[0].parsed.clone().unwrap_err();
        assert_eq!(
            reason("Host a b\n  HostName x\n"),
            "Host line declares 2 aliases"
        );
        assert_eq!(
            reason("Host a\n  ForwardAgent yes\n"),
            "ForwardAgent is not supported by Portico"
        );
        assert_eq!(
            reason("Host a\n  User x\n  user y\n"),
            "user is repeated on line 3"
        );
        assert!(reason("Host a\n  Port 99999\n").starts_with("invalid port"));
        assert_eq!(
            reason("Host a\n  User\n"),
            "missing value for User on line 2"
        );
    }

    #[test]
    fn remove_lines_keeps_every_other_byte_including_crlf() {
        let input = b"A\r\nHost a\r\n  HostName x\r\n\r\nB\r\n";
        let ranges = [std::ops::Range { start: 1, end: 4 }];
        assert_eq!(remove_lines(input, &ranges), b"A\r\nB\r\n");
    }

    #[test]
    fn remove_last_block_without_trailing_newline() {
        let ranges = [std::ops::Range { start: 1, end: 3 }];
        assert_eq!(remove_lines(b"A\nHost a\n  HostName x", &ranges), b"A\n");
    }

    #[test]
    fn remove_two_adjacent_ranges() {
        let text = "A\nHost a\n  HostName x\n\nHost b\n  HostName y\n\nMatch all\n  User u\n";
        let blocks = scan_blocks(text);
        let ranges = blocks
            .iter()
            .map(|block| block.removed.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            remove_lines(text.as_bytes(), &ranges),
            b"A\nMatch all\n  User u\n"
        );
    }

    #[test]
    fn insert_after_first_line_variants() {
        assert_eq!(
            insert_after_first_line(b"Include x\nrest\n", b"Host a\n\n"),
            b"Include x\nHost a\n\nrest\n"
        );
        assert_eq!(
            insert_after_first_line(b"Include x", b"Host a\n"),
            b"Include x\nHost a\n"
        );
    }
}
