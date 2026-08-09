use similar::{ChangeTag, TextDiff};

use crate::model::{Line, LineKind};

/// The bytes as text, when they are valid UTF-8 without NUL — the
/// precondition for diffing at the text rung without a projection.
pub fn as_text(bytes: &[u8]) -> Option<&str> {
    let text = str::from_utf8(bytes).ok()?;
    if text.contains('\0') {
        return None;
    }
    Some(text)
}

/// The marker line carried after a changed line that has no trailing
/// newline, following git's convention, so a terminal-newline edit stays
/// visible in the comparison.
pub const NO_NEWLINE_MARKER: &str = "\\ no newline at end of file";

/// The text rung: the line-level comparison of two texts.
///
/// Only changed lines are carried, in document order; an unchanged line
/// yields nothing. The same inputs always yield the same lines. A carried
/// line strips its one trailing `\n` but keeps a `\r` — a CRLF conversion
/// stays visible — and a changed line without any trailing newline is
/// followed by [`NO_NEWLINE_MARKER`].
pub fn diff_lines(before: &str, after: &str) -> Vec<Line> {
    let mut lines = Vec::new();
    for change in TextDiff::from_lines(before, after).iter_all_changes() {
        let kind = match change.tag() {
            ChangeTag::Delete => LineKind::Removed,
            ChangeTag::Insert => LineKind::Added,
            ChangeTag::Equal => continue,
        };
        let raw = change.value();
        let text = match raw.strip_suffix('\n') {
            Some(stripped) => stripped,
            None => raw,
        };
        let newline_missing = !raw.ends_with('\n');
        lines.push(Line {
            kind,
            text: text.to_string(),
        });
        if newline_missing {
            lines.push(Line {
                kind: LineKind::NoNewline,
                text: NO_NEWLINE_MARKER.to_string(),
            });
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(kind: LineKind, text: &str) -> Line {
        Line {
            kind,
            text: text.to_string(),
        }
    }

    #[test]
    fn valid_utf8_is_text() {
        assert_eq!(as_text(b"hello\n"), Some("hello\n"));
    }

    #[test]
    fn invalid_utf8_and_nul_bytes_are_not_text() {
        assert_eq!(as_text(&[0xff, 0xfe]), None);
        assert_eq!(as_text(b"he\0llo"), None);
    }

    #[test]
    fn identical_texts_yield_no_lines() {
        assert!(diff_lines("a\nb\n", "a\nb\n").is_empty());
    }

    #[test]
    fn an_edited_line_yields_its_removal_and_addition() {
        let lines = diff_lines("a\nold\nc\n", "a\nnew\nc\n");
        assert_eq!(
            lines,
            vec![line(LineKind::Removed, "old"), line(LineKind::Added, "new")]
        );
    }

    #[test]
    fn growth_from_empty_yields_only_additions() {
        let lines = diff_lines("", "a\nb\n");
        assert_eq!(
            lines,
            vec![line(LineKind::Added, "a"), line(LineKind::Added, "b")]
        );
    }

    #[test]
    fn carriage_returns_stay_visible_in_carried_lines() {
        let lines = diff_lines("same\n", "same\r\n");
        assert_eq!(
            lines,
            vec![
                line(LineKind::Removed, "same"),
                line(LineKind::Added, "same\r")
            ]
        );
    }

    #[test]
    fn a_missing_terminal_newline_carries_the_git_marker() {
        let lines = diff_lines("same\n", "same");
        assert_eq!(
            lines,
            vec![
                line(LineKind::Removed, "same"),
                line(LineKind::Added, "same"),
                line(LineKind::NoNewline, NO_NEWLINE_MARKER),
            ]
        );
    }
}
