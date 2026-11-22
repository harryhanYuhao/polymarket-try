//! Command-line interface definition (clap derive).

use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};
use pm_try::config::ENV_HELP;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "pm",
    version,
    about = "Polymarket CLI: active orders and positions across prediction markets and perps, \
             plus perps take-profit / stop-loss management",
    after_help = ENV_HELP
)]
pub struct Cli {
    /// Print JSON instead of tables
    #[arg(long, global = true, display_order = 100)]
    pub json: bool,

    /// Load environment variables from this file (default: ./.env when present)
    #[arg(long, global = true, value_name = "PATH", display_order = 101)]
    pub env_file: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Active orders on prediction markets and perps
    Orders(OrdersArgs),
    /// Open positions on prediction markets and perps
    Positions(PositionsArgs),
    /// Prediction markets (CLOB, Data API, Gamma)
    #[command(subcommand, visible_alias = "pred")]
    Prediction(PredictionCommand),
    /// Perpetual futures
    #[command(subcommand)]
    Perps(PerpsCommand),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum Venue {
    All,
    Prediction,
    Perps,
}

impl Venue {
    pub fn includes_prediction(self) -> bool {
        self != Venue::Perps
    }

    pub fn includes_perps(self) -> bool {
        self != Venue::Prediction
    }
}

#[derive(Args, Debug)]
pub struct OrdersArgs {
    /// Which venue(s) to query
    #[arg(long, value_enum, default_value_t = Venue::All)]
    pub venue: Venue,
    /// Prediction: only orders in this market (condition ID)
    #[arg(long, value_name = "CONDITION_ID")]
    pub market: Option<String>,
    /// Perps: only orders on this instrument (e.g. BTC-USD, BTC or numeric ID)
    #[arg(long, value_name = "SYMBOL")]
    pub instrument: Option<String>,
}

#[derive(Args, Debug)]
pub struct PositionsArgs {
    /// Which venue(s) to query
    #[arg(long, value_enum, default_value_t = Venue::All)]
    pub venue: Venue,
    /// Prediction: wallet holding the positions (default: PM_WALLET, then PM_ADDRESS)
    #[arg(long, value_name = "ADDRESS")]
    pub user: Option<String>,
    /// Prediction: position lifecycle filter
    #[arg(long, value_enum, default_value_t = PositionStatus::Open)]
    pub status: PositionStatus,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum PositionStatus {
    Open,
    Redeemable,
    RedeemableLost,
    Mergeable,
    Closed,
}

impl PositionStatus {
    pub fn as_api(self) -> &'static str {
        match self {
            PositionStatus::Open => "OPEN",
            PositionStatus::Redeemable => "REDEEMABLE",
            PositionStatus::RedeemableLost => "REDEEMABLE_LOST",
            PositionStatus::Mergeable => "MERGEABLE",
            PositionStatus::Closed => "CLOSED",
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum PredictionCommand {
    /// Your live CLOB orders
    Orders {
        /// Only orders in this market (condition ID)
        #[arg(long, value_name = "CONDITION_ID")]
        market: Option<String>,
        /// Only orders on this outcome token (requires --market)
        #[arg(long, value_name = "TOKEN_ID", requires = "market")]
        asset_id: Option<String>,
        /// Every live order on your deposit wallet, whichever signer or API key placed it
        #[arg(long)]
        wallet: bool,
    },
    /// Your prediction-market positions (Data API v2)
    Positions {
        /// Wallet holding the positions (default: PM_WALLET, then PM_ADDRESS)
        #[arg(long, value_name = "ADDRESS")]
        user: Option<String>,
        /// Position lifecycle filter
        #[arg(long, value_enum, default_value_t = PositionStatus::Open)]
        status: PositionStatus,
    },
    /// Browse markets: top by 24h volume, or full-text search
    Markets {
        /// Search text
        #[arg(long, short)]
        search: Option<String>,
        /// Maximum number of markets
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Derive (or create) CLOB API credentials, signing with PM_PRIVATE_KEY
    ApiKey {
        /// Create a new API key instead of deriving the existing one
        #[arg(long)]
        create: bool,
        /// Credential nonce
        #[arg(long, default_value_t = 0)]
        nonce: u64,
        /// Append the credentials to this env file (0600) instead of printing them
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
pub enum PerpsCommand {
    /// Instruments with live mark, index and funding (public)
    Markets {
        /// Filter by symbol, base asset or category
        #[arg(long, short)]
        search: Option<String>,
    },
    /// Your open perps orders, including TP/SL triggers
    Orders {
        /// Only this instrument (e.g. BTC-USD, BTC or numeric ID)
        #[arg(long, value_name = "SYMBOL")]
        instrument: Option<String>,
    },
    /// Your perps positions with attached TP/SL and margin summary
    Positions,
    /// Create, modify, list or cancel take-profit / stop-loss on a position
    #[command(subcommand)]
    Tpsl(TpslCommand),
    /// Manage Perps proxy (API) credentials
    #[command(subcommand)]
    Proxy(ProxyCommand),
}

#[derive(Subcommand, Debug)]
pub enum TpslCommand {
    /// Create or modify take-profit and/or stop-loss on an open position
    ///
    /// Triggers watch the mark price and close the whole position with a
    /// reduce-only market order. An existing trigger of the same kind is
    /// replaced (cancel, then create), because Perps cannot edit TP/SL in place.
    #[command(visible_aliases = ["create", "modify"])]
    Set(TpslSetArgs),
    /// Cancel take-profit and/or stop-loss on a position
    Cancel(TpslCancelArgs),
    /// List take-profit / stop-loss orders
    List {
        /// Only this instrument
        #[arg(long, value_name = "SYMBOL")]
        instrument: Option<String>,
    },
}

#[derive(Args, Debug)]
#[command(group(ArgGroup::new("trigger").required(true).multiple(true).args(["tp", "sl"])))]
pub struct TpslSetArgs {
    /// Instrument with the open position (e.g. BTC-USD, BTC or numeric ID)
    pub instrument: String,
    /// Take-profit trigger price
    #[arg(long, value_name = "PRICE")]
    pub tp: Option<String>,
    /// Stop-loss trigger price
    #[arg(long, value_name = "PRICE")]
    pub sl: Option<String>,
    /// Keep armed bracket (order-scoped) TP/SL legs instead of replacing them
    #[arg(long)]
    pub keep_bracket: bool,
    #[command(flatten)]
    pub exec: ExecArgs,
}

#[derive(Args, Debug)]
pub struct TpslCancelArgs {
    /// Instrument with the position (e.g. BTC-USD, BTC or numeric ID)
    pub instrument: String,
    /// Cancel the take-profit (default: both when neither --tp nor --sl is given)
    #[arg(long)]
    pub tp: bool,
    /// Cancel the stop-loss
    #[arg(long)]
    pub sl: bool,
    /// Keep armed bracket (order-scoped) legs; only cancel position triggers
    #[arg(long)]
    pub keep_bracket: bool,
    #[command(flatten)]
    pub exec: ExecArgs,
}

#[derive(Args, Debug, Clone, Copy)]
pub struct ExecArgs {
    /// Show the plan and the signed requests without sending anything
    #[arg(long)]
    pub dry_run: bool,
    /// Skip the confirmation prompt
    #[arg(long, short)]
    pub yes: bool,
}

#[derive(Subcommand, Debug)]
pub enum ProxyCommand {
    /// Register a new proxy signer for your Perps account (signed with PM_PRIVATE_KEY)
    Create {
        /// Label for this credential
        #[arg(long, default_value = "pm-cli")]
        label: String,
        /// Lifetime in days
        #[arg(long, default_value_t = 7, value_parser = clap::value_parser!(u64).range(1..=365))]
        days: u64,
        /// Append the credentials to this env file (0600) instead of printing them
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
    },
    /// Show the Perps account and its proxy keys
    Show,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn tpsl_set_requires_a_trigger() {
        assert!(Cli::try_parse_from(["pm", "perps", "tpsl", "set", "BTC-USD"]).is_err());
        let cli = Cli::try_parse_from([
            "pm", "perps", "tpsl", "modify", "BTC", "--sl", "60000", "-y",
        ])
        .unwrap();
        let Command::Perps(PerpsCommand::Tpsl(TpslCommand::Set(args))) = cli.command else {
            panic!("expected tpsl set");
        };
        assert_eq!(args.sl.as_deref(), Some("60000"));
        assert!(args.tp.is_none() && args.exec.yes && !args.exec.dry_run);
    }

    #[test]
    fn asset_id_requires_market() {
        assert!(Cli::try_parse_from(["pm", "prediction", "orders", "--asset-id", "1"]).is_err());
        assert!(Cli::try_parse_from([
            "pm",
            "pred",
            "orders",
            "--asset-id",
            "1",
            "--market",
            "0xc"
        ])
        .is_ok());
    }
}
