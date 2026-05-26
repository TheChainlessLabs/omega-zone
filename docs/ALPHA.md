# Private Alpha Runbook — OALPHA / PATH.USD Market

This runbook brings a frontend tester up to a usable OALPHA / PATH.USD
market on the private-alpha zone. It is **not** a production faucet flow:
amounts are small, the maker is a single shared wallet, and the steps
assume one well-known portal and one well-known darkpool.

If you only want the TL;DR: set the env vars in
[Required environment](#required-environment), then run:

```bash
just alpha-setup
```

That single recipe verifies / enables OALPHA on the portal, prefunds and
deposits a USER and a MAKER wallet, places one resting bid and one
resting ask around price 1, and prints the resulting frontend-visible
state.

## Pinned addresses

These are the only addresses the alpha recipes touch. They are pinned in
the Justfile (`alpha_*` variables) so a stray env var cannot redirect
setup at the moderato shared portal or at the wrong TIP-20 alias.

| What           | Address                                              |
|----------------|------------------------------------------------------|
| Zone portal    | `0xA6b5f8aF076DaAFBfd373a2629e4E46c8e03e6b2`         |
| Zone ID        | `35`                                                 |
| Chain ID       | `421700035`                                          |
| OALPHA (base)  | `0x20C000000000000000000000518dDADD37eD1d28`         |
| PATH.USD (quote) | `0x20C0000000000000000000000000000000000000`       |
| Darkpool       | `0x0B00000000000000000000000000000000000001`         |

## ⚠ Do not use the `alphausd` alias

The older `alphausd` alias used by `just enable-token` / `just create-zone`
resolves to **`0x20C0000000000000000000000000000000000001`** — a separate
moderato-wide test TIP-20, not the private-alpha OALPHA. Using it on the
alpha portal silently sets up the *wrong* market and confused several
earlier bring-ups.

The alpha recipes refuse this alias:

```text
$ just alpha-resolve-token alphausd
ERROR: 'alphausd' resolves to 0x20C0000000000000000000000000000000000001,
       which is NOT the private-alpha OALPHA token.
       Private alpha uses OALPHA = 0x20C000000000000000000000518dDADD37eD1d28.
       Use the 'oalpha' alias or the explicit address.
```

Pass `oalpha` (or the full OALPHA address) instead.

## Approvals: portal yes, darkpool no

Two TIP-20 spenders show up in this flow — the **L1 portal** and the
**zone darkpool** — and they behave differently:

- **L1 portal (alpha-approve-portal)**: a standard L1 TIP-20 allowance.
  Required. The portal calls `transferFrom(user, portal, amount)` when
  it pulls deposit funds, so without `approve(portal, ...)` the deposit
  reverts. The `alpha-deposit` step depends on `alpha-approve-portal`
  having run first for the same key.
- **Zone darkpool (no recipe needed)**: not required for the EOA
  main-key flow this runbook assumes. The darkpool precompile pulls
  base/quote from the maker via `system_transfer_from`, which is the
  precompile-only privileged path on the zone TIP-20 — it does **not**
  read a TIP-20 allowance. For an EOA signing with its main key, the
  AccountKeychain spending-limit check also short-circuits to allow.
  See `crates/precompiles/src/orderbook.rs::place` and the integration
  test `crates/tempo-zone/tests/it/precompiles.rs::test_darkpool_resting_bid_escrow_is_not_withdrawable`,
  which places a resting bid with no preceding TIP-20 approve.

The only zone-side prerequisite for `alpha-seed-liquidity` is that the
maker holds enough OALPHA + pathUSD on the zone to cover the escrow
(`amount * bid_price` pathUSD for the bid, `amount` OALPHA for the ask).
`alpha-setup` deposits both before seeding.

> If a future change moves the alpha flow onto session keys / access
> keys (i.e., `transaction_key != ZERO`), AccountKeychain spending
> limits start applying and the maker will need a properly-scoped
> access key, not a TIP-20 approve. Revisit this section before doing
> that.

## Required environment

```bash
export L1_RPC_URL="wss://rpc.moderato.tempo.xyz"
export ZONE_RPC_URL="http://localhost:8546"   # or the alpha zone's public RPC

# Sequencer key for the alpha portal — only needed when OALPHA is not yet
# enabled. After step 1 succeeds you can drop it.
export SEQUENCER_KEY="0x<alpha-sequencer-key>"

# Frontend tester (USER) and maker (MAKER) wallets. Both must already
# hold OALPHA on L1 (the alpha admin pre-mints; see "OALPHA on L1" below).
export USER_KEY="0x<frontend-tester-key>"
export MAKER_KEY="0x<resting-liquidity-key>"
```

`USER_KEY` is what a frontend tester will use to sign transactions in the
UI. `MAKER_KEY` is a separate wallet that owns the resting bid/ask so the
order book is non-empty before anyone connects.

## OALPHA on L1

The alpha recipes do not mint OALPHA — that is a one-time admin step
held by whoever has `ISSUER_ROLE` on the OALPHA contract. Before running
`alpha-setup`, the alpha admin should pre-mint OALPHA to both USER and
MAKER on L1:

```bash
export PRIVATE_KEY="0x<oalpha-admin-key>"
just mint-tokens 0x20C000000000000000000000518dDADD37eD1d28 <user-address>  100000000
just mint-tokens 0x20C000000000000000000000518dDADD37eD1d28 <maker-address> 100000000
```

`pathUSD` does not need pre-minting — `alpha-setup` calls
`tempo_fundAddress` on the moderato faucet to fund both wallets.

## One-shot setup

```bash
just alpha-setup
```

What it does:

1. `alpha-enable-oalpha` — checks `isTokenEnabled(OALPHA)` on the alpha
   portal and calls `enableToken(OALPHA)` (signed by `SEQUENCER_KEY`) if
   needed. Idempotent — safe to re-run.
2. `alpha-prefund-l1` for USER and MAKER — funds each with pathUSD on L1
   via `tempo_fundAddress`.
3. USER `alpha-approve-portal` + `alpha-deposit` — max-approves the
   alpha portal for pathUSD and OALPHA, then deposits both into zone 35.
4. MAKER `alpha-approve-portal` + `alpha-deposit` — same flow for the
   maker wallet.
5. `alpha-seed-liquidity` — MAKER places a resting bid (price=1) and a
   resting ask (price=2) for OALPHA against pathUSD on the alpha darkpool.
6. `alpha-state` — prints USER/MAKER L1 and zone balances plus
   `bestBid(OALPHA)` / `bestAsk(OALPHA)` so the tester can confirm the UI
   has a market to trade against.

Defaults (override as positional args to `just alpha-setup`):

| Arg               | Default     | Meaning                                     |
|-------------------|-------------|---------------------------------------------|
| `oalpha_amount`   | `10000000`  | OALPHA deposited per wallet (10 OALPHA)     |
| `pathusd_amount`  | `10000000`  | pathUSD deposited per wallet (10 PATH.USD)  |
| `seed_amount`     | `1000000`   | Maker order quantity (1 OALPHA per side)    |
| `bid_price`       | `1`         | Resting bid price                            |
| `ask_price`       | `2`         | Resting ask price (must be > `bid_price`)   |

## Re-running individual steps

If something fails partway through, the sub-recipes are safe to call on
their own:

```bash
# Verify a token's portal-enablement status
just alpha-token-status oalpha
just alpha-token-status pathusd

# Re-enable OALPHA (no-op if already enabled)
just alpha-enable-oalpha

# Top up either wallet on L1
just alpha-prefund-l1 <address>

# Deposit more from the current PRIVATE_KEY wallet
PRIVATE_KEY="$USER_KEY"  just alpha-deposit 5000000 5000000
PRIVATE_KEY="$MAKER_KEY" just alpha-deposit 5000000 5000000

# Add more resting liquidity (does not cancel existing orders)
just alpha-seed-liquidity 500000 1 2

# Re-print the frontend state
just alpha-state
```

## Acceptance check

After `just alpha-setup` completes you should see:

- `pathusd ... enabled=true`
- `oalpha  ... enabled=true`
- Both USER and MAKER showing nonzero zone balances for both tokens
- `best bid (price, quantity): 1 1000000` (or whichever bid_price/seed_amount you passed)
- `best ask (price, quantity): 2 1000000`

If `bestBid` or `bestAsk` come back as `0 0`, `alpha-seed-liquidity`
failed (most often: maker zone balances are below `seed_amount`, or
`MIN_ORDER_AMOUNT = 100` was violated). Re-run that step after topping
up the maker.

## Midpoint chart history (`zone_getMidpointHistory`)

The private RPC backs the alpha frontend chart with an in-memory aggregate
midpoint sampler. It reads the darkpool's aggregate top-of-book
(`bestBid` / `bestAsk`) on a fixed cadence and stores `(timestamp, midpoint)`
samples — no account, order id, maker, taker, or fill-level data.

Response contract:

- `history.enabled` is `true` whenever the sampler is wired (i.e., the
  private RPC has booted). The frontend can render the chart.
- `samples` is empty when the book has never had two-sided liquidity since
  process start — the sampler skips writes when either side is missing.
- `pair`, `base`, and `quote` echo the canonical alpha market.

Supported `interval` labels (bucket size in parentheses):

| Label | Bucket |
|-------|--------|
| `"1m"` | 60 s   |
| `"5m"` | 300 s  |
| `"1h"` | 3600 s |

Any other value returns `invalid_params`. Within a bucket the last observed
midpoint wins.

Retention: the store keeps roughly 12 hours of raw samples at the default
15-second sampler cadence, then evicts oldest-first. History is in-process
and **does not survive a node restart** — for alpha that is acceptable;
post-alpha the indexer should persist samples.

Pagination: `next_cursor` is a hex `U64` of the oldest `bucket_end` in the
current page when older buckets exist. Re-issue the call with that value as
`cursor` to walk further back. Limits cap at `5000` samples per page; the
default is `500`.

Unsupported pairs (anything other than `OALPHA/PATH.USD`) still return the
existing unsupported-pair `invalid_params` error.

## Public reference price (alpha guardrail, not an oracle)

The private RPC publishes a single public method for the canonical
`OALPHA/PATH.USD` pair:

```text
zone_getReferencePrice([{ "base": "<OALPHA>", "quote": "<PATH.USD>" }])
```

This is **alpha infrastructure**, not a production oracle. When the sequencer
has configured a reference price, the response includes the current snapshot
(price, source, block, timestamp), the configured guardrail bounds
(`maxDeviationBps`, `maxStalenessSecs`), and a `fresh` flag. When no provider
is configured, the response is `{ "enabled": false, "reason": "…" }` and the
frontend MUST treat that as "no public reference price available" — do not
fall back to a private orderbook midpoint and pretend it is an oracle.

### Units

Prices are raw integer values matching the darkpool orderbook precompile.
For a trade of `baseAmount` base tokens at `price`:

```text
quoteAmount = baseAmount * price
```

(No additional decimals scaling — both base and quote are TIP-20 6-decimal
tokens, and the precompile does not apply implicit decimal adjustments.)

### Guardrail semantics

The guardrail helper in `zone_precompiles::refprice` rejects orders whose
limit price deviates from the configured reference by more than
`maxDeviationBps`. `maxStalenessSecs = 0` disables the freshness check, which
is the natural setting for a static provider during alpha. Frontends can
pre-flight an order against the published reference; the RPC reports the same
rejection categories the helper uses (`ProviderDisabled`, `StaleReference`,
`OutOfRange`, `ZeroReferencePrice`).

The guardrails are advisory in the current build — the precompile does not
yet reject orders on-chain. Wiring enforcement into the orderbook precompile
is a separate change once a non-static provider lands.

### Enabling the static provider

The alpha node defaults to the disabled state (no provider). Operators opt
in via CLI flags / env vars on `tempo-zone`:

| Flag                                | Env var                          | Default          | Notes                                                                  |
|-------------------------------------|----------------------------------|------------------|------------------------------------------------------------------------|
| `--ref-price.static-price <u128>`   | `REF_PRICE_STATIC_PRICE`         | unset (disabled) | When unset, `zone_getReferencePrice` returns `enabled: false`.         |
| `--ref-price.source <string>`       | `REF_PRICE_SOURCE`               | `static:alpha`   | Origin tag surfaced verbatim to clients.                               |
| `--ref-price.max-deviation-bps <u32>` | `REF_PRICE_MAX_DEVIATION_BPS`  | `1000` (±10%)    | Order-vs-reference bound in basis points.                              |
| `--ref-price.max-staleness-secs <u64>` | `REF_PRICE_MAX_STALENESS_SECS` | `0` (no expiry)  | `0` is the natural setting for a static provider.                      |

Example: pin the OALPHA/PATH.USD static reference at `1` with the default
±10% bound:

```bash
tempo-zone \
  --ref-price.static-price 1 \
  --ref-price.source static:alpha \
  --ref-price.max-deviation-bps 1000 \
  ...
```

Omitting `--ref-price.static-price` keeps the provider disabled. The other
three flags are inert until a price is supplied.

## Troubleshooting

| Symptom                                                    | Likely cause / fix                                                                                          |
|------------------------------------------------------------|-------------------------------------------------------------------------------------------------------------|
| `ERROR: 'alphausd' resolves to 0x20C0…0001`                | You passed the wrong alias — use `oalpha`.                                                                  |
| `isTokenEnabled(OALPHA) = false` after `alpha-enable-oalpha` | `SEQUENCER_KEY` does not match the alpha portal's sequencer. Check `zone-info 35`.                          |
| `alpha-deposit` reverts                                    | The signing wallet has no OALPHA on L1. Ask the alpha admin to `mint-tokens` to that address.               |
| `alpha-seed-liquidity` reverts on the bid                  | Maker has not deposited enough pathUSD into the zone (bid escrows `amount * bid_price` pathUSD).            |
| `alpha-seed-liquidity` reverts on the ask                  | Maker has not deposited enough OALPHA into the zone (ask escrows `amount` OALPHA).                          |
| `best bid` and `best ask` show `0 0`                       | No resting liquidity. Re-run `alpha-seed-liquidity` after confirming maker zone balances cover the escrow.  |
