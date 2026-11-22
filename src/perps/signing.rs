//! Perps signed operations.
//!
//! Every trading request carries an EIP-712 `Op(bytes32 data,uint64 salt,uint64 ts)`
//! signature by the proxy key, where `data = keccak256(msgpack(compact_op))`.
//! The compact op is the positional array form used by the official SDK, with
//! absent entries removed, e.g. a position take-profit is
//! `["createOrders", [[iid, buy, qty, po, ro, [market, trp, "tp"]]], "position"]`.
//! The JSON body sent alongside it must describe exactly the same operation,
//! because the server rebuilds the compact form from the JSON to verify.

use super::msgpack::{self, Value as Mp};
use crate::eth::{domain_separator, typed_data_digest, word_address, word_u64, LocalSigner};
use alloy_primitives::{keccak256, Address, B256};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value as Json};
use std::fmt;

const DOMAIN_NAME: &str = "Polymarket";
const DOMAIN_VERSION: &str = "1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TpSlKind {
    Tp,
    Sl,
}

impl TpSlKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TpSlKind::Tp => "tp",
            TpSlKind::Sl => "sl",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            TpSlKind::Tp => "take-profit",
            TpSlKind::Sl => "stop-loss",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "tp" => Some(TpSlKind::Tp),
            "sl" => Some(TpSlKind::Sl),
            _ => None,
        }
    }
}

impl fmt::Display for TpSlKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// `grp` of a TP/SL `createOrders`: children of an entry order, or attached
/// to the open position (sized at trigger time).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TpSlScope {
    Order,
    Position,
}

impl TpSlScope {
    pub fn as_str(self) -> &'static str {
        match self {
            TpSlScope::Order => "order",
            TpSlScope::Position => "position",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeInForce {
    Gtc,
    Ioc,
    Fok,
}

impl TimeInForce {
    pub fn as_str(self) -> &'static str {
        match self {
            TimeInForce::Gtc => "gtc",
            TimeInForce::Ioc => "ioc",
            TimeInForce::Fok => "fok",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trigger {
    pub kind: TpSlKind,
    pub trigger_price: String,
    /// Exit with a market order when fired (always true for position TP/SL).
    pub market: bool,
}

/// One entry of a `createOrders` operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewOrder {
    pub instrument_id: u64,
    pub buy: bool,
    pub price: Option<String>,
    pub quantity: String,
    pub time_in_force: Option<TimeInForce>,
    pub post_only: bool,
    pub reduce_only: bool,
    pub client_order_id: Option<String>,
    pub trigger: Option<Trigger>,
}

impl NewOrder {
    /// Position-scoped TP/SL: a reduce-only market exit sized to the whole
    /// position when the mark price touches `trigger_price` (`qty` is "0").
    /// `close_with_buy` is the closing side: `false` for a long, `true` for a short.
    pub fn position_tpsl(
        instrument_id: u64,
        close_with_buy: bool,
        kind: TpSlKind,
        trigger_price: impl Into<String>,
    ) -> Self {
        Self {
            instrument_id,
            buy: close_with_buy,
            price: None,
            quantity: "0".to_string(),
            time_in_force: None,
            post_only: false,
            reduce_only: true,
            client_order_id: None,
            trigger: Some(Trigger {
                kind,
                trigger_price: trigger_price.into(),
                market: true,
            }),
        }
    }

    /// `[iid, buy, p?, qty, tif?, po, ro?, c?, tr?]`, absent entries dropped.
    /// `ro` and `tr.market` only appear when true, matching the SDK.
    fn compact(&self) -> Mp {
        let mut fields: Vec<Mp> = vec![self.instrument_id.into(), self.buy.into()];
        if let Some(price) = &self.price {
            fields.push(price.as_str().into());
        }
        fields.push(self.quantity.as_str().into());
        if let Some(tif) = self.time_in_force {
            fields.push(tif.as_str().into());
        }
        fields.push(self.post_only.into());
        if self.reduce_only {
            fields.push(true.into());
        }
        if let Some(coid) = &self.client_order_id {
            fields.push(coid.as_str().into());
        }
        if let Some(trigger) = &self.trigger {
            let mut tr: Vec<Mp> = Vec::with_capacity(3);
            if trigger.market {
                tr.push(true.into());
            }
            tr.push(trigger.trigger_price.as_str().into());
            tr.push(trigger.kind.as_str().into());
            fields.push(Mp::Array(tr));
        }
        Mp::Array(fields)
    }

    fn body(&self) -> Json {
        let mut order = Map::new();
        order.insert("iid".into(), json!(self.instrument_id));
        order.insert("buy".into(), json!(self.buy));
        order.insert("po".into(), json!(self.post_only));
        order.insert("qty".into(), json!(self.quantity));
        if let Some(tif) = self.time_in_force {
            order.insert("tif".into(), json!(tif.as_str()));
        }
        if self.reduce_only {
            order.insert("ro".into(), json!(true));
        }
        if let Some(price) = &self.price {
            order.insert("p".into(), json!(price));
        }
        if let Some(coid) = &self.client_order_id {
            order.insert("c".into(), json!(coid));
        }
        if let Some(trigger) = &self.trigger {
            let mut tr = Map::new();
            tr.insert("tpsl".into(), json!(trigger.kind.as_str()));
            tr.insert("trp".into(), json!(trigger.trigger_price));
            if trigger.market {
                tr.insert("market".into(), json!(true));
            }
            order.insert("tr".into(), Json::Object(tr));
        }
        Json::Object(order)
    }
}

/// The Perps operations this crate signs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PerpsOp {
    CreateOrders {
        orders: Vec<NewOrder>,
        group: Option<TpSlScope>,
    },
    CancelOrders {
        order_ids: Vec<u64>,
    },
}

impl PerpsOp {
    /// Positional form that gets MessagePack-encoded and hashed.
    pub fn compact(&self) -> Mp {
        match self {
            PerpsOp::CreateOrders { orders, group } => {
                let mut op = vec![
                    Mp::from("createOrders"),
                    Mp::Array(orders.iter().map(NewOrder::compact).collect()),
                ];
                if let Some(group) = group {
                    op.push(group.as_str().into());
                }
                Mp::Array(op)
            }
            PerpsOp::CancelOrders { order_ids } => Mp::Array(vec![
                "cancelOrders".into(),
                Mp::Array(order_ids.iter().map(|id| Mp::UInt(*id)).collect()),
            ]),
        }
    }

    /// JSON `op` object for the request body.
    pub fn body(&self) -> Json {
        match self {
            PerpsOp::CreateOrders { orders, group } => {
                let mut op = json!({
                    "type": "createOrders",
                    "args": orders.iter().map(NewOrder::body).collect::<Vec<_>>(),
                });
                if let Some(group) = group {
                    op["grp"] = json!(group.as_str());
                }
                op
            }
            PerpsOp::CancelOrders { order_ids } => json!({
                "type": "cancelOrders",
                "args": order_ids,
            }),
        }
    }

    /// `keccak256(msgpack(compact))`, the `data` field of the signed `Op`.
    pub fn hash(&self) -> B256 {
        keccak256(msgpack::encode(&self.compact()))
    }
}

/// Request body `{op, sig, salt, ts}` for `/v1/trade/*` endpoints.
#[derive(Debug, Clone, Serialize)]
pub struct SignedRequest {
    pub op: Json,
    pub sig: String,
    pub salt: u64,
    pub ts: u64,
}

/// EIP-712 digest of `Op(bytes32 data,uint64 salt,uint64 ts)`.
pub fn op_digest(op_hash: &B256, salt: u64, ts_ms: u64, chain_id: u64) -> B256 {
    let type_hash = keccak256(b"Op(bytes32 data,uint64 salt,uint64 ts)");
    let mut encoded = Vec::with_capacity(32 * 4);
    encoded.extend_from_slice(type_hash.as_slice());
    encoded.extend_from_slice(op_hash.as_slice());
    encoded.extend_from_slice(&word_u64(salt));
    encoded.extend_from_slice(&word_u64(ts_ms));
    typed_data_digest(
        &domain_separator(DOMAIN_NAME, DOMAIN_VERSION, chain_id),
        &keccak256(&encoded),
    )
}

/// EIP-712 digest of `CreateProxy(address addr,uint64 exp,uint64 salt,uint64 ts)`.
pub fn create_proxy_digest(
    proxy: &Address,
    expiry_ms: u64,
    salt: u64,
    ts_ms: u64,
    chain_id: u64,
) -> B256 {
    let type_hash = keccak256(b"CreateProxy(address addr,uint64 exp,uint64 salt,uint64 ts)");
    let mut encoded = Vec::with_capacity(32 * 5);
    encoded.extend_from_slice(type_hash.as_slice());
    encoded.extend_from_slice(&word_address(proxy));
    encoded.extend_from_slice(&word_u64(expiry_ms));
    encoded.extend_from_slice(&word_u64(salt));
    encoded.extend_from_slice(&word_u64(ts_ms));
    typed_data_digest(
        &domain_separator(DOMAIN_NAME, DOMAIN_VERSION, chain_id),
        &keccak256(&encoded),
    )
}

/// Sign `op` with the proxy key.
pub fn sign_op(
    op: &PerpsOp,
    proxy_key: &LocalSigner,
    chain_id: u64,
    salt: u64,
    ts_ms: u64,
) -> Result<SignedRequest> {
    let digest = op_digest(&op.hash(), salt, ts_ms, chain_id);
    Ok(SignedRequest {
        op: op.body(),
        sig: proxy_key.sign_digest_hex(&digest)?,
        salt,
        ts: ts_ms,
    })
}

/// Body for `POST /v1/account/proxy`, signed by the account owner.
pub fn create_proxy_request(
    owner: &LocalSigner,
    proxy: &Address,
    expiry_ms: u64,
    salt: u64,
    ts_ms: u64,
    chain_id: u64,
    label: Option<&str>,
) -> Result<Json> {
    let digest = create_proxy_digest(proxy, expiry_ms, salt, ts_ms, chain_id);
    let mut body = json!({
        "op": {
            "type": "createProxy",
            "args": {
                "owner": owner.address().to_string(),
                "proxy": proxy.to_string(),
                "expiry": expiry_ms,
            }
        },
        "sig": owner.sign_digest_hex(&digest)?,
        "salt": salt,
        "ts": ts_ms,
    });
    if let Some(label) = label {
        body["label"] = json!(label);
    }
    Ok(body)
}

/// Random 32-bit salt, as the official SDK uses.
pub fn random_salt() -> u64 {
    u64::from(rand::random::<u32>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eth::recover_address;

    const DEV_KEY_0: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
    const DEV_KEY_1: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";

    fn arr(items: Vec<Mp>) -> Mp {
        Mp::Array(items)
    }

    fn position_bracket() -> PerpsOp {
        PerpsOp::CreateOrders {
            orders: vec![
                NewOrder::position_tpsl(1, false, TpSlKind::Tp, "70000"),
                NewOrder::position_tpsl(1, false, TpSlKind::Sl, "62000"),
            ],
            group: Some(TpSlScope::Position),
        }
    }

    #[test]
    fn position_tpsl_compact_form_matches_sdk_layout() {
        // SDK: [iid, buy, undefined, "0", undefined, false, true, undefined,
        //       [true, trp, kind]] with undefined entries removed.
        let leg = |trp: &str, kind: &str| {
            arr(vec![
                1u64.into(),
                false.into(),
                "0".into(),
                false.into(),
                true.into(),
                arr(vec![true.into(), trp.into(), kind.into()]),
            ])
        };
        let expected = arr(vec![
            "createOrders".into(),
            arr(vec![leg("70000", "tp"), leg("62000", "sl")]),
            "position".into(),
        ]);
        assert_eq!(position_bracket().compact(), expected);
    }

    #[test]
    fn position_tp_msgpack_bytes() {
        let op = PerpsOp::CreateOrders {
            orders: vec![NewOrder::position_tpsl(1, false, TpSlKind::Tp, "70000")],
            group: Some(TpSlScope::Position),
        };
        let mut expected = vec![0x93, 0xac];
        expected.extend_from_slice(b"createOrders");
        expected.extend_from_slice(&[
            0x91, 0x96, 0x01, 0xc2, 0xa1, b'0', 0xc2, 0xc3, 0x93, 0xc3, 0xa5,
        ]);
        expected.extend_from_slice(b"70000");
        expected.extend_from_slice(&[0xa2, b't', b'p', 0xa8]);
        expected.extend_from_slice(b"position");
        assert_eq!(msgpack::encode(&op.compact()), expected);
    }

    #[test]
    fn position_tpsl_body_matches_documented_request() {
        let body = position_bracket().body();
        assert_eq!(
            body,
            json!({
                "type": "createOrders",
                "grp": "position",
                "args": [
                    {"iid": 1, "buy": false, "po": false, "qty": "0", "ro": true,
                     "tr": {"tpsl": "tp", "trp": "70000", "market": true}},
                    {"iid": 1, "buy": false, "po": false, "qty": "0", "ro": true,
                     "tr": {"tpsl": "sl", "trp": "62000", "market": true}}
                ]
            })
        );
    }

    #[test]
    fn plain_order_compact_form_matches_docs_example() {
        // Docs: [1, true, "65000", "0.01", "ioc", false, undefined, coid, undefined]
        let coid = "7f9e4a2b6c8d0e1f1234567890abcdef";
        let op = PerpsOp::CreateOrders {
            orders: vec![NewOrder {
                instrument_id: 1,
                buy: true,
                price: Some("65000".into()),
                quantity: "0.01".into(),
                time_in_force: Some(TimeInForce::Ioc),
                post_only: false,
                reduce_only: false,
                client_order_id: Some(coid.into()),
                trigger: None,
            }],
            group: None,
        };
        let expected = arr(vec![
            "createOrders".into(),
            arr(vec![arr(vec![
                1u64.into(),
                true.into(),
                "65000".into(),
                "0.01".into(),
                "ioc".into(),
                false.into(),
                coid.into(),
            ])]),
        ]);
        assert_eq!(op.compact(), expected);
        assert_eq!(
            op.body(),
            json!({"type": "createOrders", "args": [
                {"iid": 1, "buy": true, "po": false, "qty": "0.01", "tif": "ioc", "p": "65000", "c": coid}
            ]})
        );
    }

    #[test]
    fn limit_trigger_omits_market_flag() {
        let mut order = NewOrder::position_tpsl(3, true, TpSlKind::Sl, "1.5");
        order.trigger.as_mut().unwrap().market = false;
        order.price = Some("1.49".into());
        let Mp::Array(fields) = order.compact() else {
            panic!()
        };
        assert_eq!(fields.last(), Some(&arr(vec!["1.5".into(), "sl".into()])));
        assert_eq!(order.body()["tr"], json!({"tpsl": "sl", "trp": "1.5"}));
    }

    #[test]
    fn cancel_op_forms() {
        let op = PerpsOp::CancelOrders {
            order_ids: vec![1_234_567_890, 7],
        };
        assert_eq!(
            op.compact(),
            arr(vec![
                "cancelOrders".into(),
                arr(vec![1_234_567_890u64.into(), 7u64.into()])
            ])
        );
        assert_eq!(
            op.body(),
            json!({"type": "cancelOrders", "args": [1_234_567_890_u64, 7]})
        );
    }

    #[test]
    fn signed_op_recovers_to_proxy_address() {
        let proxy = LocalSigner::from_hex(DEV_KEY_1).unwrap();
        let op = position_bracket();
        let signed = sign_op(&op, &proxy, 137, 234_567_890, 1_767_000_010_000).unwrap();
        assert_eq!(signed.salt, 234_567_890);
        assert_eq!(signed.ts, 1_767_000_010_000);
        assert_eq!(signed.op, op.body());

        let sig_bytes: [u8; 65] = alloy_primitives::hex::decode(&signed.sig)
            .unwrap()
            .try_into()
            .unwrap();
        let digest = op_digest(&op.hash(), 234_567_890, 1_767_000_010_000, 137);
        assert_eq!(
            recover_address(&digest, &sig_bytes).unwrap(),
            proxy.address()
        );
        // Deterministic (RFC 6979) signatures.
        let again = sign_op(&op, &proxy, 137, 234_567_890, 1_767_000_010_000).unwrap();
        assert_eq!(signed.sig, again.sig);
        // Any change to the operation changes the digest.
        let other = PerpsOp::CancelOrders { order_ids: vec![1] };
        assert_ne!(
            op_digest(&other.hash(), 234_567_890, 1_767_000_010_000, 137),
            digest
        );
    }

    #[test]
    fn create_proxy_request_is_owner_signed() {
        let owner = LocalSigner::from_hex(DEV_KEY_0).unwrap();
        let proxy = LocalSigner::from_hex(DEV_KEY_1).unwrap().address();
        let body = create_proxy_request(
            &owner,
            &proxy,
            1_767_225_600_000,
            123_456_789,
            1_767_000_000_000,
            137,
            Some("bot"),
        )
        .unwrap();
        assert_eq!(body["op"]["type"], "createProxy");
        assert_eq!(
            body["op"]["args"]["owner"],
            "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
        );
        assert_eq!(
            body["op"]["args"]["proxy"],
            "0x70997970C51812dc3A010C7d01b50e0d17dc79C8"
        );
        assert_eq!(body["label"], "bot");
        let sig: [u8; 65] = alloy_primitives::hex::decode(body["sig"].as_str().unwrap())
            .unwrap()
            .try_into()
            .unwrap();
        let digest = create_proxy_digest(
            &proxy,
            1_767_225_600_000,
            123_456_789,
            1_767_000_000_000,
            137,
        );
        assert_eq!(recover_address(&digest, &sig).unwrap(), owner.address());
    }
}
