//! Polymarket Perps HTTP client and signed trading operations.

use anyhow::{bail, Context, Result};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE};
use reqwest::{Method, Url};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::auth::{current_timestamp_millis, Address, SecretKey};
use crate::msgpack::Mp;

#[derive(Clone)]
pub struct PerpsClient {
    http: reqwest::Client,
    base: Url,
    proxy: Option<Address>,
    secret: Option<String>,
    chain_id: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Instrument {
    pub instrument_id: u32,
    pub symbol: String,
    #[serde(default)]
    pub display_symbol: Option<String>,
    pub category: String,
    pub base_asset: String,
    pub quote_asset: String,
    pub quantity_decimals: u32,
    pub price_decimals: u32,
    pub max_leverage: u32,
    pub min_notional: String,
    #[serde(default)]
    pub isolated_only: bool,
    #[serde(default)]
    pub close_only: bool,
}

impl Instrument {
    pub fn label(&self) -> &str {
        self.display_symbol.as_deref().unwrap_or(&self.symbol)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Ticker {
    pub instrument_id: u32,
    pub symbol: String,
    pub mark_price: String,
    pub last_price: String,
    pub index_price: String,
    pub mid_price: String,
    pub funding_rate: String,
    pub next_funding: u64,
    pub open_interest: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Portfolio {
    pub positions: Vec<PerpPosition>,
    pub margin: MarginSummary,
    pub withdrawable: String,
    pub in_liquidation: bool,
    pub fee_tier: u32,
    pub timestamp: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MarginSummary {
    pub total_account_value: String,
    pub available_order_margin: String,
    pub total_initial_margin: String,
    pub total_maintenance_margin: String,
    pub total_position_value: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PerpPosition {
    pub instrument_id: u32,
    pub symbol: String,
    pub size: String,
    pub entry_price: String,
    pub leverage: u32,
    pub cross: bool,
    pub initial_margin: String,
    pub maintenance_margin: String,
    pub position_value: String,
    pub liquidation_price: String,
    pub unrealized_pnl: String,
    pub return_on_equity: String,
    pub cumulative_funding: String,
}

impl PerpPosition {
    pub fn is_long(&self) -> Result<bool> {
        let size: f64 = self
            .size
            .parse()
            .with_context(|| format!("invalid position size {:?}", self.size))?;
        if size == 0.0 {
            bail!("position size is zero");
        }
        Ok(size > 0.0)
    }

    pub fn notional_size(&self) -> f64 {
        self.size.parse::<f64>().map(f64::abs).unwrap_or(0.0)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PerpOrder {
    pub order_id: u64,
    pub instrument_id: u32,
    pub buy: bool,
    pub price: String,
    pub quantity: String,
    pub tif: String,
    pub post_only: bool,
    pub ro: bool,
    pub status: String,
    pub resting_quantity: String,
    pub filled_quantity: String,
    pub created_timestamp: u64,
    pub updated_timestamp: u64,
    #[serde(default)]
    pub client_order_id: Option<String>,
    #[serde(default)]
    pub tpsl: Option<TpSlFields>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TpSlFields {
    pub kind: String,
    pub scope: String,
    pub trp: String,
    #[serde(default)]
    pub parent_oid: Option<u64>,
    #[serde(default)]
    pub armed_qty: Option<String>,
    #[serde(default)]
    pub slip_bps: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OrderAck {
    pub status: String,
    #[serde(default)]
    pub oid: Option<u64>,
    #[serde(default)]
    pub coid: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CancelAck {
    pub status: String,
    #[serde(default)]
    pub oid: Option<u64>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub ts: Option<u64>,
}

impl PerpsClient {
    pub fn new(
        http: reqwest::Client,
        base_url: &str,
        proxy: Address,
        secret: String,
        chain_id: u64,
    ) -> Result<Self> {
        let base = Url::parse(base_url).with_context(|| "invalid Perps base URL")?;
        Ok(Self {
            http,
            base,
            proxy: Some(proxy),
            secret: Some(secret),
            chain_id,
        })
    }

    /// A client for public market-data endpoints (instruments, tickers).
    pub fn public(http: reqwest::Client, base_url: &str, chain_id: u64) -> Result<Self> {
        let base = Url::parse(base_url).with_context(|| "invalid Perps base URL")?;
        Ok(Self {
            http,
            base,
            proxy: None,
            secret: None,
            chain_id,
        })
    }

    fn url(&self, path: &str) -> Result<Url> {
        self.base
            .join(path)
            .with_context(|| format!("invalid Perps URL path {path}"))
    }

    fn auth_headers(&self) -> Result<Option<HeaderMap>> {
        let (Some(proxy), Some(secret)) = (&self.proxy, &self.secret) else {
            return Ok(None);
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("polymarket-proxy"),
            HeaderValue::from_str(&proxy.to_string())
                .context("invalid Perps proxy address header")?,
        );
        headers.insert(
            HeaderName::from_static("polymarket-secret"),
            HeaderValue::from_str(secret).context("invalid Perps secret header")?,
        );
        Ok(Some(headers))
    }

    fn require_auth_headers(&self) -> Result<HeaderMap> {
        self.auth_headers()?.context(
            "Perps credentials are required for this request: set \
             POLYMARKET_PERPS_PROXY and POLYMARKET_PERPS_SECRET",
        )
    }

    async fn get_json<T: DeserializeOwned>(&self, url: Url) -> Result<T> {
        let mut request = self.http.get(url.clone());
        if let Some(headers) = self.auth_headers()? {
            request = request.headers(headers);
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("GET {url} failed"))?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .with_context(|| format!("failed to read response body from {url}"))?;
        if !status.is_success() {
            bail!("GET {url} returned HTTP {status}: {}", body_snippet(&bytes));
        }
        serde_json::from_slice(&bytes)
            .with_context(|| format!("unexpected JSON response from {url}"))
    }

    pub async fn instruments(&self) -> Result<Vec<Instrument>> {
        self.get_json(self.url("/v1/info/instruments")?).await
    }

    pub async fn ticker(&self, instrument_id: u32) -> Result<Ticker> {
        let mut url = self.url("/v1/info/tickers")?;
        url.query_pairs_mut()
            .append_pair("instrument_id", &instrument_id.to_string());
        let tickers: Vec<Ticker> = self.get_json(url).await?;
        tickers
            .into_iter()
            .find(|ticker| ticker.instrument_id == instrument_id)
            .with_context(|| format!("no ticker returned for instrument {instrument_id}"))
    }

    pub async fn portfolio(&self) -> Result<Portfolio> {
        self.get_json(self.url("/v1/account/portfolio")?).await
    }

    pub async fn open_orders(&self, instrument_id: Option<u32>) -> Result<Vec<PerpOrder>> {
        let mut url = self.url("/v1/account/open-orders")?;
        if let Some(id) = instrument_id {
            url.query_pairs_mut()
                .append_pair("instrument_id", &id.to_string());
        }
        self.get_json(url).await
    }

    pub async fn resolve_instrument(&self, input: &str) -> Result<Instrument> {
        let needle = input.trim();
        if needle.is_empty() {
            bail!("instrument must not be empty");
        }
        let instruments = self.instruments().await?;
        if let Ok(id) = needle.parse::<u32>() {
            if let Some(instrument) = instruments
                .iter()
                .find(|instrument| instrument.instrument_id == id)
                .cloned()
            {
                return Ok(instrument);
            }
        }
        let matches = |candidate: &str| instrument_matches(candidate, needle);
        if let Some(instrument) = instruments
            .iter()
            .find(|instrument| {
                instrument_matches(&instrument.symbol, needle)
                    || instrument.display_symbol.as_deref().is_some_and(matches)
            })
            .cloned()
        {
            return Ok(instrument);
        }
        bail!("unknown Perps instrument {needle:?} (try `pml perps instruments`)");
    }

    /// Submits position-scoped TP/SL triggers. The engine sizes each exit to the
    /// full position when its trigger fires.
    pub async fn create_position_tpsl(
        &self,
        signer: &SecretKey,
        instrument_id: u32,
        close_buy: bool,
        take_profit: Option<&str>,
        stop_loss: Option<&str>,
    ) -> Result<Vec<OrderAck>> {
        let (compact, body) =
            build_position_tpsl_operation(instrument_id, close_buy, take_profit, stop_loss)?;
        self.send_signed::<Vec<OrderAck>>(signer, Method::POST, "/v1/trade/orders", &compact, body)
            .await
    }

    pub async fn cancel_orders(
        &self,
        signer: &SecretKey,
        order_ids: &[u64],
    ) -> Result<Vec<CancelAck>> {
        let (compact, body) = build_cancel_operation(order_ids)?;
        self.send_signed::<Vec<CancelAck>>(
            signer,
            Method::DELETE,
            "/v1/trade/orders",
            &compact,
            body,
        )
        .await
    }

    async fn send_signed<T: DeserializeOwned>(
        &self,
        signer: &SecretKey,
        method: Method,
        path: &str,
        compact: &Mp,
        operation: Value,
    ) -> Result<T> {
        let salt = rand::random::<u32>();
        let timestamp = current_timestamp_millis();
        let signature = signer.sign_perps_op(self.chain_id, compact, salt, timestamp)?;
        let body = serde_json::to_vec(&json!({
            "op": operation,
            "sig": signature,
            "salt": salt,
            "ts": timestamp,
        }))
        .context("failed to serialize signed Perps operation")?;

        let url = self.url(path)?;
        let response = self
            .http
            .request(method.clone(), url.clone())
            .headers(self.require_auth_headers()?)
            .header(CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .with_context(|| format!("{method} {url} failed"))?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .with_context(|| format!("failed to read response body from {url}"))?;
        if !status.is_success() {
            bail!(
                "{method} {url} returned HTTP {status}: {}",
                body_snippet(&bytes)
            );
        }
        serde_json::from_slice(&bytes)
            .with_context(|| format!("unexpected JSON response from {method} {url}"))
    }
}

/// Builds the compact (signing) and structured (wire) forms of a position TP/SL
/// operation. Both must describe exactly the same orders.
pub fn build_position_tpsl_operation(
    instrument_id: u32,
    close_buy: bool,
    take_profit: Option<&str>,
    stop_loss: Option<&str>,
) -> Result<(Mp, Value)> {
    if take_profit.is_none() && stop_loss.is_none() {
        bail!("at least one of take-profit or stop-loss is required");
    }

    let mut compact_orders = Vec::new();
    let mut json_orders = Vec::new();
    for (kind, price) in [("tp", take_profit), ("sl", stop_loss)] {
        let Some(price) = price else { continue };
        validate_price_text(price)?;
        compact_orders.push(Mp::Arr(vec![
            Mp::Int(u64::from(instrument_id)),
            Mp::Bool(close_buy),
            Mp::Str("0".to_owned()),
            Mp::Bool(false),
            Mp::Bool(true),
            Mp::Arr(vec![
                Mp::Bool(true),
                Mp::Str(price.to_owned()),
                Mp::Str(kind.to_owned()),
            ]),
        ]));
        json_orders.push(json!({
            "iid": instrument_id,
            "buy": close_buy,
            "qty": "0",
            "po": false,
            "ro": true,
            "tr": {
                "market": true,
                "trp": price,
                "tpsl": kind,
            },
        }));
    }

    let compact = Mp::Arr(vec![
        Mp::Str("createOrders".to_owned()),
        Mp::Arr(compact_orders),
        Mp::Str("position".to_owned()),
    ]);
    let body = json!({
        "type": "createOrders",
        "grp": "position",
        "args": json_orders,
    });
    Ok((compact, body))
}

pub fn build_cancel_operation(order_ids: &[u64]) -> Result<(Mp, Value)> {
    if order_ids.is_empty() {
        bail!("at least one order ID is required");
    }
    let compact = Mp::Arr(vec![
        Mp::Str("cancelOrders".to_owned()),
        Mp::Arr(order_ids.iter().map(|id| Mp::Int(*id)).collect()),
    ]);
    let body = json!({
        "type": "cancelOrders",
        "args": order_ids,
    });
    Ok((compact, body))
}

/// Accepts the exchange symbol (`BTC-USD`) and the documentation-style
/// perpetual suffix (`BTC-PERP`) for convenience.
fn instrument_matches(candidate: &str, needle: &str) -> bool {
    if candidate.eq_ignore_ascii_case(needle) {
        return true;
    }
    if let Some(base) = needle
        .strip_suffix("-PERP")
        .or_else(|| needle.strip_suffix("-perp"))
    {
        return candidate.eq_ignore_ascii_case(&format!("{base}-USD"));
    }
    false
}

fn validate_price_text(price: &str) -> Result<()> {
    let trimmed = price.trim();
    if trimmed.is_empty() || trimmed != price {
        bail!("price {price:?} must be a non-empty decimal string without whitespace");
    }
    if trimmed.starts_with('-') || trimmed.starts_with('+') {
        bail!("price {price:?} must be positive");
    }
    if trimmed.contains('e') || trimmed.contains('E') {
        bail!("price {price:?} must not use exponent notation");
    }
    let value: f64 = trimmed
        .parse()
        .with_context(|| format!("price {price:?} is not a decimal number"))?;
    if !value.is_finite() || value <= 0.0 {
        bail!("price {price:?} must be positive");
    }
    Ok(())
}

fn body_snippet(bytes: &[u8]) -> String {
    const LIMIT: usize = 500;
    let text = String::from_utf8_lossy(bytes);
    if text.len() <= LIMIT {
        text.into_owned()
    } else {
        format!("{}…", &text[..LIMIT])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_tpsl_compact_matches_sdk_shape() {
        let (compact, body) =
            build_position_tpsl_operation(1, false, Some("70000"), Some("62000")).unwrap();
        let expected = Mp::Arr(vec![
            Mp::Str("createOrders".to_owned()),
            Mp::Arr(vec![
                Mp::Arr(vec![
                    Mp::Int(1),
                    Mp::Bool(false),
                    Mp::Str("0".to_owned()),
                    Mp::Bool(false),
                    Mp::Bool(true),
                    Mp::Arr(vec![
                        Mp::Bool(true),
                        Mp::Str("70000".to_owned()),
                        Mp::Str("tp".to_owned()),
                    ]),
                ]),
                Mp::Arr(vec![
                    Mp::Int(1),
                    Mp::Bool(false),
                    Mp::Str("0".to_owned()),
                    Mp::Bool(false),
                    Mp::Bool(true),
                    Mp::Arr(vec![
                        Mp::Bool(true),
                        Mp::Str("62000".to_owned()),
                        Mp::Str("sl".to_owned()),
                    ]),
                ]),
            ]),
            Mp::Str("position".to_owned()),
        ]);
        assert_eq!(compact, expected);
        assert_eq!(body["grp"], "position");
        assert_eq!(body["args"][0]["tr"]["tpsl"], "tp");
        assert_eq!(body["args"][1]["tr"]["trp"], "62000");
        assert_eq!(body["args"][0]["ro"], true);
        assert_eq!(body["args"][0]["qty"], "0");
    }

    #[test]
    fn cancel_compact_shape() {
        let (compact, body) = build_cancel_operation(&[9, 10]).unwrap();
        assert_eq!(
            compact,
            Mp::Arr(vec![
                Mp::Str("cancelOrders".to_owned()),
                Mp::Arr(vec![Mp::Int(9), Mp::Int(10)]),
            ])
        );
        assert_eq!(body["args"][1], 10);
    }

    #[test]
    fn rejects_bad_prices() {
        assert!(build_position_tpsl_operation(1, false, Some("1e3"), None).is_err());
        assert!(build_position_tpsl_operation(1, false, Some("-1"), None).is_err());
        assert!(build_position_tpsl_operation(1, false, Some("0"), None).is_err());
    }
}
