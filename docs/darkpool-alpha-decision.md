# Alpha darkpool implementation: keep the Rust precompile

**Status:** Decision proposed. Pending review for issue #9.

**Date:** 2026-05-25.

**Milestone:** Tempo testnet batching & settlement (alpha integration freeze).

## TL;DR

**Keep the Rust precompile.** Switching to Solidity for alpha is a larger
distraction than the wallet/tooling pain it removes, because:

1. The frontend's hardest darkpool problems were not contract-vs-precompile
   problems — they were keychain, gas-estimation, balance privacy, and access-
   key signing problems, all of which would carry forward to a Solidity port.
2. The precompile's outstanding correctness bug (storage persistence on first
   call) is small, well-scoped, and already proposed in
   [PR #1](https://github.com/TheChainlessLabs/omega-zone/pull/1).
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
   resting-bid escrow, and self-crossing limit-order matching. Still
   missing — file as follow-up after the freeze:
   - Multi-maker / multi-taker fill ordering test.
   - Partial-fill across two `place` calls with reconstruction-via-events
     verification.
   - Cancel after partial fill — proves `cancelTxHash` propagates into
     the reconstructed `OrderEntry`.
5. **ABI / selector / units doc.** The `sol!` block in
   `crates/precompiles/src/orderbook.rs` is the canonical ABI; the events
   are mirrored verbatim in `crates/rpc/src/darkpool.rs` for off-chain
   decoding. Price is raw integer quote-per-base; the frontend handles
   decimals. Collateral rules: bid escrow = `amount * price` in quote;
   ask escrow = `amount` in base. These need a one-page doc — file as
   follow-up. Not a freeze blocker; the frontend already encodes them
   correctly per `tasks/todo.md` review notes.

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
- [ ] PR #1 (storage-init fix) merged. Out of scope for this branch by
      instruction; tracked in PR #1.
- [ ] Multi-maker / partial-fill / cancel-after-fill test coverage. File
      as a follow-up issue after the freeze.

## How this branch contributes

This branch implements the API surface described under "Owner-scoped
history APIs". Files added / changed:

- `crates/rpc/src/darkpool.rs` — new module: address constant, event
  topic constants (alloy-derived, drift-tested), response types, log
  decoders, pagination cursor.
- `crates/rpc/src/handlers.rs` — adds `zone_get_my_orders`,
  `zone_get_my_fills`, `zone_get_my_transfers`, `zone_get_order` to the
  `ZoneRpcApi` trait, the dispatch, and the test mock.
- `crates/rpc/src/proxy.rs` — `ProxyZoneRpc` implementation of the four
  methods using upstream `eth_getLogs` with caller-scoped topic filters
  and client-side post-filtering.
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
