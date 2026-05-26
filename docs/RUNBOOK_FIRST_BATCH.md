# Runbook — First-Batch Settlement on Tempo

Covers two scenarios from [issue #3](https://github.com/TheChainlessLabs/omega-zone/issues/3):

1. [Fresh zone never settled a batch](#scenario-1-fresh-zone-never-settled)
2. [Existing zone is stuck after a revert](#scenario-2-existing-zone-stuck)

Both leverage the read-only `xtask settlement-preflight` diagnostic and the
preflight logging the sequencer emits before every `submitBatch` call.

## What you need

- `L1_RPC_URL` — Tempo L1 RPC (Moderato: `https://rpc.moderato.tempo.xyz`).
- `ZONE_RPC_URL` — Zone L2 RPC for the deployed sequencer.
- `PORTAL_ADDRESS` — `ZonePortal` address for the zone (from
  `xtask zone-info`).
- For interpreting verifier rejections: the TEE payload spec in
  [`docs/TEE_PROOF.md`](TEE_PROOF.md).

## Quick diagnostic

```bash
cargo run -p tempo-xtask -- settlement-preflight \
    --l1-rpc-url "$L1_RPC_URL" \
    --zone-rpc-url "$ZONE_RPC_URL" \
    --portal-address "$PORTAL_ADDRESS"
```

Output sections:

- **Portal** — sequencer, verifier, `blockHash`, withdrawal queue cursors,
  `lastProcessedDepositNumber`, `currentDepositQueueHash`,
  `lastSyncedTempoBlockNumber`.
- **Zone state** — `FRESH` vs `ACTIVE`.
- **Proposed batch** — the values the next `submitBatch` would carry plus the
  TEE public-input commitment (`BatchPublicInputs::commitment()`).
- **Diagnostics** — explicit blockers if any of the per-input checks fail
  (block hash mismatch, deposit-number desync, Tempo anchor out of range), or
  the standard "verifier acceptance depends on proof payload" hint when the
  portal side is consistent.

This command never signs anything.

## Scenario 1 — fresh zone, never settled

Observable state on Moderato today (issue #3):

```
portal.blockHash()                = 0x000...0
portal.lastSyncedTempoBlockNumber = 0
portal.withdrawalQueueHead/Tail   = 0/0
portal.withdrawalBatchIndex       = 0
```

The sequencer keeps producing zone blocks but every `submitBatch` reverts
because the configured verifier rejects empty `(verifierConfig, proof)` bytes.

### Steps

1. **Run the preflight**:

   ```bash
   cargo run -p tempo-xtask -- settlement-preflight \
       --l1-rpc-url "$L1_RPC_URL" \
       --zone-rpc-url "$ZONE_RPC_URL" \
       --portal-address "$PORTAL_ADDRESS"
   ```

   Expected on a fresh zone: zone state `FRESH`, the portal-side fields are all
   zero, and the diagnostics section says *"verifier acceptance still depends
   on the proof payload"*. That means no portal mismatch and the only thing
   keeping settlement from succeeding is the proof itself.

2. **Confirm the proof backend on the sequencer**:

   ```bash
   # Default is fail-fast — it will surface "no proof backend configured" on
   # every batch and not sign anything. For Moderato use --proof.backend=tee.
   ```

   - Devnet / in-process verifier that accepts empty proofs:
     `--proof.backend=empty-legacy`. **Never** use this against Moderato.
   - Moderato: `--proof.backend=tee`. Behaviour now depends on whether
     `--proof.tee.endpoint` is set:
     - **No endpoint** — falls back to
       [`PendingTeeAttestationProvider`](../crates/tempo-zone/src/proof.rs):
       logs the commitment that *would* be signed and refuses to submit with
       `TempoIntegrationPending`. Useful for triage on a fresh deployment.
     - **Endpoint set** —
       [`HttpTeeAttestationProvider`](../crates/tempo-zone/src/proof.rs) POSTs
       each batch to the configured attestation service and forwards the
       returned `verifierConfig` / `proof` bytes into `submitBatch`. Surfaces
       `MissingAttestationEndpoint`, `RemoteAttestationFailed`, or
       `MalformedAttestationResponse` instead of silently submitting if any
       precondition fails. See [`docs/TEE_PROOF.md`](TEE_PROOF.md) for the
       request/response contract and the open Tempo verifier questions.

3. **Read the sequencer logs**. Each retry now prints the preflight snapshot
   plus the decoded portal revert reason. With `--proof.backend=tee` you will
   see one of:

   - `Portal preflight snapshot phase=submitBatch ...` — exact portal state and
     batch public inputs.
   - `Computed batch public-input commitment commitment=0x...
     expected_withdrawal_batch_index=1 sequencer=0x...` — the commitment the
     enclave would sign.
   - Without an endpoint: `TEE provider invoked without a connected enclave
     runtime; refusing to submit` — diagnostic-only fallback, no L1 traffic.
   - With an endpoint: `Requesting batch attestation from configured TEE
     service endpoint=...` followed by either `TEE attestation service
     returned a verifier payload` (success path) or one of the three structured
     refusal lines (`TEE attestation service request failed before producing a
     response`, `TEE attestation service returned non-success status`, `TEE
     attestation service returned a malformed response`).

4. **Validate the wire format end-to-end**. Live Moderato success still
   requires Tempo confirming the canonical `verifierConfig` / `proof` layout
   (see [`docs/TEE_PROOF.md`](TEE_PROOF.md) §Open questions) *and* the
   attestation service emitting that exact layout. The sequencer is
   intentionally proof-agnostic — bytes flow through unchanged once the
   provider accepts them.

### Success signal

After the proof path is connected the sequencer logs should show:

```
submitBatch tx accepted by RPC; waiting for confirmation
Batch submitted to L1                       tx_hash=0x...
Batch successfully submitted to L1          last_zone_block=... tempo_block_number=...
```

And `xtask settlement-preflight` re-run reports zone state `ACTIVE` with
`portal.blockHash() != 0` and `withdrawalBatchIndex >= 1`.

## Scenario 2 — existing zone, stuck

The sequencer was running, submitted at least one batch, then `submitBatch`
started reverting. Symptoms:

- Sequencer logs: repeated `Batch submission failed, retrying` followed by
  `Batch submission failed after 3 retries`.
- Eventually: `Resynced from portal and zone state` as `resync_from_portal()`
  re-anchors on `portal.blockHash()`.

### Steps

1. **Capture the revert reason**. Recent retries log
   `revert_reason=<decoded>` directly. Match against the four known portal
   errors:

   | Selector | Error                       | Likely cause |
   |---|---|---|
   | `NotSequencer`              | Sequencer key mismatch | The signing key does not match `portal.sequencer()` |
   | `InvalidProof`              | Verifier rejected payload | Most common on Moderato with empty proofs |
   | `InvalidTempoBlockNumber`   | Anchor out of EIP-2935 window or below genesis | Zone fell too far behind or zone is ahead of L1 |
   | `DepositPolicyForbids`      | TIP-403 policy rejected a deposit-routed transfer | Token policy desynced |

   Anything else surfaces as `unknown revert selector 0x...` plus the data
   length, which is most often a verifier-side custom error not in our ABI.

2. **Run preflight against the same portal**:

   ```bash
   cargo run -p tempo-xtask -- settlement-preflight \
       --l1-rpc-url "$L1_RPC_URL" \
       --zone-rpc-url "$ZONE_RPC_URL" \
       --portal-address "$PORTAL_ADDRESS" \
       --next-zone-block <N>            # optional, defaults to current tip
   ```

   Cross-reference the diagnostic blockers against the revert reason:

   - **`portal.blockHash() != batch.prev_block_hash`** — the zone L2 has
     forked away from the portal's confirmed head. The sequencer's own
     `resync_from_portal()` should already have re-anchored; if it hasn't,
     restart the node so it picks up the portal-confirmed block at startup.
   - **`lastProcessedDepositNumber` mismatch** — a deposit was processed on
     L2 but the batch was rolled back. Either restart the node or run the
     existing test fixtures' restart suite to validate the rebuild path.
   - **Tempo block out of range** — usually the zone L2 fell more than 8192
     L1 blocks behind. The stepping path in `process_block_range_stepping`
     handles this automatically once new zone blocks arrive; verify the
     deposit subscriber is making progress.

3. **If portal state is fine but the verifier keeps rejecting** — confirm the
   `--proof.backend` setting on the sequencer matches what the verifier
   expects:

   - Moderato will reject `--proof.backend=empty-legacy`.
   - Devnet may reject `--proof.backend=tee` if no real verifier is wired up.
   - With `--proof.backend=tee --proof.tee.endpoint=...`, double-check the
     attestation service is emitting the canonical Moderato `verifierConfig` /
     `proof` layout (the sequencer is intentionally proof-agnostic and only
     enforces the request/response envelope shape). Tempo verifier rejections
     of well-formed bytes look identical from the sequencer side to "service
     returned junk" — both surface as a portal-side revert on the next batch.

4. **If you suspect a state divergence on L2** — capture the zone L2 head and
   the portal block hash. If `portal.blockHash()` is no longer present on the
   zone L2 (`get_block_by_hash` returns `None`), the zone was reset out from
   under the portal and a manual operator step is required:

   - Stop the sequencer.
   - Either re-deploy a new portal pointing at the new zone, or replay the L2
     history from genesis so the portal's anchor block exists again.

   This is the same "zone may have been reset" warning the monitor already
   logs (`Portal blockHash not found on zone L2`).

## Reference: what the preflight prints

- `portal_block_hash` should equal `batch_prev_block_hash` after every
  successful settlement.
- `last_processed_deposit_number` should equal `batch_prev_deposit_number`.
- `withdrawal_batch_index + 1` is the slot the next batch will occupy
  (`expected_withdrawal_batch_index` field on the TEE public inputs).
- `commitment` is the value the enclave key would sign — keep it stable for
  later comparison if you re-run the diagnostic.

## Reference: known open questions

For the verifier-side payload format and the enclave attestation pipeline, see
[`docs/TEE_PROOF.md`](TEE_PROOF.md) §Open questions. Until the wire format is
confirmed, even a portal-consistent batch will revert on Moderato.
