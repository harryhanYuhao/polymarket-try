//! Prediction-market clients: CLOB private orders and the public Data API.

use anyhow::{bail, Context, Result};
use reqwest::Url;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::auth::{
    current_timestamp_seconds, l1_headers, l2_headers, Address, ApiCredentials, SecretKey,
};

const MAX_PAGES: usize = 1_000;

fn body_snippet(bytes: &[u8]) -> String {
    const LIMIT: usize = 500;
    let text = String::from_utf8_lossy(bytes);
    if text.chars().count() <= LIMIT {
        text.into_owned()
    } else {
        let shortened: String = text.chars().take(LIMIT).collect();
        format!("{shortened}…")
    }
}

fn request_path(url: &Url) -> String {
    match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_owned(),
    }
}

async fn read_json<T: DeserializeOwned>(response: reqwest::Response, label: &str) -> Result<T> {
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .with_context(|| format!("failed to read response body from {label}"))?;
    if !status.is_success() {
        bail!("{label} returned HTTP {status}: {}", body_snippet(&bytes));
    }
    serde_json::from_slice(&bytes).with_context(|| format!("unexpected JSON response from {label}"))
}

// ---------------------------------------------------------------------------
// CLOB
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OpenOrder {
    pub id: String,
    pub status: String,
    pub owner: String,
    pub maker_address: String,
    pub market: String,
    pub asset_id: String,
    pub side: String,
    pub original_size: String,
    pub size_matched: String,
    pub price: String,
    pub outcome: String,
    pub expiration: String,
    pub order_type: String,
    pub created_at: i64,
    #[serde(default)]
    pub associate_trades: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct OrdersPage {
    data: Vec<OpenOrder>,
    next_cursor: String,
}

#[derive(Clone)]
pub struct ClobClient {
    http: reqwest::Client,
    base: Url,
    address: Address,
    credentials: ApiCredentials,
}

impl ClobClient {
    pub fn new(
        http: reqwest::Client,
        base_url: &str,
        address: Address,
        credentials: ApiCredentials,
    ) -> Result<Self> {
        let base = Url::parse(base_url).with_context(|| "invalid CLOB base URL")?;
        Ok(Self {
            http,
            base,
            address,
            credentials,
        })
    }

    fn url(&self, path: &str) -> Result<Url> {
        self.base
            .join(path)
            .with_context(|| format!("invalid CLOB URL path {path}"))
    }

    async fn get_signed<T: DeserializeOwned>(&self, url: Url) -> Result<T> {
        let path = request_path(&url);
        let headers = l2_headers(
            self.address,
            &self.credentials,
            current_timestamp_seconds(),
            "GET",
            &path,
            &[],
        )?;
        let mut request = self.http.get(url.clone());
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("GET {url} failed"))?;
        read_json::<T>(response, &format!("GET {url}")).await
    }

    /// Returns every live order for the authenticated account.
    pub async fn open_orders(&self, market: Option<&str>) -> Result<Vec<OpenOrder>> {
        let mut cursor: Option<String> = None;
        let mut orders = Vec::new();
        for _ in 0..MAX_PAGES {
            let mut url = self.url("/data/orders")?;
            {
                let mut query = url.query_pairs_mut();
                if let Some(market) = market {
                    query.append_pair("market", market);
                }
                if let Some(cursor) = cursor.as_deref() {
                    query.append_pair("next_cursor", cursor);
                }
            }
            let page: OrdersPage = self.get_signed(url).await?;
            let next = page.next_cursor.trim().to_owned();
            let previous = cursor.clone();
            orders.extend(page.data);
            if next.is_empty() {
                return Ok(orders);
            }
            if previous.as_deref() == Some(next.as_str()) {
                bail!("CLOB order pagination repeated cursor {next:?}");
            }
            cursor = Some(next);
        }
        bail!("CLOB order pagination exceeded {MAX_PAGES} pages")
    }
}

/// Derives an existing CLOB API key, creating one when the wallet has none.
pub async fn create_or_derive_credentials(
    http: &reqwest::Client,
    base_url: &str,
    signer: &SecretKey,
    chain_id: u64,
) -> Result<ApiCredentials> {
    let base = Url::parse(base_url).with_context(|| "invalid CLOB base URL")?;
    let derive_url = base
        .join("/auth/derive-api-key")
        .context("invalid CLOB credential URL")?;
    let create_url = base
        .join("/auth/api-key")
        .context("invalid CLOB credential URL")?;

    let derive_response = request_with_l1(http.get(derive_url.clone()), signer, chain_id)?
        .send()
        .await
        .with_context(|| format!("GET {derive_url} failed"))?;
    let derive_status = derive_response.status();
    let derive_body = derive_response
        .bytes()
        .await
        .context("failed to read CLOB derive response")?;
    if derive_status.is_success() {
        return serde_json::from_slice(&derive_body)
            .context("unexpected CLOB derive-credentials response");
    }

    // A missing key is reported as 400/404. Authentication failures are not
    // fixed by creating a new key, so surface them directly.
    if derive_status != reqwest::StatusCode::NOT_FOUND
        && derive_status != reqwest::StatusCode::BAD_REQUEST
    {
        bail!(
            "GET {derive_url} returned HTTP {derive_status}: {}",
            body_snippet(&derive_body)
        );
    }

    let create_response = request_with_l1(http.post(create_url.clone()), signer, chain_id)?
        .send()
        .await
        .with_context(|| format!("POST {create_url} failed"))?;
    let create_status = create_response.status();
    let create_body = create_response
        .bytes()
        .await
        .context("failed to read CLOB create response")?;
    if !create_status.is_success() {
        bail!(
            "could not derive or create CLOB API credentials (derive HTTP {derive_status}: {}; \
             create HTTP {create_status}: {})",
            body_snippet(&derive_body),
            body_snippet(&create_body)
        );
    }
    serde_json::from_slice(&create_body).context("unexpected CLOB create-credentials response")
}

fn request_with_l1(
    request: reqwest::RequestBuilder,
    signer: &SecretKey,
    chain_id: u64,
) -> Result<reqwest::RequestBuilder> {
    let headers = l1_headers(signer, chain_id, 0)?;
    let mut request = request;
    for (name, value) in headers {
        request = request.header(name, value);
    }
    Ok(request)
}

// ---------------------------------------------------------------------------
// Data API (public prediction-market positions)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PredictionPosition {
    pub proxy_wallet: String,
    pub token_id: String,
    pub condition_id: String,
    pub current_size: f64,
    pub avg_price: f64,
    pub entry_cost_usdc: f64,
    pub current_price: f64,
    pub current_value: f64,
    pub total_size: f64,
    pub realized_pnl: f64,
    pub unrealized_pnl: f64,
    pub total_pnl: f64,
    pub percent_pnl: f64,
    pub status: String,
    pub redeemable: bool,
    pub mergeable: bool,
    pub negative_risk: bool,
    pub title: String,
    pub slug: String,
    pub outcome: String,
    pub end_date: String,
    pub event_slug: String,
}

#[derive(Debug, Deserialize)]
struct PositionsPage {
    data: Vec<PredictionPosition>,
    pagination: DataPagination,
}

#[derive(Debug, Deserialize)]
struct DataPagination {
    #[serde(default)]
    next_cursor: Option<String>,
}

#[derive(Clone)]
pub struct DataApiClient {
    http: reqwest::Client,
    base: Url,
}

impl DataApiClient {
    pub fn new(http: reqwest::Client, base_url: &str) -> Result<Self> {
        let base = Url::parse(base_url).with_context(|| "invalid Data API base URL")?;
        Ok(Self { http, base })
    }

    pub async fn positions(&self, user: Address, limit: usize) -> Result<Vec<PredictionPosition>> {
        let mut positions = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            if positions.len() >= limit {
                break;
            }
            let mut url = self
                .base
                .join("/v2/positions")
                .context("invalid Data API positions URL")?;
            {
                let page_size = limit.saturating_sub(positions.len()).clamp(1, 500);
                let mut query = url.query_pairs_mut();
                query.append_pair("user", &user.to_string());
                query.append_pair("status", "OPEN");
                query.append_pair("limit", &page_size.to_string());
                if let Some(cursor) = cursor.as_deref() {
                    query.append_pair("cursor", cursor);
                }
            }
            let response = self
                .http
                .get(url.clone())
                .send()
                .await
                .with_context(|| format!("GET {url} failed"))?;
            let page: PositionsPage =
                read_json::<PositionsPage>(response, &format!("GET {url}")).await?;
            positions.extend(page.data);
            let next = page.pagination.next_cursor.unwrap_or_default();
            if next.is_empty() {
                break;
            }
            if cursor.as_deref() == Some(next.as_str()) {
                bail!("Data API position pagination repeated cursor {next:?}");
            }
            cursor = Some(next);
        }
        positions.truncate(limit);
        Ok(positions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_path_includes_query() {
        let url = Url::parse("https://clob.polymarket.com/data/orders?next_cursor=abc%3D").unwrap();
        assert_eq!(request_path(&url), "/data/orders?next_cursor=abc%3D");
    }

    #[test]
    fn request_path_without_query() {
        let url = Url::parse("https://clob.polymarket.com/data/orders").unwrap();
        assert_eq!(request_path(&url), "/data/orders");
    }
}
