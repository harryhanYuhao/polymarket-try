//! Credentials and endpoints, read from `PM_*` environment variables.
//!
//! Secrets are never accepted as command-line flags so they don't end up in
//! shell history; the binary can load them from a `.env` file instead.

use crate::eth::{parse_address, LocalSigner};
use alloy_primitives::Address;
use anyhow::{bail, Context, Result};
use std::fmt;

pub const DEFAULT_CLOB_URL: &str = "https://clob.polymarket.com";
pub const DEFAULT_DATA_URL: &str = "https://data-api.polymarket.com";
pub const DEFAULT_GAMMA_URL: &str = "https://gamma-api.polymarket.com";
pub const DEFAULT_PERPS_URL: &str = "https://api.perpetuals.polymarket.com";
/// Polygon mainnet; used as the EIP-712 `chainId` for CLOB and Perps signatures.
pub const DEFAULT_CHAIN_ID: u64 = 137;

/// A command needs credentials that are not configured. Kept as a distinct
/// type so multi-venue commands can skip a venue instead of failing.
#[derive(Debug)]
pub struct MissingCredentials {
    pub venue: &'static str,
    pub hint: String,
}

impl fmt::Display for MissingCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} credentials missing: {}", self.venue, self.hint)
    }
}

impl std::error::Error for MissingCredentials {}

/// CLOB level-2 API credentials plus the signer address they belong to.
#[derive(Clone)]
pub struct ClobCredentials {
    pub address: Address,
    pub api_key: String,
    pub secret: String,
    pub passphrase: String,
}

impl fmt::Debug for ClobCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClobCredentials")
            .field("address", &self.address)
            .field("api_key", &self.api_key)
            .finish_non_exhaustive()
    }
}

/// Perps proxy credentials: the proxy address and secret authenticate private
/// reads; the proxy private key signs trading operations.
#[derive(Clone)]
pub struct PerpsCredentials {
    pub proxy: Address,
    pub secret: String,
    pub signer: Option<LocalSigner>,
}

impl fmt::Debug for PerpsCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PerpsCredentials")
            .field("proxy", &self.proxy)
            .field("has_signing_key", &self.signer.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct Config {
    pub chain_id: u64,
    pub clob_url: String,
    pub data_url: String,
    pub gamma_url: String,
    pub perps_url: String,
    /// `PM_PRIVATE_KEY`: the account owner's EOA key (only needed to create credentials).
    pub owner: Option<LocalSigner>,
    /// Signer EOA address (`PM_ADDRESS`, or derived from `PM_PRIVATE_KEY`).
    pub address: Option<Address>,
    /// Polymarket wallet that holds prediction positions (`PM_WALLET`).
    pub wallet: Option<Address>,
    clob_api_key: Option<String>,
    clob_secret: Option<String>,
    clob_passphrase: Option<String>,
    perps_proxy: Option<Address>,
    perps_secret: Option<String>,
    perps_signer: Option<LocalSigner>,
}

pub const ENV_HELP: &str = "\
Environment (also read from ./.env or --env-file):
  PM_PRIVATE_KEY     Owner EOA key; only for `prediction api-key` and `perps proxy create`
  PM_ADDRESS         Signer EOA address (derived from PM_PRIVATE_KEY when unset)
  PM_WALLET          Polymarket wallet holding prediction positions (default: PM_ADDRESS)
  PM_CLOB_API_KEY, PM_CLOB_SECRET, PM_CLOB_PASSPHRASE
                     CLOB API credentials, from `pm prediction api-key`
  PM_PERPS_PROXY, PM_PERPS_SECRET, PM_PERPS_PROXY_KEY
                     Perps proxy credentials, from `pm perps proxy create`
                     (the proxy key is only needed to sign TP/SL orders)
  PM_CHAIN_ID, PM_CLOB_URL, PM_DATA_URL, PM_GAMMA_URL, PM_PERPS_URL
                     Optional overrides";

impl Config {
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Build from an arbitrary variable lookup (used by tests).
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let var = |key: &str| {
            lookup(key)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let url = |key: &str, default: &str| {
            var(key)
                .unwrap_or_else(|| default.to_string())
                .trim_end_matches('/')
                .to_string()
        };

        let chain_id = match var("PM_CHAIN_ID") {
            Some(v) => v.parse().context("PM_CHAIN_ID must be an integer")?,
            None => DEFAULT_CHAIN_ID,
        };

        let owner = var("PM_PRIVATE_KEY")
            .map(|k| LocalSigner::from_hex(&k).context("PM_PRIVATE_KEY"))
            .transpose()?;
        let explicit_address = var("PM_ADDRESS")
            .map(|a| parse_address(&a).context("PM_ADDRESS"))
            .transpose()?;
        if let (Some(owner), Some(address)) = (&owner, explicit_address) {
            if owner.address() != address {
                bail!(
                    "PM_ADDRESS ({address}) is not the address of PM_PRIVATE_KEY ({}); \
                     PM_ADDRESS must be the signer EOA (put your Polymarket wallet in PM_WALLET)",
                    owner.address()
                );
            }
        }
        let address = explicit_address.or_else(|| owner.as_ref().map(LocalSigner::address));
        let wallet = var("PM_WALLET")
            .map(|a| parse_address(&a).context("PM_WALLET"))
            .transpose()?;

        let perps_signer = var("PM_PERPS_PROXY_KEY")
            .map(|k| LocalSigner::from_hex(&k).context("PM_PERPS_PROXY_KEY"))
            .transpose()?;
        let mut perps_proxy = var("PM_PERPS_PROXY")
            .map(|a| parse_address(&a).context("PM_PERPS_PROXY"))
            .transpose()?;
        if let Some(signer) = &perps_signer {
            match perps_proxy {
                Some(proxy) if proxy != signer.address() => bail!(
                    "PM_PERPS_PROXY ({proxy}) does not match the address of PM_PERPS_PROXY_KEY ({})",
                    signer.address()
                ),
                _ => perps_proxy = Some(signer.address()),
            }
        }

        Ok(Self {
            chain_id,
            clob_url: url("PM_CLOB_URL", DEFAULT_CLOB_URL),
            data_url: url("PM_DATA_URL", DEFAULT_DATA_URL),
            gamma_url: url("PM_GAMMA_URL", DEFAULT_GAMMA_URL),
            perps_url: url("PM_PERPS_URL", DEFAULT_PERPS_URL),
            owner,
            address,
            wallet,
            clob_api_key: var("PM_CLOB_API_KEY"),
            clob_secret: var("PM_CLOB_SECRET"),
            clob_passphrase: var("PM_CLOB_PASSPHRASE"),
            perps_proxy,
            perps_secret: var("PM_PERPS_SECRET"),
            perps_signer,
        })
    }

    /// Credentials for authenticated CLOB (prediction) requests.
    pub fn clob_credentials(&self) -> Result<ClobCredentials> {
        let missing: Vec<&str> = [
            ("PM_CLOB_API_KEY", self.clob_api_key.is_none()),
            ("PM_CLOB_SECRET", self.clob_secret.is_none()),
            ("PM_CLOB_PASSPHRASE", self.clob_passphrase.is_none()),
        ]
        .into_iter()
        .filter_map(|(name, absent)| absent.then_some(name))
        .collect();
        if !missing.is_empty() {
            return Err(MissingCredentials {
                venue: "prediction",
                hint: format!(
                    "set {} (run `pm prediction api-key` with PM_PRIVATE_KEY to derive them)",
                    missing.join(", ")
                ),
            }
            .into());
        }
        let address = self.address.ok_or_else(|| MissingCredentials {
            venue: "prediction",
            hint: "set PM_ADDRESS (the signer address that owns the API key) or PM_PRIVATE_KEY"
                .to_string(),
        })?;
        Ok(ClobCredentials {
            address,
            api_key: self.clob_api_key.clone().unwrap_or_default(),
            secret: self.clob_secret.clone().unwrap_or_default(),
            passphrase: self.clob_passphrase.clone().unwrap_or_default(),
        })
    }

    /// Credentials for private Perps reads (portfolio, open orders).
    pub fn perps_credentials(&self) -> Result<PerpsCredentials> {
        match (self.perps_proxy, &self.perps_secret) {
            (Some(proxy), Some(secret)) => Ok(PerpsCredentials {
                proxy,
                secret: secret.clone(),
                signer: self.perps_signer.clone(),
            }),
            _ => Err(MissingCredentials {
                venue: "perps",
                hint: "set PM_PERPS_PROXY (or PM_PERPS_PROXY_KEY) and PM_PERPS_SECRET \
                       (run `pm perps proxy create` with PM_PRIVATE_KEY to get them)"
                    .to_string(),
            }
            .into()),
        }
    }

    /// Credentials that can also sign Perps trading operations.
    pub fn perps_trading_credentials(&self) -> Result<(PerpsCredentials, LocalSigner)> {
        let creds = self.perps_credentials()?;
        let signer = creds.signer.clone().ok_or_else(|| MissingCredentials {
            venue: "perps",
            hint: "set PM_PERPS_PROXY_KEY (the proxy private key) to sign TP/SL orders".to_string(),
        })?;
        Ok((creds, signer))
    }

    /// The account owner's key, needed only to create CLOB / Perps credentials.
    pub fn owner_signer(&self, venue: &'static str) -> Result<LocalSigner> {
        self.owner.clone().ok_or_else(|| {
            MissingCredentials {
                venue,
                hint: "set PM_PRIVATE_KEY (the EOA that controls your Polymarket account)"
                    .to_string(),
            }
            .into()
        })
    }

    /// Wallet whose prediction positions to show: explicit flag, then
    /// `PM_WALLET`, then the signer address (EOA accounts).
    pub fn positions_wallet(&self, explicit: Option<&str>) -> Result<Address> {
        if let Some(user) = explicit {
            return parse_address(user);
        }
        self.wallet.or(self.address).ok_or_else(|| {
            MissingCredentials {
                venue: "prediction",
                hint: "set PM_WALLET (your Polymarket wallet address) or pass --user".to_string(),
            }
            .into()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const DEV_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
    const DEV_ADDR: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
    const DEV_KEY_1: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

    fn config(vars: &[(&str, &str)]) -> Result<Config> {
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::from_lookup(|k| map.get(k).cloned())
    }

    #[test]
    fn empty_env_has_defaults_and_no_credentials() {
        let cfg = config(&[]).unwrap();
        assert_eq!(cfg.chain_id, 137);
        assert_eq!(cfg.perps_url, DEFAULT_PERPS_URL);
        let err = cfg.clob_credentials().unwrap_err();
        assert!(err.downcast_ref::<MissingCredentials>().is_some());
        assert!(cfg.perps_credentials().is_err());
        assert!(cfg.positions_wallet(None).is_err());
    }

    #[test]
    fn address_is_derived_from_private_key() {
        let cfg = config(&[("PM_PRIVATE_KEY", DEV_KEY)]).unwrap();
        assert_eq!(cfg.address.unwrap().to_string(), DEV_ADDR);
        assert_eq!(cfg.positions_wallet(None).unwrap().to_string(), DEV_ADDR);
    }

    #[test]
    fn mismatched_address_is_rejected() {
        let err = config(&[
            ("PM_PRIVATE_KEY", DEV_KEY),
            ("PM_ADDRESS", "0x0000000000000000000000000000000000000001"),
        ])
        .unwrap_err();
        assert!(err.to_string().contains("PM_WALLET"));
    }

    #[test]
    fn partial_clob_credentials_name_the_missing_variables() {
        let cfg = config(&[("PM_ADDRESS", DEV_ADDR), ("PM_CLOB_API_KEY", "k")]).unwrap();
        let msg = cfg.clob_credentials().unwrap_err().to_string();
        assert!(msg.contains("PM_CLOB_SECRET") && msg.contains("PM_CLOB_PASSPHRASE"));
    }

    #[test]
    fn perps_proxy_is_derived_from_proxy_key() {
        let cfg = config(&[("PM_PERPS_PROXY_KEY", DEV_KEY_1), ("PM_PERPS_SECRET", "s")]).unwrap();
        let (creds, signer) = cfg.perps_trading_credentials().unwrap();
        assert_eq!(creds.proxy, signer.address());
        assert_eq!(
            creds.proxy.to_string(),
            "0x70997970C51812dc3A010C7d01b50e0d17dc79C8"
        );
    }

    #[test]
    fn perps_reads_work_without_signing_key() {
        let cfg = config(&[
            (
                "PM_PERPS_PROXY",
                "0x70997970c51812dc3a010c7d01b50e0d17dc79c8",
            ),
            ("PM_PERPS_SECRET", "s"),
        ])
        .unwrap();
        assert!(cfg.perps_credentials().is_ok());
        let err = cfg.perps_trading_credentials().unwrap_err();
        assert!(err.to_string().contains("PM_PERPS_PROXY_KEY"));
    }

    #[test]
    fn wallet_override_wins() {
        let cfg = config(&[
            ("PM_PRIVATE_KEY", DEV_KEY),
            ("PM_WALLET", "0x0000000000000000000000000000000000000002"),
        ])
        .unwrap();
        assert_eq!(
            cfg.positions_wallet(None).unwrap(),
            parse_address("0x0000000000000000000000000000000000000002").unwrap()
        );
        assert_eq!(
            cfg.positions_wallet(Some("0x0000000000000000000000000000000000000003"))
                .unwrap(),
            parse_address("0x0000000000000000000000000000000000000003").unwrap()
        );
    }
}
