# Zone Account E2E

Runs a live account flow against a running zone:

1. create or reuse an account
2. create or reuse a maker account for resting market-order liquidity
3. request faucet funds on L1
3. approve and deposit `pathUSD` and `alphaUSD`
4. place maker liquidity and fill it with one market buy and one market sell
5. place one sell order and one buy order on the zone darkpool
6. withdraw half of the deposited token amounts
7. wait for L1 withdrawal settlement and `WithdrawalProcessed` events
8. print public status plus private-RPC account data

## Prerequisites

- `cast`, `jq`, and `curl` on `PATH`
- an L1 RPC with `tempo_fundAddress`
- a running zone RPC and private RPC
- `generated/<zone>/zone.json` and `generated/<zone>/genesis.json`
- the selected portal must have both `pathUSD` and `alphaUSD` enabled
- the faucet or account must hold enough `alphaUSD`; the standard faucet may only fund native gas plus `pathUSD`
- the zone node must run with sequencer settlement enabled for the default L1
  withdrawal settlement assertion
- private RPC methods must be available; the script fails if expected order events,
  auth-token info, or private balance reads are missing. Top-of-book data is
  validated when the selected pair is supported by the private RPC market config.

## Run

```bash
export ZONE_NAME=my-zone
export L1_RPC_URL=https://rpc.moderato.tempo.xyz
export ZONE_RPC_URL=http://localhost:8546
export PRIVATE_ZONE_RPC_URL=http://localhost:8544

./e2e/account-flow.sh
```

If `PRIVATE_KEY` is not set, the script creates a new wallet and stores it in
`e2e/.account.json` for reuse. To use a specific account:

```bash
PRIVATE_KEY=0x... ./e2e/account-flow.sh
```

If `MAKER_PRIVATE_KEY` is not set, the script creates a second wallet and
stores it in `e2e/.maker-account.json`. The maker account deposits enough
liquidity to seed one ask for `marketBuy` and one bid for `marketSell`.

If you are not using `ZONE_NAME`, provide all metadata needed for private RPC
authorization:

```bash
L1_PORTAL_ADDRESS=0x... \
ZONE_ID=51 \
ZONE_CHAIN_ID=421700051 \
./e2e/account-flow.sh
```

Useful overrides:

```bash
PATHUSD_AMOUNT=10000000 \
ALPHAUSD_AMOUNT=10000000 \
ORDER_AMOUNT=1000000 \
SELL_PRICE=2 \
BUY_PRICE=1 \
MARKET_ORDER_AMOUNT=1000000 \
MARKET_ASK_PRICE=2 \
MARKET_BID_PRICE=3 \
MARKET_BUY_MAX_QUOTE_IN=2000000 \
MARKET_SELL_MIN_QUOTE_OUT=3000000 \
MAKER_PATHUSD_AMOUNT=5000000 \
MAKER_ALPHAUSD_AMOUNT=1000000 \
./e2e/account-flow.sh
```

Amounts are token sub-units. The defaults assume 6-decimal test tokens.

The script estimates gas and adds headroom before each write. If estimation
fails, these fallbacks are used and can be overridden:

```bash
APPROVE_GAS_FALLBACK=500000 \
DEPOSIT_GAS_FALLBACK=900000 \
ORDER_GAS_FALLBACK=4000000 \
WITHDRAW_GAS_FALLBACK=2000000 \
./e2e/account-flow.sh
```

Set `VERIFY_L1_WITHDRAWAL_SETTLEMENT=0` only when intentionally running the
old zone-side request/debit smoke test without proving the L1 payout.
