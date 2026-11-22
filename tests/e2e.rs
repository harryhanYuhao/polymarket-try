//! End-to-end tests: run the real `pm` binary against a local mock of the
//! Polymarket APIs and check exactly what it sends, including signatures.

use alloy_primitives::{hex, Address};
use pm_try::clob::l2_signature;
use pm_try::eth::{recover_address, LocalSigner};
use pm_try::http::now_millis;
use pm_try::perps::signing::{op_digest, NewOrder, PerpsOp, TpSlKind, TpSlScope};
use serde_json::{json, Value};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

// Public Hardhat/Anvil development keys (accounts #0 and #1); never funded.
const OWNER_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
const PROXY_KEY: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
const PROXY_SECRET: &str = "test-proxy-secret";
const CLOB_SECRET: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

const PM_VARS: &[&str] = &[
    "PM_PRIVATE_KEY",
    "PM_ADDRESS",
    "PM_WALLET",
    "PM_CLOB_API_KEY",
    "PM_CLOB_SECRET",
    "PM_CLOB_PASSPHRASE",
    "PM_PERPS_PROXY",
    "PM_PERPS_SECRET",
    "PM_PERPS_PROXY_KEY",
    "PM_CHAIN_ID",
    "PM_CLOB_URL",
    "PM_DATA_URL",
    "PM_GAMMA_URL",
    "PM_PERPS_URL",
];

/// Run `pm` with every API pointed at `server`, a clean `PM_*` environment,
/// no stdin (so confirmation prompts can't be answered) and an empty working
/// directory (so no `.env` is picked up).
async fn pm(server: &MockServer, env: &[(&str, &str)], args: &[&str]) -> Output {
    static RUN: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "pm-e2e-{}-{}",
        std::process::id(),
        RUN.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();

    let uri = server.uri();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_pm"));
    for var in PM_VARS {
        cmd.env_remove(var);
    }
    for var in ["PM_CLOB_URL", "PM_DATA_URL", "PM_GAMMA_URL", "PM_PERPS_URL"] {
        cmd.env(var, &uri);
    }
    cmd.envs(env.iter().copied())
        .args(args)
        .current_dir(&dir)
        .stdin(Stdio::null());
    tokio::task::spawn_blocking(move || cmd.output().expect("failed to run pm"))
        .await
        .unwrap()
}

fn ok(body: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(body)
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn perps_env() -> Vec<(&'static str, &'static str)> {
    vec![
        ("PM_PERPS_PROXY_KEY", PROXY_KEY),
        ("PM_PERPS_SECRET", PROXY_SECRET),
    ]
}

fn proxy_address() -> Address {
    LocalSigner::from_hex(PROXY_KEY).unwrap().address()
}

fn tpsl_order(oid: u64, kind: &str, scope: &str, trp: &str, parent: Option<u64>) -> Value {
    let mut tpsl = json!({ "kind": kind, "scope": scope, "trp": trp, "armed_qty": "0" });
    if let Some(parent) = parent {
        tpsl["parent_oid"] = json!(parent);
    }
    json!({
        "order_id": oid, "instrument_id": 6, "buy": false, "price": "0", "quantity": "0",
        "tif": "gtc", "post_only": false, "ro": true, "status": "open", "resting_quantity": "0",
        "filled_quantity": "0", "created_timestamp": 1_767_000_000_000_u64,
        "updated_timestamp": 1_767_000_000_000_u64, "tpsl": tpsl
    })
}

/// A BTC-USD account (instrument 6, mark 66000) holding `size` with the given
/// open orders. Private reads only match with the right proxy credentials.
async fn mount_perps_account(server: &MockServer, size: &str, open_orders: Value) {
    Mock::given(method("GET"))
        .and(path("/v1/info/instruments"))
        .respond_with(ok(json!([{
            "instrument_id": 6, "instrument_type": "perpetual", "category": "crypto",
            "symbol": "BTC-USD", "base_asset": "BTC", "quote_asset": "USD",
            "price_decimals": 1, "quantity_decimals": 5, "max_leverage": 50
        }])))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/info/tickers"))
        .respond_with(ok(json!([{
            "instrument_id": 6, "symbol": "BTC-USD", "mark_price": "66000", "index_price": "66001",
            "last_price": "66000", "mid_price": "66000", "open_interest": "10",
            "funding_rate": "0.00001", "next_funding": 1, "timestamp": 1
        }])))
        .mount(server)
        .await;
    let liquidation = if size.starts_with('-') {
        "80000"
    } else {
        "52000"
    };
    Mock::given(method("GET"))
        .and(path("/v1/account/portfolio"))
        .and(header(
            "polymarket-proxy",
            proxy_address().to_string().as_str(),
        ))
        .and(header("polymarket-secret", PROXY_SECRET))
        .respond_with(ok(json!({
            "positions": [{
                "instrument_id": 6, "symbol": "BTC-USD", "size": size, "entry_price": "65000",
                "leverage": 5, "cross": false, "initial_margin": "264", "maintenance_margin": "13",
                "position_value": "1320", "liquidation_price": liquidation, "unrealized_pnl": "20",
                "return_on_equity": "0.1", "cumulative_funding": "0"
            }],
            "margin": {
                "total_account_value": "1000", "available_order_margin": "700",
                "total_initial_margin": "264", "total_maintenance_margin": "13",
                "total_position_value": "1320"
            },
            "withdrawable": "700", "in_liquidation": false, "fee_tier": 0, "timestamp": 1
        })))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/account/open-orders"))
        .and(header("polymarket-secret", PROXY_SECRET))
        .respond_with(ok(open_orders))
        .mount(server)
        .await;
}

async fn mount_trade_acks(server: &MockServer, cancel: Value, create: Value) {
    Mock::given(method("DELETE"))
        .and(path("/v1/trade/orders"))
        .respond_with(ok(cancel))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/trade/orders"))
        .respond_with(ok(create))
        .mount(server)
        .await;
}

async fn trade_requests(server: &MockServer) -> Vec<Request> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path().starts_with("/v1/trade/"))
        .collect()
}

/// The request body describes `op` and carries a fresh EIP-712 signature by `signer`.
fn assert_signed(request: &Request, op: &PerpsOp, signer: Address) {
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["op"], op.body(), "request op");
    let salt = body["salt"].as_u64().unwrap();
    let ts = body["ts"].as_u64().unwrap();
    assert!(salt <= u64::from(u32::MAX));
    let now = now_millis();
    assert!(ts <= now && now - ts < 60_000, "timestamp is fresh");
    let sig: [u8; 65] = hex::decode(body["sig"].as_str().unwrap())
        .unwrap()
        .try_into()
        .unwrap();
    let recovered = recover_address(&op_digest(&op.hash(), salt, ts, 137), &sig).unwrap();
    assert_eq!(recovered, signer, "signature must recover to the proxy");
    assert_eq!(
        request
            .headers
            .get("polymarket-proxy")
            .unwrap()
            .to_str()
            .unwrap(),
        signer.to_string()
    );
}

fn position_tpsl(buy: bool, legs: &[(TpSlKind, &str)]) -> PerpsOp {
    PerpsOp::CreateOrders {
        orders: legs
            .iter()
            .map(|(kind, price)| NewOrder::position_tpsl(6, buy, *kind, *price))
            .collect(),
        group: Some(TpSlScope::Position),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tpsl_set_replaces_existing_triggers_cancel_first() {
    let server = MockServer::start().await;
    mount_perps_account(
        &server,
        "0.02",
        json!([
            tpsl_order(111, "tp", "position", "69000", None),
            tpsl_order(222, "sl", "order", "60000", Some(5)), // armed bracket leg
        ]),
    )
    .await;
    mount_trade_acks(
        &server,
        json!([{"status": "ok", "oid": 111, "ts": 1}, {"status": "ok", "oid": 222, "ts": 1}]),
        json!([{"status": "ok", "oid": 333}, {"status": "ok", "oid": 444}]),
    )
    .await;

    let out = pm(
        &server,
        &perps_env(),
        &[
            "perps", "tpsl", "set", "BTC", "--tp", "71000", "--sl", "63000.0", "--yes",
        ],
    )
    .await;
    assert!(out.status.success(), "stderr: {}", stderr(&out));

    let trades = trade_requests(&server).await;
    assert_eq!(trades.len(), 2);
    assert_eq!(
        trades[0].method.as_str(),
        "DELETE",
        "old triggers are cancelled first"
    );
    assert_eq!(trades[1].method.as_str(), "POST");
    assert_signed(
        &trades[0],
        &PerpsOp::CancelOrders {
            order_ids: vec![111, 222],
        },
        proxy_address(),
    );
    // Long position: exits sell; the stop price is sent in canonical form.
    assert_signed(
        &trades[1],
        &position_tpsl(false, &[(TpSlKind::Tp, "71000"), (TpSlKind::Sl, "63000")]),
        proxy_address(),
    );
    let text = stdout(&out);
    assert!(
        text.contains("placed take-profit @ 71000 (order #333)"),
        "{text}"
    );
    assert!(
        text.contains("placed stop-loss @ 63000 (order #444)"),
        "{text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tpsl_set_on_unprotected_short_only_creates() {
    let server = MockServer::start().await;
    mount_perps_account(&server, "-0.5", json!([])).await;
    mount_trade_acks(&server, json!([]), json!([{"status": "ok", "oid": 9}])).await;

    let out = pm(
        &server,
        &perps_env(),
        &["perps", "tpsl", "create", "BTC-USD", "--sl", "70000", "-y"],
    )
    .await;
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let trades = trade_requests(&server).await;
    assert_eq!(trades.len(), 1);
    // Short position: the exit buys.
    assert_signed(
        &trades[0],
        &position_tpsl(true, &[(TpSlKind::Sl, "70000")]),
        proxy_address(),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tpsl_set_refuses_without_confirmation() {
    let server = MockServer::start().await;
    mount_perps_account(&server, "0.02", json!([])).await;
    mount_trade_acks(&server, json!([]), json!([])).await;

    let out = pm(
        &server,
        &perps_env(),
        &["perps", "tpsl", "set", "BTC", "--tp", "71000"],
    )
    .await;
    assert!(!out.status.success());
    assert!(stderr(&out).contains("--yes"), "{}", stderr(&out));
    assert!(
        trade_requests(&server).await.is_empty(),
        "nothing may be sent"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tpsl_dry_run_sends_nothing() {
    let server = MockServer::start().await;
    mount_perps_account(
        &server,
        "0.02",
        json!([tpsl_order(111, "tp", "position", "69000", None)]),
    )
    .await;
    mount_trade_acks(&server, json!([]), json!([])).await;

    let out = pm(
        &server,
        &perps_env(),
        &[
            "perps",
            "tpsl",
            "modify",
            "BTC",
            "--tp",
            "72000",
            "--dry-run",
        ],
    )
    .await;
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(
        text.contains("cancelOrders") && text.contains("createOrders"),
        "{text}"
    );
    assert!(text.contains("Dry run: nothing was sent."), "{text}");
    assert!(trade_requests(&server).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tpsl_set_rejects_trigger_already_crossed() {
    let server = MockServer::start().await;
    mount_perps_account(&server, "0.02", json!([])).await;
    mount_trade_acks(&server, json!([]), json!([])).await;

    // Long with mark 66000: a take-profit at 65000 would fire immediately.
    let out = pm(
        &server,
        &perps_env(),
        &["perps", "tpsl", "set", "BTC", "--tp", "65000", "--yes"],
    )
    .await;
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("above the current mark price 66000"),
        "{}",
        stderr(&out)
    );
    assert!(trade_requests(&server).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tpsl_warns_when_replacement_is_rejected() {
    let server = MockServer::start().await;
    mount_perps_account(
        &server,
        "0.02",
        json!([tpsl_order(111, "tp", "position", "69000", None)]),
    )
    .await;
    mount_trade_acks(
        &server,
        json!([{"status": "ok", "oid": 111, "ts": 1}]),
        json!([{"status": "err", "error": "price_outside_bounds"}]),
    )
    .await;

    let out = pm(
        &server,
        &perps_env(),
        &["perps", "tpsl", "set", "BTC", "--tp", "90000", "--yes"],
    )
    .await;
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("price_outside_bounds"), "{err}");
    assert!(
        err.contains("the position currently has NO take-profit"),
        "{err}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tpsl_cancel_only_selected_kind() {
    let server = MockServer::start().await;
    mount_perps_account(
        &server,
        "0.02",
        json!([
            tpsl_order(111, "tp", "position", "69000", None),
            tpsl_order(112, "sl", "position", "61000", None),
        ]),
    )
    .await;
    mount_trade_acks(
        &server,
        json!([{"status": "ok", "oid": 112, "ts": 1}]),
        json!([]),
    )
    .await;

    let out = pm(
        &server,
        &perps_env(),
        &["perps", "tpsl", "cancel", "BTC", "--sl", "--yes"],
    )
    .await;
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let trades = trade_requests(&server).await;
    assert_eq!(trades.len(), 1);
    assert_signed(
        &trades[0],
        &PerpsOp::CancelOrders {
            order_ids: vec![112],
        },
        proxy_address(),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prediction_orders_use_l2_auth_pagination_and_titles() {
    let server = MockServer::start().await;
    let order = |id: &str, market: &str| {
        json!({
            "id": id, "status": "LIVE", "owner": "o", "maker_address": "0x1", "market": market,
            "asset_id": "123", "side": "BUY", "original_size": "100", "size_matched": "0",
            "price": "0.5", "outcome": "Yes", "expiration": "0", "order_type": "GTC",
            "created_at": 1_700_000_000
        })
    };
    Mock::given(method("GET"))
        .and(path("/data/orders"))
        .and(query_param("next_cursor", "MQ=="))
        .respond_with(ok(
            json!({"limit": 1, "count": 1, "next_cursor": "LTE=", "data": [order("0xb", "0xc2")]}),
        ))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/data/orders"))
        .respond_with(ok(
            json!({"limit": 1, "count": 1, "next_cursor": "MQ==", "data": [order("0xa", "0xc1")]}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/markets"))
        .and(query_param("closed", "any"))
        .respond_with(ok(
            json!([{"id": "1", "conditionId": "0xc1", "question": "Will it rain?"}]),
        ))
        .mount(&server)
        .await;

    let env = [
        ("PM_PRIVATE_KEY", OWNER_KEY),
        ("PM_CLOB_API_KEY", "api-key"),
        ("PM_CLOB_SECRET", CLOB_SECRET),
        ("PM_CLOB_PASSPHRASE", "pass"),
    ];
    let out = pm(&server, &env, &["prediction", "orders", "--json"]).await;
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let orders: Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(
        orders.as_array().unwrap().len(),
        2,
        "both pages are fetched"
    );
    assert_eq!(orders[0]["question"], "Will it rain?");
    assert_eq!(orders[1]["id"], "0xb");

    let owner = LocalSigner::from_hex(OWNER_KEY)
        .unwrap()
        .address()
        .to_string();
    let requests: Vec<Request> = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path() == "/data/orders")
        .collect();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        let h = |name: &str| {
            request
                .headers
                .get(name)
                .unwrap()
                .to_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(h("poly_address"), owner);
        assert_eq!(h("poly_api_key"), "api-key");
        assert_eq!(h("poly_passphrase"), "pass");
        // The signed path excludes the query string.
        let expected = l2_signature(
            CLOB_SECRET,
            &h("poly_timestamp"),
            "GET",
            "/data/orders",
            None,
        )
        .unwrap();
        assert_eq!(h("poly_signature"), expected);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn combined_orders_skip_unconfigured_venue() {
    let server = MockServer::start().await;
    mount_perps_account(
        &server,
        "0.02",
        json!([tpsl_order(111, "tp", "position", "69000", None)]),
    )
    .await;

    let out = pm(&server, &perps_env(), &["orders", "--json"]).await;
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let report: Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert!(report["prediction"]["skipped"]
        .as_str()
        .unwrap()
        .contains("PM_CLOB_API_KEY"));
    assert_eq!(report["perps"][0]["symbol"], "BTC-USD");
    assert_eq!(report["perps"][0]["tpsl"]["trp"], "69000");

    // Asking for the unconfigured venue explicitly is an error.
    let out = pm(&server, &perps_env(), &["orders", "--venue", "prediction"]).await;
    assert!(!out.status.success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn positions_follow_data_api_cursor_and_show_perps_tpsl() {
    let server = MockServer::start().await;
    mount_perps_account(
        &server,
        "0.02",
        json!([
            tpsl_order(111, "tp", "position", "69000", None),
            tpsl_order(112, "sl", "position", "61000", None)
        ]),
    )
    .await;
    let row = |title: &str| json!({"title": title, "outcome": "Yes", "current_size": 10.0, "current_value": 7.0, "status": "OPEN"});
    Mock::given(method("GET"))
        .and(path("/v2/positions"))
        .and(query_param("cursor", "next-page"))
        .respond_with(ok(json!({"data": [row("Second")], "pagination": {"has_more": false, "next_cursor": null}})))
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/positions"))
        .and(query_param("status", "OPEN"))
        .respond_with(ok(json!({"data": [row("First")], "pagination": {"has_more": true, "next_cursor": "next-page"}})))
        .mount(&server)
        .await;

    let mut env = perps_env();
    env.push(("PM_WALLET", "0x00000000000000000000000000000000000000AB"));
    let out = pm(&server, &env, &["positions", "--json"]).await;
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let report: Value = serde_json::from_str(&stdout(&out)).unwrap();
    let titles: Vec<&str> = report["prediction"]["positions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, ["First", "Second"]);
    let position = &report["perps"]["positions"][0];
    assert_eq!(position["mark_price"], "66000");
    assert_eq!(position["take_profit"], json!(["69000"]));
    assert_eq!(position["stop_loss"], json!(["61000"]));

    let data_requests: Vec<Request> = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path() == "/v2/positions")
        .collect();
    assert_eq!(data_requests.len(), 2);
    let query = data_requests[1].url.query().unwrap_or_default();
    assert!(
        query.contains("cursor=next-page") && !query.contains("status="),
        "{query}"
    );
    assert!(data_requests[0]
        .url
        .query()
        .unwrap()
        .contains("user=0x00000000000000000000000000000000000000ab"));
}

async fn run_replacement_with_create_status(status: u16) -> Output {
    let server = MockServer::start().await;
    mount_perps_account(
        &server,
        "0.02",
        json!([tpsl_order(111, "tp", "position", "69000", None)]),
    )
    .await;
    Mock::given(method("DELETE"))
        .and(path("/v1/trade/orders"))
        .respond_with(ok(json!([{"status": "ok", "oid": 111, "ts": 1}])))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/trade/orders"))
        .respond_with(
            ResponseTemplate::new(status)
                .set_body_json(json!({"status": "err", "error": "server says no"})),
        )
        .mount(&server)
        .await;
    pm(
        &server,
        &perps_env(),
        &["perps", "tpsl", "set", "BTC", "--tp", "71000", "--yes"],
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_4xx_on_create_is_reported_as_rejected() {
    let out = run_replacement_with_create_status(400).await;
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("take-profit was rejected"), "{err}");
    assert!(err.contains("currently has NO take-profit"), "{err}");
    assert!(err.contains("1 of 1 trigger(s) were not placed"), "{err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_5xx_on_create_is_reported_as_unknown() {
    let out = run_replacement_with_create_status(500).await;
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("could not confirm"), "{err}");
    assert!(err.contains("may be unprotected"), "{err}");
    assert!(
        err.contains("pm perps tpsl list --instrument BTC-USD"),
        "{err}"
    );
    assert!(!err.contains("was rejected"), "{err}");
}
