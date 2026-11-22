//! Environment-driven configuration and credential resolution.

use std::env;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::auth::{Address, ApiCredentials, SecretKey, POLYGON_CHAIN_ID};
use crate::cli::Cli;

pub const DEFAULT_CLOB_URL: &str = "https://clob.polymarket.com";
pub const DEFAULT_PERPS_URL: &str = "https://api.perpetuals.polymarket.com";
pub const DEFAULT_DATA_API_URL: &str = "https://data-api.polymarket.com";

/// Resolved runtime configuration. Secret values are kept as plain strings but
/// are never included in `Debug` output or error messages.
pub struct Config {
    pub clob_url: String,
    pub perps_url: String,
    pub data_api_url: String,
    pub private_key: Option<SecretKey>,
    pub address: Option<Address>,
    pub signer_address: Option<Address>,
    pub api_key: Option<String>,
    pub api_secret: Option<String>,
    pub api_passphrase: Option<String>,
    pub perps_proxy: Option<Address>,
    pub perps_secret: Option<String>,
    pub perps_proxy_key: Option<SecretKey>,
    pub chain_id: u64,
    pub timeout: Duration,
}

impl Config {
    pub fn load(cli: &Cli) -> Result<Self> {
        let private_key = parse_optional_secret_key("POLYMARKET_PRIVATE_KEY")?;
        let address = parse_optional_address("POLYMARKET_ADDRESS")?;
        let signer_address = parse_optional_address("POLYMARKET_SIGNER_ADDRESS")?;

        let chain_id = match non_empty_env("POLYMARKET_CHAIN_ID") {
            Some(raw) => raw
                .parse()
                .with_context(|| "POLYMARKET_CHAIN_ID must be a positive integer")?,
            None => POLYGON_CHAIN_ID,
        };
        let timeout_secs = match non_empty_env("POLYMARKET_TIMEOUT_SECS") {
            Some(raw) => raw
                .parse()
                .with_context(|| "POLYMARKET_TIMEOUT_SECS must be a positive integer")?,
            None => 30,
        };

        Ok(Self {
            clob_url: cli
                .clob_url
                .clone()
                .unwrap_or_else(|| DEFAULT_CLOB_URL.to_owned()),
            perps_url: cli
                .perps_url
                .clone()
                .unwrap_or_else(|| DEFAULT_PERPS_URL.to_owned()),
            data_api_url: cli
                .data_api_url
                .clone()
                .unwrap_or_else(|| DEFAULT_DATA_API_URL.to_owned()),
            private_key,
            address,
            signer_address,
            api_key: non_empty_env("POLYMARKET_API_KEY"),
            api_secret: non_empty_env("POLYMARKET_API_SECRET"),
            api_passphrase: non_empty_env("POLYMARKET_API_PASSPHRASE"),
            perps_proxy: parse_optional_address("POLYMARKET_PERPS_PROXY")?,
            perps_secret: non_empty_env("POLYMARKET_PERPS_SECRET"),
            perps_proxy_key: parse_optional_secret_key("POLYMARKET_PERPS_PROXY_KEY")?,
            chain_id,
            timeout: Duration::from_secs(timeout_secs),
        })
    }

    pub fn http_client(&self) -> Result<reqwest::Client> {
        reqwest::Client::builder()
            .timeout(self.timeout)
            .user_agent(format!("polymarket-cli/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .context("failed to build HTTP client")
    }

    /// The address used as `POLY_ADDRESS` for L2 requests.
    pub fn clob_l2_address(&self) -> Result<Address> {
        self.signer_address
            .or_else(|| self.private_key.as_ref().map(SecretKey::address))
            .or(self.address)
            .context(
                "missing CLOB signer address: set POLYMARKET_SIGNER_ADDRESS, \
                 POLYMARKET_PRIVATE_KEY, or POLYMARKET_ADDRESS",
            )
    }

    /// The wallet whose prediction-market positions should be queried.
    pub fn wallet_address(&self) -> Result<Address> {
        self.address
            .or_else(|| self.private_key.as_ref().map(SecretKey::address))
            .or(self.signer_address)
            .context(
                "missing wallet address: set POLYMARKET_ADDRESS to the proxy wallet \
                 that holds the positions",
            )
    }

    pub fn clob_credentials(&self) -> Result<ApiCredentials> {
        match (&self.api_key, &self.api_secret, &self.api_passphrase) {
            (Some(api_key), Some(secret), Some(passphrase)) => Ok(ApiCredentials {
                api_key: api_key.clone(),
                secret: secret.clone(),
                passphrase: passphrase.clone(),
            }),
            _ => bail!(
                "missing CLOB API credentials: set POLYMARKET_API_KEY, \
                 POLYMARKET_API_SECRET and POLYMARKET_API_PASSPHRASE \
                 (run `pml auth derive` with POLYMARKET_PRIVATE_KEY to create them)"
            ),
        }
    }

    pub fn perps_read_credentials(&self) -> Result<(&Address, &str)> {
        match (&self.perps_proxy, &self.perps_secret) {
            (Some(proxy), Some(secret)) => Ok((proxy, secret)),
            _ => bail!(
                "missing Perps credentials: set POLYMARKET_PERPS_PROXY and \
                 POLYMARKET_PERPS_SECRET"
            ),
        }
    }

    pub fn perps_signing_key(&self) -> Result<&SecretKey> {
        let key = self.perps_proxy_key.as_ref().context(
            "missing Perps proxy signing key: set POLYMARKET_PERPS_PROXY_KEY \
             (the private key of the proxy signer, not your main wallet key)",
        )?;
        if let Some(proxy) = self.perps_proxy {
            if key.address() != proxy {
                bail!(
                    "POLYMARKET_PERPS_PROXY_KEY derives address {}, but \
                     POLYMARKET_PERPS_PROXY is {}; they must match",
                    key.address(),
                    proxy
                );
            }
        }
        Ok(key)
    }

    pub fn summary(&self) -> Vec<(&'static str, String)> {
        vec![
            ("CLOB URL", self.clob_url.clone()),
            ("Perps URL", self.perps_url.clone()),
            ("Data API URL", self.data_api_url.clone()),
            (
                "Wallet address",
                self.address
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "<not set>".to_owned()),
            ),
            (
                "CLOB signer address",
                self.signer_address
                    .or_else(|| self.private_key.as_ref().map(SecretKey::address))
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "<not set>".to_owned()),
            ),
            ("CLOB private key", present(self.private_key.is_some())),
            (
                "CLOB API credentials",
                present(
                    self.api_key.is_some()
                        && self.api_secret.is_some()
                        && self.api_passphrase.is_some(),
                ),
            ),
            ("Perps proxy", present(self.perps_proxy.is_some())),
            ("Perps secret", present(self.perps_secret.is_some())),
            (
                "Perps proxy signing key",
                present(self.perps_proxy_key.is_some()),
            ),
        ]
    }
}

fn present(value: bool) -> String {
    if value {
        "set".to_owned()
    } else {
        "<not set>".to_owned()
    }
}

fn non_empty_env(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn parse_optional_address(name: &str) -> Result<Option<Address>> {
    match non_empty_env(name) {
        Some(value) => {
            Ok(Some(value.parse().with_context(|| {
                format!("{name} is not a valid Ethereum address")
            })?))
        }
        None => Ok(None),
    }
}

fn parse_optional_secret_key(name: &str) -> Result<Option<SecretKey>> {
    match non_empty_env(name) {
        Some(value) => Ok(Some(SecretKey::parse(&value).with_context(|| {
            format!("{name} is not a valid secp256k1 private key")
        })?)),
        None => Ok(None),
    }
}
