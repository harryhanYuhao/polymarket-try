mod cli;
mod commands;
mod output;

use anyhow::{Context, Result};
use clap::Parser;
use pm_try::config::Config;
use pm_try::http::build_client;
use std::path::Path;
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = cli::Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: cli::Cli) -> Result<()> {
    // Variables already set in the environment take precedence over the file.
    match &cli.env_file {
        Some(path) => {
            dotenvy::from_path(path).with_context(|| format!("loading {}", path.display()))?;
        }
        None if Path::new(".env").is_file() => {
            dotenvy::from_path(".env").context("loading ./.env")?;
        }
        None => {}
    }
    let ctx = commands::Ctx {
        cfg: Config::from_env()?,
        http: build_client()?,
        json: cli.json,
    };
    commands::run(&ctx, cli.command).await
}
