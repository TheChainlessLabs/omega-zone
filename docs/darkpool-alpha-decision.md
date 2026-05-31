# Alpha darkpool implementation: keep the Rust precompile

**Status:** Accepted for alpha. Issue #9 is now a hardening tracker.

**Date:** 2026-05-26.

**Milestone:** Tempo testnet batching & settlement (alpha integration freeze).

## TL;DR

**Keep the Rust precompile.** Switching to Solidity for alpha is a larger
distraction than the wallet/tooling pain it removes, because:

1. The frontend's hardest darkpool problems were not contract-vs-precompile
   problems — they were keychain, gas-estimation, balance privacy, and access-
   key signing problems, all of which would carry forward to a Solidity port.
2. The precompile's outstanding correctness bug (storage persistence on first
   call) was fixed by
   [PR #1](https://github.com/TheChainlessLabs/omega-zone/pull/1), so the
   account initializes on first non-static call and its state persists.
3. With this branch's `zone_getMyOrders` / `zone_getMyFills` /
   `zone_getMyTransfers` / `zone_getOrder` private RPC methods, the
   precompile's existing indexed events are sufficient to reconstruct
   owner-scoped history without a separate indexer, removing the largest
   remaining alpha-blocker.

Solidity stays the long-term migration target — but it is a post-alpha
exercise, not a pre-freeze one.

## What we evaluated

Issue #9 framed the choice as A vs. B:

- **Option A — keep precompile.** Merge the storage-init fix, harden tests,
  document the ABI, and confirm events are indexable for owner-scoped
  history.
- **Option B — port to Solidity.** Re-implement the same ABI in Solidity,
  deploy at genesis, reuse standard ERC-20 approval and event semantics,
  defer the precompile as future optimization.

## Decision criteria

| Criterion | A (precompile) | B (Solidity) |
|---|---|---|
| Storage persistence fix | One small diff, already in PR #1 | N/A — Solidity uses normal account storage |
| Wallet / tooling predictability | Custom precompile address, custom call semantics | Standard contract, walkable in any explorer / wallet |
| Owner-scoped history feasibility | Already emits `OrderSubmitted`, `OrderPlaced`, `OrderFilled`, `OrderMatched`, `OrderCancelled` with maker / taker indexed | Identical events possible (same shape) |
| Frontend integration cost from here | Adopt new `zone_getMy*` methods, remove localStorage cache | Reimplement Rust precompile in Solidity, redeploy, then adopt new RPC methods anyway |
| Gas / latency | Native precompile speed | Solidity is the slower path; matters once volume is non-trivial |
| Audit surface | Existing Rust code, integration tests, sequencer-only TEE assumption | New Solidity contract requires its own review |
| Time to alpha freeze | Days (PR #1 + this branch) | Weeks (new contract + redeploy + frontend) |

The decisive lines are *time to freeze* and *non-substitutability of the
work*: every line of frontend work needed for Option B is also needed for
Option A. Option A is therefore strictly cheaper in calendar time.

## Required work to harden Option A (precompile path)

These are the acceptance gates that must be green before alpha integration
freeze, regardless of which side of the decision is chosen for the long
term:

1. **Merge the storage init fix.** PR #1 lazily initializes the darkpool
   precompile account on first non-static call so its storage persists.
   Without this, mutating calls succeed but state is dropped — every
   frontend session sees an empty book. Blocking.
2. **Owner-scoped history APIs.** Delivered by this branch:
   - `zone_getMyOrders(query?)`
   - `zone_getMyFills(query?)`
   - `zone_getMyTransfers(query?)`
   - `zone_getOrder(orderId)`
   Each enforces: caller-supplied `account` must equal the authenticated
   caller (`-32004` on mismatch), responses are reconstructed from
   `maker` / `taker` indexed topics, pagination is via an opaque
   `(block:logIndex)` cursor.
3. **Privacy invariant.** Verified by:
   - `crates/rpc/src/proxy.rs` ::
     `zone_get_my_orders_only_returns_callers_logs` — proves a foreign
     maker's logs are dropped even when the upstream returns them.
   - `crates/rpc/src/proxy.rs` ::
     `zone_get_my_orders_rejects_foreign_account_param` — proves
     `account != caller` is rejected before any upstream query.
   - `crates/rpc/src/proxy.rs` ::
     `zone_get_order_returns_null_for_other_owners_order` — proves
     `null` (not "not found") is returned for someone else's order, so
     existence is not leaked.
   - `crates/rpc/src/handlers.rs` ::
     `zone_get_my_{orders,fills,transfers}_rejects_foreign_account` —
     dispatch-level proof that the privacy gate runs before any
     handler-specific logic.
4. **Durable precompile tests.** The existing integration tests in
   `crates/tempo-zone/tests/it/precompiles.rs` cover availability,
   resting-bid escrow, self-crossing limit-order matching, multi-maker /
   multi-taker fill ordering, partial-fill reconstruction, and cancel after
   partial fill.
5. **ABI / selector / units doc.** The `sol!` block in
   `crates/precompiles/src/orderbook.rs` is the canonical ABI; the events
   are mirrored verbatim in `crates/rpc/src/darkpool.rs` for off-chain
   decoding. The alpha-facing reference is recorded below.

## Alpha ABI, selectors, units, and collateral

The alpha darkpool lives at
`0x0b00000000000000000000000000000000000001` on the zone. It is an
in-process zone precompile with marker bytecode for persistence, not deployed
Solidity bytecode.

Canonical write selectors:

| Selector | Signature | Notes |
|---|---|---|
| `0xb3fb6564` | `deposit(address,uint128)` | Pulls `amount` of `token` into the caller's internal darkpool balance. |
| `0x08fab167` | `withdraw(address,uint128)` | Withdraws available internal balance back to the caller's zone TIP-20 wallet. |
| `0xee60dde5` | `place(address,uint128,uint128,bool)` | Places a limit order for `base, amount, price, isBid`; returns the accepted order id. |
| `0x81649d06` | `cancel(uint128)` | Cancels a resting order owned by the caller. |
| `0x7345f144` | `marketBuy(address,uint128,uint128)` | Buys exact `amount` of base, spending up to `maxQuoteIn`. |
| `0xf005c804` | `marketSell(address,uint128,uint128)` | Sells exact `amount` of base, receiving at least `minQuoteOut`. |

Canonical read selectors:

| Selector | Signature | Notes |
|---|---|---|
| `0x117d4128` | `getOrder(uint128)` | Owner-scoped live resting-order read; filled/cancelled orders are reconstructed through private RPC history. |
| `0xf7888aec` | `balanceOf(address,address)` | Owner-scoped total internal balance. |
| `0x2a7575ee` | `availableBalanceOf(address,address)` | Owner-scoped internal balance minus resting-order escrow. |
| `0xcd27ca82` | `pairKey(address,address)` | Pure pair-key helper. |
| `0x9ccb0744` | `createPair(address)` | Explicit pair creation; `place` also lazily creates pairs. |
| `0x835801d7` | `bestBid(address)` | Aggregate top bid for alpha readiness only. |
| `0x64d5a61c` | `bestAsk(address)` | Aggregate top ask for alpha readiness only. |
| `0x40bf2aa4` | `MIN_ORDER_AMOUNT()` | Dust floor, currently `100`. |

Prices are raw integer quote-per-base units. The precompile does not apply
token decimals; callers and frontends format decimals at the edge. For the
alpha OALPHA/pathUSD pair, `base` is OALPHA and `quote` is pathUSD.

Collateral is reserved by side:

- Bid escrow: `amount * price` in quote token.
- Ask escrow: `amount` in base token.
- Filled bid takers pay the resting maker's price, not necessarily their
  submitted limit price.
- `availableBalanceOf` excludes all resting escrow. Cancelling a partially
  filled order releases only the unfilled residual.

Accepted limit orders emit `OrderSubmitted` before matching. Residual resting
orders additionally emit `OrderPlaced`. Each consumed resting leg emits
`OrderFilled`, and limit-order matches also emit `OrderMatched` to link the
maker order id and taker submission id for owner-scoped history.

## Top-of-book stance for alpha

`bestBid`, `bestAsk`, and private RPC `zone_getTopOfBook` remain temporary
alpha readiness surfaces. They exist so the frontend and runbooks can confirm
seed liquidity and live crossing behavior without a standalone indexer.

Strict darkpool/privacy claims must not depend on public top-of-book
visibility. Before beta, either remove/gate the aggregate surface or explicitly
productize it as a public market-data feature with updated privacy language.

## What we defer

- **Solidity port.** Tracked as future work. Acceptable trigger to revisit:
  (a) the precompile blocks a new event we cannot add without a Tempo fork,
  or (b) a wallet / tooling integration requires a fully ABI-discoverable
  contract that the precompile address cannot satisfy.
- **A standalone indexer.** Not needed for alpha; the private RPC's
  log-scan path is sufficient under the alpha load profile (single-digit
  TPS, single zone, weeks of retention). Revisit at the start of the
  beta milestone if `eth_getLogs` latency degrades or the retention
  window is exceeded.

## Acceptance checks for closing issue #9

- [x] Decision recorded with rationale (this document).
- [x] Non-chosen path explicitly deferred with re-evaluation triggers.
- [x] Chosen path's frontend flows confirmed feasible:
  - approve — works (system-transfer, no allowance prompts).
  - darkpool deposit / withdraw — covered by `deposit` / `withdraw`
    selectors.
  - place limit bid / ask — covered by `place(base, amount, price, isBid)`.
  - cancel order — covered by `cancel(orderId)`.
  - market buy / sell — covered by `marketBuy` / `marketSell`.
  - owner-scoped history / indexing — covered by `zone_getMy*` this branch.
- [x] PR #1 (storage-init fix) merged.
- [x] Multi-maker / partial-fill / cancel-after-fill test coverage added.
- [x] ABI, selector, unit, collateral, and top-of-book alpha stance documented.

## How this branch contributes

This branch implements the API surface described under "Owner-scoped
history APIs". Files added / changed:

- `crates/rpc/src/darkpool.rs` — new module: address constant, event
  topic constants (alloy-derived, drift-tested), response types, log
  decoders, pagination cursor.
- `crates/rpc/src/handlers.rs` — adds `zone_get_my_orders`,
  `zone_get_my_fills`, `zone_get_my_transfers`, `zone_get_order` to the
  `ZoneRpcApi` trait, the dispatch, and the test mock.
- `crates/rpc/src/proxy.rs` — proxy backend intentionally rejects
  zone-specific darkpool methods; the in-process zone RPC path is canonical
  for alpha.
- `crates/rpc/src/types.rs` — adds the four method names to the
  `Public` classification tier.
- `crates/rpc/tests/it/ws.rs` — mock impls for the four methods.
- `crates/tempo-zone/src/rpc.rs` — `TempoZoneRpc` (in-process reth path)
  implementation of the four methods using `EthFilterApiServer::logs`.
- `crates/tempo-zone/tests/it/private_rpc.rs` — drift guards: the
  rpc-side address constant must match the precompile-side address; the
  four new method names must be `Public`.
- `docs/darkpool-alpha-decision.md` — this document.

The frontend (`omega-interface`, separate repo) is the next consumer.
Migration plan for that repo, in this order:

1. Read the new methods via the existing `zonePrivateRpc` client.
2. Replace localStorage-backed order / fill / transfer caches with
   server-authoritative responses.
3. Keep localStorage only as a write-through optimistic cache for the
   first ~5 seconds after a darkpool write, until the event-derived
   history reflects the new row.
