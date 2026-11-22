//! Key handling, EIP-712 signing and Polymarket authentication headers.
//!
//! Two independent authentication systems live here:
//!
//! * CLOB (prediction markets): L1 = EIP-712 `ClobAuth` signature used to
//!   create/derive API credentials; L2 = HMAC-SHA256 over
//!   `timestamp + method + path [+ body]` using those credentials.
//! * Perps: EIP-712 `Op` signatures over a keccak256 hash of the MessagePack
//!   encoded compact operation. Private reads use static proxy headers instead.

use std::fmt;
use std::str::FromStr;

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine as _;
use hmac::{Hmac, Mac};
use k256::ecdsa::{RecoveryId, Signature, SigningKey};
use sha2::Sha256;
use sha3::{Digest, Keccak256};

use crate::msgpack::{self, Mp};

pub const CLOB_AUTH_MESSAGE: &str = "This message attests that I control the given wallet";
pub const POLYGON_CHAIN_ID: u64 = 137;

/// An Ethereum address (20 bytes).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Address([u8; 20]);

impl Address {
    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    pub fn to_lowercase_hex(self) -> String {
        format!("0x{}", hex::encode(self.0))
    }
}

impl fmt::Display for Address {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_lowercase_hex())
    }
}

impl fmt::Debug for Address {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_lowercase_hex())
    }
}

impl FromStr for Address {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        let trimmed = text.trim();
        let value = trimmed
            .strip_prefix("0x")
            .or_else(|| trimmed.strip_prefix("0X"))
            .unwrap_or(trimmed);
        if value.len() != 40 {
            bail!("expected a 20-byte hex address (0x followed by 40 hex characters)");
        }
        let bytes = hex::decode(value).context("address contains invalid hex characters")?;
        let mut address = [0u8; 20];
        address.copy_from_slice(&bytes);
        Ok(Self(address))
    }
}

/// A secp256k1 private key. `Debug` never prints the key material.
#[derive(Clone)]
pub struct SecretKey(SigningKey);

impl fmt::Debug for SecretKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretKey(<redacted>)")
    }
}

impl SecretKey {
    pub fn parse(text: &str) -> Result<Self> {
        let trimmed = text.trim();
        let value = trimmed
            .strip_prefix("0x")
            .or_else(|| trimmed.strip_prefix("0X"))
            .unwrap_or(trimmed);
        if value.len() != 64 {
            bail!("expected a 32-byte hex private key (64 hex characters)");
        }
        let bytes = hex::decode(value).context("private key contains invalid hex characters")?;
        let signing_key =
            SigningKey::from_slice(&bytes).context("invalid secp256k1 private key")?;
        Ok(Self(signing_key))
    }

    pub fn address(&self) -> Address {
        let public_key = self.0.verifying_key().to_encoded_point(false);
        let digest = keccak(public_key.as_bytes().get(1..).unwrap_or_default());
        let mut address = [0u8; 20];
        address.copy_from_slice(&digest[12..]);
        Address(address)
    }

    /// Signs a 32-byte digest and returns the 65-byte `r || s || v` signature
    /// as a `0x`-prefixed hex string (Ethereum style, `v` is 27 or 28).
    pub fn sign_digest(&self, digest: &[u8; 32]) -> Result<String> {
        let (signature, recovery_id) = self
            .0
            .sign_prehash_recoverable(digest)
            .context("failed to sign digest")?;
        Ok(encode_signature(&signature, recovery_id))
    }

    /// Signs a Perps compact operation.
    pub fn sign_perps_op(
        &self,
        chain_id: u64,
        operation: &Mp,
        salt: u32,
        timestamp_ms: u64,
    ) -> Result<String> {
        let operation_hash = keccak(&msgpack::encode(operation));
        let struct_hash = hash_struct(
            "Op(bytes32 data,uint64 salt,uint64 ts)",
            &[
                &operation_hash,
                &abi_u64(u64::from(salt)),
                &abi_u64(timestamp_ms),
            ],
        );
        let digest = eip712_digest("Polymarket", "1", chain_id, &struct_hash);
        self.sign_digest(&digest)
    }

    /// Signs the CLOB L1 `ClobAuth` typed data.
    pub fn sign_clob_auth(&self, chain_id: u64, timestamp: &str, nonce: u64) -> Result<String> {
        let address = self.address();
        let timestamp_hash = keccak(timestamp.as_bytes());
        let message_hash = keccak(CLOB_AUTH_MESSAGE.as_bytes());
        let struct_hash = hash_struct(
            "ClobAuth(address address,string timestamp,uint256 nonce,string message)",
            &[
                &abi_address(address),
                &timestamp_hash,
                &abi_u64(nonce),
                &message_hash,
            ],
        );
        let digest = eip712_digest("ClobAuthDomain", "1", chain_id, &struct_hash);
        self.sign_digest(&digest)
    }
}

fn encode_signature(signature: &Signature, recovery_id: RecoveryId) -> String {
    let mut bytes = [0u8; 65];
    bytes[..64].copy_from_slice(&signature.to_bytes());
    bytes[64] = 27 + recovery_id.to_byte();
    format!("0x{}", hex::encode(bytes))
}

fn eip712_digest(
    domain_name: &str,
    domain_version: &str,
    chain_id: u64,
    struct_hash: &[u8; 32],
) -> [u8; 32] {
    let domain_separator = hash_struct(
        "EIP712Domain(string name,string version,uint256 chainId)",
        &[
            &keccak(domain_name.as_bytes()),
            &keccak(domain_version.as_bytes()),
            &abi_u64(chain_id),
        ],
    );
    let mut buffer = Vec::with_capacity(66);
    buffer.extend_from_slice(b"\x19\x01");
    buffer.extend_from_slice(&domain_separator);
    buffer.extend_from_slice(struct_hash);
    keccak(&buffer)
}

fn hash_struct(type_string: &str, fields: &[&[u8; 32]]) -> [u8; 32] {
    let mut buffer = Vec::with_capacity(32 + fields.len() * 32);
    buffer.extend_from_slice(&keccak(type_string.as_bytes()));
    for field in fields {
        buffer.extend_from_slice(*field);
    }
    keccak(&buffer)
}

fn abi_u64(value: u64) -> [u8; 32] {
    let mut encoded = [0u8; 32];
    encoded[24..].copy_from_slice(&value.to_be_bytes());
    encoded
}

fn abi_address(address: Address) -> [u8; 32] {
    let mut encoded = [0u8; 32];
    encoded[12..].copy_from_slice(address.as_bytes());
    encoded
}

pub fn keccak(bytes: &[u8]) -> [u8; 32] {
    Keccak256::digest(bytes).into()
}

/// Credentials returned by the CLOB API key endpoints.
#[derive(Clone, serde::Deserialize)]
pub struct ApiCredentials {
    #[serde(alias = "apiKey", alias = "api_key")]
    pub api_key: String,
    pub secret: String,
    pub passphrase: String,
}

impl fmt::Debug for ApiCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ApiCredentials")
            .field("api_key", &"<redacted>")
            .field("secret", &"<redacted>")
            .field("passphrase", &"<redacted>")
            .finish()
    }
}

/// A header list that can be applied to a `reqwest::RequestBuilder`.
pub type HeaderList = Vec<(&'static str, String)>;

pub fn current_timestamp_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn current_timestamp_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn l1_headers(signer: &SecretKey, chain_id: u64, nonce: u64) -> Result<HeaderList> {
    let timestamp = current_timestamp_seconds().to_string();
    let signature = signer.sign_clob_auth(chain_id, &timestamp, nonce)?;
    Ok(vec![
        ("POLY_ADDRESS", signer.address().to_lowercase_hex()),
        ("POLY_SIGNATURE", signature),
        ("POLY_TIMESTAMP", timestamp),
        ("POLY_NONCE", nonce.to_string()),
    ])
}

pub fn l2_headers(
    address: Address,
    credentials: &ApiCredentials,
    timestamp: u64,
    method: &str,
    path: &str,
    body: &[u8],
) -> Result<HeaderList> {
    let signature = hmac_signature(&credentials.secret, timestamp, method, path, body)?;
    Ok(vec![
        ("POLY_ADDRESS", address.to_lowercase_hex()),
        ("POLY_SIGNATURE", signature),
        ("POLY_TIMESTAMP", timestamp.to_string()),
        ("POLY_API_KEY", credentials.api_key.clone()),
        ("POLY_PASSPHRASE", credentials.passphrase.clone()),
    ])
}

/// Computes the CLOB L2 HMAC signature.
///
/// The message is the decimal timestamp, the uppercase HTTP method, the request
/// path including its query string, and the request body (if any). The secret is
/// URL-safe base64 encoded and the result is URL-safe base64 **with** padding.
pub fn hmac_signature(
    secret: &str,
    timestamp: u64,
    method: &str,
    path: &str,
    body: &[u8],
) -> Result<String> {
    let key = decode_api_secret(secret)?;
    let mut mac = Hmac::<Sha256>::new_from_slice(&key).context("invalid HMAC key")?;
    mac.update(timestamp.to_string().as_bytes());
    mac.update(method.to_ascii_uppercase().as_bytes());
    mac.update(path.as_bytes());
    if !body.is_empty() {
        // The canonical body uses double quotes. Replace any single quotes so a
        // Python-style serialization still verifies the same bytes.
        let normalized: Vec<u8> = body
            .iter()
            .map(|byte| if *byte == b'\'' { b'"' } else { *byte })
            .collect();
        mac.update(&normalized);
    }
    Ok(URL_SAFE.encode(mac.finalize().into_bytes()))
}

pub fn decode_api_secret(secret: &str) -> Result<Vec<u8>> {
    let trimmed = secret.trim();
    for engine in [&URL_SAFE, &URL_SAFE_NO_PAD, &STANDARD, &STANDARD_NO_PAD] {
        if let Ok(decoded) = engine.decode(trimmed) {
            return Ok(decoded);
        }
    }
    bail!("API secret is not valid base64")
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

    #[test]
    fn derives_known_address() {
        let signer = SecretKey::parse(TEST_KEY).unwrap();
        assert_eq!(
            signer.address().to_string(),
            "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
        );
    }

    #[test]
    fn hmac_matches_reference_vector() {
        let signature =
            hmac_signature("c2VjcmV0", 1_700_000_000, "GET", "/data/orders", &[]).unwrap();
        assert_eq!(signature, "rght4Ai-8fubeLpGeBeoKAOna_ak8s8DmrlZwiqNDRU=");
    }

    #[test]
    fn perps_signature_matches_reference_vector() {
        let signer = SecretKey::parse(TEST_KEY).unwrap();
        let operation = Mp::Arr(vec![
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
        let signature = signer
            .sign_perps_op(137, &operation, 123_456_789, 1_767_000_010_000)
            .unwrap();
        assert_eq!(
            signature,
            "0xfebe58c4e756ec87318975093af0fda4fe61703f415bce061b1d86e0ed38cc0d\
             4f26f81e755e94afa6fa425c260463647a8f233f592a2283163f6686b46a318a1b"
        );
    }

    #[test]
    fn clob_auth_signature_matches_reference_vector() {
        let signer = SecretKey::parse(TEST_KEY).unwrap();
        let signature = signer.sign_clob_auth(137, "1700000000", 0).unwrap();
        assert_eq!(
            signature,
            "0x659ed4b28ae28e0f038fdf0023c00863c9559caacb9ebc83f44eea87059a099a\
             36f1e1dee110e7faa1c4f65d17489b2da1333ebef78bbe2116d81207b975052d1c"
        );
    }

    #[test]
    fn rejects_short_private_keys() {
        assert!(SecretKey::parse("0x1234").is_err());
    }

    #[test]
    fn parses_addresses() {
        let address: Address = "0xF39FD6E51AAD88F6F4CE6AB8827279CFFFB92266"
            .parse()
            .unwrap();
        assert_eq!(
            address.to_string(),
            "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
        );
    }
}
