//! Gamma API (`gamma-api.polymarket.com`): prediction-market metadata.
//! ProxyWallet address is also found by gamma api

use crate::eth::parse_address;
use crate::http::{de, is_not_found, read_json};
use alloy_primitives::Address;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Max identifiers per Gamma lookup request.
const MAX_IDS_PER_REQUEST: usize = 100;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GammaMarket {
    #[serde(default, deserialize_with = "de::opt_string_any")]
    pub id: Option<String>,
    #[serde(default)]
    pub question: Option<String>,
    #[serde(default, rename = "conditionId")]
    pub condition_id: Option<String>,
    #[serde(default)]
    pub slug: Option<String>,
    /// JSON-encoded list, e.g. `["Yes", "No"]`.
    #[serde(default)]
    pub outcomes: Option<String>,
    /// JSON-encoded list of prices aligned with `outcomes`.
    #[serde(default, rename = "outcomePrices")]
    pub outcome_prices: Option<String>,
    #[serde(default)]
    pub active: Option<bool>,
    #[serde(default)]
    pub closed: Option<bool>,
    #[serde(default, rename = "endDate")]
    pub end_date: Option<String>,
    #[serde(
        default,
        rename = "volume24hr",
        deserialize_with = "de::opt_string_any"
    )]
    pub volume_24hr: Option<String>,
    #[serde(default, deserialize_with = "de::opt_string_any")]
    pub liquidity: Option<String>,
}

impl GammaMarket {
    /// `Yes 0.73 · No 0.27` from the JSON-encoded outcome lists.
    pub fn outcome_summary(&self) -> String {
        let parse = |s: &Option<String>| -> Vec<String> {
            s.as_deref()
                .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
                .unwrap_or_default()
        };
        let names = parse(&self.outcomes);
        let prices = parse(&self.outcome_prices);
        names
            .iter()
            .enumerate()
            .map(|(i, name)| match prices.get(i) {
                Some(p) => format!("{name} {p}"),
                None => name.clone(),
            })
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    events: Option<Vec<SearchEvent>>,
}

#[derive(Debug, Deserialize)]
struct SearchEvent {
    #[serde(default)]
    markets: Option<Vec<GammaMarket>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ProxyWalletAddress {
    #[serde(rename = "proxyWallet")]
    proxy_wallet: Option<String>,
}

pub struct GammaClient {
    http: reqwest::Client,
    base_url: String,
}

impl GammaClient {
    pub fn new(http: reqwest::Client, base_url: impl Into<String>) -> Self {
        Self {
            http,
            base_url: base_url.into(),
        }
    }

    /// Markets by condition ID, open or closed.
    pub async fn markets_by_condition_ids(&self, ids: &[String]) -> Result<Vec<GammaMarket>> {
        let mut markets = Vec::new();
        for chunk in ids.chunks(MAX_IDS_PER_REQUEST) {
            let mut query: Vec<(&str, &str)> = vec![("closed", "any")];
            query.extend(chunk.iter().map(|id| ("condition_ids", id.as_str())));
            let response = self
                .http
                .get(format!("{}/markets", self.base_url))
                .query(&query)
                .send()
                .await
                .context("GET /markets")?;
            markets.extend(read_json::<Vec<GammaMarket>>("Gamma", response).await?);
        }
        Ok(markets)
    }

    /// Open markets ordered by 24h volume.
    pub async fn top_markets(&self, limit: usize) -> Result<Vec<GammaMarket>> {
        let response = self
            .http
            .get(format!("{}/markets", self.base_url))
            .query(&[
                ("closed", "false"),
                ("order", "volume24hr"),
                ("ascending", "false"),
                ("limit", &limit.to_string()),
            ])
            .send()
            .await
            .context("GET /markets")?;
        read_json("Gamma", response).await
    }

    /// Full-text search; returns the markets of matching events.
    pub async fn search_markets(&self, text: &str, limit: usize) -> Result<Vec<GammaMarket>> {
        let response = self
            .http
            .get(format!("{}/public-search", self.base_url))
            .query(&[("q", text), ("limit_per_type", &limit.to_string())])
            .send()
            .await
            .context("GET /public-search")?;
        let found: SearchResponse = read_json("Gamma", response).await?;
        let mut markets: Vec<GammaMarket> = found
            .events
            .unwrap_or_default()
            .into_iter()
            .flat_map(|e| e.markets.unwrap_or_default())
            .filter(|m| m.closed != Some(true))
            .collect();
        markets.truncate(limit);
        Ok(markets)
    }

    pub async fn proxy_wallet(&self, owner: Address) -> Result<Option<Address>> {
        let response = self
            .http
            .get(format!("{}/public-profile", self.base_url))
            .query(&[("address", owner.to_string())])
            .send()
            .await
            .context("GET /public-profile")?;

        let address: ProxyWalletAddress = match read_json("Gamma", response).await {
            Ok(p) => p,
            Err(e) => {
                if is_not_found(&e) {
                    return Ok(None);
                } else {
                    return Err(e);
                }
            }
        };

        if let Some(raw) = address.proxy_wallet.filter(|s| !s.trim().is_empty()) {
            let wallet = parse_address(&raw).context("Gamma public-profile: proxyWallet")?;
            if wallet.is_zero() {
                return Ok(None);
            }
            Ok(Some(wallet))
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_summary_pairs_names_and_prices() {
        let m: GammaMarket = serde_json::from_str(
            r#"{"id":"1","question":"Q?","conditionId":"0xc","outcomes":"[\"Yes\", \"No\"]",
                "outcomePrices":"[\"0.73\", \"0.27\"]","volume24hr":1234.5}"#,
        )
        .unwrap();
        assert_eq!(m.outcome_summary(), "Yes 0.73 · No 0.27");
        assert_eq!(m.volume_24hr.as_deref(), Some("1234.5"));
    }

    mod proxy_wallet {
        use super::*;
        use alloy_primitives::address;
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        const OWNER: Address = address!("92231F8A8F2dd481e1c58E535f8b9C37E323524b");
        const WALLET: Address = address!("bc7afbcd28f5e5e95156f9c8e9e48da5212e07de");

        /// Look up `OWNER` against a Gamma mock answering `status` with `body`.
        async fn lookup(status: u16, body: serde_json::Value) -> Result<Option<Address>> {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/public-profile"))
                .and(query_param("address", OWNER.to_string()))
                .respond_with(ResponseTemplate::new(status).set_body_json(body))
                .expect(1)
                .mount(&server)
                .await;
            GammaClient::new(reqwest::Client::new(), server.uri())
                .proxy_wallet(OWNER)
                .await
        }

        #[tokio::test]
        async fn returns_the_profile_wallet() {
            let found = lookup(
                200,
                serde_json::json!({"proxyWallet": WALLET.to_string().to_lowercase(), "name": "x"}),
            )
            .await
            .unwrap();
            assert_eq!(found, Some(WALLET));
        }

        #[tokio::test]
        async fn unknown_profile_is_none() {
            let found = lookup(
                404,
                serde_json::json!({"type": "not found error", "error": "profile not found"}),
            )
            .await
            .unwrap();
            assert_eq!(found, None);
        }

        #[tokio::test]
        async fn missing_or_null_wallet_is_none() {
            assert_eq!(lookup(200, serde_json::json!({})).await.unwrap(), None);
            assert_eq!(
                lookup(200, serde_json::json!({"proxyWallet": null}))
                    .await
                    .unwrap(),
                None
            );
        }

        #[tokio::test]
        async fn empty_wallet_is_none() {
            let found = lookup(200, serde_json::json!({"proxyWallet": ""})).await;
            assert_eq!(found.unwrap(), None);
        }

        #[tokio::test]
        async fn zero_wallet_is_none() {
            let found = lookup(
                200,
                serde_json::json!({"proxyWallet": Address::ZERO.to_string()}),
            )
            .await
            .unwrap();
            assert_eq!(found, None);
        }

        #[tokio::test]
        async fn malformed_wallet_is_an_error() {
            let err = lookup(200, serde_json::json!({"proxyWallet": "nonsense"}))
                .await
                .unwrap_err();
            assert!(format!("{err:#}").contains("proxyWallet"), "{err:#}");
        }

        #[tokio::test]
        async fn server_error_is_an_error() {
            let err = lookup(500, serde_json::json!({"error": "boom"}))
                .await
                .unwrap_err();
            assert!(!crate::http::is_not_found(&err));
            assert!(format!("{err:#}").contains("boom"), "{err:#}");
        }
    }
}
