//! Planning for position take-profit / stop-loss changes.
//!
//! Perps TP/SL orders cannot be modified in place. Per the API docs, a change
//! is "cancel the old trigger, then create the replacement", and an armed
//! bracket leg (entry already filled) is replaced by a position-scoped
//! trigger. This module validates the request and decides what to cancel and
//! what to create; it does no I/O so it can be tested directly.

use super::signing::{NewOrder, PerpsOp, TpSlKind, TpSlScope};
use super::types::{Instrument, PerpsOrder, PerpsPosition, PositionSide};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashSet;

/// A TP/SL order currently on the book for an instrument.
#[derive(Debug, Clone, PartialEq)]
pub struct ExistingTrigger {
    pub order_id: u64,
    pub kind: TpSlKind,
    /// `position` or `order` (bracket leg).
    pub scope: String,
    pub trigger_price: String,
    /// True for position triggers and armed bracket legs; false for dormant
    /// bracket legs still waiting for their entry order to fill.
    pub protects_position: bool,
    pub parent_order_id: Option<u64>,
}

impl ExistingTrigger {
    pub fn describe(&self) -> String {
        let scope = match (self.scope.as_str(), self.protects_position) {
            ("position", _) => "position".to_string(),
            (_, true) => "armed bracket leg".to_string(),
            (_, false) => match self.parent_order_id {
                Some(parent) => format!("dormant bracket leg of entry #{parent}"),
                None => "dormant bracket leg".to_string(),
            },
        };
        format!(
            "{} @ {} ({scope}, #{})",
            self.kind, self.trigger_price, self.order_id
        )
    }
}

/// TP/SL orders on `instrument_id` among the account's open orders.
pub fn existing_triggers(instrument_id: u64, open_orders: &[PerpsOrder]) -> Vec<ExistingTrigger> {
    let open_ids: HashSet<u64> = open_orders.iter().map(|o| o.order_id).collect();
    open_orders
        .iter()
        .filter(|o| o.instrument_id == instrument_id)
        .filter_map(|o| {
            let fields = o.tpsl.as_ref()?;
            let kind = TpSlKind::parse(&fields.kind)?;
            let dormant = fields.scope == "order"
                && (o.status.eq_ignore_ascii_case("dormant")
                    || fields.parent_oid.is_some_and(|p| open_ids.contains(&p)));
            Some(ExistingTrigger {
                order_id: o.order_id,
                kind,
                scope: fields.scope.clone(),
                trigger_price: fields.trp.clone(),
                protects_position: !dormant,
                parent_order_id: fields.parent_oid,
            })
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct PositionSummary {
    pub side: PositionSide,
    pub size: String,
    pub entry_price: String,
    pub liquidation_price: String,
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub instrument_id: u64,
    pub symbol: String,
    pub position: Option<PositionSummary>,
    pub mark_price: Option<String>,
    /// Existing triggers to cancel (sent first).
    pub cancel: Vec<ExistingTrigger>,
    /// New position-scoped triggers to create after the cancels.
    pub create: Vec<(TpSlKind, String)>,
    /// Existing triggers left in place.
    pub keep: Vec<ExistingTrigger>,
    pub notes: Vec<String>,
}

impl Plan {
    pub fn is_noop(&self) -> bool {
        self.cancel.is_empty() && self.create.is_empty()
    }

    pub fn cancel_op(&self) -> Option<PerpsOp> {
        (!self.cancel.is_empty()).then(|| PerpsOp::CancelOrders {
            order_ids: self.cancel.iter().map(|t| t.order_id).collect(),
        })
    }

    pub fn create_op(&self) -> Option<PerpsOp> {
        let position = self.position.as_ref()?;
        (!self.create.is_empty()).then(|| PerpsOp::CreateOrders {
            orders: self
                .create
                .iter()
                .map(|(kind, price)| {
                    NewOrder::position_tpsl(
                        self.instrument_id,
                        position.side.closing_buy(),
                        *kind,
                        price.clone(),
                    )
                })
                .collect(),
            group: Some(TpSlScope::Position),
        })
    }

    /// Kinds whose old trigger is cancelled; if the create then fails the
    /// position is unprotected for that kind.
    pub fn replaced_kinds(&self) -> Vec<TpSlKind> {
        let mut kinds: Vec<TpSlKind> = self.cancel.iter().map(|t| t.kind).collect();
        kinds.sort();
        kinds.dedup();
        kinds
    }
}

#[derive(Debug, Clone, Default)]
pub struct SetRequest {
    pub take_profit: Option<String>,
    pub stop_loss: Option<String>,
    /// Leave armed bracket legs alone instead of replacing them.
    pub keep_bracket: bool,
}

/// Plan creating or replacing TP and/or SL on an open position.
pub fn plan_set(
    instrument: &Instrument,
    position: Option<&PerpsPosition>,
    mark_price: Option<&str>,
    open_orders: &[PerpsOrder],
    request: &SetRequest,
) -> Result<Plan> {
    let position = position
        .filter(|p| p.side().is_some())
        .ok_or_else(|| anyhow!("no open {} position to protect", instrument.label()))?;
    let side = position.side().expect("filtered above");

    let mut requested: Vec<(TpSlKind, String)> = Vec::new();
    for (kind, price) in [
        (TpSlKind::Tp, &request.take_profit),
        (TpSlKind::Sl, &request.stop_loss),
    ] {
        if let Some(price) = price {
            let price = validate_price(price, instrument.price_decimals)
                .with_context(|| format!("invalid {kind} price"))?;
            requested.push((kind, price));
        }
    }
    if requested.is_empty() {
        bail!("nothing to set: pass --tp and/or --sl");
    }

    let mut notes = Vec::new();
    let mark = mark_price
        .and_then(|m| m.parse::<f64>().ok())
        .filter(|m| *m > 0.0);
    match mark {
        Some(mark) => {
            for (kind, price) in &requested {
                check_direction(
                    side,
                    *kind,
                    price.parse()?,
                    mark,
                    mark_price.unwrap_or_default(),
                )?;
            }
        }
        None => notes
            .push("mark price unavailable: trigger direction was not checked locally".to_string()),
    }
    if let (Some(tp), Some(sl)) = (
        requested.iter().find(|(k, _)| *k == TpSlKind::Tp),
        requested.iter().find(|(k, _)| *k == TpSlKind::Sl),
    ) {
        let (tp_v, sl_v): (f64, f64) = (tp.1.parse()?, sl.1.parse()?);
        let consistent = match side {
            PositionSide::Long => sl_v < tp_v,
            PositionSide::Short => sl_v > tp_v,
        };
        if !consistent {
            bail!(
                "for a {} position the stop-loss ({}) must be {} the take-profit ({})",
                side.as_str().to_lowercase(),
                sl.1,
                if side == PositionSide::Long {
                    "below"
                } else {
                    "above"
                },
                tp.1
            );
        }
    }
    if let Some((_, sl)) = requested.iter().find(|(k, _)| *k == TpSlKind::Sl) {
        if let Ok(liq) = position.liquidation_price.parse::<f64>() {
            let sl_v: f64 = sl.parse()?;
            let past = liq > 0.0
                && match side {
                    PositionSide::Long => sl_v <= liq,
                    PositionSide::Short => sl_v >= liq,
                };
            if past {
                notes.push(format!(
                    "stop-loss {sl} is beyond the liquidation price {}; the exchange may reject it",
                    position.liquidation_price
                ));
            }
        }
    }

    let existing = existing_triggers(instrument.instrument_id, open_orders);
    let mut cancel: Vec<ExistingTrigger> = Vec::new();
    let mut create = Vec::new();
    for (kind, price) in requested {
        let replaceable: Vec<ExistingTrigger> = existing
            .iter()
            .filter(|t| t.kind == kind && t.protects_position)
            .filter(|t| t.scope == "position" || !request.keep_bracket)
            .cloned()
            .collect();
        if let [only] = replaceable.as_slice() {
            if only.scope == "position" && same_price(&only.trigger_price, &price) {
                notes.push(format!("{kind} is already at {price}; left unchanged"));
                continue;
            }
        }
        if request.keep_bracket
            && existing
                .iter()
                .any(|t| t.kind == kind && t.protects_position && t.scope != "position")
        {
            notes.push(format!(
                "keeping armed bracket {kind} leg(s); the new position {kind} is added alongside"
            ));
        }
        cancel.extend(replaceable);
        create.push((kind, price));
    }
    let cancel_ids: HashSet<u64> = cancel.iter().map(|t| t.order_id).collect();
    let keep = existing
        .into_iter()
        .filter(|t| !cancel_ids.contains(&t.order_id))
        .collect();

    Ok(Plan {
        instrument_id: instrument.instrument_id,
        symbol: instrument.label().to_string(),
        position: Some(PositionSummary {
            side,
            size: position.abs_size().to_string(),
            entry_price: position.entry_price.clone(),
            liquidation_price: position.liquidation_price.clone(),
        }),
        mark_price: mark_price.map(str::to_string),
        cancel,
        create,
        keep,
        notes,
    })
}

/// Plan cancelling TP and/or SL triggers that protect the position.
pub fn plan_cancel(
    instrument: &Instrument,
    position: Option<&PerpsPosition>,
    mark_price: Option<&str>,
    open_orders: &[PerpsOrder],
    kinds: &[TpSlKind],
    keep_bracket: bool,
) -> Plan {
    let existing = existing_triggers(instrument.instrument_id, open_orders);
    let (cancel, keep): (Vec<_>, Vec<_>) = existing.into_iter().partition(|t| {
        kinds.contains(&t.kind) && t.protects_position && (t.scope == "position" || !keep_bracket)
    });
    let mut notes = Vec::new();
    if keep.iter().any(|t| !t.protects_position) {
        notes.push(
            "dormant bracket legs are kept; cancel their entry order to remove them".to_string(),
        );
    }
    Plan {
        instrument_id: instrument.instrument_id,
        symbol: instrument.label().to_string(),
        position: position.and_then(|p| {
            p.side().map(|side| PositionSummary {
                side,
                size: p.abs_size().to_string(),
                entry_price: p.entry_price.clone(),
                liquidation_price: p.liquidation_price.clone(),
            })
        }),
        mark_price: mark_price.map(str::to_string),
        cancel,
        create: Vec::new(),
        keep,
        notes,
    }
}

fn check_direction(
    side: PositionSide,
    kind: TpSlKind,
    price: f64,
    mark: f64,
    mark_text: &str,
) -> Result<()> {
    // Take-profit sits on the profitable side of the mark, stop-loss on the
    // losing side; a trigger already crossed would fire immediately and is
    // rejected by the exchange.
    let above = match (side, kind) {
        (PositionSide::Long, TpSlKind::Tp) | (PositionSide::Short, TpSlKind::Sl) => true,
        (PositionSide::Long, TpSlKind::Sl) | (PositionSide::Short, TpSlKind::Tp) => false,
    };
    let ok = if above { price > mark } else { price < mark };
    if !ok {
        bail!(
            "{kind} for a {} position must be {} the current mark price {mark_text}",
            side.as_str().to_lowercase(),
            if above { "above" } else { "below" },
        );
    }
    Ok(())
}

/// Validate a price against the instrument's precision rules and return its
/// canonical form (no leading/trailing zeros). The returned string is exactly
/// what gets sent and signed.
pub fn validate_price(input: &str, price_decimals: u32) -> Result<String> {
    let canonical = canonical_decimal(input)?;
    if canonical == "0" {
        bail!("price must be greater than zero");
    }
    if let Some((int_part, frac)) = canonical.split_once('.') {
        if frac.len() > price_decimals as usize {
            bail!(
                "{input} has {} decimal places; this market allows at most {price_decimals}",
                frac.len()
            );
        }
        let significant = format!("{int_part}{frac}").trim_start_matches('0').len();
        if significant > 5 {
            bail!(
                "{input} has {significant} significant figures; non-integer prices allow at most 5"
            );
        }
    }
    Ok(canonical)
}

/// `"0070.50"` → `"70.5"`, `"65000.0"` → `"65000"`. Only plain decimals are accepted.
pub fn canonical_decimal(input: &str) -> Result<String> {
    let s = input.trim();
    let (int_part, frac) = s.split_once('.').unwrap_or((s, ""));
    let digits_only = |t: &str| t.chars().all(|c| c.is_ascii_digit());
    if (int_part.is_empty() && frac.is_empty()) || !digits_only(int_part) || !digits_only(frac) {
        bail!("{input:?} is not a plain decimal number");
    }
    let int_part = int_part.trim_start_matches('0');
    let frac = frac.trim_end_matches('0');
    let int_part = if int_part.is_empty() { "0" } else { int_part };
    Ok(if frac.is_empty() {
        int_part.to_string()
    } else {
        format!("{int_part}.{frac}")
    })
}

fn same_price(a: &str, b: &str) -> bool {
    match (canonical_decimal(a), canonical_decimal(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn instrument() -> Instrument {
        serde_json::from_value(json!({
            "instrument_id": 6, "symbol": "BTC-USD", "price_decimals": 1, "quantity_decimals": 5
        }))
        .unwrap()
    }

    fn position(size: &str, liq: &str) -> PerpsPosition {
        serde_json::from_value(json!({
            "instrument_id": 6, "symbol": "BTC-USD", "size": size, "entry_price": "65000",
            "liquidation_price": liq
        }))
        .unwrap()
    }

    fn trigger(oid: u64, kind: &str, scope: &str, trp: &str, parent: Option<u64>) -> PerpsOrder {
        let mut tpsl = json!({"kind": kind, "scope": scope, "trp": trp});
        if let Some(p) = parent {
            tpsl["parent_oid"] = json!(p);
        }
        serde_json::from_value(json!({
            "order_id": oid, "instrument_id": 6, "buy": false, "price": "0", "quantity": "0",
            "status": "open", "tpsl": tpsl
        }))
        .unwrap()
    }

    fn entry(oid: u64) -> PerpsOrder {
        serde_json::from_value(json!({
            "order_id": oid, "instrument_id": 6, "buy": true, "price": "64000", "quantity": "0.01",
            "status": "open"
        }))
        .unwrap()
    }

    fn set(tp: Option<&str>, sl: Option<&str>) -> SetRequest {
        SetRequest {
            take_profit: tp.map(str::to_string),
            stop_loss: sl.map(str::to_string),
            keep_bracket: false,
        }
    }

    #[test]
    fn creates_both_for_unprotected_long() {
        let pos = position("0.02", "50000");
        let plan = plan_set(
            &instrument(),
            Some(&pos),
            Some("66000"),
            &[],
            &set(Some("70000.0"), Some("62000")),
        )
        .unwrap();
        assert!(plan.cancel.is_empty());
        assert_eq!(
            plan.create,
            vec![
                (TpSlKind::Tp, "70000".into()),
                (TpSlKind::Sl, "62000".into())
            ]
        );
        let Some(PerpsOp::CreateOrders { orders, group }) = plan.create_op() else {
            panic!()
        };
        assert_eq!(group, Some(TpSlScope::Position));
        assert!(orders
            .iter()
            .all(|o| !o.buy && o.reduce_only && o.quantity == "0"));
        assert!(plan.cancel_op().is_none());
    }

    #[test]
    fn short_position_closes_with_buy_and_mirrors_direction() {
        let pos = position("-1.5", "80000");
        let plan = plan_set(
            &instrument(),
            Some(&pos),
            Some("66000"),
            &[],
            &set(Some("60000"), Some("70000")),
        )
        .unwrap();
        let Some(PerpsOp::CreateOrders { orders, .. }) = plan.create_op() else {
            panic!()
        };
        assert!(orders.iter().all(|o| o.buy));
        // Long-style prices are wrong for a short.
        assert!(plan_set(
            &instrument(),
            Some(&pos),
            Some("66000"),
            &[],
            &set(Some("70000"), None)
        )
        .is_err());
        assert!(plan_set(
            &instrument(),
            Some(&pos),
            Some("66000"),
            &[],
            &set(None, Some("60000"))
        )
        .is_err());
    }

    #[test]
    fn rejects_triggers_already_crossed_by_mark() {
        let pos = position("1", "50000");
        let err = plan_set(
            &instrument(),
            Some(&pos),
            Some("66000"),
            &[],
            &set(Some("65000"), None),
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("above the current mark price 66000"),
            "{err}"
        );
        let err = plan_set(
            &instrument(),
            Some(&pos),
            Some("66000"),
            &[],
            &set(None, Some("67000")),
        )
        .unwrap_err();
        assert!(err.to_string().contains("below"), "{err}");
    }

    #[test]
    fn modifies_existing_position_trigger_by_cancel_then_create() {
        let pos = position("1", "50000");
        let orders = vec![
            trigger(11, "tp", "position", "69000", None),
            trigger(12, "sl", "position", "61000", None),
        ];
        let plan = plan_set(
            &instrument(),
            Some(&pos),
            Some("66000"),
            &orders,
            &set(Some("71000"), None),
        )
        .unwrap();
        assert_eq!(
            plan.cancel.iter().map(|t| t.order_id).collect::<Vec<_>>(),
            vec![11]
        );
        assert_eq!(plan.create, vec![(TpSlKind::Tp, "71000".into())]);
        assert_eq!(
            plan.keep.iter().map(|t| t.order_id).collect::<Vec<_>>(),
            vec![12]
        );
        assert_eq!(plan.replaced_kinds(), vec![TpSlKind::Tp]);
        assert_eq!(
            plan.cancel_op(),
            Some(PerpsOp::CancelOrders {
                order_ids: vec![11]
            })
        );
    }

    #[test]
    fn unchanged_price_is_a_noop() {
        let pos = position("1", "50000");
        let orders = vec![trigger(11, "tp", "position", "70000.0", None)];
        let plan = plan_set(
            &instrument(),
            Some(&pos),
            Some("66000"),
            &orders,
            &set(Some("70000"), None),
        )
        .unwrap();
        assert!(plan.is_noop());
        assert!(plan.notes.iter().any(|n| n.contains("unchanged")));
    }

    #[test]
    fn armed_bracket_legs_are_replaced_unless_kept_and_dormant_legs_stay() {
        let pos = position("1", "50000");
        let orders = vec![
            trigger(21, "sl", "order", "60000", Some(5)), // armed: entry #5 no longer open
            trigger(22, "sl", "order", "59000", Some(6)), // dormant: entry #6 still resting
            entry(6),
        ];
        let plan = plan_set(
            &instrument(),
            Some(&pos),
            Some("66000"),
            &orders,
            &set(None, Some("63000")),
        )
        .unwrap();
        assert_eq!(
            plan.cancel.iter().map(|t| t.order_id).collect::<Vec<_>>(),
            vec![21]
        );
        assert_eq!(
            plan.keep.iter().map(|t| t.order_id).collect::<Vec<_>>(),
            vec![22]
        );

        let keep = SetRequest {
            keep_bracket: true,
            ..set(None, Some("63000"))
        };
        let plan = plan_set(&instrument(), Some(&pos), Some("66000"), &orders, &keep).unwrap();
        assert!(plan.cancel.is_empty());
        assert_eq!(plan.create.len(), 1);
        assert!(plan
            .notes
            .iter()
            .any(|n| n.contains("keeping armed bracket")));
    }

    #[test]
    fn requires_an_open_position() {
        let flat = position("0", "0");
        assert!(plan_set(
            &instrument(),
            Some(&flat),
            Some("1"),
            &[],
            &set(Some("2"), None)
        )
        .is_err());
        assert!(plan_set(&instrument(), None, Some("1"), &[], &set(Some("2"), None)).is_err());
    }

    #[test]
    fn warns_when_stop_is_past_liquidation() {
        let pos = position("1", "63000");
        let plan = plan_set(
            &instrument(),
            Some(&pos),
            Some("66000"),
            &[],
            &set(None, Some("62000")),
        )
        .unwrap();
        assert!(plan.notes.iter().any(|n| n.contains("liquidation")));
    }

    #[test]
    fn rejects_inconsistent_tp_and_sl_without_mark() {
        let pos = position("1", "0");
        assert!(plan_set(
            &instrument(),
            Some(&pos),
            None,
            &[],
            &set(Some("60000"), Some("62000"))
        )
        .is_err());
        let plan = plan_set(
            &instrument(),
            Some(&pos),
            None,
            &[],
            &set(Some("70000"), Some("62000")),
        )
        .unwrap();
        assert!(plan
            .notes
            .iter()
            .any(|n| n.contains("mark price unavailable")));
    }

    #[test]
    fn cancel_plan_selects_kinds() {
        let pos = position("1", "50000");
        let orders = vec![
            trigger(11, "tp", "position", "69000", None),
            trigger(12, "sl", "position", "61000", None),
            trigger(21, "sl", "order", "60000", Some(5)),
        ];
        let plan = plan_cancel(
            &instrument(),
            Some(&pos),
            None,
            &orders,
            &[TpSlKind::Sl],
            false,
        );
        assert_eq!(
            plan.cancel.iter().map(|t| t.order_id).collect::<Vec<_>>(),
            vec![12, 21]
        );
        assert!(plan.create_op().is_none());
        let plan = plan_cancel(
            &instrument(),
            Some(&pos),
            None,
            &orders,
            &[TpSlKind::Tp, TpSlKind::Sl],
            true,
        );
        assert_eq!(
            plan.cancel.iter().map(|t| t.order_id).collect::<Vec<_>>(),
            vec![11, 12]
        );
    }

    #[test]
    fn price_validation_follows_instrument_rules() {
        assert_eq!(validate_price("70000", 1).unwrap(), "70000");
        assert_eq!(validate_price(" 007000.50 ", 1).unwrap(), "7000.5");
        assert!(
            validate_price("70000.5", 1).is_err(),
            "6 significant figures"
        );
        assert!(
            validate_price("65000.25", 2).is_err(),
            "7 significant figures"
        );
        assert!(validate_price("1.234", 2).is_err(), "too many decimals");
        assert_eq!(validate_price("0.012345", 6).unwrap(), "0.012345");
        assert_eq!(validate_price("1234567", 0).unwrap(), "1234567");
        assert!(validate_price("0", 2).is_err());
        assert!(validate_price("-5", 2).is_err());
        assert!(validate_price("1e5", 2).is_err());
        assert!(validate_price("abc", 2).is_err());
        assert_eq!(canonical_decimal(".5").unwrap(), "0.5");
        assert_eq!(canonical_decimal("5.").unwrap(), "5");
    }
}
