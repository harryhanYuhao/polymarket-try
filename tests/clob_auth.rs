use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use polymarket_cli::auth::{hmac_signature, Address, ApiCredentials, SecretKey};
use polymarket_cli::clob::ClobClient;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const TEST_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const SECRET: &str = "c2VjcmV0"; // base64url("secret")

struct OrdersResponder {
    calls: Arc<AtomicUsize>,
}

impl Respond for OrdersResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let cursor = request
            .url
            .query_pairs()
            .find(|(name, _)| name == "next_cursor")
            .map(|(_, value)| value.into_owned());
        if call == 0 {
            assert_eq!(cursor, None);
            assert!(request
                .url
                .query_pairs()
                .any(|(name, value)| name == "market" && value == "0xmarket"));
            ResponseTemplate::new(200).set_body_json(json!({
                "limit": 100,
                "next_cursor": "MTAw",
                "count": 1,
                "data": [order("0xaaa")]
            }))
        } else {
            assert_eq!(cursor.as_deref(), Some("MTAw"));
            ResponseTemplate::new(200).set_body_json(json!({
                "limit": 100,
                "next_cursor": "",
                "count": 1,
                "data": [order("0xbbb")]
            }))
        }
    }
}

fn order(id: &str) -> Value {
    json!({
        "id": id,
        "status": "LIVE",
        "owner": "owner",
        "maker_address": "0x1234567890123456789012345678901234567890",
        "market": "0xmarket",
        "asset_id": "0xasset",
        "side": "BUY",
        "original_size": "10",
        "size_matched": "0",
        "price": "0.5",
        "outcome": "YES",
        "expiration": "0",
        "order_type": "GTC",
        "created_at": 1_700_000_000
    })
}

#[tokio::test]
async fn signs_and_paginates_clob_open_orders() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("GET"))
        .and(path("/data/orders"))
        .respond_with(OrdersResponder {
            calls: calls.clone(),
        })
        .mount(&server)
        .await;

    let signer = SecretKey::parse(TEST_KEY).unwrap();
    let address: Address = signer.address();
    let credentials = ApiCredentials {
        api_key: "api-key".to_owned(),
        secret: SECRET.to_owned(),
        passphrase: "passphrase".to_owned(),
    };
    let client =
        ClobClient::new(reqwest::Client::new(), &server.uri(), address, credentials).unwrap();

    let orders = client.open_orders(Some("0xmarket")).await.unwrap();
    assert_eq!(orders.len(), 2);
    assert_eq!(orders[0].id, "0xaaa");
    assert_eq!(orders[1].id, "0xbbb");
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    let requests = server.received_requests().await.unwrap();
    for request in &requests {
        let timestamp = request.headers.get("POLY_TIMESTAMP").unwrap();
        let signature = request.headers.get("POLY_SIGNATURE").unwrap();
        assert_eq!(
            request.headers.get("POLY_ADDRESS").unwrap(),
            "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
        );
        assert_eq!(request.headers.get("POLY_API_KEY").unwrap(), "api-key");
        assert_eq!(
            request.headers.get("POLY_PASSPHRASE").unwrap(),
            "passphrase"
        );

        let path_and_query = match request.url.query() {
            Some(query) => format!("{}?{query}", request.url.path()),
            None => request.url.path().to_owned(),
        };
        let expected = hmac_signature(
            SECRET,
            timestamp.to_str().unwrap().parse().unwrap(),
            "GET",
            &path_and_query,
            &[],
        )
        .unwrap();
        assert_eq!(signature.to_str().unwrap(), expected);
    }
}
