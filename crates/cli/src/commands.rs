use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use atelier_core::{Delta, DeltaKind, LineKind, Workspace};
use clap::{Parser, Subcommand};

const JOURNAL_LIMIT: usize = 100;

#[derive(Debug, Parser)]
#[command(name = "ws", about = "Versioned workspaces for humans and agents")]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Initialize a workspace.
    Init { path: Option<PathBuf> },
    /// Attach a local folder to the current workspace.
    Attach { folder: PathBuf },
    /// Show the changes between the two latest snapshots.
    Diff,
    /// Show recent workspace acts.
    Journal,
}

pub fn execute(cli: Cli) -> Result<Vec<String>> {
    match cli.command {
        Command::Init { path } => init(path),
        Command::Attach { folder } => attach(&folder),
        Command::Diff => diff(),
        Command::Journal => journal(),
    }
}

fn init(path: Option<PathBuf>) -> Result<Vec<String>> {
    let path = match path {
        Some(path) => path,
        None => env::current_dir().context("read the current directory")?,
    };
    Workspace::init(&path)?;

    Ok(vec![format!(
        "initialized workspace {} at {}",
        workspace_name(&path),
        path.display()
    )])
}

fn attach(folder: &Path) -> Result<Vec<String>> {
    let root = env::current_dir().context("read the current directory")?;
    let mut workspace = Workspace::open(root)?;
    let source = workspace.attach(folder)?;

    Ok(vec![format!(
        "attached {} {}",
        source.kind,
        source.path.display()
    )])
}

fn diff() -> Result<Vec<String>> {
    let root = env::current_dir().context("read the current directory")?;
    let mut workspace = Workspace::open(root)?;
    let diff = workspace.diff_latest()?;

    if diff.deltas.is_empty() {
        return Ok(vec![
            "no changes between the two latest snapshots".to_string(),
        ]);
    }

    Ok(diff.deltas.iter().flat_map(render_delta).collect())
}

/// The delta's listing line, then its line comparison when the ladder
/// raised it to the text rung; a binary-rung delta stays a bare listing.
fn render_delta(delta: &Delta) -> Vec<String> {
    let mut rendered = vec![printable(&format!(
        "{} {}",
        delta_label(&delta.kind),
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
fn printable(text: &str) -> String {
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

fn journal() -> Result<Vec<String>> {
    let root = env::current_dir().context("read the current directory")?;
    let mut workspace = Workspace::open(root)?;

    workspace
        .journal(JOURNAL_LIMIT)?
        .iter()
        .map(|entry| {
            let reference = match &entry.reference {
                Some(reference) => format!("  {reference}"),
                None => String::new(),
            };
            Ok(printable(&format!(
                "{}  {} ({})  {}{}",
                format_rfc3339_utc(entry.at_ms)?,
                entry.actor_name,
                entry.actor_kind,
                entry.act,
                reference
            )))
        })
        .collect()
}

fn workspace_name(path: &Path) -> String {
    match path.file_name().and_then(|name| name.to_str()) {
        Some(name) => name.to_string(),
        None => "workspace".to_string(),
    }
}

fn delta_label(kind: &DeltaKind) -> &'static str {
    match kind {
        DeltaKind::Added => "A",
        DeltaKind::Removed => "D",
        DeltaKind::Changed | DeltaKind::Moved => "M",
    }
}

fn format_rfc3339_utc(at_ms: i64) -> Result<String> {
    let at = time::OffsetDateTime::from_unix_timestamp(at_ms.div_euclid(1_000))
        .context("timestamp is outside the supported range")?;
    at.format(&time::format_description::well_known::Rfc3339)
        .context("format timestamp as rfc3339")
}

#[cfg(test)]
mod tests {
    use atelier_core::{Address, Delta, DeltaKind, Fidelity};

    use super::{format_rfc3339_utc, printable, render_delta};

    #[test]
    fn formats_epoch_and_leap_day_as_utc() {
        assert_eq!(format_rfc3339_utc(0).unwrap(), "1970-01-01T00:00:00Z");
        assert_eq!(
            format_rfc3339_utc(951_782_400_000).unwrap(),
            "2000-02-29T00:00:00Z"
        );
    }

    #[test]
    fn delta_addresses_print_escaped_like_line_contents() {
        let delta = Delta {
            address: Address::new("x\n+forged.txt\u{1b}[31m"),
            kind: DeltaKind::Changed,
            fidelity: Fidelity::Binary,
            before: Some("id1".to_string()),
            after: Some("id2".to_string()),
            lines: Vec::new(),
            package: None,
        };

        assert_eq!(
            render_delta(&delta),
            vec!["M x\\n+forged.txt\\u{1b}[31m".to_string()]
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
