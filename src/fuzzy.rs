//! Fuzzy matching shared by the CLI pickers and the TUI filters: a query matches a row when each
//! of its terms appears, in order but not necessarily together, in one of the row's fields.

use ratatui::{style::Style, text::Span};

use crate::suggest::fold;

const MATCH: i64 = 16;
const BOUNDARY: i64 = 8;
const FIELD_START: i64 = 8;
const CONSECUTIVE: i64 = 4;
const GAP_START: i64 = -3;
const GAP_EXTEND: i64 = -1;

/// A matching row: its score (higher is better) and, for each field, the matched characters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Match {
    pub score: i64,
    /// Sorted character indices into each original field, in the order the fields were given.
    pub positions: Vec<Vec<usize>>,
}

/// Matches `query` against a row's `fields`, ignoring case and the accents of Latin letters.
/// Whitespace separates terms, and every term must match some field. `None` when one does not.
pub fn match_fields(query: &str, fields: &[&str]) -> Option<Match> {
    let folded: Vec<Vec<Folded>> = fields.iter().map(|field| fold_field(field)).collect();
    let mut result = Match {
        score: 0,
        positions: vec![Vec::new(); fields.len()],
    };
    for term in query.split_whitespace() {
        // One character at a time, like the fields, so lowercasing does not depend on the word.
        let term: Vec<char> = term
            .chars()
            .flat_map(|character| fold(&character.to_string()).chars().collect::<Vec<_>>())
            .collect();
        let (field, score, positions) = folded
            .iter()
            .enumerate()
            .filter_map(|(index, text)| {
                align(&term, text).map(|(score, positions)| (index, score, positions))
            })
            // The first field wins a tie.
            .rev()
            .max_by_key(|(_, score, _)| *score)?;
        result.score += score;
        result.positions[field].extend(positions);
    }
    for positions in &mut result.positions {
        positions.sort_unstable();
        positions.dedup();
    }
    Some(result)
}

/// The rows matching `query`, as their index and match, best first and otherwise in input order.
pub fn rank<'a>(query: &str, rows: impl IntoIterator<Item = &'a [&'a str]>) -> Vec<(usize, Match)> {
    let mut ranked: Vec<(usize, Match)> = rows
        .into_iter()
        .enumerate()
        .filter_map(|(index, fields)| match_fields(query, fields).map(|found| (index, found)))
        .collect();
    ranked.sort_by_key(|(_, found)| std::cmp::Reverse(found.score));
    ranked
}

/// `text` as spans, the characters at `positions` styled `hit` and the rest `base`.
pub fn highlight(text: &str, positions: &[usize], base: Style, hit: Style) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_hit = false;
    for (index, character) in text.chars().enumerate() {
        let is_hit = positions.binary_search(&index).is_ok();
        if is_hit != run_hit && !run.is_empty() {
            let style = if run_hit { hit } else { base };
            spans.push(Span::styled(std::mem::take(&mut run), style));
        }
        run_hit = is_hit;
        run.push(character);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, if run_hit { hit } else { base }));
    }
    spans
}

/// A folded character, the original character it came from and the bonus for matching it.
struct Folded {
    character: char,
    original: usize,
    bonus: i64,
}

fn fold_field(field: &str) -> Vec<Folded> {
    let mut folded = Vec::new();
    let mut previous: Option<char> = None;
    for (original, character) in field.chars().enumerate() {
        let bonus = match previous {
            None => BOUNDARY + FIELD_START,
            Some(before)
                if before.is_whitespace()
                    || "-_./:@".contains(before)
                    || (before.is_lowercase() && character.is_uppercase()) =>
            {
                BOUNDARY
            }
            Some(_) => 0,
        };
        for (part, character) in fold(&character.to_string()).chars().enumerate() {
            folded.push(Folded {
                character,
                original,
                // The rest of a character folded to several continues it.
                bonus: if part == 0 { bonus } else { 0 },
            });
        }
        previous = Some(character);
    }
    folded
}

/// The best alignment of `term` in `text`: its score and the original characters it matched.
fn align(term: &[char], text: &[Folded]) -> Option<(i64, Vec<usize>)> {
    let (rows, columns) = (term.len(), text.len());
    if rows == 0 || rows > columns {
        return (rows == 0).then(|| (0, Vec::new()));
    }
    // best[j][i]: the best score with term[j] matched at text[i]; from[j][i]: where term[j - 1] was.
    let mut best = vec![vec![None::<i64>; columns]; rows];
    let mut from = vec![vec![0usize; columns]; rows];
    for (j, &wanted) in term.iter().enumerate() {
        // A gap from k to i costs GAP_START + (i - k - 2) * GAP_EXTEND, so the best gapped
        // predecessor is the best of best[j - 1][k] - k * GAP_EXTEND, kept as i moves right.
        // The earliest k wins a tie.
        let mut gapped: Option<(i64, usize)> = None;
        for i in 0..columns {
            if j > 0
                && let Some(k) = i.checked_sub(2)
                && let Some(before) = best[j - 1][k]
            {
                let value = before - k as i64 * GAP_EXTEND;
                if gapped.is_none_or(|(kept, _)| value > kept) {
                    gapped = Some((value, k));
                }
            }
            if text[i].character != wanted {
                continue;
            }
            let here = MATCH + text[i].bonus;
            if j == 0 {
                best[j][i] = Some(here);
                continue;
            }
            let gap = gapped.map(|(value, k)| (value + GAP_START + (i as i64 - 2) * GAP_EXTEND, k));
            let consecutive = i
                .checked_sub(1)
                .and_then(|k| Some((best[j - 1][k]? + CONSECUTIVE, k)));
            let previous = match (gap, consecutive) {
                (Some(gap), Some(consecutive)) if consecutive.0 > gap.0 => Some(consecutive),
                (Some(gap), _) => Some(gap),
                (None, consecutive) => consecutive,
            };
            if let Some((score, k)) = previous {
                best[j][i] = Some(score + here);
                from[j][i] = k;
            }
        }
    }
    let (score, mut at) = (0..columns)
        .filter_map(|i| best[rows - 1][i].map(|score| (score, i)))
        .max_by_key(|&(score, i)| (score, std::cmp::Reverse(i)))?;
    let mut positions = vec![0; rows];
    for j in (0..rows).rev() {
        positions[j] = text[at].original;
        at = from[j][at];
    }
    Some((score, positions))
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Modifier, Style};

    use super::*;

    fn positions(query: &str, fields: &[&str]) -> Vec<Vec<usize>> {
        match_fields(query, fields).expect("a match").positions
    }

    fn score(query: &str, field: &str) -> i64 {
        match_fields(query, &[field]).expect("a match").score
    }

    #[test]
    fn subsequence_in_order_matches() {
        assert_eq!(positions("prdb", &["prod-db"]), vec![vec![0, 1, 5, 6]]);
        assert!(match_fields("bdrp", &["prod-db"]).is_none());
    }

    #[test]
    fn every_term_must_match() {
        assert!(match_fields("db prod", &["prod-db"]).is_some());
        assert!(match_fields("db web", &["prod-db"]).is_none());
    }

    #[test]
    fn terms_may_match_different_fields() {
        assert_eq!(
            positions("app nginx", &["web-app", "nginx:1.27"]),
            vec![vec![4, 5, 6], vec![0, 1, 2, 3, 4]]
        );
    }

    #[test]
    fn case_and_accents_are_ignored() {
        assert!(match_fields("PROD", &["prod"]).is_some());
        assert!(match_fields("pröd", &["prod"]).is_some());
        assert_eq!(positions("e", &["Émile"]), vec![vec![0]]);
    }

    #[test]
    fn query_folds_like_the_fields() {
        // Lowercasing a whole word turns its last Σ into ς; one character at a time gives σ.
        assert_eq!(positions("ΟΔΟΣ", &["ΟΔΟΣ-1"]), vec![vec![0, 1, 2, 3]]);
    }

    #[test]
    fn positions_refer_to_original_characters() {
        assert_eq!(positions("aen", &["æn"]), vec![vec![0, 1]]);
    }

    #[test]
    fn empty_query_matches_everything() {
        for query in ["", "  "] {
            assert_eq!(
                match_fields(query, &["prod"]),
                Some(Match {
                    score: 0,
                    positions: vec![vec![]],
                })
            );
        }
    }

    #[test]
    fn field_start_beats_middle() {
        assert!(score("db", "db-main") > score("db", "prod-db"));
        assert!(score("db", "prod-db") > score("db", "sandbox"));
        let rows: [&[&str]; 3] = [&["sandbox"], &["prod-db"], &["db-main"]];
        let order: Vec<usize> = rank("db", rows)
            .into_iter()
            .map(|(index, _)| index)
            .collect();
        assert_eq!(order, vec![2, 1, 0]);
    }

    #[test]
    fn consecutive_beats_scattered() {
        assert!(score("web", "web-1") > score("web", "wxexb"));
    }

    #[test]
    fn ties_keep_input_order() {
        let rows: [&[&str]; 2] = [&["xa"], &["ya"]];
        let order: Vec<usize> = rank("a", rows)
            .into_iter()
            .map(|(index, _)| index)
            .collect();
        assert_eq!(order, vec![0, 1]);
    }

    /// `align` as first written: for each match, every earlier match of the previous character.
    fn naive_align(term: &[char], text: &[Folded]) -> Option<(i64, Vec<usize>)> {
        let (rows, columns) = (term.len(), text.len());
        if rows == 0 || rows > columns {
            return (rows == 0).then(|| (0, Vec::new()));
        }
        let mut best = vec![vec![None::<i64>; columns]; rows];
        let mut from = vec![vec![0usize; columns]; rows];
        for (j, &wanted) in term.iter().enumerate() {
            for i in 0..columns {
                if text[i].character != wanted {
                    continue;
                }
                let here = MATCH + text[i].bonus;
                if j == 0 {
                    best[j][i] = Some(here);
                    continue;
                }
                let previous = (0..i)
                    .filter_map(|k| {
                        let before = best[j - 1][k]?;
                        let gap = i - k - 1;
                        let joined = if gap == 0 {
                            CONSECUTIVE
                        } else {
                            GAP_START + (gap as i64 - 1) * GAP_EXTEND
                        };
                        Some((before + joined, k))
                    })
                    .max_by_key(|&(score, k)| (score, std::cmp::Reverse(k)));
                if let Some((score, k)) = previous {
                    best[j][i] = Some(score + here);
                    from[j][i] = k;
                }
            }
        }
        let (score, mut at) = (0..columns)
            .filter_map(|i| best[rows - 1][i].map(|score| (score, i)))
            .max_by_key(|&(score, i)| (score, std::cmp::Reverse(i)))?;
        let mut positions = vec![0; rows];
        for j in (0..rows).rev() {
            positions[j] = text[at].original;
            at = from[j][at];
        }
        Some((score, positions))
    }

    #[test]
    fn align_agrees_with_the_naive_alignment() {
        // The last field ties two gapped predecessors of its `b`: `a` after `-`, and `a` 8 later.
        let fields = [
            "ab-ab",
            "aab-bba",
            "a-b-a-b-a",
            "abba-ab",
            "baab",
            "Ab-aB",
            "a--b--ab",
            "x-axxxxxxxaxb",
        ];
        // Every term of up to four characters of `a`, `b`, `-` and `x`.
        let mut terms: Vec<Vec<char>> = vec![Vec::new()];
        let mut level = terms.clone();
        for _ in 0..4 {
            level = level
                .iter()
                .flat_map(|term| {
                    ['a', 'b', '-', 'x'].map(|character| [term.as_slice(), &[character]].concat())
                })
                .collect();
            terms.extend(level.iter().cloned());
        }
        for field in fields {
            let text = fold_field(field);
            for term in &terms {
                assert_eq!(
                    align(term, &text),
                    naive_align(term, &text),
                    "{term:?} in {field}"
                );
            }
        }
    }

    #[test]
    fn highlight_splits_matched_characters() {
        let (base, hit) = (
            Style::default(),
            Style::default().add_modifier(Modifier::BOLD),
        );
        let spans = highlight("prod", &[0, 2], base, hit);
        let parts: Vec<(&str, Style)> = spans
            .iter()
            .map(|span| (span.content.as_ref(), span.style))
            .collect();
        assert_eq!(
            parts,
            vec![("p", hit), ("r", base), ("o", hit), ("d", base)]
        );
        let merged = highlight("prod", &[0, 1], base, hit);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].content, "pr");
    }
}
