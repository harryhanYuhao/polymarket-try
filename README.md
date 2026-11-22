# pm-try

A small Rust CLI (`pm`, built with clap) for Polymarket that:

1. lists your **active orders** on prediction markets and perps,
2. queries **positions on both prediction markets and perps** (plus public market listings for both),
3. **creates or modifies take-profit / stop-loss** on an open perps position.

It talks to four Polymarket APIs:

| API | Base URL | Used for |
|---|---|---|
| CLOB | `https://clob.polymarket.com` | your live prediction orders (L2 HMAC auth), API-key derivation (L1 EIP-712) |
| Data API v2 | `https://data-api.polymarket.com` | prediction positions |
| Gamma | `https://gamma-api.polymarket.com` | market questions, market search |
| Perps | `https://api.perpetuals.polymarket.com` | instruments, tickers, portfolio, open orders, signed TP/SL orders |

## Build

```bash
cargo build --release
```

The binary is `target/release/pm` (or `cargo install --path .` to put `pm` on your `PATH`).

## Credentials

Everything is configured through `PM_*` environment variables, read from the environment or
from `./.env` (or `--env-file PATH`). Real environment variables win over the file. Secrets are
never accepted as command-line flags. Copy [.env.example](.env.example) to `.env` and
`chmod 600 .env`.

| Variable | Needed for |
|---|---|
| `PM_PRIVATE_KEY` | only to create credentials (`pm prediction api-key`, `pm perps proxy create`) |
| `PM_ADDRESS` | signer address for CLOB requests (derived from `PM_PRIVATE_KEY` if unset) |
| `PM_WALLET` | the Polymarket wallet that holds your prediction positions (defaults to `PM_ADDRESS`) |
| `PM_CLOB_API_KEY`, `PM_CLOB_SECRET`, `PM_CLOB_PASSPHRASE` | prediction orders |
| `PM_PERPS_PROXY`, `PM_PERPS_SECRET` | perps reads (positions, orders) |
| `PM_PERPS_PROXY_KEY` | signing perps TP/SL orders (the proxy address is derived from it) |

One-time setup, with `PM_PRIVATE_KEY` set to the EOA that controls your Polymarket account:

```bash
pm prediction api-key --out .env      # derive CLOB API credentials (add --create if none exist yet)
```

```bash
pm perps proxy create --out .env      # register a Perps proxy signer (7 days by default, --days N)
```

`--out` writes the variables into the file (created with mode 0600, replacing older values of
the same variables). Without `--out` they are printed. After that you can drop `PM_PRIVATE_KEY`
from `.env`: day-to-day commands only need the API credentials and the proxy key.

## Usage

### Active orders (prediction + perps)

```bash
pm orders                                  # both venues
pm orders --venue perps --instrument BTC   # one venue, filtered
pm prediction orders --wallet              # every live order on your deposit wallet
pm perps orders
```

With `--venue all` (the default), a venue without credentials is skipped with a hint instead of
failing the command.

### Positions and markets (prediction + perps)

```bash
pm positions                        # prediction (Data API) + perps (portfolio, mark, attached TP/SL)
pm positions --venue prediction --status redeemable
pm perps positions
pm perps markets --search btc       # instruments with mark / index / funding (no credentials needed)
pm prediction markets --search "fed rates"   # or omit --search for the top markets by 24h volume
```

### Take-profit / stop-loss on a perps position

```bash
pm perps tpsl set BTC --tp 71000 --sl 59500              # create or modify; asks for confirmation
pm perps tpsl set BTC-USD --sl 60500 --dry-run           # show the plan and signed requests only
pm perps tpsl modify ETH --tp 2300 --yes                 # aliases: create, modify; --yes skips the prompt
pm perps tpsl cancel BTC --sl                            # cancel just the stop-loss (default: both)
pm perps tpsl list
```

Example plan:

```
BTC-USD: LONG 0.02 @ 62000 (liquidation 50120.5)
Mark price: 63417.4
  cancel  take-profit @ 69000 (position, #111)
  cancel  stop-loss @ 58000 (armed bracket leg, #222)
  create  take-profit @ 71000 (whole position, market exit)
  create  stop-loss @ 59500 (whole position, market exit)
TP/SL cannot be edited in place: old triggers are cancelled first, then replaced.
Send these orders? [y/N]
```

How it works:

* Triggers are **position-scoped**: they watch the **mark price** and close the **whole position**
  with a reduce-only market order when hit (`grp: "position"`, `qty: "0"`, `tr.market: true`).
  The closing side is derived from the position: sell for a long, buy for a short.
* Perps **cannot edit TP/SL in place**. To modify one, `pm` cancels the existing trigger of that
  kind and then creates the replacement, as the Perps docs prescribe. The two steps are not atomic.
  If the replacement is rejected after the cancel, `pm` says so loudly, because the position is
  then unprotected for that kind. A trigger already at the requested price is left alone.
* **Armed bracket legs** (TP/SL attached to an entry order that has already filled) are replaced
  by default, as the docs recommend. Pass `--keep-bracket` to leave them. **Dormant** bracket legs,
  whose entry order is still resting, are never touched.
* Checks run before anything is sent: an open position must exist; the take-profit must be above
  the mark for a long (below for a short) and the stop-loss on the other side, because a trigger
  the mark has already crossed would fire immediately and the exchange rejects it; price
  decimals and the 5-significant-figure rule are enforced; and you get a warning when a stop sits
  beyond the liquidation price.
* Nothing is sent without confirmation. Without a terminal, `--yes` is required.

`--json` works on every command and prints machine-readable output to stdout.

## Signing details

* **CLOB L1**: EIP-712 `ClobAuth(address address,string timestamp,uint256 nonce,string message)`
  in domain `ClobAuthDomain`/`1`/137, sent as `POLY_ADDRESS/SIGNATURE/TIMESTAMP/NONCE`.
* **CLOB L2**: `urlsafe_b64(HMAC_SHA256(b64decode(secret), ts + METHOD + path + body))`. The
  query string is not part of the signed path.
* **Perps trading**: the operation is reduced to the SDK's compact positional form, e.g.
  `["createOrders", [[iid, buy, qty, po, ro, [market, trp, "tp"]]], "position"]`, then
  MessagePack-encoded and hashed with keccak256. The resulting `data` hash is signed by the
  proxy key as EIP-712 `Op(bytes32 data,uint64 salt,uint64 ts)` in domain `Polymarket`/`1`/137.
  The JSON body sent alongside describes the same operation.
* **Perps proxy registration**: the owner signs EIP-712 `CreateProxy(address addr,uint64 exp,uint64 salt,uint64 ts)`.

## Tests

```bash
cargo test
```

* Unit tests include known-answer vectors from Polymarket's reference client (HMAC and the
  `ClobAuth` EIP-712 signature), byte-level MessagePack checks for the Perps compact operations,
  and the TP/SL planner.
* `tests/e2e.rs` runs the real `pm` binary against a local mock of the APIs. It checks that
  `tpsl set` cancels before it creates, that every request body matches its signed operation and
  recovers to the proxy address, that nothing is sent on `--dry-run` or without `--yes`, plus
  the failure reporting, CLOB L2 auth headers and pagination for every listing.

## Limitations

* The private endpoints (orders, portfolio, TP/SL submission, credential creation) were
  verified against the documented formats and a mock server, not against a live account.
  Try `--dry-run` and a small position first.
* Perps order placement is not available in some jurisdictions; see Polymarket's geographic
  restrictions.
* Only market-exit, whole-position TP/SL is supported. Partial-size and trailing stops are not.
