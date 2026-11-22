use clap::Parser;

use polymarket_cli::cli::Cli;

#[tokio::main]
async fn main() {
    // Loading a local .env is a convenience; missing files are ignored.
    let _ = dotenvy::dotenv();

    let cli = Cli::parse();
    if let Err(error) = polymarket_cli::commands::run(cli).await {
        eprintln!("Error: {error:#}");
        std::process::exit(1);
    }
}
