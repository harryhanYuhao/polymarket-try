//! Polymarket Perps HTTP API (`api.perpetuals.polymarket.com`).
//!
//! Private reads authenticate with the `polymarket-proxy` / `polymarket-secret`
//! headers; trading operations are additionally signed by the proxy key (see
//! [`signing`]).

pub mod msgpack;
pub mod signing;
pub mod tpsl;
pub mod types;

use crate::config::PerpsCredentials;
use crate::eth::LocalSigner;
use crate::http::{now_millis, read_json};
use alloy_primitives::Address;
use anyhow::{anyhow, bail, Context, Result};
use reqwest::RequestBuilder;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use signing::{random_salt, sign_op, PerpsOp, SignedRequest};
use types::{AccountCredentials, Ack, Instrument, PerpsOrder, Portfolio, Ticker};

pub struct PerpsClient {
    http: reqwest::Client,
    base_url: String,
    chain_id: u64,
}

impl PerpsClient {
    pub fn new(http: reqwest::Client, base_url: impl Into<String>, chain_id: u64) -> Self {
        Self {
            http,
            base_url: base_url.into(),
            chain_id,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    fn authed(request: RequestBuilder, creds: &PerpsCredentials) -> RequestBuilder {
        request
            .header("polymarket-proxy", creds.proxy.to_string())
            .header("polymarket-secret", &creds.secret)
    }

    async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
        creds: Option<&PerpsCredentials>,
    ) -> Result<T> {
        let mut request = self.http.get(self.url(path)).query(query);
        if let Some(creds) = creds {
            request = Self::authed(request, creds);
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("GET {path}"))?;
        read_json("Perps", response).await
    }

    /// Enabled instruments (public).
    pub async fn instruments(&self) -> Result<Vec<Instrument>> {
        self.get("/v1/info/instruments", &[], None).await
    }

    /// Live tickers for all instruments (public).
    pub async fn tickers(&self) -> Result<Vec<Ticker>> {
        self.get("/v1/info/tickers", &[], None).await
    }

    /// Live ticker for one instrument (public).
    pub async fn ticker(&self, instrument_id: u64) -> Result<Option<Ticker>> {
        let tickers: Vec<Ticker> = self
            .get(
                "/v1/info/tickers",
                &[("instrument_id", instrument_id.to_string())],
                None,
            )
            .await?;
        Ok(tickers
            .into_iter()
            .find(|t| t.instrument_id == instrument_id))
    }

    /// Whether `address` has a Perps account (public).
    pub async fn is_registered(&self, address: &Address) -> Result<bool> {
        #[derive(Deserialize)]
        struct Registered {
            registered: bool,
        }
        let r: Registered = self
            .get(
                "/v1/info/registered",
                &[("address", address.to_string())],
                None,
            )
            .await?;
        Ok(r.registered)
    }

    pub async fn portfolio(&self, creds: &PerpsCredentials) -> Result<Portfolio> {
        self.get("/v1/account/portfolio", &[], Some(creds)).await
    }

    /// Resting orders, including TP/SL triggers.
    pub async fn open_orders(
        &self,
        creds: &PerpsCredentials,
        instrument_id: Option<u64>,
    ) -> Result<Vec<PerpsOrder>> {
        let query: Vec<(&str, String)> = instrument_id
            .map(|id| vec![("instrument_id", id.to_string())])
            .unwrap_or_default();
        self.get("/v1/account/open-orders", &query, Some(creds))
            .await
    }

    /// Account address and registered proxy keys.
    pub async fn account_credentials(
        &self,
        creds: &PerpsCredentials,
    ) -> Result<AccountCredentials> {
        self.get("/v1/account/credentials", &[], Some(creds)).await
    }

    /// Sign `op` with the proxy key using a fresh salt and timestamp.
    pub fn sign(&self, op: &PerpsOp, proxy_key: &LocalSigner) -> Result<SignedRequest> {
        sign_op(op, proxy_key, self.chain_id, random_salt(), now_millis())
    }

    /// Send a signed operation. Returns one acknowledgement per order, in
    /// request order; individual orders can still be rejected (`status: "err"`).
    pub async fn submit(
        &self,
        creds: &PerpsCredentials,
        op: &PerpsOp,
        signed: &SignedRequest,
    ) -> Result<Vec<Ack>> {
        let (request, label) = match op {
            PerpsOp::CreateOrders { .. } => (
                self.http.post(self.url("/v1/trade/orders")),
                "POST /v1/trade/orders",
            ),
            PerpsOp::CancelOrders { .. } => (
                self.http.delete(self.url("/v1/trade/orders")),
                "DELETE /v1/trade/orders",
            ),
        };
        let response = Self::authed(request, creds)
            .json(signed)
            .send()
            .await
            .context(label)?;
        let acks: Vec<Ack> = read_json("Perps", response).await?;
        let expected = match op {
            PerpsOp::CreateOrders { orders, .. } => orders.len(),
            PerpsOp::CancelOrders { order_ids } => order_ids.len(),
        };
        if acks.len() != expected {
            bail!(
                "{label}: expected {expected} acknowledgements, got {}",
                acks.len()
            );
        }
        Ok(acks)
    }

    /// Register a proxy signer (`POST /v1/account/proxy`); returns its secret.
    pub async fn create_proxy(&self, body: &serde_json::Value) -> Result<String> {
        #[derive(Deserialize)]
        struct Created {
            secret: String,
        }
        let response = self
            .http
            .post(self.url("/v1/account/proxy"))
            .json(body)
            .send()
            .await
            .context("POST /v1/account/proxy")?;
        let created: Created = read_json("Perps", response).await?;
        Ok(created.secret)
    }
}

/// Find an instrument by numeric ID, symbol (`BTC-USD`), display symbol
/// (`USA500-USD`) or unambiguous base asset (`BTC`), case-insensitively.
pub fn find_instrument<'a>(instruments: &'a [Instrument], query: &str) -> Result<&'a Instrument> {
    let q = query.trim();
    if let Ok(id) = q.parse::<u64>() {
        return instruments
            .iter()
            .find(|i| i.instrument_id == id)
            .ok_or_else(|| anyhow!("no perps instrument with ID {id}"));
    }
    if let Some(found) = instruments.iter().find(|i| {
        i.symbol.eq_ignore_ascii_case(q)
            || i.display_symbol
                .as_deref()
                .is_some_and(|d| d.eq_ignore_ascii_case(q))
    }) {
        return Ok(found);
    }
    let by_base: Vec<&Instrument> = instruments
        .iter()
        .filter(|i| {
            i.base_asset
                .as_deref()
                .is_some_and(|b| b.eq_ignore_ascii_case(q))
                || i.label()
                    .split('-')
                    .next()
                    .is_some_and(|b| b.eq_ignore_ascii_case(q))
        })
        .collect();
    match by_base.as_slice() {
        [one] => Ok(one),
        [] => bail!("unknown perps instrument {q:?}; run `pm perps markets` to list symbols"),
        many => bail!(
            "{q:?} matches several instruments: {}",
            many.iter()
                .map(|i| i.symbol.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn instruments() -> Vec<Instrument> {
        serde_json::from_value(json!([
            {"instrument_id": 1, "symbol": "SP500-USD", "display_symbol": "USA500-USD", "base_asset": "SP500"},
            {"instrument_id": 6, "symbol": "BTC-USD", "base_asset": "BTC"},
            {"instrument_id": 14, "symbol": "SKHYNIX-USD", "base_asset": "SKHYNIX"},
        ]))
        .unwrap()
    }

    #[test]
    fn finds_instruments_by_id_symbol_display_and_base() {
        let all = instruments();
        assert_eq!(find_instrument(&all, "6").unwrap().symbol, "BTC-USD");
        assert_eq!(find_instrument(&all, "btc-usd").unwrap().instrument_id, 6);
        assert_eq!(
            find_instrument(&all, "USA500-USD").unwrap().instrument_id,
            1
        );
        assert_eq!(find_instrument(&all, "btc").unwrap().instrument_id, 6);
        assert_eq!(find_instrument(&all, "usa500").unwrap().instrument_id, 1);
        assert!(find_instrument(&all, "DOGE").is_err());
        assert!(find_instrument(&all, "99").is_err());
    }
}
