use std::fmt;
use std::path::{Path, PathBuf};

use crate::suggest::{Origin, origin_label};

/// Beyond this many cells the LCS table is not built and the whole middle is reported as changed.
const MAX_LCS_CELLS: usize = 4_000_000;

/// The exact line changes of every file an operation would touch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Preview {
    pub files: Vec<FilePreview>,
}

/// The changes of one file, in file order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilePreview {
    pub path: PathBuf,
    pub lines: Vec<PreviewLine>,
}

/// A changed line without its line ending. Numbers are 1-based: in the old file for `Removed`,
/// in the new file for `Added`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PreviewLine {
    Removed { number: usize, text: String },
    Added { number: usize, text: String },
}

impl Preview {
    /// The preview as text, with `home` shown as `~` in the file paths.
    pub fn render(&self, home: Option<&Path>) -> String {
        let mut out = String::new();
        for (index, file) in self.files.iter().enumerate() {
            if index > 0 {
                out.push('\n');
            }
            let label = origin_label(&Origin::External(file.path.clone()), home);
            out.push_str(&format!("{label}\n"));
            for line in &file.lines {
                match line {
                    PreviewLine::Removed { number, text } => {
                        out.push_str(&format!("  -{number}: {text}\n"));
                    }
                    PreviewLine::Added { number, text } => {
                        out.push_str(&format!("  +{number}: {text}\n"));
                    }
                }
            }
        }
        out
    }
}

impl fmt::Display for Preview {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render(None))
    }
}

fn split_lines(text: &str) -> Vec<&str> {
    text.lines()
        .map(|line| line.trim_end_matches('\r'))
        .collect()
}

/// The line changes from `old` to `new`: the common prefix and suffix are skipped, then a longest
/// common subsequence decides what is kept in the middle.
pub(crate) fn diff(path: &Path, old: &str, new: &str) -> FilePreview {
    let old = split_lines(old);
    let new = split_lines(new);
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let old_mid = &old[prefix..old.len() - suffix];
    let new_mid = &new[prefix..new.len() - suffix];

    let mut lines = Vec::new();
    let removed = |index: usize| PreviewLine::Removed {
        number: prefix + index + 1,
        text: old_mid[index].to_string(),
    };
    let added = |index: usize| PreviewLine::Added {
        number: prefix + index + 1,
        text: new_mid[index].to_string(),
    };

    let cells = old_mid.len().saturating_mul(new_mid.len());
    if cells > MAX_LCS_CELLS {
        lines.extend((0..old_mid.len()).map(removed));
        lines.extend((0..new_mid.len()).map(added));
    } else {
        // table[i][j]: LCS length of old_mid[i..] and new_mid[j..].
        let width = new_mid.len() + 1;
        let mut table = vec![0u32; (old_mid.len() + 1) * width];
        for i in (0..old_mid.len()).rev() {
            for j in (0..new_mid.len()).rev() {
                table[i * width + j] = if old_mid[i] == new_mid[j] {
                    table[(i + 1) * width + j + 1] + 1
                } else {
                    table[(i + 1) * width + j].max(table[i * width + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < old_mid.len() || j < new_mid.len() {
            if i < old_mid.len() && j < new_mid.len() && old_mid[i] == new_mid[j] {
                i += 1;
                j += 1;
            } else if j == new_mid.len()
                || (i < old_mid.len() && table[(i + 1) * width + j] >= table[i * width + j + 1])
            {
                lines.push(removed(i));
                i += 1;
            } else {
                lines.push(added(j));
                j += 1;
            }
        }
    }
    FilePreview {
        path: path.to_path_buf(),
        lines,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(old: &str, new: &str) -> Vec<PreviewLine> {
        diff(Path::new("f"), old, new).lines
    }

    fn removed(number: usize, text: &str) -> PreviewLine {
        PreviewLine::Removed {
            number,
            text: text.into(),
        }
    }

    fn added(number: usize, text: &str) -> PreviewLine {
        PreviewLine::Added {
            number,
            text: text.into(),
        }
    }

    #[test]
    fn pure_deletion_reports_old_line_numbers() {
        assert_eq!(
            lines("a\nb\nc\nd\ne\n", "a\nd\ne\n"),
            [removed(2, "b"), removed(3, "c")]
        );
    }

    #[test]
    fn pure_insertion_reports_new_line_numbers() {
        assert_eq!(
            lines("a\nd\n", "a\nb\nc\nd\n"),
            [added(2, "b"), added(3, "c")]
        );
    }

    #[test]
    fn identical_text_has_no_lines() {
        assert!(lines("a\r\nb\n", "a\r\nb\n").is_empty());
    }

    #[test]
    fn replacement_lists_removed_then_added_without_carriage_returns() {
        assert_eq!(
            lines("a\r\nb\r\nc\r\n", "a\r\nx\r\nc\r\n"),
            [removed(2, "b"), added(2, "x")]
        );
    }

    #[test]
    fn lcs_keeps_common_lines_inside_the_middle() {
        assert_eq!(
            lines("a\nb\nc\nd\ne\n", "a\nc\nx\ne\n"),
            [removed(2, "b"), removed(4, "d"), added(3, "x")]
        );
    }

    #[test]
    fn display_groups_by_file_with_signs() {
        let preview = Preview {
            files: vec![
                diff(Path::new("/home/me/.ssh/config"), "a\nb\nc\n", "a\nc\n"),
                diff(Path::new("/etc/ssh/ssh_config"), "x\n", "x\ny\n"),
            ],
        };
        let expected = "/home/me/.ssh/config\n  -2: b\n\n/etc/ssh/ssh_config\n  +2: y\n";
        assert_eq!(preview.to_string(), expected);
        assert_eq!(
            preview.render(Some(Path::new("/home/me"))),
            "~/.ssh/config\n  -2: b\n\n/etc/ssh/ssh_config\n  +2: y\n"
        );
    }
}
