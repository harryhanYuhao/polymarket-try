//! Minimal Ethereum helpers: secp256k1 keys, addresses and EIP-712 hashing.
//!
//! Only the handful of typed-data structs used by Polymarket are needed, so the
//! EIP-712 encoding is done by hand (and covered by known-answer tests) instead
//! of pulling in a full Ethereum stack.

use alloy_primitives::{hex, keccak256, Address, B256, U256};
use anyhow::{anyhow, bail, Context, Result};
use k256::ecdsa::{SigningKey, VerifyingKey};
use rand::RngCore;

/// A local private key together with its Ethereum address.
#[derive(Clone)]
pub struct LocalSigner {
    key: SigningKey,
    address: Address,
}

impl std::fmt::Debug for LocalSigner {
    // Never print key material.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalSigner")
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

impl LocalSigner {
    /// Parse a hex private key (with or without `0x`).
    pub fn from_hex(input: &str) -> Result<Self> {
        let trimmed = input.trim();
        let digits = trimmed
            .strip_prefix("0x")
            .or_else(|| trimmed.strip_prefix("0X"))
            .unwrap_or(trimmed);
        let bytes = hex::decode(digits).context("private key is not valid hex")?;
        if bytes.len() != 32 {
            bail!(
                "private key must be 32 bytes (64 hex characters), got {} bytes",
                bytes.len()
            );
        }
        let key = SigningKey::from_slice(&bytes)
            .map_err(|_| anyhow!("private key is not a valid secp256k1 scalar"))?;
        Ok(Self::from_key(key))
    }

    /// Generate a fresh random key (used for Perps proxy signers).
    pub fn random() -> Self {
        let mut rng = rand::thread_rng();
        loop {
            let mut bytes = [0u8; 32];
            rng.fill_bytes(&mut bytes);
            if let Ok(key) = SigningKey::from_slice(&bytes) {
                return Self::from_key(key);
            }
        }
    }

    fn from_key(key: SigningKey) -> Self {
        let address = address_of(key.verifying_key());
        Self { key, address }
    }

    pub fn address(&self) -> Address {
        self.address
    }

    /// The private key as `0x`-prefixed hex. Only used when the user asks the
    /// CLI to create new credentials and store them.
    pub fn private_key_hex(&self) -> String {
        format!("0x{}", hex::encode(self.key.to_bytes()))
    }

    /// Sign a 32-byte digest. Returns `r || s || v` with `v` in {27, 28}
    /// (the encoding produced by viem/ox and eth-account).
    pub fn sign_digest(&self, digest: &B256) -> Result<[u8; 65]> {
        let (signature, recovery_id) = self
            .key
            .sign_prehash_recoverable(digest.as_slice())
            .map_err(|e| anyhow!("signing failed: {e}"))?;
        let mut out = [0u8; 65];
        out[..64].copy_from_slice(&signature.to_bytes());
        out[64] = 27 + recovery_id.to_byte();
        Ok(out)
    }

    /// [`Self::sign_digest`] as a `0x`-prefixed hex string.
    pub fn sign_digest_hex(&self, digest: &B256) -> Result<String> {
        Ok(format!("0x{}", hex::encode(self.sign_digest(digest)?)))
    }
}

/// Ethereum address of a secp256k1 public key.
pub fn address_of(key: &VerifyingKey) -> Address {
    let point = key.to_encoded_point(false);
    let hash = keccak256(&point.as_bytes()[1..]);
    Address::from_slice(&hash[12..])
}

/// Parse an address, accepting any hex casing.
pub fn parse_address(input: &str) -> Result<Address> {
    input
        .trim()
        .parse::<Address>()
        .map_err(|e| anyhow!("invalid address {input:?}: {e}"))
}

/// Recover the signer address of a 65-byte `r || s || v` signature (test and
/// self-check helper).
pub fn recover_address(digest: &B256, signature: &[u8; 65]) -> Result<Address> {
    let sig = k256::ecdsa::Signature::from_slice(&signature[..64])?;
    let v = signature[64];
    let recovery = k256::ecdsa::RecoveryId::from_byte(if v >= 27 { v - 27 } else { v })
        .ok_or_else(|| anyhow!("invalid recovery id {v}"))?;
    let key = VerifyingKey::recover_from_prehash(digest.as_slice(), &sig, recovery)?;
    Ok(address_of(&key))
}

/// `EIP712Domain(string name,string version,uint256 chainId)` separator.
/// Both Polymarket domains used here omit `verifyingContract` and `salt`.
pub fn domain_separator(name: &str, version: &str, chain_id: u64) -> B256 {
    let type_hash = keccak256(b"EIP712Domain(string name,string version,uint256 chainId)");
    let mut encoded = Vec::with_capacity(32 * 4);
    encoded.extend_from_slice(type_hash.as_slice());
    encoded.extend_from_slice(keccak256(name.as_bytes()).as_slice());
    encoded.extend_from_slice(keccak256(version.as_bytes()).as_slice());
    encoded.extend_from_slice(&word_u64(chain_id));
    keccak256(&encoded)
}

/// Final EIP-712 digest: `keccak256(0x1901 || domainSeparator || structHash)`.
pub fn typed_data_digest(domain_separator: &B256, struct_hash: &B256) -> B256 {
    let mut buf = [0u8; 66];
    buf[0] = 0x19;
    buf[1] = 0x01;
    buf[2..34].copy_from_slice(domain_separator.as_slice());
    buf[34..].copy_from_slice(struct_hash.as_slice());
    keccak256(buf)
}

/// ABI word for an unsigned integer.
pub fn word_u64(value: u64) -> [u8; 32] {
    U256::from(value).to_be_bytes::<32>()
}

/// ABI word for an address (left-padded).
pub fn word_address(address: &Address) -> [u8; 32] {
    let mut word = [0u8; 32];
    word[12..].copy_from_slice(address.as_slice());
    word
}

#[cfg(test)]
mod tests {
    use super::*;

    // Well-known Hardhat/Anvil development key #0. Never holds real funds.
    pub const DEV_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

    #[test]
    fn derives_known_address() {
        let signer = LocalSigner::from_hex(DEV_KEY).unwrap();
        assert_eq!(
            signer.address().to_string(),
            "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
        );
    }

    #[test]
    fn rejects_bad_keys() {
        assert!(LocalSigner::from_hex("0x1234").is_err());
        assert!(LocalSigner::from_hex("zz").is_err());
        assert!(LocalSigner::from_hex(&format!("0x{}", "00".repeat(32))).is_err());
    }

    #[test]
    fn signatures_recover_to_signer_and_are_low_s() {
        let signer = LocalSigner::random();
        let digest = keccak256(b"hello");
        let sig = signer.sign_digest(&digest).unwrap();
        assert!(sig[64] == 27 || sig[64] == 28);
        assert_eq!(recover_address(&digest, &sig).unwrap(), signer.address());
        // EIP-2: s must be in the lower half of the curve order.
        let s = k256::ecdsa::Signature::from_slice(&sig[..64]).unwrap();
        assert!(s.normalize_s().is_none(), "signature must already be low-s");
    }

    #[test]
    fn debug_does_not_leak_key() {
        let signer = LocalSigner::from_hex(DEV_KEY).unwrap();
        let shown = format!("{signer:?}");
        assert!(!shown.contains("ac0974"));
    }
}
