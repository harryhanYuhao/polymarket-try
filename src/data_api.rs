//! Data API v2 (`data-api.polymarket.com/v2`): prediction-market positions.

use crate::http::{read_json, MAX_PAGES};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One row of `GET /v2/positions`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PredictionPosition {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub outcome: String,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub condition_id: String,
    #[serde(default)]
    pub token_id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub current_size: f64,
    #[serde(default)]
    pub avg_price: f64,
    #[serde(default)]
    pub current_price: f64,
    #[serde(default)]
    pub current_value: f64,
    #[serde(default)]
    pub unrealized_pnl: f64,
    #[serde(default)]
    pub realized_pnl: f64,
    #[serde(default)]
    pub percent_pnl: f64,
    #[serde(default)]
    pub redeemable: bool,
    #[serde(default)]
    pub end_date: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Deserialize)]
struct Page<T> {
    #[serde(default = "Vec::new")]
    data: Vec<T>,
    #[serde(default)]
    pagination: Option<Pagination>,
}

#[derive(Debug, Deserialize)]
struct Pagination {
    #[serde(default)]
    next_cursor: Option<String>,
}

pub struct DataApiClient {
    http: reqwest::Client,
    base_url: String,
}

impl DataApiClient {
    pub fn new(http: reqwest::Client, base_url: impl Into<String>) -> Self {
        Self {
            http,
            base_url: base_url.into(),
        }
    }

    /// All positions of `user` with the given lifecycle `status`
    /// (`OPEN`, `REDEEMABLE`, `REDEEMABLE_LOST`, `MERGEABLE`, `CLOSED`).
    pub async fn positions(&self, user: &str, status: &str) -> Result<Vec<PredictionPosition>> {
        let url = format!("{}/v2/positions", self.base_url);
        let mut rows = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            // A cursor binds the status/sort of the first page; follow-up
            // pages send only the anchor and the cursor.
            let query: Vec<(&str, &str)> = match &cursor {
                None => vec![("user", user), ("status", status), ("limit", "100")],
                Some(c) => vec![("user", user), ("cursor", c.as_str())],
            };
            let response = self
                .http
                .get(&url)
                .query(&query)
                .send()
                .await
                .context("GET /v2/positions")?;
            let page: Page<PredictionPosition> = read_json("Data", response).await?;
            rows.extend(page.data);
            match page.pagination.and_then(|p| p.next_cursor) {
                Some(next) if !next.is_empty() && cursor.as_deref() != Some(next.as_str()) => {
                    cursor = Some(next);
                }
                _ => break,
            }
        }
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_v2_positions_page() {
        let page: Page<PredictionPosition> = serde_json::from_str(
            r#"{"data":[{"proxy_wallet":"0x1","token_id":"1","condition_id":"0xc",
              "current_size":10.0,"avg_price":0.5,"current_price":0.7,"current_value":7.0,
              "realized_pnl":0.0,"unrealized_pnl":2.0,"percent_pnl":40.0,"status":"OPEN",
              "redeemable":false,"title":"Will it rain?","outcome":"Yes","end_date":"2026-12-31"}],
              "pagination":{"has_more":false,"limit":100,"next_cursor":null,"offset":0}}"#,
        )
        .unwrap();
        assert_eq!(page.data.len(), 1);
        assert_eq!(page.data[0].title, "Will it rain?");
        assert!(page.pagination.unwrap().next_cursor.is_none());
    }
}
