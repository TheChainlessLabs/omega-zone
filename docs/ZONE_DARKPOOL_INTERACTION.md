# Zone and Darkpool Interaction Guide

This guide shows how to interact with a running Tempo Zone and its darkpool
orderbook from the command line. It assumes the zone node exposes:

- public zone RPC on `http://localhost:8546`
- private zone RPC on `http://localhost:8544`
- darkpool precompile at `0x0B00000000000000000000000000000000000001`

The examples use the common test tokens:

| Token | Address |
|---|---|
| pathUSD | `0x20C0000000000000000000000000000000000000` |
| alphaUSD | `0x20C0000000000000000000000000000000000001` |

Amounts are raw token sub-units. These test tokens use 6 decimals, so
`1000000` means `1.0` token.

## Environment

```bash
export ZONE_NAME=my-zone
export L1_RPC_URL=https://rpc.moderato.tempo.xyz
export ZONE_RPC_URL=http://localhost:8546
export PRIVATE_ZONE_RPC_URL=http://localhost:8544

export PATHUSD=0x20C0000000000000000000000000000000000000
export ALPHAUSD=0x20C0000000000000000000000000000000000001
export DARKPOOL=0x0B00000000000000000000000000000000000001
export OUTBOX=0x1c00000000000000000000000000000000000002

export L1_PORTAL_ADDRESS=$(jq -r '.portal' "generated/$ZONE_NAME/zone.json")
export PRIVATE_KEY=0x...
export ADDR=$(cast wallet address "$PRIVATE_KEY")
```

Check the zone is reachable:

```bash
cast chain-id --rpc-url "$ZONE_RPC_URL"
cast block-number --rpc-url "$ZONE_RPC_URL"
```

## L1 to Zone Deposits

The portal must have the token enabled before it can accept deposits:

```bash
cast call "$L1_PORTAL_ADDRESS" "isTokenEnabled(address)(bool)" "$PATHUSD" \
  --rpc-url "$L1_RPC_URL"
```

Approve the portal on L1:

```bash
cast send "$PATHUSD" "approve(address,uint256)" \
  "$L1_PORTAL_ADDRESS" "$(cast max-uint)" \
  --rpc-url "$L1_RPC_URL" \
  --private-key "$PRIVATE_KEY" \
  --gas-limit 500000
```

Deposit to your zone account:

```bash
cast send "$L1_PORTAL_ADDRESS" "deposit(address,address,uint128,bytes32)" \
  "$PATHUSD" "$ADDR" 10000000 \
  0x0000000000000000000000000000000000000000000000000000000000000000 \
  --rpc-url "$L1_RPC_URL" \
  --private-key "$PRIVATE_KEY" \
  --gas-limit 900000
```

Check the zone balance after the zone processes the L1 deposit:

```bash
cast call "$PATHUSD" "balanceOf(address)(uint256)" "$ADDR" \
  --from "$ADDR" \
  --rpc-url "$ZONE_RPC_URL"
```

The `--from "$ADDR"` matters because zone TIP-20 balance reads are
caller-scoped.

## Private RPC Auth

The private RPC requires an `X-Authorization-Token` header. The easiest path is
the helper recipe:

```bash
export TOKEN=$(just zone-auth-token "$ZONE_NAME")

cast rpc zone_getAuthorizationTokenInfo \
  --rpc-url "$PRIVATE_ZONE_RPC_URL" \
  --rpc-headers "X-Authorization-Token: $TOKEN"
```

The private RPC never owns signing keys. Use it for authenticated reads and for
submitting already-signed raw transactions. Use the public zone RPC for simple
`cast send` flows in local tests.

Private TIP-20 balance read:

```bash
DATA=$(cast calldata "balanceOf(address)" "$ADDR")

cast rpc eth_call \
  "{\"from\":\"$ADDR\",\"to\":\"$PATHUSD\",\"data\":\"$DATA\"}" \
  latest \
  --rpc-url "$PRIVATE_ZONE_RPC_URL" \
  --rpc-headers "X-Authorization-Token: $TOKEN"
```

## Darkpool Model

The darkpool is an in-process zone precompile, not a Solidity contract
deployment. It pulls TIP-20 tokens directly into internal darkpool balances and
tracks resting-order escrow internally.

Important rules:

- prices are six-decimal fixed-point integers: `quote = floor(baseAmount * price / 1_000_000)`
- bid escrow is `ceil(amount * price / 1_000_000)` in quote token
- ask escrow is `amount` in base token
- `availableBalanceOf` excludes resting-order escrow
- stable time-in-force values are GTC `0`, IOC `1`, and FOK `2`
- limit orders currently accept GTC only; market orders accept IOC or FOK
- every accepted limit or market order creates an order id and emits `OrderSubmitted`
- resting GTC residuals also emit `OrderPlaced`
- each consumed resting leg emits one `OrderFilled` with maker and taker order ids
- IOC succeeds with full, partial, or zero fill and emits `OrderCancelled` for any remainder
- FOK fills the entire amount inside its protection price or reverts atomically

Darkpool write methods:

| Method | Purpose |
|---|---|
| `deposit(address token, uint128 amount)` | Pull TIP-20 tokens from the caller's zone wallet into internal darkpool balance. |
| `withdraw(address token, uint128 amount)` | Move available internal darkpool balance back to the caller's zone wallet. |
| `place(address base, address quote, uint128 amount, uint128 price, bool isBid, uint8 timeInForce)` | Submit a GTC limit bid or ask. Bids use `isBid=true`; asks use `false`. |
| `cancel(uint128 orderId)` | Cancel a live resting order owned by the caller. |
| `marketBuy(address base, address quote, uint128 amount, uint128 maxPrice, uint8 timeInForce)` | Buy base from resting asks at or below the per-unit maximum price, using IOC or FOK. |
| `marketSell(address base, address quote, uint128 amount, uint128 minPrice, uint8 timeInForce)` | Sell base into resting bids at or above the per-unit minimum price, using IOC or FOK. |

Darkpool read methods:

| Method | Purpose |
|---|---|
| `getOrder(uint128 orderId)` | Owner-scoped live resting-order read. |
| `balanceOf(address user, address token)` | Owner-scoped total internal balance. |
| `availableBalanceOf(address user, address token)` | Owner-scoped internal balance excluding resting-order escrow. |
| `pairKey(address base, address quote)` | Pure pair-key helper. |
| `createPair(address base, address quote)` | Explicit pair creation; `place` also lazily creates pairs. |
| `pairCount()` | Number of markets currently registered by `createPair` or `place`. |
| `pairAt(uint256 index)` | Base and quote addresses for a registered market. |
| `pairExists(address base, address quote)` | Whether the exact market is registered. |
| `bestBid(address base)` | Aggregate best bid for readiness/debugging. |
| `bestAsk(address base)` | Aggregate best ask for readiness/debugging. |
| `MIN_ORDER_AMOUNT()` | Current dust floor. |

## Darkpool Balances

Deposit into the darkpool internal balance:

```bash
cast send "$DARKPOOL" "deposit(address,uint128)" "$PATHUSD" 5000000 \
  --rpc-url "$ZONE_RPC_URL" \
  --private-key "$PRIVATE_KEY" \
  --gas-limit 4000000
```

Read total internal balance:

```bash
cast call "$DARKPOOL" "balanceOf(address,address)(uint128)" \
  "$ADDR" "$PATHUSD" \
  --from "$ADDR" \
  --rpc-url "$ZONE_RPC_URL"
```

Read available, non-escrowed internal balance:

```bash
cast call "$DARKPOOL" "availableBalanceOf(address,address)(uint128)" \
  "$ADDR" "$PATHUSD" \
  --from "$ADDR" \
  --rpc-url "$ZONE_RPC_URL"
```

Withdraw available internal balance back to your zone TIP-20 wallet:

```bash
cast send "$DARKPOOL" "withdraw(address,uint128)" "$PATHUSD" 1000000 \
  --rpc-url "$ZONE_RPC_URL" \
  --private-key "$PRIVATE_KEY" \
  --gas-limit 4000000
```

## Limit Orders

Place a limit ask: sell `alphaUSD` for `pathUSD`.

```bash
cast send "$DARKPOOL" "place(address,address,uint128,uint128,bool,uint8)" \
  "$ALPHAUSD" "$PATHUSD" 1000000 2000000 false 0 \
  --rpc-url "$ZONE_RPC_URL" \
  --private-key "$PRIVATE_KEY" \
  --gas-limit 4000000
```

Place a limit bid: buy `alphaUSD` with `pathUSD`.

```bash
cast send "$DARKPOOL" "place(address,address,uint128,uint128,bool,uint8)" \
  "$ALPHAUSD" "$PATHUSD" 1000000 1000000 true 0 \
  --rpc-url "$ZONE_RPC_URL" \
  --private-key "$PRIVATE_KEY" \
  --gas-limit 4000000
```

The return value and `OrderSubmitted` event contain the accepted order id. To
read a live resting order:

```bash
cast call "$DARKPOOL" "getOrder(uint128)((uint128,address,address,address,bool,uint128,uint128))" \
  1 \
  --from "$ADDR" \
  --rpc-url "$ZONE_RPC_URL"
```

`getOrder` is owner-scoped. It reverts for another maker's order. Filled and
cancelled orders are no longer live resting orders; use the private RPC history
methods, such as `zone_getOrder`, for event-derived status.

Cancel your resting order:

```bash
cast send "$DARKPOOL" "cancel(uint128)" 1 \
  --rpc-url "$ZONE_RPC_URL" \
  --private-key "$PRIVATE_KEY" \
  --gas-limit 1000000
```

## Market Orders

Market orders consume existing opposite-side liquidity. They never rest on the
book.

Market buy: buy `amount` of base with a per-unit `maxPrice`. The final argument
is FOK (`2`); use IOC (`1`) to accept a partial or zero fill.

```bash
cast send "$DARKPOOL" "marketBuy(address,address,uint128,uint128,uint8)" \
  "$ALPHAUSD" "$PATHUSD" 1000000 2000000 2 \
  --rpc-url "$ZONE_RPC_URL" \
  --private-key "$PRIVATE_KEY" \
  --gas-limit 4000000
```

Market sell: sell `amount` of base with a per-unit `minPrice`.

```bash
cast send "$DARKPOOL" "marketSell(address,address,uint128,uint128,uint8)" \
  "$ALPHAUSD" "$PATHUSD" 1000000 1000000 2 \
  --rpc-url "$ZONE_RPC_URL" \
  --private-key "$PRIVATE_KEY" \
  --gas-limit 4000000
```

Use explicit per-unit guards. `maxPrice=0` makes a market buy unable to fill
any nonzero-priced ask. `minPrice=0` means no minimum for a market sell. Buy
escrow is `ceil(amount * maxPrice / 1_000_000)`; price improvement remains
available in the caller's internal darkpool balance.

## Unified Private Activity

Use authenticated `zone_getMyActivity` as the primary portfolio/activity read:

```bash
cast rpc zone_getMyActivity \
  '{"cursor":null,"limit":50}' \
  --rpc-url "$PRIVATE_ZONE_RPC_URL" \
  --rpc-headers "X-Authorization-Token: $TOKEN"
```

The response is a newest-first `{ items, nextCursor, indexedThrough }` page.
Items merge owner-scoped order summaries, individual fills, transfers,
deposits, and withdrawals across Zone and Tempo. Order and fill payloads carry
`orderType` and `timeInForce`; `indexedThrough` reports the actual Zone and
Tempo tips used for the page. Pass `nextCursor` unchanged for the next stable
page.

`zone_getMyOrders`, `zone_getMyFills`, and `zone_getMyTransfers` remain
available to consumers that need a single record type. Fill reconstruction uses
the maker and taker order ids carried by `OrderFilled`; clients must not scan
foreign submissions or decode private transaction calldata.

## Book Reads

Read aggregate top-of-book from the public zone RPC:

```bash
cast call "$DARKPOOL" "bestBid(address,address)(uint128,uint128)" "$ALPHAUSD" "$PATHUSD" \
  --rpc-url "$ZONE_RPC_URL"

cast call "$DARKPOOL" "bestAsk(address,address)(uint128,uint128)" "$ALPHAUSD" "$PATHUSD" \
  --rpc-url "$ZONE_RPC_URL"
```

Read market metadata from the private RPC:

```bash
cast rpc zone_getMarketConfig \
  --rpc-url "$PRIVATE_ZONE_RPC_URL" \
  --rpc-headers "X-Authorization-Token: $TOKEN"
```

Read top-of-book through the private RPC:

```bash
cast rpc zone_getTopOfBook \
  '{"base":"0x20C0000000000000000000000000000000000001","quote":"0x20C0000000000000000000000000000000000000"}' \
  --rpc-url "$PRIVATE_ZONE_RPC_URL" \
  --rpc-headers "X-Authorization-Token: $TOKEN"
```

Market RPCs accept any exact `(base, quote)` pair registered in the darkpool.
They reject pairs that `pairExists(base, quote)` reports as absent. Market
labels and token decimals come from each TIP-20 contract rather than RPC-local
constants.

## Private Order Status

`zone_getOrder` reconstructs order status from the darkpool event stream. It is
the right read path for orders that may have filled or cancelled.

```bash
cast rpc zone_getOrder 0x1 \
  --rpc-url "$PRIVATE_ZONE_RPC_URL" \
  --rpc-headers "X-Authorization-Token: $TOKEN"
```

Expected status values include `open`, `partiallyFilled`, `filled`, and
`cancelled`.

## Zone to L1 Withdrawals

For a normal zone TIP-20 withdrawal to L1, approve the outbox on the zone:

```bash
cast send "$PATHUSD" "approve(address,uint256)" "$OUTBOX" "$(cast max-uint)" \
  --rpc-url "$ZONE_RPC_URL" \
  --private-key "$PRIVATE_KEY" \
  --gas-limit 500000
```

Request the withdrawal:

```bash
cast send "$OUTBOX" \
  "requestWithdrawal(address,address,uint128,bytes32,uint64,address,bytes,bytes)" \
  "$PATHUSD" "$ADDR" 1000000 \
  0x0000000000000000000000000000000000000000000000000000000000000000 \
  0 "$ADDR" 0x 0x \
  --rpc-url "$ZONE_RPC_URL" \
  --private-key "$PRIVATE_KEY" \
  --gas-limit 2000000
```

End-to-end withdrawal success means the sequencer submitted a batch and the L1
portal emitted `WithdrawalProcessed`, not just that the zone-side request
transaction succeeded.

## Full E2E Reference

The live scripted flow is in `e2e/account-flow.sh`. It covers:

- L1 funding, approval, and deposit
- maker liquidity for market orders
- `marketBuy` and `marketSell`
- limit bid and ask placement
- private `zone_getOrder` assertions
- zone withdrawal requests
- L1 `WithdrawalProcessed` settlement checks
- final public and private balance assertions

Run it with:

```bash
WAIT_TIMEOUT_SECONDS=1800 \
ZONE_NAME=my-zone \
L1_RPC_URL=https://rpc.moderato.tempo.xyz \
ZONE_RPC_URL=http://localhost:8546 \
PRIVATE_ZONE_RPC_URL=http://localhost:8544 \
./e2e/account-flow.sh
```
