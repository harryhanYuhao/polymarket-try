//! Command handlers.

use crate::cli::{
    Command, ExecArgs, OrdersArgs, PerpsCommand, PositionStatus, PositionsArgs, PredictionCommand,
    ProxyCommand, TpslCancelArgs, TpslCommand, TpslSetArgs, Venue,
};
use crate::output::{
    align_right, dec, ellipsize, heading, num, print_json, short_id, signed, table, timestamp_text,
    unix_secs,
};
use anyhow::{bail, Context, Result};
use pm_try::clob::{ClobClient, ClobOrder, OrderFilter};
use pm_try::config::{Config, MissingCredentials};
use pm_try::data_api::{DataApiClient, PredictionPosition};
use pm_try::eth::LocalSigner;
use pm_try::gamma::GammaClient;
use pm_try::http::{now_millis, ApiError};
use pm_try::perps::signing::{create_proxy_request, random_salt, TpSlKind};
use pm_try::perps::tpsl::{existing_triggers, plan_cancel, plan_set, Plan, SetRequest};
use pm_try::perps::types::{Ack, Instrument, Margin, PerpsOrder, PerpsPosition};
use pm_try::perps::{find_instrument, PerpsClient};
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::path::Path;

pub struct Ctx {
    pub cfg: Config,
    pub http: reqwest::Client,
    pub json: bool,
}

impl Ctx {
    fn clob(&self) -> ClobClient {
        ClobClient::new(self.http.clone(), &self.cfg.clob_url, self.cfg.chain_id)
    }

    fn data(&self) -> DataApiClient {
        DataApiClient::new(self.http.clone(), &self.cfg.data_url)
    }

    fn gamma(&self) -> GammaClient {
        GammaClient::new(self.http.clone(), &self.cfg.gamma_url)
    }

    fn perps(&self) -> PerpsClient {
        PerpsClient::new(self.http.clone(), &self.cfg.perps_url, self.cfg.chain_id)
    }

    /// Progress text: stdout normally, stderr with --json so stdout stays valid JSON.
    fn say(&self, line: impl AsRef<str>) {
        if self.json {
            eprintln!("{}", line.as_ref());
        } else {
            println!("{}", line.as_ref());
        }
    }
}

pub async fn run(ctx: &Ctx, command: Command) -> Result<()> {
    match command {
        Command::Orders(args) => orders(ctx, args).await,
        Command::Positions(args) => positions(ctx, args).await,
        Command::Prediction(cmd) => match cmd {
            PredictionCommand::Orders {
                market,
                asset_id,
                wallet,
            } => {
                let rows =
                    prediction_orders(ctx, &OrderFilter { market, asset_id }, wallet).await?;
                render(ctx, &rows, render_prediction_orders)
            }
            PredictionCommand::Positions { user, status } => {
                let view = prediction_positions(ctx, user.as_deref(), status).await?;
                render(ctx, &view, render_prediction_positions)
            }
            PredictionCommand::Markets { search, limit } => {
                prediction_markets(ctx, search, limit).await
            }
            PredictionCommand::ApiKey { create, nonce, out } => {
                api_key(ctx, create, nonce, out.as_deref()).await
            }
        },
        Command::Perps(cmd) => match cmd {
            PerpsCommand::Markets { search } => perps_markets(ctx, search).await,
            PerpsCommand::Orders { instrument } => {
                let rows = perps_orders(ctx, instrument.as_deref()).await?;
                render(ctx, &rows, render_perps_orders)
            }
            PerpsCommand::Positions => {
                let view = perps_positions(ctx).await?;
                render(ctx, &view, render_perps_positions)
            }
            PerpsCommand::Tpsl(TpslCommand::Set(args)) => tpsl_set(ctx, args).await,
            PerpsCommand::Tpsl(TpslCommand::Cancel(args)) => tpsl_cancel(ctx, args).await,
            PerpsCommand::Tpsl(TpslCommand::List { instrument }) => {
                tpsl_list(ctx, instrument.as_deref()).await
            }
            PerpsCommand::Proxy(ProxyCommand::Create { label, days, out }) => {
                proxy_create(ctx, &label, days, out.as_deref()).await
            }
            PerpsCommand::Proxy(ProxyCommand::Show) => proxy_show(ctx).await,
        },
    }
}

fn render<T: Serialize + ?Sized>(ctx: &Ctx, value: &T, table_view: impl FnOnce(&T)) -> Result<()> {
    if ctx.json {
        print_json(value)
    } else {
        table_view(value);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Multi-venue commands
// ---------------------------------------------------------------------------

/// Collects per-venue results. With `--venue all`, a venue without
/// credentials is skipped instead of failing the whole command.
#[derive(Default)]
struct VenueReport {
    json: Map<String, Value>,
    failed: Vec<&'static str>,
    succeeded: usize,
}

impl VenueReport {
    fn record<T: Serialize>(
        &mut self,
        ctx: &Ctx,
        venue: &'static str,
        explicit: bool,
        result: Result<T>,
        table_view: impl FnOnce(&T),
    ) {
        match result {
            Ok(data) => {
                self.succeeded += 1;
                if ctx.json {
                    let value = serde_json::to_value(&data).unwrap_or(Value::Null);
                    self.json.insert(venue.into(), value);
                } else {
                    table_view(&data);
                }
            }
            Err(err) if !explicit && err.downcast_ref::<MissingCredentials>().is_some() => {
                if ctx.json {
                    self.json
                        .insert(venue.into(), json!({ "skipped": err.to_string() }));
                } else {
                    eprintln!("\n{venue}: skipped, {err}");
                }
            }
            Err(err) => {
                self.failed.push(venue);
                if ctx.json {
                    self.json
                        .insert(venue.into(), json!({ "error": format!("{err:#}") }));
                } else {
                    eprintln!("\n{venue}: error: {err:#}");
                }
            }
        }
    }

    fn finish(self, ctx: &Ctx) -> Result<()> {
        if ctx.json {
            print_json(&self.json)?;
        }
        if !self.failed.is_empty() {
            bail!("{} request(s) failed", self.failed.join(" and "));
        }
        if self.succeeded == 0 {
            bail!("no venue has credentials configured (see `pm --help`)");
        }
        Ok(())
    }
}

async fn orders(ctx: &Ctx, args: OrdersArgs) -> Result<()> {
    let explicit = args.venue != Venue::All;
    let filter = OrderFilter {
        market: args.market.clone(),
        asset_id: None,
    };
    let (prediction, perps) = tokio::join!(
        async {
            if args.venue.includes_prediction() {
                Some(prediction_orders(ctx, &filter, false).await)
            } else {
                None
            }
        },
        async {
            if args.venue.includes_perps() {
                Some(perps_orders(ctx, args.instrument.as_deref()).await)
            } else {
                None
            }
        },
    );
    let mut report = VenueReport::default();
    if let Some(result) = prediction {
        report.record(ctx, "prediction", explicit, result, |rows| {
            heading(&format!("Prediction orders ({})", rows.len()));
            render_prediction_orders(rows);
        });
    }
    if let Some(result) = perps {
        report.record(ctx, "perps", explicit, result, |rows| {
            heading(&format!("Perps orders ({})", rows.len()));
            render_perps_orders(rows);
        });
    }
    report.finish(ctx)
}

async fn positions(ctx: &Ctx, args: PositionsArgs) -> Result<()> {
    let explicit = args.venue != Venue::All;
    let (prediction, perps) = tokio::join!(
        async {
            if args.venue.includes_prediction() {
                Some(prediction_positions(ctx, args.user.as_deref(), args.status).await)
            } else {
                None
            }
        },
        async {
            if args.venue.includes_perps() {
                Some(perps_positions(ctx).await)
            } else {
                None
            }
        },
    );
    let mut report = VenueReport::default();
    if let Some(result) = prediction {
        report.record(ctx, "prediction", explicit, result, |view| {
            heading(&format!(
                "Prediction positions ({}) for {}",
                view.positions.len(),
                view.wallet
            ));
            render_prediction_positions(view);
        });
    }
    if let Some(result) = perps {
        report.record(ctx, "perps", explicit, result, |view| {
            heading(&format!("Perps positions ({})", view.positions.len()));
            render_perps_positions(view);
        });
    }
    report.finish(ctx)
}

// ---------------------------------------------------------------------------
// Prediction markets
// ---------------------------------------------------------------------------

async fn prediction_orders(
    ctx: &Ctx,
    filter: &OrderFilter,
    wallet_wide: bool,
) -> Result<Vec<ClobOrder>> {
    let creds = ctx.cfg.clob_credentials()?;
    let clob = ctx.clob();
    let mut orders = if wallet_wide {
        clob.wallet_open_orders(&creds, filter).await?
    } else {
        clob.open_orders(&creds, filter).await?
    };

    // Orders only carry condition IDs; look up the market questions (best effort).
    let mut ids: Vec<String> = orders
        .iter()
        .map(|o| o.market.to_lowercase())
        .filter(|m| !m.is_empty())
        .collect();
    ids.sort();
    ids.dedup();
    if !ids.is_empty() {
        match ctx.gamma().markets_by_condition_ids(&ids).await {
            Ok(markets) => {
                let questions: HashMap<String, String> = markets
                    .into_iter()
                    .filter_map(|m| Some((m.condition_id?.to_lowercase(), m.question?)))
                    .collect();
                for order in &mut orders {
                    order.question = questions.get(&order.market.to_lowercase()).cloned();
                }
            }
            Err(err) => eprintln!("note: could not load market titles: {err:#}"),
        }
    }
    Ok(orders)
}

fn render_prediction_orders(rows: &Vec<ClobOrder>) {
    if rows.is_empty() {
        println!("No live prediction orders.");
        return;
    }
    let mut t = table(&[
        "Market",
        "Outcome",
        "Side",
        "Price",
        "Size",
        "Filled",
        "Type",
        "Status",
        "Order",
        "Created (UTC)",
    ]);
    for o in rows {
        let market = o
            .question
            .as_deref()
            .map(|q| ellipsize(q, 48))
            .unwrap_or_else(|| short_id(&o.market));
        t.add_row(vec![
            market,
            o.outcome.clone().unwrap_or_default(),
            o.side.clone(),
            o.price.clone(),
            o.original_size.clone(),
            o.size_matched.clone(),
            o.order_type.clone().unwrap_or_default(),
            o.status.clone(),
            short_id(&o.id),
            o.created_at
                .as_deref()
                .map(timestamp_text)
                .unwrap_or_default(),
        ]);
    }
    align_right(&mut t, &[3, 4, 5]);
    println!("{t}");
}

#[derive(Serialize)]
struct PredictionPositionsView {
    wallet: String,
    positions: Vec<PredictionPosition>,
}

async fn prediction_positions(
    ctx: &Ctx,
    user: Option<&str>,
    status: PositionStatus,
) -> Result<PredictionPositionsView> {
    let wallet = ctx.cfg.positions_wallet(user)?.to_string().to_lowercase();
    let positions = ctx.data().positions(&wallet, status.as_api()).await?;
    Ok(PredictionPositionsView { wallet, positions })
}

fn render_prediction_positions(view: &PredictionPositionsView) {
    if view.positions.is_empty() {
        println!("No prediction positions.");
        return;
    }
    let mut t = table(&[
        "Market",
        "Outcome",
        "Shares",
        "Avg",
        "Price",
        "Value",
        "Unrealized",
        "Realized",
        "PnL %",
        "Status",
    ]);
    let (mut value, mut unrealized) = (0.0, 0.0);
    for p in &view.positions {
        value += p.current_value;
        unrealized += p.unrealized_pnl;
        let status = if p.redeemable {
            "redeemable".to_string()
        } else {
            p.status.to_lowercase()
        };
        t.add_row(vec![
            ellipsize(&p.title, 48),
            p.outcome.clone(),
            num(p.current_size, 2),
            num(p.avg_price, 4),
            num(p.current_price, 4),
            format!("{:.2}", p.current_value),
            signed(p.unrealized_pnl, 2),
            signed(p.realized_pnl, 2),
            format!("{:+.1}%", p.percent_pnl),
            status,
        ]);
    }
    t.add_row(vec![
        "Total".to_string(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        format!("{value:.2}"),
        signed(unrealized, 2),
        String::new(),
        String::new(),
        String::new(),
    ]);
    align_right(&mut t, &[2, 3, 4, 5, 6, 7, 8]);
    println!("{t}");
}

async fn prediction_markets(ctx: &Ctx, search: Option<String>, limit: usize) -> Result<()> {
    let gamma = ctx.gamma();
    let markets = match &search {
        Some(text) => gamma.search_markets(text, limit).await?,
        None => gamma.top_markets(limit).await?,
    };
    render(ctx, &markets, |markets| {
        if markets.is_empty() {
            println!("No markets found.");
            return;
        }
        let mut t = table(&["Question", "Outcomes", "24h volume", "Ends", "Condition ID"]);
        for m in markets {
            t.add_row(vec![
                ellipsize(m.question.as_deref().unwrap_or_default(), 60),
                m.outcome_summary(),
                m.volume_24hr
                    .as_deref()
                    .and_then(|v| v.parse::<f64>().ok())
                    .map(|v| format!("{v:.0}"))
                    .unwrap_or_default(),
                m.end_date
                    .as_deref()
                    .map(|d| d.chars().take(10).collect())
                    .unwrap_or_default(),
                m.condition_id.clone().unwrap_or_default(),
            ]);
        }
        align_right(&mut t, &[2]);
        println!("{t}");
    })
}

async fn api_key(ctx: &Ctx, create: bool, nonce: u64, out: Option<&Path>) -> Result<()> {
    let owner = ctx.cfg.owner_signer("prediction")?;
    let clob = ctx.clob();
    let creds = if create {
        clob.create_api_key(&owner, nonce).await?
    } else {
        clob.derive_api_key(&owner, nonce)
            .await
            .context("could not derive an existing API key (use --create to make one)")?
    };
    emit_env(
        ctx,
        &[
            ("PM_ADDRESS", owner.address().to_string()),
            ("PM_CLOB_API_KEY", creds.api_key),
            ("PM_CLOB_SECRET", creds.secret),
            ("PM_CLOB_PASSPHRASE", creds.passphrase),
        ],
        out,
        "CLOB API credentials",
    )
}

// ---------------------------------------------------------------------------
// Perps: reads
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct PerpsOrderRow {
    symbol: String,
    #[serde(flatten)]
    order: PerpsOrder,
}

fn symbols(instruments: &[Instrument]) -> HashMap<u64, String> {
    instruments
        .iter()
        .map(|i| (i.instrument_id, i.label().to_string()))
        .collect()
}

fn symbol_of(symbols: &HashMap<u64, String>, id: u64) -> String {
    symbols
        .get(&id)
        .cloned()
        .unwrap_or_else(|| format!("#{id}"))
}

async fn perps_orders(ctx: &Ctx, instrument: Option<&str>) -> Result<Vec<PerpsOrderRow>> {
    let creds = ctx.cfg.perps_credentials()?;
    let perps = ctx.perps();
    let instruments = perps.instruments().await?;
    let instrument_id = instrument
        .map(|q| find_instrument(&instruments, q).map(|i| i.instrument_id))
        .transpose()?;
    let orders = perps.open_orders(&creds, instrument_id).await?;
    let symbols = symbols(&instruments);
    Ok(orders
        .into_iter()
        .map(|order| PerpsOrderRow {
            symbol: symbol_of(&symbols, order.instrument_id),
            order,
        })
        .collect())
}

fn is_zero(text: &str) -> bool {
    text.parse::<f64>().map_or(text.is_empty(), |v| v == 0.0)
}

fn render_perps_orders(rows: &Vec<PerpsOrderRow>) {
    if rows.is_empty() {
        println!("No open perps orders.");
        return;
    }
    let mut t = table(&[
        "Symbol",
        "Side",
        "Type",
        "Price",
        "Trigger",
        "Qty",
        "Filled",
        "Status",
        "TIF",
        "Flags",
        "Order",
        "Created (UTC)",
    ]);
    for row in rows {
        let o = &row.order;
        let (kind, trigger) = match &o.tpsl {
            Some(tr) => (
                format!("{} ({})", tr.kind.to_uppercase(), tr.scope),
                dec(&tr.trp),
            ),
            None => ("LIMIT".to_string(), String::new()),
        };
        let price = if o.tpsl.is_some() && is_zero(&o.price) {
            "market".to_string()
        } else {
            dec(&o.price)
        };
        // Position triggers are sized at trigger time; bracket legs report
        // their size in `armed_qty`.
        let qty = match &o.tpsl {
            Some(tr) if is_zero(&o.quantity) && tr.scope == "position" => "position".to_string(),
            Some(tr) if is_zero(&o.quantity) => tr
                .armed_qty
                .as_deref()
                .filter(|q| !is_zero(q))
                .map_or_else(|| dec(&o.quantity), dec),
            _ => dec(&o.quantity),
        };
        let flags = [(o.post_only, "PO"), (o.ro, "RO")]
            .iter()
            .filter(|(on, _)| *on)
            .map(|(_, f)| *f)
            .collect::<Vec<_>>()
            .join(" ");
        t.add_row(vec![
            row.symbol.clone(),
            if o.buy { "BUY" } else { "SELL" }.to_string(),
            kind,
            price,
            trigger,
            qty,
            dec(&o.filled_quantity),
            o.status.clone(),
            o.tif.clone().unwrap_or_default(),
            flags,
            o.order_id.to_string(),
            o.created_timestamp
                .map(|ms| unix_secs(ms / 1000))
                .unwrap_or_default(),
        ]);
    }
    align_right(&mut t, &[3, 4, 5, 6]);
    println!("{t}");
}

#[derive(Serialize)]
struct PerpsPositionRow {
    #[serde(flatten)]
    position: PerpsPosition,
    mark_price: Option<String>,
    take_profit: Vec<String>,
    stop_loss: Vec<String>,
}

#[derive(Serialize)]
struct PerpsPositionsView {
    positions: Vec<PerpsPositionRow>,
    margin: Option<Margin>,
    withdrawable: Option<String>,
    in_liquidation: bool,
}

async fn perps_positions(ctx: &Ctx) -> Result<PerpsPositionsView> {
    let creds = ctx.cfg.perps_credentials()?;
    let perps = ctx.perps();
    let (portfolio, tickers, orders) = tokio::try_join!(
        perps.portfolio(&creds),
        perps.tickers(),
        perps.open_orders(&creds, None),
    )?;
    let marks: HashMap<u64, String> = tickers
        .into_iter()
        .map(|t| (t.instrument_id, t.mark_price))
        .collect();
    let positions = portfolio
        .positions
        .into_iter()
        .map(|position| {
            let triggers = existing_triggers(position.instrument_id, &orders);
            let prices = |kind: TpSlKind| {
                triggers
                    .iter()
                    .filter(|t| t.kind == kind && t.protects_position)
                    .map(|t| t.trigger_price.clone())
                    .collect::<Vec<_>>()
            };
            PerpsPositionRow {
                mark_price: marks.get(&position.instrument_id).cloned(),
                take_profit: prices(TpSlKind::Tp),
                stop_loss: prices(TpSlKind::Sl),
                position,
            }
        })
        .collect();
    Ok(PerpsPositionsView {
        positions,
        margin: portfolio.margin,
        withdrawable: portfolio.withdrawable,
        in_liquidation: portfolio.in_liquidation,
    })
}

fn render_perps_positions(view: &PerpsPositionsView) {
    if view.positions.is_empty() {
        println!("No open perps positions.");
    } else {
        let mut t = table(&[
            "Symbol", "Side", "Size", "Entry", "Mark", "Liq.", "Lev", "Margin", "uPnL", "ROE",
            "TP", "SL",
        ]);
        for row in &view.positions {
            let p = &row.position;
            let roe = p
                .return_on_equity
                .parse::<f64>()
                .map(|r| format!("{:+.2}%", r * 100.0))
                .unwrap_or_default();
            t.add_row(vec![
                p.symbol.clone(),
                p.side().map_or("FLAT", |s| s.as_str()).to_string(),
                dec(p.abs_size()),
                dec(&p.entry_price),
                row.mark_price.as_deref().map(dec).unwrap_or_default(),
                dec(&p.liquidation_price),
                p.leverage.map(|l| format!("{l}x")).unwrap_or_default(),
                match p.cross {
                    Some(true) => "cross",
                    Some(false) => "isolated",
                    None => "",
                }
                .to_string(),
                dec(&p.unrealized_pnl),
                roe,
                display_list(&row.take_profit),
                display_list(&row.stop_loss),
            ]);
        }
        align_right(&mut t, &[2, 3, 4, 5, 8, 9, 10, 11]);
        println!("{t}");
    }
    if let Some(m) = &view.margin {
        println!(
            "Account value {} · available margin {} · initial margin {} · maintenance {} · withdrawable {}",
            m.total_account_value,
            m.available_order_margin,
            m.total_initial_margin,
            m.total_maintenance_margin,
            view.withdrawable.as_deref().unwrap_or("-"),
        );
    }
    if view.in_liquidation {
        println!("WARNING: the account is being liquidated.");
    }
}

fn display_list(items: &[String]) -> String {
    if items.is_empty() {
        "-".to_string()
    } else {
        items.iter().map(|i| dec(i)).collect::<Vec<_>>().join(", ")
    }
}

async fn perps_markets(ctx: &Ctx, search: Option<String>) -> Result<()> {
    let perps = ctx.perps();
    let (instruments, tickers) = tokio::try_join!(perps.instruments(), perps.tickers())?;
    let mut tickers: HashMap<u64, _> = tickers.into_iter().map(|t| (t.instrument_id, t)).collect();
    let needle = search.map(|s| s.to_lowercase());
    let rows: Vec<Value> = instruments
        .into_iter()
        .filter(|i| match &needle {
            None => true,
            Some(n) => [
                Some(i.symbol.as_str()),
                i.display_symbol.as_deref(),
                i.base_asset.as_deref(),
                i.category.as_deref(),
            ]
            .into_iter()
            .flatten()
            .any(|field| field.to_lowercase().contains(n)),
        })
        .map(|i| {
            let ticker = tickers.remove(&i.instrument_id);
            json!({ "instrument": i, "ticker": ticker })
        })
        .collect();
    render(ctx, &rows, |rows| {
        if rows.is_empty() {
            println!("No matching instruments.");
            return;
        }
        let mut t = table(&[
            "ID",
            "Symbol",
            "Category",
            "Mark",
            "Index",
            "Funding",
            "Open interest",
            "Max lev",
            "Price dp",
            "Qty dp",
        ]);
        for row in rows {
            let (i, tk) = (&row["instrument"], &row["ticker"]);
            let text = |v: &Value| match v {
                Value::String(s) => dec(s),
                Value::Null => String::new(),
                other => other.to_string(),
            };
            let funding = tk["funding_rate"]
                .as_str()
                .and_then(|f| f.parse::<f64>().ok())
                .map(|f| format!("{:.4}%", f * 100.0))
                .unwrap_or_default();
            let symbol = i["display_symbol"]
                .as_str()
                .or(i["symbol"].as_str())
                .unwrap_or_default();
            t.add_row(vec![
                text(&i["instrument_id"]),
                symbol.to_string(),
                text(&i["category"]),
                text(&tk["mark_price"]),
                text(&tk["index_price"]),
                funding,
                text(&tk["open_interest"]),
                i["max_leverage"]
                    .as_u64()
                    .map(|l| format!("{l}x"))
                    .unwrap_or_default(),
                text(&i["price_decimals"]),
                text(&i["quantity_decimals"]),
            ]);
        }
        align_right(&mut t, &[0, 3, 4, 5, 6, 7, 8, 9]);
        println!("{t}");
    })
}

// ---------------------------------------------------------------------------
// Perps: TP/SL
// ---------------------------------------------------------------------------

/// Everything needed to plan a TP/SL change for one instrument.
struct TpslContext {
    perps: PerpsClient,
    creds: pm_try::config::PerpsCredentials,
    instrument: Instrument,
    position: Option<PerpsPosition>,
    mark_price: Option<String>,
    orders: Vec<PerpsOrder>,
}

async fn load_tpsl_context(ctx: &Ctx, instrument: &str) -> Result<TpslContext> {
    let creds = ctx.cfg.perps_credentials()?;
    let perps = ctx.perps();
    let instruments = perps.instruments().await?;
    let instrument = find_instrument(&instruments, instrument)?.clone();
    let id = instrument.instrument_id;
    let (portfolio, ticker, orders) = tokio::try_join!(
        perps.portfolio(&creds),
        perps.ticker(id),
        perps.open_orders(&creds, Some(id)),
    )?;
    Ok(TpslContext {
        position: portfolio
            .positions
            .into_iter()
            .find(|p| p.instrument_id == id),
        mark_price: ticker.map(|t| t.mark_price).filter(|m| !m.is_empty()),
        perps,
        creds,
        instrument,
        orders,
    })
}

async fn tpsl_set(ctx: &Ctx, args: TpslSetArgs) -> Result<()> {
    let tc = load_tpsl_context(ctx, &args.instrument).await?;
    let plan = plan_set(
        &tc.instrument,
        tc.position.as_ref(),
        tc.mark_price.as_deref(),
        &tc.orders,
        &SetRequest {
            take_profit: args.tp.clone(),
            stop_loss: args.sl.clone(),
            keep_bracket: args.keep_bracket,
        },
    )?;
    execute_plan(ctx, &tc, &plan, args.exec).await
}

async fn tpsl_cancel(ctx: &Ctx, args: TpslCancelArgs) -> Result<()> {
    let kinds = match (args.tp, args.sl) {
        (true, false) => vec![TpSlKind::Tp],
        (false, true) => vec![TpSlKind::Sl],
        _ => vec![TpSlKind::Tp, TpSlKind::Sl],
    };
    let tc = load_tpsl_context(ctx, &args.instrument).await?;
    let plan = plan_cancel(
        &tc.instrument,
        tc.position.as_ref(),
        tc.mark_price.as_deref(),
        &tc.orders,
        &kinds,
        args.keep_bracket,
    );
    execute_plan(ctx, &tc, &plan, args.exec).await
}

/// Cancel errors meaning the order no longer exists, so a replacement can go ahead.
const GONE: &[&str] = &[
    "order_not_found",
    "order_already_terminal",
    "order_not_in_orderbook",
    "order_unknown",
];

#[derive(Serialize, Default)]
struct ExecutionReport {
    instrument: String,
    dry_run: bool,
    notes: Vec<String>,
    cancelled: Vec<Ack>,
    created: Vec<Ack>,
    requests: Vec<Value>,
}

async fn execute_plan(ctx: &Ctx, tc: &TpslContext, plan: &Plan, exec: ExecArgs) -> Result<()> {
    print_plan(ctx, plan);
    let mut report = ExecutionReport {
        instrument: plan.symbol.clone(),
        dry_run: exec.dry_run,
        notes: plan.notes.clone(),
        ..Default::default()
    };
    if plan.is_noop() {
        ctx.say("Nothing to change.");
        return if ctx.json {
            print_json(&report)
        } else {
            Ok(())
        };
    }
    let steps: Vec<(&str, pm_try::perps::signing::PerpsOp)> = [
        ("DELETE /v1/trade/orders", plan.cancel_op()),
        ("POST /v1/trade/orders", plan.create_op()),
    ]
    .into_iter()
    .filter_map(|(label, op)| op.map(|op| (label, op)))
    .collect();

    if exec.dry_run {
        for (label, op) in &steps {
            let request = match &tc.creds.signer {
                Some(key) => serde_json::to_value(tc.perps.sign(op, key)?)?,
                None => json!({ "op": op.body(), "sig": "<needs PM_PERPS_PROXY_KEY>" }),
            };
            ctx.say(format!("\n{label}  (signed op hash {})", op.hash()));
            ctx.say(serde_json::to_string_pretty(&request)?);
            report
                .requests
                .push(json!({ "endpoint": label, "body": request }));
        }
        ctx.say("\nDry run: nothing was sent.");
        return if ctx.json {
            print_json(&report)
        } else {
            Ok(())
        };
    }

    let (_, key) = ctx.cfg.perps_trading_credentials()?;
    confirm(exec.yes, "Send these orders?")?;

    for (label, op) in &steps {
        let signed_request = tc.perps.sign(op, &key)?;
        let is_cancel = label.starts_with("DELETE");
        let result = tc.perps.submit(&tc.creds, op, &signed_request).await;
        if is_cancel {
            let acks = result.with_context(|| {
                format!(
                    "cancelling the existing trigger(s) failed, so nothing was created; \
                     check `pm perps tpsl list --instrument {}`",
                    plan.symbol
                )
            })?;
            let mut blocking = Vec::new();
            for (trigger, ack) in plan.cancel.iter().zip(&acks) {
                let error = ack.error.as_deref().unwrap_or("unknown error");
                if ack.is_ok() {
                    ctx.say(format!("cancelled {}", trigger.describe()));
                } else if GONE.contains(&error) {
                    ctx.say(format!("{} was already gone ({error})", trigger.describe()));
                } else {
                    blocking.push(format!("{} ({error})", trigger.describe()));
                }
            }
            report.cancelled = acks;
            if !blocking.is_empty() {
                if ctx.json {
                    print_json(&report)?;
                }
                bail!(
                    "could not cancel {}; no new triggers were created",
                    blocking.join(", ")
                );
            }
        } else {
            // A 4xx means the exchange refused the whole request. Anything else
            // (timeout, dropped connection, 5xx) leaves the outcome unknown: the
            // orders may or may not have been accepted.
            let mut rejected: Vec<(TpSlKind, String)> = Vec::new();
            let mut transport_error = None;
            let acks = match result {
                Ok(acks) => acks,
                Err(err)
                    if err
                        .downcast_ref::<ApiError>()
                        .is_some_and(|e| e.status.is_client_error()) =>
                {
                    let reason = format!("{err:#}");
                    rejected.extend(plan.create.iter().map(|(kind, _)| (*kind, reason.clone())));
                    Vec::new()
                }
                Err(err) => {
                    transport_error = Some(err);
                    Vec::new()
                }
            };
            for (i, (kind, price)) in plan.create.iter().enumerate() {
                match acks.get(i) {
                    Some(ack) if ack.is_ok() => ctx.say(format!(
                        "placed {kind} @ {price} (order #{})",
                        ack.oid.map_or_else(|| "?".into(), |o| o.to_string())
                    )),
                    Some(ack) => rejected.push((*kind, ack.error.clone().unwrap_or_default())),
                    None => {}
                }
            }
            report.created = acks;
            let replaced = plan.replaced_kinds();
            let previous = |kind: TpSlKind| {
                plan.cancel
                    .iter()
                    .filter(|t| t.kind == kind)
                    .map(|t| t.trigger_price.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            for (kind, error) in &rejected {
                eprintln!("{kind} was rejected: {error}");
                if replaced.contains(kind) {
                    eprintln!(
                        "WARNING: the previous {kind} ({}) was cancelled, so the position currently has NO {kind}.",
                        previous(*kind)
                    );
                }
            }
            if let Some(err) = &transport_error {
                eprintln!("could not confirm whether the new trigger(s) were placed: {err:#}");
                if !replaced.is_empty() {
                    eprintln!(
                        "WARNING: the previous trigger(s) were cancelled; the position may be unprotected."
                    );
                }
                eprintln!(
                    "Check `pm perps tpsl list --instrument {}` before retrying.",
                    plan.symbol
                );
            }
            if !rejected.is_empty() || transport_error.is_some() {
                if ctx.json {
                    print_json(&report)?;
                }
                if transport_error.is_some() {
                    bail!("placing the new trigger(s) failed with an unknown outcome");
                }
                bail!(
                    "{} of {} trigger(s) were not placed",
                    rejected.len(),
                    plan.create.len()
                );
            }
        }
    }
    if ctx.json {
        print_json(&report)?;
    }
    Ok(())
}

fn print_plan(ctx: &Ctx, plan: &Plan) {
    let position = match &plan.position {
        Some(p) => format!(
            "{} {} @ {} (liquidation {})",
            p.side.as_str(),
            dec(&p.size),
            dec(&p.entry_price),
            if p.liquidation_price.is_empty() {
                "-".to_string()
            } else {
                dec(&p.liquidation_price)
            }
        ),
        None => "no open position".to_string(),
    };
    ctx.say(format!("{}: {position}", plan.symbol));
    if let Some(mark) = &plan.mark_price {
        ctx.say(format!("Mark price: {}", dec(mark)));
    }
    for t in &plan.cancel {
        ctx.say(format!("  cancel  {}", t.describe()));
    }
    for (kind, price) in &plan.create {
        ctx.say(format!(
            "  create  {kind} @ {price} (whole position, market exit)"
        ));
    }
    for t in &plan.keep {
        ctx.say(format!("  keep    {}", t.describe()));
    }
    for note in &plan.notes {
        ctx.say(format!("note: {note}"));
    }
    if !plan.cancel.is_empty() && !plan.create.is_empty() {
        ctx.say(
            "TP/SL cannot be edited in place: old triggers are cancelled first, then replaced.",
        );
    }
}

fn confirm(yes: bool, question: &str) -> Result<()> {
    if yes {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        bail!("not sending orders without confirmation; re-run with --yes");
    }
    eprint!("{question} [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") {
        Ok(())
    } else {
        bail!("aborted, nothing was sent")
    }
}

async fn tpsl_list(ctx: &Ctx, instrument: Option<&str>) -> Result<()> {
    let rows: Vec<PerpsOrderRow> = perps_orders(ctx, instrument)
        .await?
        .into_iter()
        .filter(|r| r.order.tpsl.is_some())
        .collect();
    render(ctx, &rows, |rows| {
        if rows.is_empty() {
            println!("No take-profit / stop-loss orders.");
            return;
        }
        let mut t = table(&[
            "Symbol",
            "Kind",
            "Scope",
            "Trigger",
            "Closes with",
            "Qty",
            "Status",
            "Entry order",
            "Order",
        ]);
        for row in rows {
            let o = &row.order;
            let Some(tr) = &o.tpsl else { continue };
            let kind = TpSlKind::parse(&tr.kind)
                .map_or_else(|| tr.kind.clone(), |k| k.label().to_string());
            let qty = match tr.armed_qty.as_deref().filter(|q| !is_zero(q)) {
                Some(q) => dec(q),
                None if tr.scope == "position" => "whole position".to_string(),
                None => dec(&o.quantity),
            };
            t.add_row(vec![
                row.symbol.clone(),
                kind,
                tr.scope.clone(),
                dec(&tr.trp),
                if o.buy { "BUY" } else { "SELL" }.to_string(),
                qty,
                o.status.clone(),
                tr.parent_oid.map(|p| p.to_string()).unwrap_or_default(),
                o.order_id.to_string(),
            ]);
        }
        align_right(&mut t, &[3, 5]);
        println!("{t}");
    })
}

// ---------------------------------------------------------------------------
// Perps: proxy credentials
// ---------------------------------------------------------------------------

async fn proxy_create(ctx: &Ctx, label: &str, days: u64, out: Option<&Path>) -> Result<()> {
    let owner = ctx.cfg.owner_signer("perps")?;
    let perps = ctx.perps();
    match perps.is_registered(&owner.address()).await {
        Ok(true) => {}
        Ok(false) => eprintln!(
            "note: {} has no Perps account yet; if the request is rejected, open Perps on polymarket.com first",
            owner.address()
        ),
        Err(err) => eprintln!("note: could not check Perps registration: {err:#}"),
    }
    let proxy = LocalSigner::random();
    let now = now_millis();
    let expiry = now + days * 86_400_000;
    let body = create_proxy_request(
        &owner,
        &proxy.address(),
        expiry,
        random_salt(),
        now,
        ctx.cfg.chain_id,
        Some(label),
    )?;
    let secret = perps.create_proxy(&body).await?;
    eprintln!(
        "Registered proxy {} for {} (expires {} UTC).",
        proxy.address(),
        owner.address(),
        unix_secs(expiry / 1000)
    );
    emit_env(
        ctx,
        &[
            ("PM_PERPS_PROXY", proxy.address().to_string()),
            ("PM_PERPS_PROXY_KEY", proxy.private_key_hex()),
            ("PM_PERPS_SECRET", secret),
        ],
        out,
        "Perps proxy credentials",
    )
}

async fn proxy_show(ctx: &Ctx) -> Result<()> {
    let creds = ctx.cfg.perps_credentials()?;
    let info = ctx.perps().account_credentials(&creds).await?;
    render(ctx, &info, |info| {
        println!("Perps account {}", info.address);
        let mut t = table(&["Proxy", "Label", "Expires (UTC)", "In use"]);
        for key in &info.keys {
            let current = key.proxy.eq_ignore_ascii_case(&creds.proxy.to_string());
            t.add_row(vec![
                key.proxy.clone(),
                key.label.clone().unwrap_or_default(),
                key.expiry
                    .map(|ms| unix_secs(ms / 1000))
                    .unwrap_or_default(),
                if current { "yes" } else { "" }.to_string(),
            ]);
        }
        println!("{t}");
    })
}

/// Print `KEY=value` lines, or write them into an env file.
fn emit_env(ctx: &Ctx, vars: &[(&str, String)], out: Option<&Path>, what: &str) -> Result<()> {
    match out {
        Some(path) => {
            upsert_env_file(path, vars)?;
            eprintln!("Saved {what} to {} (permissions 0600).", path.display());
            Ok(())
        }
        None if ctx.json => {
            let map: Map<String, Value> = vars
                .iter()
                .map(|(k, v)| (k.to_string(), json!(v)))
                .collect();
            print_json(&map)
        }
        None => {
            for (key, value) in vars {
                println!("{key}={value}");
            }
            eprintln!("Keep these secret: add them to your .env (or re-run with --out .env).");
            Ok(())
        }
    }
}

/// Set `vars` in an env file: existing assignments of the same keys are
/// replaced (dotenv files keep the first definition, so appending would not
/// take effect), other lines are kept. New files are created with mode 0600.
fn upsert_env_file(path: &Path, vars: &[(&str, String)]) -> Result<()> {
    let existing = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };
    let defines = |line: &str, key: &str| {
        let line = line.trim_start();
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        line.strip_prefix(key)
            .is_some_and(|rest| rest.trim_start().starts_with('='))
    };
    let mut text: String = existing
        .lines()
        .filter(|line| !vars.iter().any(|(key, _)| defines(line, key)))
        .map(|line| format!("{line}\n"))
        .collect();
    for (key, value) in vars {
        text.push_str(&format!("{key}={value}\n"));
    }

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    file.write_all(text.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = file.metadata()?.permissions().mode();
        if mode & 0o077 != 0 {
            eprintln!(
                "warning: {} is readable by other users (mode {:o}); consider `chmod 600 {}`",
                path.display(),
                mode & 0o777,
                path.display()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_replaces_keys_and_keeps_other_lines() {
        let dir = std::env::temp_dir().join(format!("pm-try-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("env");
        std::fs::write(
            &path,
            "# comment\nPM_PERPS_SECRET=old\nexport PM_PERPS_PROXY = 0xold\nOTHER=1\n",
        )
        .unwrap();
        upsert_env_file(
            &path,
            &[
                ("PM_PERPS_PROXY", "0xnew".into()),
                ("PM_PERPS_SECRET", "new".into()),
            ],
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "# comment\nOTHER=1\nPM_PERPS_PROXY=0xnew\nPM_PERPS_SECRET=new\n"
        );

        let fresh = dir.join("fresh-env");
        upsert_env_file(&fresh, &[("A", "1".into())]).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&fresh).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
