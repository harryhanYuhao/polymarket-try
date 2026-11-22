//! Perps API response types. Unknown fields are kept in `extra` so `--json`
//! output stays faithful to what the API returned.

use crate::http::de;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Instrument {
    pub instrument_id: u64,
    pub symbol: String,
    #[serde(default)]
    pub display_symbol: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub base_asset: Option<String>,
    #[serde(default)]
    pub price_decimals: u32,
    #[serde(default)]
    pub quantity_decimals: u32,
    #[serde(default)]
    pub max_leverage: Option<u32>,
    #[serde(default)]
    pub close_only: Option<bool>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Instrument {
    /// Symbol shown in UIs (`display_symbol` when the API provides one).
    pub fn label(&self) -> &str {
        self.display_symbol.as_deref().unwrap_or(&self.symbol)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ticker {
    pub instrument_id: u64,
    #[serde(default)]
    pub symbol: String,
    #[serde(default, deserialize_with = "de::string_any")]
    pub mark_price: String,
    #[serde(default, deserialize_with = "de::string_any")]
    pub index_price: String,
    #[serde(default, deserialize_with = "de::string_any")]
    pub last_price: String,
    #[serde(default, deserialize_with = "de::string_any")]
    pub funding_rate: String,
    #[serde(default, deserialize_with = "de::string_any")]
    pub open_interest: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Portfolio {
    #[serde(default)]
    pub positions: Vec<PerpsPosition>,
    #[serde(default)]
    pub margin: Option<Margin>,
    #[serde(default, deserialize_with = "de::opt_string_any")]
    pub withdrawable: Option<String>,
    #[serde(default)]
    pub in_liquidation: bool,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Margin {
    #[serde(default, deserialize_with = "de::string_any")]
    pub total_account_value: String,
    #[serde(default, deserialize_with = "de::string_any")]
    pub available_order_margin: String,
    #[serde(default, deserialize_with = "de::string_any")]
    pub total_initial_margin: String,
    #[serde(default, deserialize_with = "de::string_any")]
    pub total_maintenance_margin: String,
    #[serde(default, deserialize_with = "de::string_any")]
    pub total_position_value: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerpsPosition {
    pub instrument_id: u64,
    #[serde(default)]
    pub symbol: String,
    /// Signed size: positive is long, negative is short.
    #[serde(deserialize_with = "de::string_any")]
    pub size: String,
    #[serde(default, deserialize_with = "de::string_any")]
    pub entry_price: String,
    #[serde(default)]
    pub leverage: Option<u32>,
    #[serde(default)]
    pub cross: Option<bool>,
    #[serde(default, deserialize_with = "de::string_any")]
    pub liquidation_price: String,
    #[serde(default, deserialize_with = "de::string_any")]
    pub unrealized_pnl: String,
    #[serde(default, deserialize_with = "de::string_any")]
    pub return_on_equity: String,
    #[serde(default, deserialize_with = "de::string_any")]
    pub position_value: String,
    #[serde(default, deserialize_with = "de::string_any")]
    pub cumulative_funding: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PositionSide {
    Long,
    Short,
}

impl PositionSide {
    pub fn as_str(self) -> &'static str {
        match self {
            PositionSide::Long => "LONG",
            PositionSide::Short => "SHORT",
        }
    }

    /// Order side that reduces this position: sell a long, buy back a short.
    pub fn closing_buy(self) -> bool {
        matches!(self, PositionSide::Short)
    }
}

impl PerpsPosition {
    pub fn side(&self) -> Option<PositionSide> {
        let size: f64 = self.size.parse().ok()?;
        if size > 0.0 {
            Some(PositionSide::Long)
        } else if size < 0.0 {
            Some(PositionSide::Short)
        } else {
            None
        }
    }

    pub fn abs_size(&self) -> &str {
        self.size.trim_start_matches(['-', '+'])
    }
}

/// TP/SL details on an open order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TpSlFields {
    /// `tp` or `sl`.
    pub kind: String,
    /// `order` (bracket child) or `position` (attached to the position).
    #[serde(default)]
    pub scope: String,
    #[serde(default, deserialize_with = "de::string_any")]
    pub trp: String,
    #[serde(default)]
    pub parent_oid: Option<u64>,
    #[serde(default, deserialize_with = "de::opt_string_any")]
    pub armed_qty: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// An order from `GET /v1/account/open-orders`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerpsOrder {
    pub order_id: u64,
    pub instrument_id: u64,
    pub buy: bool,
    #[serde(default, deserialize_with = "de::string_any")]
    pub price: String,
    #[serde(default, deserialize_with = "de::string_any")]
    pub quantity: String,
    #[serde(default)]
    pub tif: Option<String>,
    #[serde(default)]
    pub post_only: bool,
    #[serde(default)]
    pub ro: bool,
    #[serde(default)]
    pub status: String,
    #[serde(default, deserialize_with = "de::string_any")]
    pub resting_quantity: String,
    #[serde(default, deserialize_with = "de::string_any")]
    pub filled_quantity: String,
    #[serde(default)]
    pub created_timestamp: Option<u64>,
    #[serde(default)]
    pub client_order_id: Option<String>,
    #[serde(default)]
    pub tpsl: Option<TpSlFields>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// Per-order acknowledgement of `POST /v1/trade/orders` or
/// `DELETE /v1/trade/orders`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ack {
    pub status: String,
    #[serde(default)]
    pub oid: Option<u64>,
    #[serde(default)]
    pub coid: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Ack {
    pub fn is_ok(&self) -> bool {
        self.status == "ok"
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyKey {
    pub proxy: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub expiry: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountCredentials {
    pub address: String,
    #[serde(default)]
    pub keys: Vec<ProxyKey>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tpsl_order_and_position() {
        let order: PerpsOrder = serde_json::from_str(
            r#"{"order_id":1234567891,"instrument_id":6,"buy":false,"price":"0","quantity":"0",
                "tif":"gtc","post_only":false,"ro":true,"status":"armed","resting_quantity":"0",
                "filled_quantity":"0","created_timestamp":1767000010000,"updated_timestamp":1767000010000,
                "tpsl":{"kind":"sl","scope":"position","trp":"62000","armed_qty":"0","slip_bps":0}}"#,
        )
        .unwrap();
        let tpsl = order.tpsl.unwrap();
        assert_eq!(
            (tpsl.kind.as_str(), tpsl.scope.as_str(), tpsl.trp.as_str()),
            ("sl", "position", "62000")
        );
        assert!(order.extra.contains_key("updated_timestamp"));

        let pos: PerpsPosition = serde_json::from_str(
            r#"{"instrument_id":6,"symbol":"BTC-USD","size":"-0.5","entry_price":"65000","leverage":5,
                "cross":false,"initial_margin":"1","maintenance_margin":"1","position_value":"1",
                "liquidation_price":"70000","unrealized_pnl":"0","return_on_equity":"0","cumulative_funding":"0"}"#,
        )
        .unwrap();
        assert_eq!(pos.side(), Some(PositionSide::Short));
        assert!(pos.side().unwrap().closing_buy());
        assert_eq!(pos.abs_size(), "0.5");
    }
}
