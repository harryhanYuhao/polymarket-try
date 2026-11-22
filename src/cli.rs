//! Command-line interface definitions.

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(
    name = "pml",
    about = "Query Polymarket prediction-market and Perps accounts, and manage Perps TP/SL",
    version,
    propagate_version = true
)]
pub struct Cli {
    /// Emit machine-readable JSON instead of tables.
    #[arg(long, global = true)]
    pub json: bool,

    /// CLOB base URL (defaults to https://clob.polymarket.com).
    #[arg(long, global = true, env = "POLYMARKET_CLOB_URL")]
    pub clob_url: Option<String>,

    /// Perps base URL (defaults to https://api.perpetuals.polymarket.com).
    #[arg(long, global = true, env = "POLYMARKET_PERPS_URL")]
    pub perps_url: Option<String>,

    /// Data API base URL (defaults to https://data-api.polymarket.com).
    #[arg(long, global = true, env = "POLYMARKET_DATA_API_URL")]
    pub data_api_url: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create, inspect, or validate authentication credentials.
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },

    /// List active orders.
    Orders(OrdersArgs),

    /// List open positions.
    Positions(PositionsArgs),

    /// Perpetuals market data, orders, and TP/SL management.
    Perps {
        #[command(subcommand)]
        command: PerpsCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Derive an existing CLOB API key, or create one if none exists.
    ///
    /// Requires POLYMARKET_PRIVATE_KEY. The resulting credentials are printed as
    /// shell exports.
    Derive,

    /// Show which credentials are configured and validate the CLOB key.
    Check,

    /// Print the address derived from POLYMARKET_PRIVATE_KEY.
    Whoami,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Venue {
    /// Prediction markets (CLOB).
    Prediction,
    /// Perpetual futures.
    Perps,
    /// Both venues.
    All,
}

#[derive(Debug, clap::Args)]
pub struct OrdersArgs {
    /// Venue to query.
    #[arg(long, value_enum, default_value_t = Venue::All)]
    pub venue: Venue,

    /// Maximum rows to display per venue.
    #[arg(long, default_value_t = 100)]
    pub limit: usize,

    /// Narrow prediction orders to one market condition ID.
    #[arg(long)]
    pub market: Option<String>,

    /// Narrow Perps orders to one instrument ID or symbol (for example BTC-USD).
    #[arg(long)]
    pub instrument: Option<String>,
}

#[derive(Debug, clap::Args)]
pub struct PositionsArgs {
    /// Venue to query.
    #[arg(long, value_enum, default_value_t = Venue::All)]
    pub venue: Venue,

    /// Maximum rows to display per venue.
    #[arg(long, default_value_t = 100)]
    pub limit: usize,

    /// Prediction-market proxy wallet address. Defaults to POLYMARKET_ADDRESS.
    #[arg(long)]
    pub address: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum PerpsCommand {
    /// List tradable Perps instruments.
    Instruments,

    /// Show the current ticker for one instrument.
    Ticker {
        /// Instrument ID or symbol (for example BTC-USD).
        instrument: String,
    },

    /// Inspect or replace position-level take-profit / stop-loss triggers.
    Tpsl {
        #[command(subcommand)]
        command: TpslCommand,
    },

    /// List active Perps orders.
    Orders {
        /// Instrument ID or symbol (for example BTC-USD).
        #[arg(long)]
        instrument: Option<String>,

        /// Maximum rows to display.
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
}

#[derive(Debug, Subcommand)]
pub enum TpslCommand {
    /// Show active TP/SL triggers, optionally for one instrument.
    Show {
        /// Instrument ID or symbol (for example BTC-USD).
        #[arg(long)]
        instrument: Option<String>,
    },

    /// Create or replace position-level TP/SL triggers.
    ///
    /// The Perps API cannot modify a trigger in place, so this cancels the
    /// existing trigger of a kind and creates a replacement. Positions are
    /// protected one kind at a time.
    Set(TpslSetArgs),
}

#[derive(Debug, clap::Args)]
pub struct TpslSetArgs {
    /// Instrument ID or symbol (for example BTC-USD).
    #[arg(long)]
    pub instrument: String,

    /// New take-profit trigger price (mark price).
    #[arg(long)]
    pub take_profit: Option<String>,

    /// New stop-loss trigger price (mark price).
    #[arg(long)]
    pub stop_loss: Option<String>,

    /// Remove the existing take-profit trigger without creating a new one.
    #[arg(long)]
    pub clear_take_profit: bool,

    /// Remove the existing stop-loss trigger without creating a new one.
    #[arg(long)]
    pub clear_stop_loss: bool,

    /// Actually submit cancel/create operations. Without this flag the command
    /// only prints the plan.
    #[arg(long, alias = "yes")]
    pub execute: bool,

    /// Seconds to wait for canceled triggers to disappear before creating.
    #[arg(long, default_value_t = 5)]
    pub wait_secs: u64,
}
