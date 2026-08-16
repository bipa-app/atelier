use atelier_diff_core::{Delta, DeltaKind, Diff, LineKind};

/// The diff as printable lines: every face renders the same comparison
/// through this one path (ADR-0006).
#[must_use]
pub fn render_diff(diff: &Diff) -> Vec<String> {
    diff.deltas.iter().flat_map(render_delta).collect()
}

/// The delta's listing line, then its line comparison when the ladder
/// raised it to the text rung; a binary-rung delta stays a bare listing.
fn render_delta(delta: &Delta) -> Vec<String> {
    let mut rendered = vec![printable(&format!(
        "{} {}",
        delta_label(delta.kind),
        delta.address.as_str()
    ))];
    for line in &delta.lines {
        match line.kind {
            LineKind::Removed => rendered.push(format!("-{}", printable(&line.text))),
            LineKind::Added => rendered.push(format!("+{}", printable(&line.text))),
            // The synthetic marker prints bare, as git does — content
            // lines always carry a sign, so the two can never collide.
            LineKind::NoNewline => rendered.push(line.text.clone()),
        }
    }
    rendered
}

/// The line with control characters escaped, so a diffed document cannot
/// inject escape sequences into the terminal that reads it; tabs stay
/// literal. Bidi formatting characters escape too — they are not
/// `char::is_control`, but they can visually reorder or conceal diff
/// content.
#[must_use]
pub fn printable(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        // Literal backslashes escape first, so text that merely spells an
        // escape sequence never renders like a real control character.
        if c == '\\' {
            out.push_str("\\\\");
        } else if (c.is_control() && c != '\t') || is_bidi_control(c) {
            out.extend(c.escape_debug());
        } else {
            out.push(c);
        }
    }
    out
}

fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}

fn delta_label(kind: DeltaKind) -> &'static str {
    match kind {
        DeltaKind::Added => "A",
        DeltaKind::Removed => "D",
        DeltaKind::Changed | DeltaKind::Moved => "M",
    }
}

#[cfg(test)]
mod tests {
    use atelier_diff_core::{Address, Delta, DeltaKind, Fidelity};

    use super::{printable, render_delta};

    #[test]
    fn delta_addresses_print_escaped_like_line_contents() {
        let delta = Delta {
            address: Address::new("x\n+forged.txt\u{1b}[31m"),
            kind: DeltaKind::Changed,
            fidelity: Fidelity::Binary,
            before: Some("id1".to_owned()),
            after: Some("id2".to_owned()),
            lines: Vec::new(),
            package: None,
        };

        assert_eq!(
            render_delta(&delta),
            vec!["M x\\n+forged.txt\\u{1b}[31m".to_owned()]
        );
    }

    #[test]
    fn literal_backslashes_escape_so_spelled_escapes_stay_distinct() {
        assert_eq!(printable("literal \\n"), "literal \\\\n");
        assert_eq!(printable("actual \n"), "actual \\n");
        assert_ne!(printable("literal \\n"), printable("actual \n"));
    }

    #[test]
    fn bidi_formatting_characters_print_escaped() {
        assert_eq!(printable("user\u{202e}txt.exe"), "user\\u{202e}txt.exe");
        assert_eq!(printable("a\u{2066}b\u{2069}c"), "a\\u{2066}b\\u{2069}c");
    }

    #[test]
    fn control_characters_print_escaped_but_tabs_stay_literal() {
        assert_eq!(
            printable("red \u{1b}[31mnow\r\u{8}"),
            "red \\u{1b}[31mnow\\r\\u{8}"
        );
        assert_eq!(printable("a\tb"), "a\tb");
        assert_eq!(printable("plain text"), "plain text");
    }
}
