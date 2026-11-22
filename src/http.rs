//! Shared HTTP plumbing: client construction and uniform API error handling.

use anyhow::{anyhow, Result};
use reqwest::{Response, StatusCode};
use serde::de::DeserializeOwned;
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Upper bound on pages fetched by any paginated listing, as a guard against
/// a cursor that never terminates.
pub const MAX_PAGES: usize = 200;

pub fn build_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("pm-try/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(20))
        .build()?)
}

/// A non-success response from one of the Polymarket APIs.
#[derive(Debug)]
pub struct ApiError {
    pub api: &'static str,
    pub status: StatusCode,
    pub message: String,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} API returned {}: {}",
            self.api, self.status, self.message
        )
    }
}

impl std::error::Error for ApiError {}

/// Decode a JSON response, turning non-2xx statuses into [`ApiError`] with the
/// server's own message (`{"error": ...}` on every Polymarket API).
pub async fn read_json<T: DeserializeOwned>(api: &'static str, response: Response) -> Result<T> {
    let status = response.status();
    let text = response.text().await?;
    if !status.is_success() {
        return Err(ApiError {
            api,
            status,
            message: error_message(&text),
        }
        .into());
    }
    serde_json::from_str(&text).map_err(|e| {
        anyhow!(
            "{api} API: unexpected response ({e}): {}",
            truncate(&text, 400)
        )
    })
}

fn error_message(body: &str) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
        for key in ["error", "message", "detail"] {
            if let Some(text) = value.get(key).and_then(|v| v.as_str()) {
                return text.to_string();
            }
        }
    }
    let trimmed = body.trim();
    if trimmed.is_empty() {
        "<empty body>".to_string()
    } else {
        truncate(trimmed, 400)
    }
}

pub fn is_not_found(err: &anyhow::Error) -> bool {
    err.downcast_ref::<ApiError>()
        .is_some_and(|api| api.status == StatusCode::NOT_FOUND)
}

pub fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let cut: String = text.chars().take(max_chars).collect();
        format!("{cut}…")
    }
}

pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn now_secs() -> u64 {
    now_millis() / 1000
}

/// Serde helpers for fields that the APIs send as either strings or numbers.
pub mod de {
    use serde::{Deserialize, Deserializer};
    use serde_json::Value;

    /// Accept a string, number or boolean (or null/missing) and keep it as text.
    pub fn opt_string_any<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
        Ok(match Option::<Value>::deserialize(d)? {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s),
            Some(other) => Some(other.to_string()),
        })
    }

    /// Like [`opt_string_any`] but falls back to an empty string.
    pub fn string_any<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
        Ok(opt_string_any(d)?.unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api_error(status: StatusCode) -> anyhow::Error {
        ApiError {
            api: "Gamma",
            status,
            message: "x".into(),
        }
        .into()
    }

    #[test]
    fn is_not_found_matches_only_404_api_errors() {
        assert!(is_not_found(&api_error(StatusCode::NOT_FOUND)));
        assert!(is_not_found(
            &api_error(StatusCode::NOT_FOUND).context("GET /public-profile")
        ));
        assert!(!is_not_found(&api_error(StatusCode::INTERNAL_SERVER_ERROR)));
        assert!(!is_not_found(&anyhow!("not found")));
    }
}
