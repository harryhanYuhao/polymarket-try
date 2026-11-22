//! Command handlers.

use anyhow::{bail, Context, Result};
use serde_json::json;

use crate::auth::Address;
use crate::cli::{
    AuthCommand, Cli, Command, OrdersArgs, PerpsCommand, PositionsArgs, TpslCommand, Venue,
};
use crate::clob::{
    create_or_derive_credentials, ClobClient, DataApiClient, OpenOrder, PredictionPosition,
};
use crate::config::Config;
use crate::perps::{Instrument, PerpOrder, PerpPosition, PerpsClient};
use crate::render;
use crate::tpsl;

pub async fn run(cli: Cli) -> Result<()> {
    let config = Config::load(&cli)?;
    let json = cli.json;
    match cli.command {
        Command::Auth { command } => match command {
            AuthCommand::Derive => auth_derive(&config, json).await,
            AuthCommand::Check => auth_check(&config, json).await,
            AuthCommand::Whoami => auth_whoami(&config, json),
        },
        Command::Orders(args) => orders(&config, &args, json).await,
        Command::Positions(args) => positions(&config, &args, json).await,
        Command::Perps { command } => match command {
            PerpsCommand::Instruments => perps_instruments(&config, json).await,
            PerpsCommand::Ticker { instrument } => perps_ticker(&config, &instrument, json).await,
            PerpsCommand::Orders { instrument, limit } => {
                perps_orders_command(&config, instrument.as_deref(), limit, json).await
            }
            PerpsCommand::Tpsl { command } => match command {
                TpslCommand::Show { instrument } => {
                    tpsl::run_show(instrument.as_deref(), &config, json).await
                }
                TpslCommand::Set(args) => tpsl::run_set(&args, &config, json).await,
            },
        },
    }
}

async fn auth_derive(config: &Config, json: bool) -> Result<()> {
    let signer = config
        .private_key
        .as_ref()
        .context("POLYMARKET_PRIVATE_KEY is required to derive CLOB credentials")?;
    let http = config.http_client()?;
    let credentials =
        create_or_derive_credentials(&http, &config.clob_url, signer, config.chain_id).await?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "api_key": credentials.api_key,
                "secret": credentials.secret,
                "passphrase": credentials.passphrase,
                "signer_address": signer.address().to_string(),
            }))?
        );
        return Ok(());
    }

    println!(
        "# CLOB credentials for {} (keep the secret and passphrase private)",
        signer.address()
    );
    println!(
        "export POLYMARKET_API_KEY={}",
        shell_quote(&credentials.api_key)
    );
    println!(
        "export POLYMARKET_API_SECRET={}",
        shell_quote(&credentials.secret)
    );
    println!(
        "export POLYMARKET_API_PASSPHRASE={}",
        shell_quote(&credentials.passphrase)
    );
    println!(
        "export POLYMARKET_SIGNER_ADDRESS={}",
        shell_quote(&signer.address().to_string())
    );
    Ok(())
}

async fn auth_check(config: &Config, json: bool) -> Result<()> {
    let summary = config.summary();
    let mut validation = None;

    match (config.clob_credentials(), config.clob_l2_address()) {
        (Ok(credentials), Ok(address)) => {
            let http = config.http_client()?;
            let client = ClobClient::new(http, &config.clob_url, address, credentials)?;
            validation = Some(match client.open_orders(None).await {
                Ok(orders) => json!({
                    "ok": true,
                    "open_orders": orders.len(),
                }),
                Err(error) => json!({
                    "ok": false,
                    "error": format!("{error:#}"),
                }),
            });
        }
        (Ok(_), Err(error)) => {
            validation = Some(json!({
                "ok": false,
                "error": format!("{error:#}"),
            }));
        }
        (Err(_), _) => {}
    }

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "config": summary
                    .iter()
                    .map(|(name, value)| json!({"name": name, "value": value}))
                    .collect::<Vec<_>>(),
                "clob_validation": validation,
            }))?
        );
        return Ok(());
    }

    let table = render::table(
        &["Setting", "Value"],
        summary
            .into_iter()
            .map(|(name, value)| vec![name.to_owned(), value])
            .collect(),
    );
    println!("{table}");
    match validation {
        Some(value) if value["ok"].as_bool() == Some(true) => println!(
            "\nCLOB credentials are valid ({} open orders).",
            value["open_orders"]
        ),
        Some(value) => println!(
            "\nCLOB credentials could not be validated:\n{}",
            value["error"].as_str().unwrap_or("unknown error")
        ),
        None => println!(
            "\nCLOB credentials are incomplete; run `pml auth derive` or set the \
             POLYMARKET_API_* variables."
        ),
    }
    Ok(())
}

fn auth_whoami(config: &Config, json: bool) -> Result<()> {
    let signer = config
        .private_key
        .as_ref()
        .context("POLYMARKET_PRIVATE_KEY is not set")?;
    let address = signer.address();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"address": address.to_string()}))?
        );
    } else {
        println!("{address}");
    }
    Ok(())
}

async fn orders(config: &Config, args: &OrdersArgs, json: bool) -> Result<()> {
    if args.venue == Venue::Prediction && args.instrument.is_some() {
        bail!("--instrument is a Perps option; use --market for prediction orders");
    }
    if args.venue == Venue::Perps && args.market.is_some() {
        bail!("--market is a prediction-market option; use --instrument for Perps orders");
    }
    if args.limit == 0 {
        bail!("--limit must be greater than zero");
    }

    let mut prediction: Option<Vec<OpenOrder>> = None;
    let mut perps: Option<Vec<PerpOrder>> = None;
    let mut instruments: Vec<Instrument> = Vec::new();

    if matches!(args.venue, Venue::Prediction | Venue::All) {
        prediction = Some(prediction_orders(config, args.market.as_deref()).await?);
    }
    if matches!(args.venue, Venue::Perps | Venue::All) {
        let client = perps_client(config)?;
        instruments = client.instruments().await.unwrap_or_default();
        let instrument_id = match args.instrument.as_deref() {
            Some(input) => Some(client.resolve_instrument(input).await?.instrument_id),
            None => None,
        };
        perps = Some(client.open_orders(instrument_id).await?);
    }

    if json {
        let mut value = serde_json::Map::new();
        if let Some(orders) = &prediction {
            value.insert("prediction".to_owned(), serde_json::to_value(orders)?);
        }
        if let Some(orders) = &perps {
            let enriched: Vec<_> = orders
                .iter()
                .map(|order| perp_order_json(order, &instruments))
                .collect();
            value.insert("perps".to_owned(), serde_json::Value::Array(enriched));
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::Value::Object(value))?
        );
        return Ok(());
    }

    if let Some(mut orders) = prediction {
        orders.truncate(args.limit);
        println!("Prediction orders ({} live)", orders.len());
        if orders.is_empty() {
            println!("  (none)");
        } else {
            let table = render::table(
                &[
                    "Order ID", "Market", "Outcome", "Side", "Price", "Size", "Matched", "Type",
                    "Created",
                ],
                orders
                    .iter()
                    .map(|order| {
                        vec![
                            render::short(&order.id, 14),
                            render::short(&order.market, 14),
                            order.outcome.clone(),
                            order.side.clone(),
                            order.price.clone(),
                            order.original_size.clone(),
                            order.size_matched.clone(),
                            order.order_type.clone(),
                            render::format_unix_seconds(order.created_at),
                        ]
                    })
                    .collect(),
            );
            println!("{table}");
        }
    }

    if let Some(mut orders) = perps {
        orders.truncate(args.limit);
        println!("\nPerps orders ({} live)", orders.len());
        if orders.is_empty() {
            println!("  (none)");
        } else {
            print_perps_order_table(&orders, &instruments);
        }
    }
    Ok(())
}

async fn prediction_orders(config: &Config, market: Option<&str>) -> Result<Vec<OpenOrder>> {
    let credentials = config.clob_credentials()?;
    let address = config.clob_l2_address()?;
    let http = config.http_client()?;
    let client = ClobClient::new(http, &config.clob_url, address, credentials)?;
    client.open_orders(market).await
}

fn perps_public_client(config: &Config) -> Result<PerpsClient> {
    let http = config.http_client()?;
    PerpsClient::public(http, &config.perps_url, config.chain_id)
}

fn perps_client(config: &Config) -> Result<PerpsClient> {
    let http = config.http_client()?;
    let (proxy, secret) = config.perps_read_credentials()?;
    PerpsClient::new(
        http,
        &config.perps_url,
        *proxy,
        secret.to_owned(),
        config.chain_id,
    )
}

async fn positions(config: &Config, args: &PositionsArgs, json: bool) -> Result<()> {
    if args.limit == 0 {
        bail!("--limit must be greater than zero");
    }
    if args.venue == Venue::Perps && args.address.is_some() {
        bail!("--address applies to prediction-market positions only");
    }

    let mut prediction: Option<Vec<PredictionPosition>> = None;
    let mut perps: Option<Vec<PerpPosition>> = None;

    if matches!(args.venue, Venue::Prediction | Venue::All) {
        let address = match args.address.as_deref() {
            Some(value) => value
                .parse::<Address>()
                .context("--address is not a valid Ethereum address")?,
            None => config.wallet_address()?,
        };
        let http = config.http_client()?;
        let client = DataApiClient::new(http, &config.data_api_url)?;
        prediction = Some(client.positions(address, args.limit).await?);
    }
    if matches!(args.venue, Venue::Perps | Venue::All) {
        perps = Some(perps_client(config)?.portfolio().await?.positions);
    }

    if json {
        let mut value = serde_json::Map::new();
        if let Some(positions) = &prediction {
            value.insert("prediction".to_owned(), serde_json::to_value(positions)?);
        }
        if let Some(positions) = &perps {
            value.insert("perps".to_owned(), serde_json::to_value(positions)?);
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::Value::Object(value))?
        );
        return Ok(());
    }

    if let Some(positions) = prediction {
        println!("Prediction positions ({})", positions.len());
        if positions.is_empty() {
            println!("  (none)");
        } else {
            let table = render::table(
                &[
                    "Market", "Outcome", "Size", "Avg", "Current", "Value", "uPnL", "Status",
                ],
                positions
                    .iter()
                    .map(|position| {
                        vec![
                            render::short(&position.title, 48),
                            position.outcome.clone(),
                            format!("{:.4}", position.current_size),
                            format!("{:.4}", position.avg_price),
                            format!("{:.4}", position.current_price),
                            format!("{:.2}", position.current_value),
                            format!("{:.2}", position.unrealized_pnl),
                            position.status.clone(),
                        ]
                    })
                    .collect(),
            );
            println!("{table}");
        }
    }

    if let Some(positions) = perps {
        println!("\nPerps positions ({})", positions.len());
        if positions.is_empty() {
            println!("  (none)");
        } else {
            let table = render::table(
                &[
                    "Instrument",
                    "Size",
                    "Entry",
                    "Value",
                    "uPnL",
                    "Lev",
                    "Margin",
                    "Liq",
                ],
                positions
                    .iter()
                    .map(|position| {
                        vec![
                            position.symbol.clone(),
                            position.size.clone(),
                            position.entry_price.clone(),
                            position.position_value.clone(),
                            position.unrealized_pnl.clone(),
                            format!("{}x", position.leverage),
                            if position.cross { "cross" } else { "isolated" }.to_owned(),
                            position.liquidation_price.clone(),
                        ]
                    })
                    .collect(),
            );
            println!("{table}");
        }
    }
    Ok(())
}

async fn perps_instruments(config: &Config, json: bool) -> Result<()> {
    let instruments = perps_public_client(config)?.instruments().await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&instruments)?);
        return Ok(());
    }
    let table = render::table(
        &[
            "ID",
            "Symbol",
            "Category",
            "Max lev",
            "Price dec",
            "Qty dec",
            "Min notional",
            "Mode",
        ],
        instruments
            .iter()
            .map(|instrument| {
                vec![
                    instrument.instrument_id.to_string(),
                    instrument.label().to_owned(),
                    instrument.category.clone(),
                    format!("{}x", instrument.max_leverage),
                    instrument.price_decimals.to_string(),
                    instrument.quantity_decimals.to_string(),
                    instrument.min_notional.clone(),
                    if instrument.close_only {
                        "close-only".to_owned()
                    } else if instrument.isolated_only {
                        "isolated-only".to_owned()
                    } else {
                        "normal".to_owned()
                    },
                ]
            })
            .collect(),
    );
    println!("{table}");
    Ok(())
}

async fn perps_ticker(config: &Config, instrument: &str, json: bool) -> Result<()> {
    let client = perps_public_client(config)?;
    let instrument = client.resolve_instrument(instrument).await?;
    let ticker = client.ticker(instrument.instrument_id).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&ticker)?);
        return Ok(());
    }
    let table = render::table(
        &["Field", "Value"],
        vec![
            vec!["Instrument".to_owned(), instrument.label().to_owned()],
            vec!["Mark".to_owned(), ticker.mark_price],
            vec!["Index".to_owned(), ticker.index_price],
            vec!["Last".to_owned(), ticker.last_price],
            vec!["Mid".to_owned(), ticker.mid_price],
            vec!["Funding rate".to_owned(), ticker.funding_rate],
            vec![
                "Next funding".to_owned(),
                render::format_unix_millis(ticker.next_funding),
            ],
            vec!["Open interest".to_owned(), ticker.open_interest],
        ],
    );
    println!("{table}");
    Ok(())
}

async fn perps_orders_command(
    config: &Config,
    instrument: Option<&str>,
    limit: usize,
    json: bool,
) -> Result<()> {
    if limit == 0 {
        bail!("--limit must be greater than zero");
    }
    let client = perps_client(config)?;
    let instrument_id = match instrument {
        Some(input) => Some(client.resolve_instrument(input).await?.instrument_id),
        None => None,
    };
    let mut orders = client.open_orders(instrument_id).await?;
    orders.truncate(limit);
    let instruments = client.instruments().await.unwrap_or_default();
    if json {
        let enriched: Vec<_> = orders
            .iter()
            .map(|order| perp_order_json(order, &instruments))
            .collect();
        println!("{}", serde_json::to_string_pretty(&enriched)?);
        return Ok(());
    }
    println!("Perps orders ({} live)", orders.len());
    if orders.is_empty() {
        println!("  (none)");
    } else {
        print_perps_order_table(&orders, &instruments);
    }
    Ok(())
}

fn print_perps_order_table(orders: &[PerpOrder], instruments: &[Instrument]) {
    let table = render::table(
        &[
            "Order ID",
            "Instrument",
            "Side",
            "Price",
            "Qty",
            "Filled",
            "Type",
            "Status",
            "TP/SL",
            "Trigger",
        ],
        orders
            .iter()
            .map(|order| {
                let (kind, trigger) = match &order.tpsl {
                    Some(tpsl) => (format!("{} {}", tpsl.kind, tpsl.scope), tpsl.trp.clone()),
                    None => ("-".to_owned(), "-".to_owned()),
                };
                vec![
                    order.order_id.to_string(),
                    symbol_for(instruments, order.instrument_id),
                    if order.buy { "BUY" } else { "SELL" }.to_owned(),
                    order.price.clone(),
                    order.quantity.clone(),
                    order.filled_quantity.clone(),
                    order.tif.clone(),
                    order.status.clone(),
                    kind,
                    trigger,
                ]
            })
            .collect(),
    );
    println!("{table}");
}

fn perp_order_json(order: &PerpOrder, instruments: &[Instrument]) -> serde_json::Value {
    json!({
        "order_id": order.order_id,
        "instrument_id": order.instrument_id,
        "symbol": symbol_for(instruments, order.instrument_id),
        "buy": order.buy,
        "price": order.price,
        "quantity": order.quantity,
        "filled_quantity": order.filled_quantity,
        "resting_quantity": order.resting_quantity,
        "status": order.status,
        "tif": order.tif,
        "reduce_only": order.ro,
        "post_only": order.post_only,
        "created_timestamp": order.created_timestamp,
        "client_order_id": order.client_order_id,
        "tpsl": order.tpsl,
    })
}

fn symbol_for(instruments: &[Instrument], instrument_id: u32) -> String {
    instruments
        .iter()
        .find(|instrument| instrument.instrument_id == instrument_id)
        .map(|instrument| instrument.label().to_owned())
        .unwrap_or_else(|| instrument_id.to_string())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
