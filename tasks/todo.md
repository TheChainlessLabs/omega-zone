# Darkpool Open Order Escrow Withdrawal

## Plan

- [ ] Confirm the supplied withdrawal hash and current open order/balance state.
- [ ] Patch darkpool accounting so open-order escrow is not withdrawable.
- [ ] Prevent cancel from double-crediting already-accounted escrow.
- [ ] Add regression coverage for withdrawing and canceling around a resting bid.
- [ ] Run targeted verification and document results.

## Review

- Pending.

# Darkpool Limit Order Crossing Investigation

## Follow-up Plan: Stable User Order IDs

- [x] Allocate a darkpool order id for every accepted limit-order submission before matching.
- [x] Emit an order-submission event for user history even when the order fully fills.
- [x] Emit a match event that links the resting maker order id and incoming taker order id.
- [x] Update frontend ABI/history parsing to track maker and taker fills.
- [x] Add regression coverage for fully-filled incoming orders returning an id.
- [x] Run targeted protocol and frontend verification.

## Follow-up Review

- Limit `place` now allocates `order_id` before crossing and returns that id even if the incoming order fully fills.
- Added `OrderSubmitted(orderId, maker, base, quote, amount, price, isBid)` for every accepted limit-order submission.
- Added `OrderMatched(makerOrderId, takerOrderId, maker, taker, amountFilled, price)` so history can attribute executions to both the resting maker order and the incoming taker order.
- `OrderPlaced` remains the event for the residual resting quantity when `remaining > 0`.
- Frontend ABI/history parsing now understands `OrderSubmitted` and `OrderMatched`, preserving original submitted size when a later `OrderPlaced` contains only the residual amount.
- Regression coverage asserts a fully filled self-crossing incoming ask still emits `OrderSubmitted` with id `2`, emits `OrderMatched(makerOrderId=1, takerOrderId=2)`, and leaves the book empty.
- Verification passed: `cargo test -p zone --test it test_darkpool_self_crossing_limit_orders_fill -- --nocapture`, `cargo test -p zone --test it precompiles:: -- --nocapture`, `npm run lint`, and `npm run build`.
- Runtime note: the running zone node must be restarted with the updated binary before new trades emit these events; old fully-filled taker orders cannot receive retroactive submission ids.

## Follow-up Plan: Latest Trading Missing Incoming Order IDs

- [x] Pull recent darkpool logs from the local zone and decode place/fill events.
- [x] Inspect recent `place` transaction inputs and return values.
- [x] Compare fully-filled taker orders with resting/residual orders.
- [x] Query current best bid/ask and relevant open orders.
- [x] Document why some fills do not produce a new order id.

## Follow-up Review

- Recent darkpool trading happened on zone chain `421700038` through the local RPC at block height `2139`.
- `0x3301...5bb4` called `place(alphaUSD, 2000000, 1, true)`, filled resting ask `orderId=2` for `1000000`, then placed the remaining `1000000` as new bid `orderId=3`; return value was `3`.
- `0x6f6e...27dd` called `place(alphaUSD, 1000000, 1, false)`, fully filled resting bid `orderId=3`; return value was `0`, so no new ask order was inserted.
- `0xc69d...6e47e` called `place(alphaUSD, 1000000, 1, true)`, did not cross, and placed new bid `orderId=4`; return value was `4`.
- `0x0af8...855a` called `place(alphaUSD, 1000000, 2, false)`, did not cross bid `orderId=4` because ask price `2` was above bid price `1`, and placed new ask `orderId=5`; return value was `5`.
- `0x6fcc...cf7c` called `place(alphaUSD, 1000000, 3, true)`, fully filled resting ask `orderId=5` at maker price `2`; return value was `0`, so no new bid order was inserted.
- Current book confirms only `orderId=4` remains open: `bestBid(alphaUSD) = (1, 1000000)`, `bestAsk(alphaUSD) = (0, 0)`. `getOrder(2)`, `getOrder(3)`, and `getOrder(5)` revert with `OrderDoesNotExist()`.
- Explanation: the precompile only assigns and returns a new order id after matching, and only when `remaining > 0`. If the incoming taker order fully fills resting liquidity, `remaining == 0`, so `place` returns `0`. `OrderFilled.orderId` is the resting maker order id, not a new incoming taker order id.

## Follow-up Plan: Filled Order Details In Frontend

- [x] Inspect frontend order inspector/activity handling for filled orders.
- [x] Add event-backed fill/order detail parsing so closed orders have visible details.
- [x] Keep open-order `getOrder` reads for current resting state.
- [x] Verify lint/build and document the frontend behavior.

## Follow-up Review

- Root cause: `getOrder` only reads resting orders. Fully filled or cancelled orders are removed from order storage, and the frontend only remembered `OrderPlaced` IDs, so filled-order details were not visible.
- The darkpool dashboard now parses `OrderPlaced`, `OrderFilled`, and `OrderCancelled` logs from write receipts and keeps recent order history in the UI.
- The private order inspector still uses maker-scoped `getOrder` for open orders, but now falls back to darkpool event history when an order no longer exists in storage.
- Filled-order detail cards show status, side, base/quote, placed size, filled size, remaining size when still open, raw price, fill rows, block, maker/taker, and transaction hash.
- Verification passed: `npm run lint`, `npm run build`, and `curl -I http://localhost:3000/darkpool` returned `200 OK` against the already-running Next server.

## Follow-up Plan: Three Order Transaction Check

- [x] Fetch receipts and transactions for the three supplied hashes.
- [x] Decode darkpool calldata and emitted orderbook events.
- [x] Compare gas/status behavior and identify whether each order placed, filled, rested, or failed.
- [x] Query current orderbook/user state where useful.
- [x] Document the result and any next action.

## Follow-up Review

- All three transactions succeeded on zone chain `421700038`, sent by `0x7bE7fAbfc394E4d3dF6559b0046FFDF359046dDB` to darkpool `0x0b00000000000000000000000000000000000001`.
- `0x9328...9327` in block `541` called `place(alphaUSD, 1000000, 1, false)`. It emitted `OrderFilled(orderId=1, maker=user, taker=user, amountFilled=1000000, price=1)` and returned `0`, so the incoming ask fully filled existing bid order `1`.
- `0x0cc6...14dc` in block `570` called `place(alphaUSD, 2000000, 1, false)`. It emitted `OrderPlaced(orderId=2, maker=user, amount=2000000, price=1, isBid=false)` and returned `2`, so it rested as an ask because no matching bid remained.
- `0xe383...9836` in block `589` called `place(alphaUSD, 1000000, 1, true)`. It emitted `OrderFilled(orderId=2, maker=user, taker=user, amountFilled=1000000, price=1)` and returned `0`, so the incoming bid partially filled resting ask order `2`.
- Current state: `bestBid(alphaUSD) = (0, 0)`, `bestAsk(alphaUSD) = (1, 1000000)`, and `getOrder(2)` returns an ask with `quantity=1000000`.
- `getOrder(1)` reverts with `OrderDoesNotExist()`, confirming it was fully consumed.

## Follow-up Plan: Self-Crossing Limit Orders Should Fill

- [x] Record product preference that self-crossing limit orders must remain valid.
- [x] Inspect existing precompile/orderbook tests for self-match behavior.
- [x] Patch limit-order crossing so same-maker opposite-side orders can fill.
- [x] Add focused regression coverage for same-maker bid/ask at the same price.
- [x] Run targeted tests and document behavior.

## Follow-up Review

- Limit-order matching no longer stops when the resting opposite-side order has the same maker as the incoming order.
- Market-order self-match behavior was left unchanged.
- Added an integration regression where the dev wallet places a bid and then an ask for the same `alphaUSD` amount and raw price; the book is empty after the second order fills and both filled-side balances remain internally available.
- Targeted verification passed: `cargo test -p zone --test it test_darkpool_self_crossing_limit_orders_fill -- --nocapture`.
- Broader precompile verification passed: `cargo test -p zone --test it precompiles:: -- --nocapture` (`3 passed`).

## Plan: Bid/Ask Same Price Not Filling

- [x] Fetch receipts and calldata for both order transactions.
- [x] Decode order parameters and emitted darkpool events.
- [x] Inspect orderbook matching logic for crossing bid/ask behavior.
- [x] Query current best bid/ask and relevant order state if accessible.
- [x] Determine whether this is expected protocol behavior, a frontend parameter issue, or a matching bug.
- [x] Document findings and next action.

## Review

- `0x0619...5bd9` succeeded in zone block `3663` and called `place(alphaUSD, 1000000, 1, true)`.
- `0xcff9...32cfe` succeeded in zone block `3670` and called `place(alphaUSD, 1000000, 1, false)`.
- Both transactions were sent by `0x7bE7fAbfc394E4d3dF6559b0046FFDF359046dDB`.
- The first transaction emitted `OrderPlaced(orderId=1, maker=0x7bE7..., amount=1000000, price=1, isBid=true)`.
- The second transaction emitted `OrderPlaced(orderId=2, maker=0x7bE7..., amount=1000000, price=1, isBid=false)`.
- Neither transaction emitted `OrderFilled`.
- Current live state confirms a self-crossed book: `bestBid(alphaUSD) = (1, 1000000)` and `bestAsk(alphaUSD) = (1, 1000000)`.
- `getOrder(1)` returns the bid from `0x7bE7...`; `getOrder(2)` returns the ask from the same maker.
- Root cause: `cross_bids` and `cross_asks` explicitly stop when the best opposite order has the same maker as the taker (`if bid.maker == taker { break; }` / `if ask.maker == taker { break; }`). So these matching prices do not fill because they would be a self-trade.
- Product decision: self-crossing limit orders should remain valid and execute instead of resting crossed.

# Bridge Deposit Allowance Failure

## Follow-up Plan: Approval Tx `0x96b688ded1acad3a0e97fb330da2bafe6014723c57fb2c54326d0eeb55ad17cd`

- [x] Fetch and decode the failed transaction.
- [x] Confirm whether failure is allowance, policy, or gas.
- [x] Patch the frontend to avoid the failed path.
- [x] Verify `npm run lint`, `npm run build`, and live RPC simulations.
- [x] Document browser retry expectations.

## Follow-up Review

- The linked transaction is an approval, not a deposit. It called `pathUSD.approve(0x29C7391927503d13426DB94f94e0Ed5F9D54eA6D, 1000000000)`.
- The receipt failed with `gasUsed == gas limit`: `279126 / 279126`.
- Explorer and `debug_traceTransaction` both show the inner `pathUSD.approve` call received `249794` gas and failed `out of gas`.
- The portal allowance remains `0`, so the prior allowance guard correctly prevented a deposit afterward.
- A live `eth_call` simulation of the same approval with `500000` gas returns `true`.
- The bridge now sets explicit gas headroom: `500000` for TIP-20 approval and `900000` for portal deposit.
- Verification passed: `npm run lint`, `npm run build`, and live approval simulation with the new gas limit.
- Browser expectation: hard refresh the running frontend, retry Deposit, and the approval should be signed with the explicit gas limit instead of the tight `279126` estimate.

## Plan: Deposit Signing Shows `Insufficient allowance`

- [x] Inspect the bridge approve/deposit flow and current ZonePortal ABI.
- [x] Patch the frontend to verify approval success and current allowance before opening the deposit signature.
- [x] Prevent stale/misleading deposit attempts when the wallet is on the wrong chain or the approval failed.
- [x] Verify `npm run lint` and `npm run build`.
- [x] Document browser retry expectations.

## Review

- Root cause: `Bridge` waited for an approval receipt but did not check `receipt.status` or confirm `allowance(owner, portal) >= amount` before requesting the portal `deposit` signature. If the approval reverted, was still not visible, or the wallet was on the wrong chain, the wallet deposit preview could hit `ZonePortal.deposit -> TIP20.transferFrom` and report `Insufficient allowance`.
- The bridge now switches to Tempo L1 before the flow, checks the user's token balance, checks the portal allowance with `eth_call.from = owner`, submits approval only when needed, rejects failed approval receipts, and waits until allowance covers the deposit amount before opening the deposit signature.
- The existing balance read now also passes `account: owner`, matching TIP-20 privacy-gated reads.
- Deposit receipts are checked for success before the UI reports completion.
- Live sanity check passed: `cast call pathUSD allowance(owner, portal)` with `--from owner` against `https://rpc.moderato.tempo.xyz` returned `0`, proving the call shape works.
- Verification passed: `npm run lint`, `npm run build`.
- Browser expectation: hard refresh the frontend, connect on Tempo, and retry Deposit. If approval succeeds but allowance still has not propagated, the UI will stop before the deposit signature and show the observed allowance instead of letting the wallet show `Insufficient allowance`.

# Darkpool Transaction Stack Overflow

## Follow-up Plan: Private Order Inspector Revert

- [ ] Inspect the frontend `getOrder` call shape and ABI.
- [ ] Inspect the orderbook precompile `getOrder` access checks/revert behavior.
- [ ] Reproduce the read against the zone RPC for a plausible recent order id.
- [ ] Patch the frontend to avoid or explain the revert path.
- [ ] Verify `npm run lint` and `npm run build`.
- [ ] Document the browser retry expectation.

## Follow-up Review

- Pending.

## Follow-up Plan: Place Bid Revert `0x92affb03361cc3d5db76e417f16696718e2f96528f6ba0b4ef40220c24981449`

- [x] Fetch the transaction and receipt from the zone RPC.
- [x] Decode the darkpool calldata and identify the order parameters.
- [x] Trace the transaction to identify the concrete revert selector/source.
- [x] Determine whether this is a frontend parameter issue, token balance issue, or orderbook/precompile issue.
- [x] Document the browser/deployment next step.

## Follow-up Review

- Receipt status is `0` and `gasUsed` equals the submitted gas limit: `1,200,000 / 1,200,000`.
- The transaction called darkpool `place(0x20C...01, 1000000, 1, true)`, i.e. a bid for `1 alphaUSD` at raw price `1`.
- `debug_traceTransaction` returned no revert output, matching out-of-gas rather than a contract/precompile revert selector.
- Direct `eth_estimateGas` for the same call from the same wallet returned `3,111,667` gas.
- The user's `pathUSD` wallet balance was sufficient for the bid escrow, and `alphaUSD.quoteToken()` returned `pathUSD`, so this was not a balance or token-enablement failure.
- The frontend had a static `place` gas limit of `1,200,000`; it now estimates gas per darkpool write, adds a 30% + 50k buffer, and keeps higher fallback limits (`place` `4,000,000`, market orders `5,000,000`).
- Verification passed: `npm run lint`, `npm run build`.
- Browser expectation: hard refresh the darkpool page and retry the bid. The next submission should use the estimated/buffered gas instead of the stale `1,200,000` limit.

## Follow-up Plan: pathUSD Balance Read Failure

- [x] Inspect the frontend TIP20 balance read path and ABI.
- [x] Reproduce `pathUSD.balanceOf` against the zone RPC directly.
- [x] Patch the frontend or zone config based on the concrete failure.
- [x] Verify `npm run lint` and `npm run build`.
- [x] Document the browser retry expectation.

## Follow-up Review

- Direct `pathUSD.balanceOf(0x7bE7...)` without `from` reverted with `Unauthorized()` (`0x82b42900`).
- The same `pathUSD.balanceOf` call with `--from 0x7bE7...` succeeded and returned `799646264`.
- Direct darkpool `balanceOf(user,pathUSD)` with `--from user` also succeeded and returned `0`.
- The zone precompiles enforce private balance reads: TIP20 `balanceOf(account)` requires `caller == account`, and darkpool `balanceOf(user,token)` requires `msg.sender == user`.
- The frontend now passes `account: address` on all owner balance `useReadContract` calls so wagmi sends `eth_call.from` as the connected wallet.
- The failed-read UI text now says to retry with Refresh instead of claiming the RPC must recover.
- Verification passed: `npm run lint`, `npm run build`.
- Browser expectation: hard refresh the darkpool page so the new read configuration is loaded; the Funding row should show the `pathUSD` wallet balance instead of the read-failed warning.

## Follow-up Plan: pathUSD Balance Unavailable on Place Order

- [x] Inspect the current order balance/query readiness checks.
- [x] Patch the frontend to block order submission until the required wallet balance query has data.
- [x] Verify `npm run lint` and `npm run build`.
- [x] Document browser retry expectations.

## Follow-up Review

- The order handlers were checking `quoteWallet`/`baseWallet`, but the buttons were enabled before the selected wallet balance query had returned data.
- Limit bids and market buys now wait for the quote token wallet balance; limit asks and market sells now wait for the selected base token wallet balance.
- The UI shows a loading/read-failed notice beside the relevant ticket instead of allowing an early click that throws `pathUSD wallet balance unavailable`.
- The defensive handler error now says the balance is still loading rather than asking for a page refresh.
- Verification passed: `npm run lint`, `npm run build`.
- Browser expectation: after the page loads, wait until the `pathUSD` wallet balance appears in Funding and the Place Bid button is enabled; then retry placing the order.

## Follow-up Plan: Deposit Revert `0xbcb49bedb25443ebe0ad9a48c2e64f6333bf186301d6dc98f905f8f29c9b70c4`

- [x] Fetch the transaction and receipt from the zone RPC.
- [x] Check trace/log output for the revert source.
- [x] Identify the concrete contract/precompile error.
- [x] Patch the frontend to block deposits/orders for unenabled base tokens.
- [x] Verify `npm run lint` and `npm run build`.
- [x] Document the next action.

## Follow-up Review

- The receipt has `status: 0x0`; the transaction targeted darkpool `0x0b00000000000000000000000000000000000001` in block `0x400`.
- The failed call was `deposit(0x20C0000000000000000000000000000000000001, 100000000)`, i.e. `100 alphaUSD` at 6 decimals.
- `debug_traceTransaction` shows the darkpool call reverted with output `0x54cfe659`, which decodes to `PolicyForbids()`.
- Direct zone reads show `alphaUSD` is not enabled/initialized on this zone: `eth_getCode(0x20C...01)` returns `0x`, and `quoteToken()` reverts with `Uninitialized()`.
- `pathUSD` is enabled: `eth_getCode(0x20C...00)` returns `0xef`, and the user has `999837901` pathUSD units.
- The frontend now disables base-token deposit/order actions when the selected base token's `quoteToken()` read is not successful, and shows an explicit "not enabled on this zone" warning.
- Verification passed after the patch: `npm run lint`, `npm run build`.
- Next action: enable the desired base token through the portal and wait for zone sync, or test funding with the enabled quote token (`pathUSD`) only.

## Follow-up Plan: InvalidSpendingLimit

- [x] Find the keychain spending-limit validation rule for scoped access keys.
- [x] Patch the darkpool access-key limit values to satisfy the validator.
- [x] Verify `npm run lint` and `npm run build`.
- [x] Document browser retry expectations.

## Follow-up Review

- The keychain precompile rejects T3 spending limits above the TIP-20 supply cap (`u128::MAX`); the frontend was requesting `maxUint256`.
- Darkpool access-key limits now use `u128::MAX` for each bridge token and cached keys are reused only if their stored limits match that cap.
- The darkpool orderbook uses `system_transfer_from`, so TIP-20 approvals are not required for deposit/order writes. The page no longer exposes `Approve Max` or gates order placement on allowances.
- The access-key scope set now covers only darkpool orderbook top-level calls, not TIP-20 `approve`.
- `InvalidSpendingLimit` and spending-limit errors are treated as refreshable access-key failures, so stale cached bad authorizations are cleared and retried once.
- Verification passed: `npm run lint`, `npm run build`.
- Remaining browser validation: retry with Deposit or Place Order directly. The old Approve Max step should no longer be present on the darkpool page.

## Follow-up Plan: Key Authorization Gas Overhead

- [x] Add a first-use gas overhead for transactions carrying pending T3 key authorization.
- [x] Keep normal approve/orderbook gas limits unchanged after the access key authorization is consumed.
- [x] Verify `npm run lint` and `npm run build`.
- [x] Document browser retry expectations.

## Follow-up Review

- The gas error came from the first transaction carrying `keyAuthorization`: the zone reported `call gas cost (14154564) exceeds the gas limit (250000)`.
- `signAndSubmitWithAccessKey` now adds a `16_000_000` gas overhead only while the cached access key still has pending `keyAuthorization`.
- After the raw transaction is accepted, `markAccessKeyAuthorizationUsed` clears the pending authorization, so later approve/orderbook writes keep their existing per-action gas limits.
- Verification passed: `npm run lint`, `npm run build`.
- Remaining browser validation: retry Approve Max. Expected result is that the first access-key-backed transaction uses the larger gas limit, then subsequent writes use normal limits.

## Follow-up Plan: Zone Genesis Missing Tempo Forks

- [x] Confirm the wallet digest response lacks any signature capability.
- [x] Inspect generated zone genesis for Tempo fork activation fields.
- [x] Patch `generated/my-zone/genesis.json` to activate `t0Time` through `t4Time` at genesis.
- [x] Patch `just create-zone` and `just deploy-zone` so future generated zones include the Tempo fork activation fields.
- [x] Restore frontend to scoped `wallet_authorizeAccessKey` and V2 keychain signing for T3 zones.
- [x] Verify `jq`, `just --list`, `npm run lint`, and `npm run build`.
- [ ] Restart the zone with reset and retry browser Approve Max.

## Follow-up Review

- The response shape proves this is not a zone-login signature issue: `wallet_connect` returned the account but no digest signature capability.
- The actual protocol mismatch came from `generated/my-zone/genesis.json`: it had Ethereum fork times but no Tempo `t1cTime`/`t3Time`, so the zone ran pre-T1C/pre-T3.
- The current generated genesis now includes `t0Time` through `t4Time` set to `0`.
- Future `just create-zone` and `just deploy-zone` runs now patch the generated genesis with those Tempo fork fields.
- The frontend is back on the wallet-supported flow: scoped `wallet_authorizeAccessKey`, cached scope validation, and manual V2 access-key transaction signing.
- Verification passed: `jq '.config | {chainId,t1cTime,t3Time,t4Time}' generated/my-zone/genesis.json`, `just --list`, `npm run lint`, `npm run build`.
- Required runtime step: restart the zone from the patched genesis with `just zone-up my-zone true release` or equivalent reset. Without resetting the datadir, the old pre-T3 chain state remains.

## Follow-up Plan: Digest Login Request Shape

- [x] Match the accounts SDK tested digest-login request shape.
- [x] Add sanitized response-shape diagnostics when no digest signature is returned.
- [x] Keep support for nested and top-level signatures.
- [x] Verify `npm run lint` and `npm run build`.
- [ ] Document final browser validation.

## Follow-up Review

- The digest authorization request now matches the accounts SDK tests: `wallet_connect` receives `capabilities: { digest }` without extra `method`, `selectAccount`, or chain fields.
- The frontend still accepts nested and top-level signature response shapes.
- If the wallet returns no signature, the error now includes only response-shape diagnostics: top-level keys, whether a top-level signature exists, account addresses, and capability keys.
- Verification passed: `npm run lint`, `npm run build`.
- Remaining browser validation: retry Approve Max. If it still fails, copy the new `Response shape: ...` suffix.

## Follow-up Plan: Digest Signature Response Shape

- [x] Accept both nested and top-level `wallet_connect` digest signatures.
- [x] Preserve signer-account validation so a signature from the wrong account is not used silently.
- [x] Add a clear error if the wallet returns a signature for a different address.
- [x] Verify `npm run lint` and `npm run build`.
- [ ] Document final browser validation.

## Follow-up Review

- The wallet can complete the digest-signing prompt but return the signature at top-level `signature` through a connector/dialog layer instead of nested under `accounts[0].capabilities.signature`.
- The frontend now accepts both response shapes when the returned signer account matches the connected darkpool owner.
- If the signature is attached to a different wallet account, the UI now reports the signer and expected owner instead of saying no signature was returned.
- Verification passed: `npm run lint`, `npm run build`.
- Remaining browser validation: retry Approve Max. If it now reports a different signer address, reconnect/select that account in the darkpool wallet connection before retrying.

## Follow-up Plan: Wallet Requires Scopes While Zone Rejects Scopes

- [x] Stop using `wallet_authorizeAccessKey` for pre-T3 darkpool writes.
- [x] Generate the local P256 access key in the frontend.
- [x] Build a limits-only `KeyAuthorization` and ask the connected wallet to sign its digest via `wallet_connect` digest capabilities.
- [x] Store the signed limits-only authorization with the generated key pair.
- [x] Verify `npm run lint` and `npm run build`.
- [ ] Document final browser validation.

## Follow-up Review

- `wallet_authorizeAccessKey` is no longer usable for this pre-T3 darkpool flow because the wallet requires scopes and the zone rejects scopes.
- Darkpool writes now generate a P256 access key in the frontend, build a limits-only `KeyAuthorization`, ask the connected wallet to sign that raw digest through `wallet_connect`, and store the signed authorization locally.
- The raw transaction still uses the manual V1 access-key signer, so it avoids the wallet transaction preview recursion and the pre-T1C V2 keychain path.
- `ox` is now an explicit frontend dependency because the frontend imports `WebCryptoP256` and `KeyAuthorization` directly.
- Verification passed: `npm run lint`, `npm run build`.
- Remaining browser validation: retry Approve Max. Expected prompt is a wallet connect/digest signature prompt for the key authorization, not `wallet_authorizeAccessKey`.

## Follow-up Plan: Pre-T3 Call Scope Rejection

- [x] Remove call scopes from `wallet_authorizeAccessKey` so the key authorization does not encode pre-T3-gated `allowed_calls`.
- [x] Reject cached local access keys that still carry call scopes, forcing a fresh limits-only authorization.
- [x] Shorten the local access-key TTL because pre-T3 zones cannot method-scope keys.
- [x] Treat `call scopes are not active before T3` as a refreshable access-key error.
- [x] Verify `npm run lint` and `npm run build`.
- [ ] Document final browser validation.

## Follow-up Review

- The latest failure is a protocol-gating issue, not an orderbook precompile deployment issue: pre-T3 validation rejects any `KeyAuthorization` call scopes / `allowed_calls`.
- New darkpool access-key authorization requests now include `chainId`, expiry, and token limits only; they intentionally omit `scopes`.
- Cached local keys with stored scopes or pending scoped key authorizations are ignored and cleared before re-authorizing, forcing a fresh pre-T3-compatible key.
- The local access-key TTL is now 10 minutes because pre-T3 zones cannot restrict the key by contract selector.
- Verification passed: `npm run lint`, `npm run build`.
- Remaining browser validation: retry Approve Max. The first attempt should discard any cached scoped key and request a fresh limits-only key authorization.

## Follow-up Plan: KeyAuthorization Chain ID Mismatch

- [x] Pin `wallet_authorizeAccessKey` to the zone chain ID.
- [x] Reject stored local access keys whose pending `keyAuthorization.chainId` does not match the zone.
- [x] Treat `KeyAuthorization chain_id mismatch` as a refreshable access-key error.
- [x] Verify `npm run lint` and `npm run build`.
- [x] Document final browser validation.

## Follow-up Review

- `wallet_authorizeAccessKey` now includes `chainId: BigInt(zoneChain.id)`, so new scoped access keys are signed for chain `421700030` instead of the wallet's active Tempo L1 chain `42431`.
- Stored local access keys are ignored when their pending key authorization has a mismatched chain ID, which forces a fresh zone-scoped authorization.
- `KeyAuthorization chain_id mismatch` is treated as a refreshable access-key error: the frontend clears stale local keys and retries once.
- Verification passed: `npm run lint`, `npm run build`.
- Remaining browser validation: retry Approve Max. If the old bad key is still in provider state, the first attempt should clear it and request a new zone-scoped access key.

## Follow-up Plan: Stack Overflow Plus Pre-T1C Keychain

- [x] Restore scoped local access-key authorization to avoid wallet signature-preview recursion.
- [x] Bypass the provider's default V2 access-key signer by manually hydrating the access key with `internal_version: "v1"`.
- [x] Submit the manually signed raw transaction via authenticated `eth_sendRawTransaction`.
- [x] Verify `npm run lint` and `npm run build`.
- [x] Document final browser validation.

## Follow-up Review

- Direct root `eth_sendTransaction` reintroduced the wallet-side recursive `ExecutionError.ts` stack during the user signature prompt.
- Darkpool writes now restore scoped local access-key authorization, but do not let the accounts provider sign with its default V2 keychain path.
- The frontend hydrates the stored WebCrypto P256 access key with `TempoAccount.fromWebCryptoP256(..., { access, internal_version: "v1" })`, signs the Tempo transaction locally, and submits it through authenticated `eth_sendRawTransaction`.
- If an existing local key is stale or not authorized, the frontend clears it, requests a fresh scoped access key, and retries once.
- Verification passed: `npm run lint`, `npm run build`.
- Remaining browser validation: retry Approve Max. Expected flow is one access-key authorization prompt if no valid scoped key exists, then raw submission without the recursive wallet transaction-signing prompt or V2 keychain rejection.

## Follow-up Plan: Pre-T1C V2 Keychain Rejection

- [x] Identify why signed darkpool writes now produce `V2 keychain signature (type 0x04) is not valid before T1C activation`.
- [x] Remove darkpool write dependence on local access-key signing while this zone is pre-T1C.
- [x] Clear previously-created local access keys before darkpool writes so the accounts provider cannot silently use them.
- [x] Verify `npm run lint` and `npm run build`.
- [x] Document final browser validation.

## Follow-up Review

- The failure was caused by the provider's default local access-key path producing a V2 keychain signature (`0x04`), which Tempo rejects before T1C activation.
- The direct user-approved `eth_sendTransaction` fallback reintroduced the wallet-side recursive `ExecutionError.ts` stack, so it was replaced by manual V1 access-key signing.
- The frontend briefly used root `eth_sendTransaction` with the same prefilled Tempo `calls` payload; this path is no longer used for darkpool writes.
- Verification passed: `npm run lint`, `npm run build`.
- Remaining browser validation: retry Approve Max. It should show the wallet confirmation and submit as a root user-approved transaction instead of a local access-key transaction.

## Plan

- [x] Record the correction in `tasks/lessons.md`.
- [x] Replace wallet-owned zone broadcasting with wallet signing plus authenticated raw submission.
- [x] Add scoped local access-key authorization to bypass wallet transaction preview/signing.
- [x] Resync frontend zone metadata after the zone was redeployed.
- [x] Verify the redeployed zone RPC and darkpool precompile directly.
- [x] Verify the frontend build and lint checks.
- [x] Document the final behavior and remaining runtime validation.

## Review

- Darkpool writes now authorize a short-lived local access key scoped to the darkpool orderbook selectors and TIP-20 approvals.
- Existing local access keys are reused only when they are local, owned by the connected account, not near expiry, and cover the darkpool scopes.
- Darkpool approvals and orderbook writes still sign a fully prepared Tempo `calls` transaction and submit the raw signed transaction through `zonePrivateRpc` with `eth_sendRawTransaction`.
- This bypasses both wallet-owned broadcast and wallet transaction preview/signing, which are the paths that triggered recursive `ExecutionError.ts` handling.
- The redeployed zone changed from chain `421700029` / zone `29` to chain `421700030` / zone `30`; `frontend/.env.local` was stale and has been resynced from `generated/my-zone/zone.json`.
- Live RPC verification passed outside the sandbox: `eth_chainId` returned `0x1922a1be`, and darkpool `MIN_ORDER_AMOUNT()` at `0x0b00000000000000000000000000000000000001` returned `0x64`.
- Verification passed: `npm run lint`, `npm run build`, and `cargo test -p zone --test it precompiles::test_darkpool_available_on_zone -- --nocapture` outside the sandbox.
- Remaining validation: submit one darkpool action in the browser after restarting the frontend dev server so Next.js reloads the synced env. The first attempt may ask to authorize the scoped access key; subsequent darkpool writes should not open the transaction preview path.
