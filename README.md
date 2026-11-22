# polymarket-cli

`pml` is a small Rust CLI for [Polymarket](https://polymarket.com) that talks to
both sides of the platform:

- **Prediction markets** (CLOB + Data API): list your live orders and open positions.
- **Perpetuals** (Perps API): list instruments, tickers, positions and live orders,
  and create or replace position-level **take-profit** and **stop-loss** triggers.

It is built with `clap` for the CLI, `reqwest` for HTTP, and `k256`/`sha3`/`hmac`
for the two authentication schemes. It does not wrap an SDK: the CLOB L2 HMAC
signing and the Perps EIP-712-over-MessagePack signing are implemented directly.

> **Status:** public read paths are exercised against the live APIs. The Perps
> write path (cancel + create TP/SL) is covered by an end-to-end mock test that
> verifies request bodies, auth headers and signatures. It has not been run
> against a funded Perps account from this repository. Start with a dry run.

## Build

```bash
cargo build --release
./target/release/pml --help
```

Rust 1.80+ is required.

## Commands

| Command | What it does |
|---|---|
| `pml orders --venue all` | Live orders from prediction markets and Perps |
| `pml orders --venue prediction --market 0x...` | Live CLOB orders, optionally filtered by market |
| `pml orders --venue perps --instrument BTC-USD` | Live Perps orders for one instrument |
| `pml positions --venue all` | Open prediction positions and Perps positions |
| `pml positions --venue prediction --address 0x...` | Prediction positions for a proxy wallet |
| `pml perps instruments` | Tradable Perps instruments |
| `pml perps ticker BTC-USD` | Mark/index/last/funding for one instrument |
| `pml perps orders` | Live Perps orders |
| `pml perps tpsl show [--instrument BTC-USD]` | Active TP/SL triggers |
| `pml perps tpsl set --instrument BTC-USD --take-profit 70000 --stop-loss 62000` | Preview a TP/SL replacement |
| `pml perps tpsl set ... --execute` | Cancel old trigger(s) and create replacement(s) |
| `pml auth derive` | Create/derive CLOB API credentials from `POLYMARKET_PRIVATE_KEY` |
| `pml auth check` | Show configured credentials and validate the CLOB key |
| `pml auth whoami` | Print the address derived from `POLYMARKET_PRIVATE_KEY` |

Add `--json` before or after a subcommand for machine-readable output.

`--venue all` requires both credential sets. If you only have one, query
`--venue prediction` or `--venue perps` separately.

## Configuration

Configuration comes from environment variables (a local `.env` is loaded
automatically by the binary). Nothing is required for public market data.

### Prediction markets (CLOB)

The CLOB uses two auth layers:

- **L1**: an EIP-712 `ClobAuth` signature made with your wallet key, used only to
  create or derive API credentials.
- **L2**: HMAC-SHA256 headers made with the resulting API key, secret and
  passphrase, used for private reads such as `/data/orders`.

The easiest setup is:

```bash
export POLYMARKET_PRIVATE_KEY=0x...          # signer key; never commit this
pml auth derive                              # prints export lines
eval "$(pml auth derive)"                    # or copy them into your shell/.env
```

`pml auth derive` calls `GET /auth/derive-api-key` and falls back to
`POST /auth/api-key` when the wallet has no key yet. It prints:

```bash
export POLYMARKET_API_KEY='...'
export POLYMARKET_API_SECRET='...'
export POLYMARKET_API_PASSPHRASE='...'
export POLYMARKET_SIGNER_ADDRESS='0x...'
```

You can also supply credentials created elsewhere. For L2 requests the CLI sends
`POLY_ADDRESS` as the signer address (`POLYMARKET_SIGNER_ADDRESS`, otherwise the
address derived from `POLYMARKET_PRIVATE_KEY`, otherwise `POLYMARKET_ADDRESS`).

Prediction **positions** are read from the public Data API and are keyed by the
wallet that holds them — normally your Polymarket proxy wallet:

```bash
export POLYMARKET_ADDRESS=0x...   # proxy wallet shown in Polymarket settings
```

### Perps

Perps uses its own proxy credentials, unrelated to the CLOB `POLY_*` headers:

| Variable | Meaning |
|---|---|
| `POLYMARKET_PERPS_PROXY` | Proxy signer address |
| `POLYMARKET_PERPS_SECRET` | Proxy secret returned when the proxy was created |
| `POLYMARKET_PERPS_PROXY_KEY` | Proxy signer private key, used to sign trading operations |

Private Perps reads use the first two headers. Creating/canceling TP/SL also
requires the proxy signing key.

These credentials are provisioned by the Perps `CreateProxy` flow. The official
SDK can create and persist them:

```ts
import { createSecureClient } from "@polymarket/client";
import { privateKey } from "@polymarket/client/viem";

const client = await createSecureClient({
  wallet: process.env.POLYMARKET_WALLET_ADDRESS!,
  signer: privateKey(process.env.POLYMARKET_PRIVATE_KEY!),
});
const session = await client.openPerpsSession();
console.log(session.credentials);
// { proxy, privateKey, secret, expiresAt }
```

Map those values to `POLYMARKET_PERPS_PROXY`, `POLYMARKET_PERPS_PROXY_KEY` and
`POLYMARKET_PERPS_SECRET`. The proxy key is a separate key; do not use your main
wallet key unless it actually is the proxy signer.

### All variables

| Variable | Default | Purpose |
|---|---|---|
| `POLYMARKET_PRIVATE_KEY` | — | CLOB L1 signer key; also used for `auth derive` |
| `POLYMARKET_ADDRESS` | — | Proxy wallet for prediction positions |
| `POLYMARKET_SIGNER_ADDRESS` | derived | Optional `POLY_ADDRESS` override for CLOB L2 |
| `POLYMARKET_API_KEY` | — | CLOB L2 API key |
| `POLYMARKET_API_SECRET` | — | CLOB L2 secret (base64url) |
| `POLYMARKET_API_PASSPHRASE` | — | CLOB L2 passphrase |
| `POLYMARKET_PERPS_PROXY` | — | Perps proxy address |
| `POLYMARKET_PERPS_SECRET` | — | Perps proxy secret |
| `POLYMARKET_PERPS_PROXY_KEY` | — | Perps proxy signing key |
| `POLYMARKET_CLOB_URL` | `https://clob.polymarket.com` | CLOB base URL |
| `POLYMARKET_PERPS_URL` | `https://api.perpetuals.polymarket.com` | Perps base URL |
| `POLYMARKET_DATA_API_URL` | `https://data-api.polymarket.com` | Data API base URL |
| `POLYMARKET_CHAIN_ID` | `137` | EIP-712 chain ID |
| `POLYMARKET_TIMEOUT_SECS` | `30` | HTTP timeout |

The same URLs can be overridden with `--clob-url`, `--perps-url` and
`--data-api-url`.

## Examples

```bash
# Both venues, live orders
pml orders --venue all

# Prediction positions for a proxy wallet
pml positions --venue prediction --address 0x1111111111111111111111111111111111111111

# Perps market data
pml perps instruments
pml perps ticker BTC-USD          # BTC-PERP is accepted as an alias too

# Inspect existing position TP/SL
pml perps tpsl show --instrument BTC-USD

# Preview a replacement (no network mutation)
pml perps tpsl set \
  --instrument BTC-USD \
  --take-profit 70000 \
  --stop-loss 62000

# Apply it
pml perps tpsl set \
  --instrument BTC-USD \
  --take-profit 70000 \
  --stop-loss 62000 \
  --execute

# Remove both triggers
pml perps tpsl set --instrument BTC-USD --clear-take-profit --clear-stop-loss --execute

# JSON
pml orders --venue all --json
```

A dry run prints the instrument, position side/size, mark price, and the existing
triggers that would be canceled. `--execute` is required to submit anything.

## How TP/SL replacement works

Position-level TP/SL triggers close the **full position** at trigger time, using
the **mark price**, and use a market exit. The Perps API allows at most one
position take-profit and one position stop-loss per instrument.

The API cannot modify a trigger in place: changing a price means canceling the
old trigger and creating a new one. `pml perps tpsl set` therefore:

1. Resolves the instrument and requires an open position.
2. Validates each new trigger against the current mark:
   - long: TP above mark, SL below mark;
   - short: TP below mark, SL above mark.
3. Finds existing TP/SL orders of the affected kind for that instrument,
   including order-scoped (bracket) triggers.
4. Processes take-profit first, then stop-loss. For each kind it cancels the old
   trigger(s), waits until they disappear from open orders, then creates the
   replacement. The other kind stays active while this happens.
5. Refuses to create a replacement if a cancellation is not acknowledged or the
   old order is still visible.

Important caveats:

- **Cancel-then-create is not atomic.** If creation fails after a cancel, that
  kind is unprotected; the error says so. The other kind is left intact.
- Canceling an order-scoped trigger attached to a still-resting bracket entry
  does not cancel the entry. This command only manages triggers; it does not
  place or cancel entry orders.
- The exchange itself rejects triggers already crossed by the mark; the CLI
  rejects them earlier so it does not cancel a working trigger first.
- Perps order placement is geographically restricted. Check the
  [Perps restrictions](https://docs.polymarket.com/api-reference/perps/geographic-restrictions)
  before using this against a live account.

## APIs used

| Purpose | Request | Auth |
|---|---|---|
| CLOB live orders | `GET /data/orders` | L2 HMAC headers |
| Derive/create CLOB key | `GET /auth/derive-api-key`, `POST /auth/api-key` | L1 EIP-712 `ClobAuth` |
| Prediction positions | `GET /v2/positions` (Data API) | Public |
| Perps instruments/tickers | `GET /v1/info/instruments`, `GET /v1/info/tickers` | Public |
| Perps portfolio | `GET /v1/account/portfolio` | `polymarket-proxy`, `polymarket-secret` |
| Perps open orders | `GET /v1/account/open-orders` | `polymarket-proxy`, `polymarket-secret` |
| Create TP/SL | `POST /v1/trade/orders` with `createOrders`, `grp: "position"` | EIP-712 `Op` |
| Cancel TP/SL | `DELETE /v1/trade/orders` with `cancelOrders` | EIP-712 `Op` |

References:

- [Perps overview](https://docs.polymarket.com/api-reference/perps/overview)
- [Perps trading / TP-SL](https://docs.polymarket.com/perps/trading)
- [Perps authenticated sessions](https://docs.polymarket.com/perps/authenticated-sessions)
- [CLOB open orders](https://docs.polymarket.com/api-reference/trade/get-user-orders)
- [CLOB authentication](https://github.com/dilettante-trading/polyoxide/blob/main/docs/specs/clob/auth.md)

## Testing

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```

The test suite includes:

- reference vectors for the Perps EIP-712 signature and the exact MessagePack
  bytes of a `createOrders`/position operation, generated with `viem` and
  `@msgpack/msgpack` (the same libraries used by the official TypeScript SDK);
- a CLOB test that serves paginated orders from a mock server and recomputes the
  HMAC from each request's timestamp, method, path and query;
- an end-to-end Perps TP/SL replacement test against a mock server, asserting the
  cancel/create ordering, auth headers, request bodies and signatures;
- validation tests that a crossed trigger is rejected before any mutation.

## Limitations

This crate is intentionally scoped to account queries and Perps TP/SL. It does
not place prediction-market orders, Perps entry orders, withdrawals or transfers.
It is not financial advice; use test-sized amounts first.

## License

MIT
