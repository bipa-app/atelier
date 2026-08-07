use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use atelier_core::{DeltaKind, Workspace};
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

    Ok(diff
        .deltas
        .iter()
        .map(|delta| format!("{} {}", delta_label(&delta.kind), delta.address.as_str()))
        .collect())
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
            Ok(format!(
                "{}  {} ({})  {}{}",
                format_rfc3339_utc(entry.at_ms)?,
                entry.actor_name,
                entry.actor_kind,
                entry.act,
                reference
            ))
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
    let seconds = at_ms.div_euclid(1_000);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let days = seconds.div_euclid(86_400);
    let (year, month, day) = civil_from_days(days)?;
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;

    Ok(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z"
    ))
}

fn civil_from_days(days_since_epoch: i64) -> Result<(i64, i64, i64)> {
    let shifted_days = days_since_epoch
        .checked_add(719_468)
        .context("timestamp is outside the supported range")?;
    let era = shifted_days.div_euclid(146_097);
    let day_of_era = shifted_days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    let year = year + if month <= 2 { 1 } else { 0 };

    Ok((year, month, day))
}

#[cfg(test)]
mod tests {
    use super::format_rfc3339_utc;

    #[test]
    fn formats_epoch_and_leap_day_as_utc() {
        assert_eq!(format_rfc3339_utc(0).unwrap(), "1970-01-01T00:00:00Z");
        assert_eq!(
            format_rfc3339_utc(951_782_400_000).unwrap(),
            "2000-02-29T00:00:00Z"
        );
    }
}
