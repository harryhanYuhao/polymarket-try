//! Position-level take-profit / stop-loss planning and execution.
//!
//! The Perps API does not support modifying a TP/SL trigger in place. A
//! replacement is therefore cancel-then-create. We do this one kind at a time
//! (take-profit first, then stop-loss) so the position keeps its other trigger
//! while a replacement is being installed.

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::cli::TpslSetArgs;
use crate::config::Config;
use crate::perps::{CancelAck, Instrument, OrderAck, PerpOrder, PerpPosition, PerpsClient, Ticker};
use crate::render;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TpSlKind {
    TakeProfit,
    StopLoss,
}

impl TpSlKind {
    pub fn code(self) -> &'static str {
        match self {
            Self::TakeProfit => "tp",
            Self::StopLoss => "sl",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::TakeProfit => "take-profit",
            Self::StopLoss => "stop-loss",
        }
    }

    pub fn from_code(code: &str) -> Option<Self> {
        match code {
            "tp" => Some(Self::TakeProfit),
            "sl" => Some(Self::StopLoss),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct TriggerChange {
    pub kind: TpSlKind,
    pub action: ChangeAction,
}

#[derive(Clone, Debug)]
pub enum ChangeAction {
    Set(String),
    Clear,
}

#[derive(Clone, Debug)]
pub struct ExistingTrigger {
    pub order_id: u64,
    pub kind: TpSlKind,
    pub scope: String,
    pub trigger_price: String,
}

pub fn build_changes(args: &TpslSetArgs) -> Result<Vec<TriggerChange>> {
    if args.take_profit.is_some() && args.clear_take_profit {
        bail!("use either --take-profit or --clear-take-profit, not both");
    }
    if args.stop_loss.is_some() && args.clear_stop_loss {
        bail!("use either --stop-loss or --clear-stop-loss, not both");
    }

    let mut changes = Vec::new();
    if let Some(price) = &args.take_profit {
        changes.push(TriggerChange {
            kind: TpSlKind::TakeProfit,
            action: ChangeAction::Set(price.clone()),
        });
    } else if args.clear_take_profit {
        changes.push(TriggerChange {
            kind: TpSlKind::TakeProfit,
            action: ChangeAction::Clear,
        });
    }

    if let Some(price) = &args.stop_loss {
        changes.push(TriggerChange {
            kind: TpSlKind::StopLoss,
            action: ChangeAction::Set(price.clone()),
        });
    } else if args.clear_stop_loss {
        changes.push(TriggerChange {
            kind: TpSlKind::StopLoss,
            action: ChangeAction::Clear,
        });
    }

    if changes.is_empty() {
        bail!(
            "nothing to change: pass --take-profit/--stop-loss to set a trigger, \
             or --clear-take-profit/--clear-stop-loss to remove one"
        );
    }
    Ok(changes)
}

pub fn existing_triggers(
    orders: &[PerpOrder],
    instrument_id: u32,
    kind: TpSlKind,
) -> Vec<ExistingTrigger> {
    let mut triggers: Vec<ExistingTrigger> = orders
        .iter()
        .filter(|order| order.instrument_id == instrument_id)
        .filter_map(|order| {
            let fields = order.tpsl.as_ref()?;
            if fields.kind != kind.code() {
                return None;
            }
            Some(ExistingTrigger {
                order_id: order.order_id,
                kind,
                scope: fields.scope.clone(),
                trigger_price: fields.trp.clone(),
            })
        })
        .collect();
    triggers.sort_by_key(|trigger| trigger.order_id);
    triggers
}

pub fn parse_price(text: &str) -> Result<f64> {
    let trimmed = text.trim();
    if trimmed != text {
        bail!("price {text:?} must not contain leading or trailing whitespace");
    }
    if text.contains('e') || text.contains('E') {
        bail!("price {text:?} must not use exponent notation");
    }
    if text.starts_with('+') {
        bail!("price {text:?} must not start with '+'");
    }
    let value: f64 = text
        .parse()
        .with_context(|| format!("price {text:?} is not a decimal number"))?;
    if !value.is_finite() || value <= 0.0 {
        bail!("price {text:?} must be positive");
    }
    Ok(value)
}

/// Checks that a trigger is on the correct side of the current mark price.
/// The exchange also enforces this, but failing locally avoids cancelling an
/// existing trigger only to have the replacement rejected.
pub fn validate_trigger(
    kind: TpSlKind,
    is_long: bool,
    mark_price: f64,
    trigger: f64,
) -> Result<()> {
    if !mark_price.is_finite() || mark_price <= 0.0 {
        bail!("mark price is unavailable or invalid");
    }
    let valid = match (kind, is_long) {
        (TpSlKind::TakeProfit, true) => trigger > mark_price,
        (TpSlKind::TakeProfit, false) => trigger < mark_price,
        (TpSlKind::StopLoss, true) => trigger < mark_price,
        (TpSlKind::StopLoss, false) => trigger > mark_price,
    };
    if !valid {
        let expectation = match (kind, is_long) {
            (TpSlKind::TakeProfit, true) => "above the mark for a long position",
            (TpSlKind::TakeProfit, false) => "below the mark for a short position",
            (TpSlKind::StopLoss, true) => "below the mark for a long position",
            (TpSlKind::StopLoss, false) => "above the mark for a short position",
        };
        bail!(
            "{} trigger {trigger} must be {expectation} (current mark {mark_price})",
            kind.label()
        );
    }
    Ok(())
}

pub fn close_side_is_buy(is_long: bool) -> bool {
    !is_long
}

#[derive(Debug, Serialize)]
pub struct SetReport {
    pub executed: bool,
    pub instrument: InstrumentReport,
    pub position: PositionReport,
    pub mark_price: Option<String>,
    pub changes: Vec<ChangeReport>,
    pub canceled_order_ids: Vec<u64>,
    pub created: Vec<CreatedTrigger>,
}

#[derive(Debug, Serialize)]
pub struct InstrumentReport {
    pub instrument_id: u32,
    pub symbol: String,
}

#[derive(Debug, Serialize)]
pub struct PositionReport {
    pub side: String,
    pub size: String,
    pub entry_price: String,
    pub unrealized_pnl: String,
    pub leverage: u32,
    pub cross: bool,
}

#[derive(Debug, Serialize)]
pub struct ChangeReport {
    pub kind: String,
    pub action: String,
    pub previous_triggers: Vec<String>,
    pub previous_order_ids: Vec<u64>,
    pub new_trigger: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CreatedTrigger {
    pub kind: String,
    pub order_id: u64,
    pub trigger_price: String,
}

pub async fn run_set(args: &TpslSetArgs, config: &Config, json: bool) -> Result<()> {
    let changes = build_changes(args)?;
    let http = config.http_client()?;
    let (proxy, secret) = config.perps_read_credentials()?;
    let client = PerpsClient::new(
        http,
        &config.perps_url,
        *proxy,
        secret.to_owned(),
        config.chain_id,
    )?;

    let instrument = client.resolve_instrument(&args.instrument).await?;
    let portfolio = client.portfolio().await?;
    let position = portfolio
        .positions
        .iter()
        .find(|position| position.instrument_id == instrument.instrument_id)
        .cloned()
        .with_context(|| {
            format!(
                "no open Perps position for {}; TP/SL can only be attached to an open position",
                instrument.label()
            )
        })?;
    let is_long = position.is_long()?;

    let needs_mark = changes
        .iter()
        .any(|change| matches!(change.action, ChangeAction::Set(_)));
    let ticker = if needs_mark {
        Some(client.ticker(instrument.instrument_id).await?)
    } else {
        None
    };
    if let Some(ticker) = &ticker {
        let mark = parse_mark(ticker)?;
        for change in &changes {
            if let ChangeAction::Set(price) = &change.action {
                validate_trigger(change.kind, is_long, mark, parse_price(price)?)?;
            }
        }
    }

    let open_orders = client.open_orders(Some(instrument.instrument_id)).await?;
    let mut report = SetReport {
        executed: args.execute,
        instrument: InstrumentReport {
            instrument_id: instrument.instrument_id,
            symbol: instrument.label().to_owned(),
        },
        position: PositionReport {
            side: if is_long { "LONG" } else { "SHORT" }.to_owned(),
            size: position.size.clone(),
            entry_price: position.entry_price.clone(),
            unrealized_pnl: position.unrealized_pnl.clone(),
            leverage: position.leverage,
            cross: position.cross,
        },
        mark_price: ticker.as_ref().map(|ticker| ticker.mark_price.clone()),
        changes: changes
            .iter()
            .map(|change| {
                let existing =
                    existing_triggers(&open_orders, instrument.instrument_id, change.kind);
                ChangeReport {
                    kind: change.kind.label().to_owned(),
                    action: match &change.action {
                        ChangeAction::Set(_) => "set".to_owned(),
                        ChangeAction::Clear => "clear".to_owned(),
                    },
                    previous_triggers: existing
                        .iter()
                        .map(|trigger| format!("{} ({})", trigger.trigger_price, trigger.scope))
                        .collect(),
                    previous_order_ids: existing.iter().map(|trigger| trigger.order_id).collect(),
                    new_trigger: match &change.action {
                        ChangeAction::Set(price) => Some(price.clone()),
                        ChangeAction::Clear => None,
                    },
                }
            })
            .collect(),
        canceled_order_ids: Vec::new(),
        created: Vec::new(),
    };

    if !json {
        print_plan(
            &instrument,
            &position,
            is_long,
            ticker.as_ref(),
            &changes,
            &open_orders,
        );
        if !args.execute {
            println!("\nDry run: pass --execute to cancel/create these triggers.");
        }
    }

    if !args.execute {
        if json {
            print_json(&report)?;
        }
        return Ok(());
    }

    let signer = config.perps_signing_key()?;
    for change in &changes {
        let existing = existing_triggers(&open_orders, instrument.instrument_id, change.kind);
        let ids: Vec<u64> = existing.iter().map(|trigger| trigger.order_id).collect();

        if !ids.is_empty() {
            let acks = client.cancel_orders(signer, &ids).await.with_context(|| {
                format!(
                    "failed to cancel existing {} trigger(s)",
                    change.kind.label()
                )
            })?;
            ensure_cancel_acks(&acks, &ids)?;
            report.canceled_order_ids.extend(ids.iter());
            if !json {
                println!(
                    "Canceled {} {} trigger(s): {}",
                    ids.len(),
                    change.kind.label(),
                    ids.iter()
                        .map(u64::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            wait_until_gone(&client, instrument.instrument_id, &ids, args.wait_secs).await?;
        }

        if let ChangeAction::Set(price) = &change.action {
            let (take_profit, stop_loss) = match change.kind {
                TpSlKind::TakeProfit => (Some(price.as_str()), None),
                TpSlKind::StopLoss => (None, Some(price.as_str())),
            };
            let acks = client
                .create_position_tpsl(
                    signer,
                    instrument.instrument_id,
                    close_side_is_buy(is_long),
                    take_profit,
                    stop_loss,
                )
                .await
                .with_context(|| {
                    format!(
                        "failed to create replacement {} trigger; any previous trigger was \
                         already canceled and this position may now be unprotected",
                        change.kind.label()
                    )
                })?;
            let order_id = ensure_single_order_ack(&acks)?;
            report.created.push(CreatedTrigger {
                kind: change.kind.label().to_owned(),
                order_id,
                trigger_price: price.clone(),
            });
            if !json {
                println!(
                    "Created {} trigger {} at order {order_id}",
                    change.kind.label(),
                    price
                );
            }
        } else if !json {
            println!("Cleared {} trigger(s).", change.kind.label());
        }
    }

    if json {
        print_json(&report)?;
    } else {
        println!("\nDone. TP/SL changes are cancel-then-create and are not atomic.");
    }
    Ok(())
}

fn print_plan(
    instrument: &Instrument,
    position: &PerpPosition,
    is_long: bool,
    ticker: Option<&Ticker>,
    changes: &[TriggerChange],
    open_orders: &[PerpOrder],
) {
    println!(
        "Instrument   {} (id {})",
        instrument.label(),
        instrument.instrument_id
    );
    println!(
        "Position     {} {} (entry {}, uPnL {}, {}x{})",
        if is_long { "LONG" } else { "SHORT" },
        position.size,
        position.entry_price,
        position.unrealized_pnl,
        position.leverage,
        if position.cross {
            " cross"
        } else {
            " isolated"
        }
    );
    if let Some(ticker) = ticker {
        println!("Mark price   {}", ticker.mark_price);
    }
    for change in changes {
        let existing = existing_triggers(open_orders, instrument.instrument_id, change.kind);
        let existing_text = if existing.is_empty() {
            "none".to_owned()
        } else {
            existing
                .iter()
                .map(|trigger| {
                    format!(
                        "{} at {} ({}, order {})",
                        trigger.trigger_price,
                        trigger.scope,
                        trigger.kind.label(),
                        trigger.order_id
                    )
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        match &change.action {
            ChangeAction::Set(price) => println!(
                "{:<12} {}  (existing: {})",
                change.kind.label(),
                price,
                existing_text
            ),
            ChangeAction::Clear => println!(
                "{:<12} clear  (existing: {})",
                change.kind.label(),
                existing_text
            ),
        }
    }
}

async fn wait_until_gone(
    client: &PerpsClient,
    instrument_id: u32,
    order_ids: &[u64],
    wait_secs: u64,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(wait_secs);
    loop {
        let orders = client.open_orders(Some(instrument_id)).await?;
        let remaining: Vec<u64> = orders
            .iter()
            .filter(|order| order_ids.contains(&order.order_id))
            .map(|order| order.order_id)
            .collect();
        if remaining.is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "canceled order(s) still appear as open after {wait_secs}s: {:?}; \
                 refusing to create a duplicate trigger",
                remaining
            );
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn ensure_cancel_acks(acks: &[CancelAck], expected: &[u64]) -> Result<()> {
    if acks.is_empty() {
        bail!("cancel request returned no acknowledgements");
    }
    let errors: Vec<String> = acks
        .iter()
        .filter(|ack| ack.status != "ok")
        .map(|ack| {
            format!(
                "order {}: {}",
                ack.oid
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "?".to_owned()),
                ack.error.as_deref().unwrap_or("unknown error")
            )
        })
        .collect();
    if !errors.is_empty() {
        bail!("cancel rejected: {}", errors.join("; "));
    }
    for id in expected {
        let acknowledged = acks
            .iter()
            .any(|ack| ack.status == "ok" && ack.oid == Some(*id));
        if !acknowledged {
            bail!("cancel response did not acknowledge order {id}");
        }
    }
    Ok(())
}

fn ensure_single_order_ack(acks: &[OrderAck]) -> Result<u64> {
    let ack = acks
        .first()
        .context("create request returned no acknowledgements")?;
    if acks.len() != 1 {
        bail!(
            "expected one order acknowledgement, received {}",
            acks.len()
        );
    }
    if ack.status != "ok" {
        bail!(
            "trigger creation rejected: {}",
            ack.error.as_deref().unwrap_or("unknown error")
        );
    }
    ack.oid
        .context("trigger creation acknowledged without an order ID")
}

fn parse_mark(ticker: &Ticker) -> Result<f64> {
    let mark: f64 = ticker
        .mark_price
        .parse()
        .with_context(|| format!("invalid mark price {:?}", ticker.mark_price))?;
    if !mark.is_finite() || mark <= 0.0 {
        bail!("mark price {:?} is not positive", ticker.mark_price);
    }
    Ok(mark)
}

fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).context("failed to serialize JSON output")?
    );
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct TpslRow {
    pub instrument_id: u32,
    pub symbol: String,
    pub order_id: u64,
    pub kind: String,
    pub scope: String,
    pub trigger_price: String,
    pub armed_qty: Option<String>,
    pub parent_oid: Option<u64>,
    pub status: String,
}

pub async fn run_show(instrument: Option<&str>, config: &Config, json: bool) -> Result<()> {
    let http = config.http_client()?;
    let (proxy, secret) = config.perps_read_credentials()?;
    let client = PerpsClient::new(
        http,
        &config.perps_url,
        *proxy,
        secret.to_owned(),
        config.chain_id,
    )?;

    let instrument_id = match instrument {
        Some(input) => Some(client.resolve_instrument(input).await?.instrument_id),
        None => None,
    };
    let orders = client.open_orders(instrument_id).await?;
    let instruments = client.instruments().await.unwrap_or_default();
    let mut rows: Vec<TpslRow> = orders
        .iter()
        .filter_map(|order| {
            let tpsl = order.tpsl.as_ref()?;
            Some(TpslRow {
                instrument_id: order.instrument_id,
                symbol: instruments
                    .iter()
                    .find(|instrument| instrument.instrument_id == order.instrument_id)
                    .map(|instrument| instrument.label().to_owned())
                    .unwrap_or_else(|| order.instrument_id.to_string()),
                order_id: order.order_id,
                kind: tpsl.kind.clone(),
                scope: tpsl.scope.clone(),
                trigger_price: tpsl.trp.clone(),
                armed_qty: tpsl.armed_qty.clone(),
                parent_oid: tpsl.parent_oid,
                status: order.status.clone(),
            })
        })
        .collect();
    rows.sort_by(|left, right| {
        left.instrument_id
            .cmp(&right.instrument_id)
            .then_with(|| left.kind.cmp(&right.kind))
            .then_with(|| left.order_id.cmp(&right.order_id))
    });

    if json {
        print_json(&rows)?;
        return Ok(());
    }
    if rows.is_empty() {
        println!("No active TP/SL triggers.");
        return Ok(());
    }
    let table = render::table(
        &[
            "Instrument",
            "Kind",
            "Scope",
            "Trigger",
            "Armed qty",
            "Order ID",
            "Status",
        ],
        rows.iter()
            .map(|row| {
                vec![
                    row.symbol.clone(),
                    row.kind.clone(),
                    row.scope.clone(),
                    row.trigger_price.clone(),
                    row.armed_qty.clone().unwrap_or_else(|| "-".to_owned()),
                    row.order_id.to_string(),
                    row.status.clone(),
                ]
            })
            .collect(),
    );
    println!("{table}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::perps::TpSlFields;

    #[test]
    fn long_trigger_directions() {
        assert!(validate_trigger(TpSlKind::TakeProfit, true, 100.0, 110.0).is_ok());
        assert!(validate_trigger(TpSlKind::StopLoss, true, 100.0, 90.0).is_ok());
        assert!(validate_trigger(TpSlKind::TakeProfit, true, 100.0, 90.0).is_err());
        assert!(validate_trigger(TpSlKind::StopLoss, true, 100.0, 110.0).is_err());
    }

    #[test]
    fn short_trigger_directions() {
        assert!(validate_trigger(TpSlKind::TakeProfit, false, 100.0, 90.0).is_ok());
        assert!(validate_trigger(TpSlKind::StopLoss, false, 100.0, 110.0).is_ok());
        assert!(validate_trigger(TpSlKind::TakeProfit, false, 100.0, 110.0).is_err());
        assert!(validate_trigger(TpSlKind::StopLoss, false, 100.0, 90.0).is_err());
    }

    #[test]
    fn close_side_is_opposite() {
        assert!(!close_side_is_buy(true));
        assert!(close_side_is_buy(false));
    }

    #[test]
    fn collects_existing_triggers_of_one_kind() {
        let orders = vec![
            PerpOrder {
                order_id: 1,
                instrument_id: 1,
                buy: false,
                price: "65000".into(),
                quantity: "0".into(),
                tif: "gtc".into(),
                post_only: false,
                ro: true,
                status: "open".into(),
                resting_quantity: "0".into(),
                filled_quantity: "0".into(),
                created_timestamp: 0,
                updated_timestamp: 0,
                client_order_id: None,
                tpsl: Some(TpSlFields {
                    kind: "tp".into(),
                    scope: "position".into(),
                    trp: "70000".into(),
                    parent_oid: None,
                    armed_qty: Some("0".into()),
                    slip_bps: None,
                }),
            },
            PerpOrder {
                order_id: 2,
                instrument_id: 1,
                buy: false,
                price: "65000".into(),
                quantity: "0".into(),
                tif: "gtc".into(),
                post_only: false,
                ro: true,
                status: "open".into(),
                resting_quantity: "0".into(),
                filled_quantity: "0".into(),
                created_timestamp: 0,
                updated_timestamp: 0,
                client_order_id: None,
                tpsl: Some(TpSlFields {
                    kind: "sl".into(),
                    scope: "position".into(),
                    trp: "62000".into(),
                    parent_oid: None,
                    armed_qty: Some("0".into()),
                    slip_bps: None,
                }),
            },
        ];
        let tp = existing_triggers(&orders, 1, TpSlKind::TakeProfit);
        assert_eq!(tp.len(), 1);
        assert_eq!(tp[0].order_id, 1);
        assert_eq!(tp[0].trigger_price, "70000");
    }

    #[test]
    fn builds_changes_in_tp_then_sl_order() {
        let args = TpslSetArgs {
            instrument: "1".into(),
            take_profit: Some("110".into()),
            stop_loss: Some("90".into()),
            clear_take_profit: false,
            clear_stop_loss: false,
            execute: false,
            wait_secs: 5,
        };
        let changes = build_changes(&args).unwrap();
        assert_eq!(changes[0].kind, TpSlKind::TakeProfit);
        assert_eq!(changes[1].kind, TpSlKind::StopLoss);
    }
}
