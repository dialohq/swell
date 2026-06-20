use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod cache;
mod commands;
mod config;

#[derive(Parser)]
#[command(
    name = "swell",
    about = "Static type-checking for inline Postgres queries in TypeScript"
)]
struct Cli {
    /// Path to swell.toml. Defaults to ./swell.toml.
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Scan once and write the generated .d.ts.
    Gen,
    /// Watch for file changes and regenerate incrementally.
    Watch,
    /// CI: verify cache is up-to-date and queries still type-check.
    Check,
    /// Populate cache for offline build.
    Prepare,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "swell=info".into()),
        )
        .init();

    let cli = Cli::parse();
    let config_path = cli.config.unwrap_or_else(|| PathBuf::from("swell.toml"));
    let cfg = config::load(&config_path)?;

    match cli.cmd {
        Cmd::Gen => commands::gen(&cfg).await,
        Cmd::Watch => commands::watch(&cfg).await,
        Cmd::Check => commands::check(&cfg).await,
        Cmd::Prepare => commands::prepare(&cfg).await,
    }
}
