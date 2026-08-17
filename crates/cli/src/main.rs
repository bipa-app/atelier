//! The `atelier` binary: versioned workspaces for humans and AI agents,
//! thin over the SDK in `atelier-core`.

mod commands;

use anyhow::Result;
use atelier_core::Error;
use clap::Parser;
use tracing_subscriber::EnvFilter;

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {}", format_error(&error));
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    init_tracing()?;

    for line in commands::execute(commands::Cli::parse())? {
        println!("{line}");
    }

    Ok(())
}

fn init_tracing() -> Result<()> {
    let filter = match EnvFilter::try_from_default_env() {
        Ok(filter) => filter,
        Err(_) => EnvFilter::new("off"),
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init()
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    Ok(())
}

fn format_error(error: &anyhow::Error) -> String {
    let mut message = error
        .chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(": ");
    if error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<Error>(),
            Some(Error::NoActorConfigured)
        )
    }) {
        message.push_str(
            ": create ~/.config/atelier/config.toml with [actor] name = \"you\" kind = \"human\"",
        );
    }
    message
}
