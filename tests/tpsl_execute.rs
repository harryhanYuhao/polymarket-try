use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use polymarket_cli::auth::SecretKey;
use polymarket_cli::cli::TpslSetArgs;
use polymarket_cli::config::Config;
use polymarket_cli::tpsl;
use serde_json::{json, Value};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const TEST_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

#[derive(Clone)]
struct State {
    canceled: Arc<AtomicBool>,
}

struct OpenOrdersResponder {
    state: State,
}

impl Respond for OpenOrdersResponder {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        if self.state.canceled.load(Ordering::SeqCst) {
            ResponseTemplate::new(200).set_body_json(json!([]))
        } else {
            ResponseTemplate::new(200).set_body_json(json!([
                {
                    "order_id": 111,
                    "instrument_id": 1,
                    "buy": false,
                    "price": "0",
                    "quantity": "0",
                    "tif": "gtc",
                    "post_only": false,
                    "ro": true,
                    "status": "open",
                    "resting_quantity": "0",
                    "filled_quantity": "0",
                    "created_timestamp": 1767000000000u64,
                    "updated_timestamp": 1767000000000u64,
                    "tpsl": {
                        "kind": "tp",
                        "scope": "position",
                        "trp": "70000",
                        "armed_qty": "0"
                    }
                },
                {
                    "order_id": 222,
                    "instrument_id": 1,
                    "buy": false,
                    "price": "0",
                    "quantity": "0",
                    "tif": "gtc",
                    "post_only": false,
                    "ro": true,
                    "status": "open",
                    "resting_quantity": "0",
                    "filled_quantity": "0",
                    "created_timestamp": 1767000000000u64,
                    "updated_timestamp": 1767000000000u64,
                    "tpsl": {
                        "kind": "sl",
                        "scope": "position",
                        "trp": "62000",
                        "armed_qty": "0"
                    }
                }
            ]))
        }
    }
}

struct CancelResponder {
    state: State,
}

impl Respond for CancelResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        self.state.canceled.store(true, Ordering::SeqCst);
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let ids = body["op"]["args"].as_array().unwrap();
        let acks: Vec<Value> = ids
            .iter()
            .map(|id| {
                json!({
                    "status": "ok",
                    "oid": id.as_u64().unwrap(),
                    "ts": 1767000000000u64
                })
            })
            .collect();
        ResponseTemplate::new(200).set_body_json(acks)
    }
}

struct CreateResponder;

impl Respond for CreateResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let args = body["op"]["args"].as_array().unwrap();
        assert_eq!(body["op"]["grp"], "position");
        assert_eq!(args.len(), 1);
        let kind = args[0]["tr"]["tpsl"].as_str().unwrap();
        let oid = if kind == "tp" { 333 } else { 334 };
        ResponseTemplate::new(200).set_body_json(json!([{"status": "ok", "oid": oid}]))
    }
}

fn config(server_uri: &str) -> Config {
    let key = SecretKey::parse(TEST_KEY).unwrap();
    Config {
        clob_url: "http://127.0.0.1:1".to_owned(),
        perps_url: server_uri.to_owned(),
        data_api_url: "http://127.0.0.1:1".to_owned(),
        private_key: Some(key.clone()),
        address: Some(key.address()),
        signer_address: Some(key.address()),
        api_key: None,
        api_secret: None,
        api_passphrase: None,
        perps_proxy: Some(key.address()),
        perps_secret: Some("test-secret".to_owned()),
        perps_proxy_key: Some(key),
        chain_id: 137,
        timeout: Duration::from_secs(5),
    }
}

async fn mount_reads(server: &MockServer, state: State) {
    Mock::given(method("GET"))
        .and(path("/v1/info/instruments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "instrument_id": 1,
            "symbol": "BTC-USD",
            "display_symbol": null,
            "category": "crypto",
            "base_asset": "BTC",
            "quote_asset": "pUSD",
            "quantity_decimals": 4,
            "price_decimals": 2,
            "max_leverage": 10,
            "min_notional": "1",
            "isolated_only": false,
            "close_only": false
        }])))
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path("/v1/info/tickers"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "instrument_id": 1,
            "symbol": "BTC-USD",
            "index_price": "63000",
            "mark_price": "63000",
            "last_price": "63000",
            "mid_price": "63000",
            "open_interest": "1",
            "funding_rate": "0",
            "next_funding": 1767003600000u64,
            "timestamp": 1767000000000u64
        }])))
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path("/v1/account/portfolio"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "positions": [{
                "instrument_id": 1,
                "symbol": "BTC-USD",
                "size": "0.01",
                "entry_price": "60000",
                "leverage": 5,
                "cross": false,
                "initial_margin": "120",
                "maintenance_margin": "60",
                "position_value": "600",
                "liquidation_price": "50000",
                "unrealized_pnl": "0",
                "return_on_equity": "0",
                "cumulative_funding": "0"
            }],
            "margin": {
                "total_account_value": "1000",
                "available_order_margin": "880",
                "total_initial_margin": "120",
                "total_maintenance_margin": "60",
                "total_position_value": "600"
            },
            "withdrawable": "880",
            "in_liquidation": false,
            "fee_tier": 0,
            "timestamp": 1767000000000u64
        })))
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path("/v1/account/open-orders"))
        .respond_with(OpenOrdersResponder { state })
        .mount(server)
        .await;
}

#[tokio::test]
async fn execute_replaces_tp_and_sl_one_kind_at_a_time() {
    let server = MockServer::start().await;
    let state = State {
        canceled: Arc::new(AtomicBool::new(false)),
    };
    mount_reads(&server, state.clone()).await;

    Mock::given(method("DELETE"))
        .and(path("/v1/trade/orders"))
        .respond_with(CancelResponder {
            state: state.clone(),
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/trade/orders"))
        .respond_with(CreateResponder)
        .mount(&server)
        .await;

    let args = TpslSetArgs {
        instrument: "BTC-USD".to_owned(),
        take_profit: Some("70000".to_owned()),
        stop_loss: Some("62000".to_owned()),
        clear_take_profit: false,
        clear_stop_loss: false,
        execute: true,
        wait_secs: 1,
    };
    tpsl::run_set(&args, &config(&server.uri()), true)
        .await
        .expect("TP/SL replacement should succeed against the mock");

    let requests = server.received_requests().await.unwrap();
    let deletes: Vec<_> = requests
        .iter()
        .filter(|request| request.method == wiremock::http::Method::DELETE)
        .collect();
    let posts: Vec<_> = requests
        .iter()
        .filter(|request| request.method == wiremock::http::Method::POST)
        .collect();
    assert_eq!(deletes.len(), 2);
    assert_eq!(posts.len(), 2);

    for request in &posts {
        assert_eq!(
            request.headers.get("polymarket-proxy").unwrap(),
            "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
        );
        assert_eq!(
            request.headers.get("polymarket-secret").unwrap(),
            "test-secret"
        );
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["op"]["grp"], "position");
        assert_eq!(body["op"]["args"][0]["buy"], false);
        assert_eq!(body["op"]["args"][0]["qty"], "0");
        assert_eq!(body["op"]["args"][0]["ro"], true);
        assert_eq!(body["op"]["args"][0]["tr"]["market"], true);
        assert!(body["sig"].as_str().unwrap().starts_with("0x"));
        assert_eq!(body["sig"].as_str().unwrap().len(), 132);
        assert!(body["salt"].as_u64().unwrap() <= u32::MAX as u64);
    }

    let triggers: Vec<String> = posts
        .iter()
        .map(|request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            body["op"]["args"][0]["tr"]["trp"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(triggers, vec!["70000", "62000"]);
}

#[tokio::test]
async fn crossed_trigger_is_rejected_before_any_mutation() {
    let server = MockServer::start().await;
    let state = State {
        canceled: Arc::new(AtomicBool::new(false)),
    };
    mount_reads(&server, state).await;

    let args = TpslSetArgs {
        instrument: "1".to_owned(),
        take_profit: Some("62000".to_owned()), // below the 63000 mark for a long
        stop_loss: None,
        clear_take_profit: false,
        clear_stop_loss: false,
        execute: true,
        wait_secs: 1,
    };
    let error = tpsl::run_set(&args, &config(&server.uri()), true)
        .await
        .expect_err("crossed take-profit must be rejected");
    assert!(format!("{error:#}").contains("must be above the mark"));

    let requests = server.received_requests().await.unwrap();
    assert!(
        requests
            .iter()
            .all(|request| request.method == wiremock::http::Method::GET),
        "no cancel/create requests may be sent when validation fails"
    );
}
