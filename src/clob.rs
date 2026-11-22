//! Prediction-market CLOB API (`clob.polymarket.com`).
//!
//! * L1 auth: an EIP-712 `ClobAuth` signature by the account's EOA, used to
//!   create or derive API credentials.
//! * L2 auth: HMAC-SHA256 over `timestamp + METHOD + path [+ body]` with the
//!   API secret, used for private reads such as open orders.

use crate::config::ClobCredentials;
use crate::eth::{domain_separator, typed_data_digest, word_address, word_u64, LocalSigner};
use crate::http::{de, now_secs, read_json, MAX_PAGES};
use alloy_primitives::{keccak256, Address, B256};
use anyhow::{Context, Result};
use base64::engine::general_purpose::URL_SAFE;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use base64::{alphabet, Engine as _};
use hmac::{Hmac, Mac};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::Sha256;

const CLOB_AUTH_DOMAIN: &str = "ClobAuthDomain";
const CLOB_AUTH_MESSAGE: &str = "This message attests that I control the given wallet";
/// `next_cursor` value the CLOB returns on the last page (base64 of "-1").
const END_CURSOR: &str = "LTE=";

/// A live order as returned by `GET /data/orders` or `GET /data/wallet-open-orders`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClobOrder {
    pub id: String,
    #[serde(default)]
    pub status: String,
    /// Market condition ID.
    #[serde(default)]
    pub market: String,
    /// Outcome token ID.
    #[serde(default)]
    pub asset_id: String,
    #[serde(default)]
    pub side: String,
    #[serde(default, deserialize_with = "de::string_any")]
    pub price: String,
    #[serde(default, deserialize_with = "de::string_any")]
    pub original_size: String,
    #[serde(default, deserialize_with = "de::string_any")]
    pub size_matched: String,
    #[serde(default)]
    pub outcome: Option<String>,
    #[serde(default)]
    pub order_type: Option<String>,
    #[serde(default, deserialize_with = "de::opt_string_any")]
    pub expiration: Option<String>,
    #[serde(default, deserialize_with = "de::opt_string_any")]
    pub created_at: Option<String>,
    #[serde(default)]
    pub maker_address: Option<String>,
    /// Market question, filled in from the Gamma API when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Deserialize)]
struct OrdersPage {
    #[serde(default)]
    data: Vec<ClobOrder>,
    #[serde(default)]
    next_cursor: Option<String>,
}

/// CLOB API credentials returned by `/auth/api-key` and `/auth/derive-api-key`.
#[derive(Clone, Serialize, Deserialize)]
pub struct ApiKeyCredentials {
    #[serde(rename = "apiKey")]
    pub api_key: String,
    pub secret: String,
    pub passphrase: String,
}

#[derive(Debug, Default, Clone)]
pub struct OrderFilter {
    /// Condition ID.
    pub market: Option<String>,
    /// Outcome token ID (the CLOB requires `market` alongside it).
    pub asset_id: Option<String>,
}

pub struct ClobClient {
    http: reqwest::Client,
    base_url: String,
    chain_id: u64,
}

impl ClobClient {
    pub fn new(http: reqwest::Client, base_url: impl Into<String>, chain_id: u64) -> Self {
        Self {
            http,
            base_url: base_url.into(),
            chain_id,
        }
    }

    /// Live orders of the API key's account (`GET /data/orders`), all pages.
    pub async fn open_orders(
        &self,
        creds: &ClobCredentials,
        filter: &OrderFilter,
    ) -> Result<Vec<ClobOrder>> {
        self.paginate(creds, "/data/orders", "next_cursor", filter)
            .await
    }

    /// Every live order on the caller's deposit wallet regardless of which
    /// signer or API key placed it (`GET /data/wallet-open-orders`).
    pub async fn wallet_open_orders(
        &self,
        creds: &ClobCredentials,
        filter: &OrderFilter,
    ) -> Result<Vec<ClobOrder>> {
        self.paginate(creds, "/data/wallet-open-orders", "cursor", filter)
            .await
    }

    async fn paginate(
        &self,
        creds: &ClobCredentials,
        path: &str,
        cursor_param: &str,
        filter: &OrderFilter,
    ) -> Result<Vec<ClobOrder>> {
        let mut orders = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let mut query: Vec<(&str, String)> = Vec::new();
            if let Some(market) = &filter.market {
                query.push(("market", market.clone()));
            }
            if let Some(asset) = &filter.asset_id {
                query.push(("asset_id", asset.clone()));
            }
            if let Some(c) = &cursor {
                query.push((cursor_param, c.clone()));
            }
            // Query parameters are not part of the signed path.
            let headers = l2_headers(creds, now_secs(), "GET", path, None)?;
            let response = self
                .http
                .get(format!("{}{}", self.base_url, path))
                .query(&query)
                .headers(headers)
                .send()
                .await
                .with_context(|| format!("GET {path}"))?;
            let page: OrdersPage = read_json("CLOB", response).await?;
            orders.extend(page.data);
            match page.next_cursor {
                Some(next)
                    if !next.is_empty()
                        && next != END_CURSOR
                        && cursor.as_deref() != Some(next.as_str()) =>
                {
                    cursor = Some(next);
                }
                _ => break,
            }
        }
        Ok(orders)
    }

    /// Derive the existing API credentials for `nonce` (L1-authenticated).
    pub async fn derive_api_key(
        &self,
        signer: &LocalSigner,
        nonce: u64,
    ) -> Result<ApiKeyCredentials> {
        let headers = l1_headers(signer, self.chain_id, now_secs(), nonce)?;
        let response = self
            .http
            .get(format!("{}/auth/derive-api-key", self.base_url))
            .headers(headers)
            .send()
            .await?;
        read_json("CLOB", response).await
    }

    /// Create new API credentials for `nonce` (L1-authenticated).
    pub async fn create_api_key(
        &self,
        signer: &LocalSigner,
        nonce: u64,
    ) -> Result<ApiKeyCredentials> {
        let headers = l1_headers(signer, self.chain_id, now_secs(), nonce)?;
        let response = self
            .http
            .post(format!("{}/auth/api-key", self.base_url))
            .headers(headers)
            .send()
            .await?;
        read_json("CLOB", response).await
    }
}

/// EIP-712 digest of
/// `ClobAuth(address address,string timestamp,uint256 nonce,string message)`
/// in the `ClobAuthDomain` domain.
pub fn clob_auth_digest(address: &Address, timestamp: u64, nonce: u64, chain_id: u64) -> B256 {
    let type_hash =
        keccak256(b"ClobAuth(address address,string timestamp,uint256 nonce,string message)");
    let mut encoded = Vec::with_capacity(32 * 5);
    encoded.extend_from_slice(type_hash.as_slice());
    encoded.extend_from_slice(&word_address(address));
    encoded.extend_from_slice(keccak256(timestamp.to_string().as_bytes()).as_slice());
    encoded.extend_from_slice(&word_u64(nonce));
    encoded.extend_from_slice(keccak256(CLOB_AUTH_MESSAGE.as_bytes()).as_slice());
    typed_data_digest(
        &domain_separator(CLOB_AUTH_DOMAIN, "1", chain_id),
        &keccak256(&encoded),
    )
}

/// Level-1 headers proving control of the signer address.
pub fn l1_headers(
    signer: &LocalSigner,
    chain_id: u64,
    timestamp: u64,
    nonce: u64,
) -> Result<HeaderMap> {
    let digest = clob_auth_digest(&signer.address(), timestamp, nonce, chain_id);
    let signature = signer.sign_digest_hex(&digest)?;
    headers(&[
        ("POLY_ADDRESS", signer.address().to_string()),
        ("POLY_SIGNATURE", signature),
        ("POLY_TIMESTAMP", timestamp.to_string()),
        ("POLY_NONCE", nonce.to_string()),
    ])
}

/// Level-2 headers for an API-key-authenticated request.
pub fn l2_headers(
    creds: &ClobCredentials,
    timestamp: u64,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> Result<HeaderMap> {
    let timestamp = timestamp.to_string();
    let signature = l2_signature(&creds.secret, &timestamp, method, path, body)?;
    headers(&[
        ("POLY_ADDRESS", creds.address.to_string()),
        ("POLY_SIGNATURE", signature),
        ("POLY_TIMESTAMP", timestamp),
        ("POLY_API_KEY", creds.api_key.clone()),
        ("POLY_PASSPHRASE", creds.passphrase.clone()),
    ])
}

/// `urlsafe_base64(HMAC_SHA256(base64_decode(secret), ts + METHOD + path + body))`.
pub fn l2_signature(
    secret: &str,
    timestamp: &str,
    method: &str,
    path: &str,
    body: Option<&str>,
) -> Result<String> {
    let key = decode_secret(secret)?;
    let mut mac = Hmac::<Sha256>::new_from_slice(&key).expect("HMAC accepts keys of any length");
    mac.update(timestamp.as_bytes());
    mac.update(method.as_bytes());
    mac.update(path.as_bytes());
    if let Some(body) = body {
        mac.update(body.as_bytes());
    }
    Ok(URL_SAFE.encode(mac.finalize().into_bytes()))
}

/// Secrets are URL-safe base64; accept the standard alphabet and missing
/// padding too, like the reference Python client does.
fn decode_secret(secret: &str) -> Result<Vec<u8>> {
    let normalized: String = secret
        .trim()
        .chars()
        .map(|c| match c {
            '+' => '-',
            '/' => '_',
            other => other,
        })
        .collect();
    let engine = GeneralPurpose::new(
        &alphabet::URL_SAFE,
        GeneralPurposeConfig::new()
            .with_decode_padding_mode(DecodePaddingMode::Indifferent)
            .with_decode_allow_trailing_bits(true),
    );
    engine
        .decode(normalized.as_bytes())
        .context("PM_CLOB_SECRET is not valid base64")
}

/// Header names are case-insensitive; `from_bytes` stores them lowercased.
fn headers(pairs: &[(&str, String)]) -> Result<HeaderMap> {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.insert(
            HeaderName::from_bytes(name.as_bytes())?,
            HeaderValue::from_str(value).with_context(|| format!("invalid {name} header value"))?,
        );
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEV_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

    /// Known-answer vector from Polymarket's py-clob-client test suite.
    #[test]
    fn hmac_matches_reference_client() {
        let sig = l2_signature(
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            "1000000",
            "test-sign",
            "/orders",
            Some(r#"{"hash": "0x123"}"#),
        )
        .unwrap();
        assert_eq!(sig, "ZwAdJKvoYRlEKDkNMwd5BuwNNtg93kNaR_oU2HrfVvc=");
    }

    #[test]
    fn secret_decoding_is_lenient_about_alphabet_and_padding() {
        let padded = l2_signature(
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            "1",
            "GET",
            "/x",
            None,
        )
        .unwrap();
        let unpadded = l2_signature(
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "1",
            "GET",
            "/x",
            None,
        )
        .unwrap();
        assert_eq!(padded, unpadded);
        // '+' / '/' (standard alphabet) decode the same as '-' / '_'.
        let std_alpha = l2_signature("ab+/", "1", "GET", "/x", None).unwrap();
        let url_alpha = l2_signature("ab-_", "1", "GET", "/x", None).unwrap();
        assert_eq!(std_alpha, url_alpha);
        assert!(l2_signature("!!!notbase64!!!", "1", "GET", "/x", None).is_err());
    }

    /// Known-answer vector from py-clob-client: Amoy (80002), ts 10000000, nonce 23.
    #[test]
    fn clob_auth_signature_matches_reference_client() {
        let signer = LocalSigner::from_hex(DEV_KEY).unwrap();
        let digest = clob_auth_digest(&signer.address(), 10_000_000, 23, 80_002);
        assert_eq!(
            signer.sign_digest_hex(&digest).unwrap(),
            "0xf62319a987514da40e57e2f4d7529f7bac38f0355bd88bb5adbb3768d80de6c1682518e0af677d5260366425f4361e7b70c25ae232aff0ab2331e2b164a1aedc1b"
        );
    }

    #[test]
    fn l2_headers_are_complete() {
        let creds = ClobCredentials {
            address: LocalSigner::from_hex(DEV_KEY).unwrap().address(),
            api_key: "key".into(),
            secret: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            passphrase: "pass".into(),
        };
        let h = l2_headers(&creds, 1_700_000_000, "GET", "/data/orders", None).unwrap();
        assert_eq!(
            h["poly_address"],
            "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
        );
        assert_eq!(h["poly_timestamp"], "1700000000");
        assert_eq!(h["poly_api_key"], "key");
        assert_eq!(h["poly_passphrase"], "pass");
        let expected =
            l2_signature(&creds.secret, "1700000000", "GET", "/data/orders", None).unwrap();
        assert_eq!(h["poly_signature"], expected.as_str());
    }

    #[test]
    fn parses_order_page_with_numeric_fields() {
        let page: OrdersPage = serde_json::from_str(
            r#"{"limit":100,"next_cursor":"LTE=","count":1,"data":[{
                "id":"0xabc","status":"LIVE","owner":"o","maker_address":"0x1",
                "market":"0xm","asset_id":"123","side":"BUY","original_size":"100",
                "size_matched":"0","price":"0.5","outcome":"YES","expiration":"0",
                "order_type":"GTC","created_at":1700000000,"associate_trades":[]}]}"#,
        )
        .unwrap();
        let order = &page.data[0];
        assert_eq!(order.created_at.as_deref(), Some("1700000000"));
        assert_eq!(order.price, "0.5");
        assert!(order.extra.contains_key("associate_trades"));
        assert_eq!(page.next_cursor.as_deref(), Some(END_CURSOR));
    }
}
