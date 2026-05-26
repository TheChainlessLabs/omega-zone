# TEE-Backed Batch Proofs for Tempo Settlement

Status: **integration in progress**. The Rust surface area in
`crates/tempo-zone/src/proof.rs` is wired through the submitter and now exposes
a configurable HTTP attestation provider
([`HttpTeeAttestationProvider`](../crates/tempo-zone/src/proof.rs)) selected via
`--proof.backend=tee` plus `--proof.tee.endpoint`. The provider remains
**fail-closed**: with no endpoint configured the sequencer keeps the existing
diagnostic-only [`PendingTeeAttestationProvider`](../crates/tempo-zone/src/proof.rs)
behaviour, and any malformed response refuses to forward bytes to `submitBatch`.
The on-wire `verifierConfig` / `proof` byte layouts the attestation service
must emit, and the enclave runtime itself, are still external dependencies. See
[Open questions](#open-questions-for-tempo) before attempting a live Moderato
submission.

Tracks issues:

- [#2 — TEE-backed batch proof generation](https://github.com/TheChainlessLabs/omega-zone/issues/2)
- [#3 — Tempo testnet settlement smoke tests and diagnostics](https://github.com/TheChainlessLabs/omega-zone/issues/3)
- [#11 — Tempo technical requirements (TEE / batching / wallet)](https://github.com/TheChainlessLabs/omega-zone/issues/11)

## Why TEE for private alpha

The portal verifier is proof-agnostic
([spec.md §Proving System](../specs/spec.md#proving-system)): it calls
`IVerifier.verify(...)` with the public inputs, `verifierConfig`, and `proof`,
and the backend can be a ZKVM, a TEE attestation flow, or anything else with the
same shape. Private alpha targets a TEE pipeline because:

1. The state transition function (`prove_zone_batch`) is the same shape but the
   ZKVM cost is high for a private deployment that already trusts the
   sequencer for liveness.
2. We can execute the batch inside an enclave (SEV-SNP / Nitro / TDX), sign the
   public-input commitment with an attested enclave key, and let the L1
   verifier authenticate the attestation document.

## Provider interface

[`BatchProofProvider`](../crates/tempo-zone/src/proof.rs) is the single plug
point the submitter calls per batch. The submitter:

1. Builds [`BatchPublicInputs`](../crates/tempo-zone/src/proof.rs) from the
   portal preflight snapshot and the [`BatchData`](../crates/tempo-zone/src/batch.rs)
   it intends to submit.
2. Calls `proof_provider.build_proof(&inputs).await?`.
3. Submits `(verifierConfig, proof)` to `ZonePortal.submitBatch`.

If the provider errors, no L1 transaction is signed — the portal state stays
intact and the operator sees a structured error in the logs.

### Built-in providers

| Provider | Status | When to use |
|---|---|---|
| `FailFastProofProvider` | Default | Until the operator opts into a real backend. Surfaces `NoBackendConfigured` and refuses to submit. |
| `EmptyLegacyProofProvider` | Dev only | Devnet or in-process integration tests whose verifier accepts empty proofs. **Will revert on Moderato.** |
| `StaticTeeProofProvider` | Test fixture | Replay / unit-test path with a pre-built payload. Used in the unit test that proves `BatchSubmitter` forwards the bytes verbatim into `ZonePortal.submitBatch`. |
| `PendingTeeAttestationProvider` | Diagnostic-only fallback | Selected via `--proof.backend=tee` when no `--proof.tee.endpoint` is set. Computes the commitment that *would* be signed and logs the proposed `verifierConfig`, but still errors with `TempoIntegrationPending` until an attestation endpoint is configured. |
| `HttpTeeAttestationProvider` | Configurable, fail-closed | Selected via `--proof.backend=tee` *plus* `--proof.tee.endpoint=<url>`. POSTs each batch's public inputs to the configured attestation service and forwards the returned `verifierConfig` / `proof` bytes into `submitBatch`. Refuses to submit on missing config, transport failures, non-2xx responses, version drift, commitment drift, or empty bytes. |

### CLI

```
# Diagnostic only — logs the proposed commitment, refuses to submit.
zone --sequencer \
    --proof.backend=tee \
    ...

# Configured — POSTs each batch to https://attestation.example/sign and
# forwards the returned verifierConfig / proof bytes to ZonePortal.submitBatch.
zone --sequencer \
    --proof.backend=tee \
    --proof.tee.endpoint=https://attestation.example/sign \
    --proof.tee.auth-bearer=$ATTESTATION_TOKEN \
    --proof.tee.timeout-secs=15 \
    --proof.tee.enclave-id=0xdeadbeef... \
    --proof.tee.domain=0xportal||zoneid \
    --proof.tee.format=sev-snp \
    ...
```

Backends: `fail-fast` (default), `empty-legacy`, `tee`. TEE knobs (all
`tee`-only, all also available as env vars):

| Flag | Env var | Default | Purpose |
|---|---|---|---|
| `--proof.tee.endpoint` | `PROOF_TEE_ENDPOINT` | _unset_ | URL the sequencer POSTs each `TeeAttestationRequest` to. Leave unset to stay diagnostic-only. |
| `--proof.tee.auth-bearer` | `PROOF_TEE_AUTH_BEARER` | _unset_ | Optional bearer token. Sent as `Authorization: Bearer <token>`. |
| `--proof.tee.timeout-secs` | `PROOF_TEE_TIMEOUT_SECS` | `15` | Per-request timeout, in seconds. |
| `--proof.tee.enclave-id` | `PROOF_TEE_ENCLAVE_ID` | _empty_ | Enclave identity (hex, with or without `0x`). Echoed in the request and returned `verifierConfig`. |
| `--proof.tee.domain` | `PROOF_TEE_DOMAIN` | _empty_ | Domain separator (hex). Typically `portal_address ‖ zone_id`. |
| `--proof.tee.format` | `PROOF_TEE_FORMAT` | `unconfirmed` | One of `sev-snp`, `nitro-enclaves`, `intel-tdx`, `unconfirmed`. |

Settings: also exposed via the legacy `PROOF_BACKEND` env var for the backend
selector itself.

### Attestation service contract

The attestation service the sequencer POSTs to receives a JSON envelope shaped
like [`TeeAttestationRequest`](../crates/tempo-zone/src/proof.rs) (camelCase keys):

```json
{
  "version": 1,
  "protocol": "tempo-zone-tee-batch-v1",
  "commitment": "0x...32-byte hex...",
  "publicInputs": {
    "portalAddress": "0x...",
    "sequencerAddress": "0x...",
    "tempoBlockNumber": 1234,
    "recentTempoBlockNumber": 0,
    "prevBlockHash": "0x...",
    "nextBlockHash": "0x...",
    "prevProcessedDepositHash": "0x...",
    "nextProcessedDepositHash": "0x...",
    "prevDepositNumber": 7,
    "nextDepositNumber": 9,
    "withdrawalQueueHash": "0x...",
    "expectedWithdrawalBatchIndex": 12
  },
  "enclaveId": "0x...",
  "domain": "0x...",
  "format": "sev-snp"
}
```

And must respond with a [`TeeAttestationResponse`](../crates/tempo-zone/src/proof.rs):

```json
{
  "version": 1,
  "commitment": "0x...same 32 bytes...",
  "verifierConfig": "0x...arbitrary bytes...",
  "proof": "0x...arbitrary bytes..."
}
```

The sequencer refuses to forward the response to `submitBatch` if `version` is
not `1`, if `commitment` does not exactly match the value it sent (a sanity
check against attesting to the wrong batch), or if `verifierConfig` / `proof`
are empty. The failure surfaces as
[`ProofProviderError::MalformedAttestationResponse`](../crates/tempo-zone/src/proof.rs)
with a structured log line so operators can tell "remote failure" from
"remote produced junk" at a glance.

> **Note:** The byte layout of `verifierConfig` and `proof` is whatever the
> attestation service returns — the sequencer is intentionally proof-agnostic.
> Live Moderato success still requires Tempo confirming the canonical
> `verifierConfig` / `proof` wire format (see
> [Open questions](#open-questions-for-tempo) below) and the attestation
> service emitting that exact layout.

### Structured logs

Each batch surfaces one of these states so operators can triage settlement
failures from the log stream alone:

| Outcome | Provider | Log signal |
|---|---|---|
| No backend chosen | `FailFastProofProvider` | `proof provider 'fail-fast' refused to produce a proof ... NoBackendConfigured` |
| TEE endpoint unset | `PendingTeeAttestationProvider` | `TEE provider invoked without a connected enclave runtime` plus `TempoIntegrationPending` |
| Remote service unreachable / non-2xx | `HttpTeeAttestationProvider` | `TEE attestation service request failed before producing a response` or `TEE attestation service returned non-success status` |
| Remote service returned junk | `HttpTeeAttestationProvider` | `TEE attestation service returned a malformed response` |
| Portal verifier rejected the payload | (post-submit) | `submitBatch tx ... was included but reverted on L1` — decoded selector in the retry log |

## Proposed wire format (placeholder)

**The Moderato verifier's accepted layout is unconfirmed.** The shapes below are
the encoding the placeholder provider emits today so the rest of the integration
can be exercised end-to-end. Replace `TeeVerifierConfig::encode` /
`TeeAttestation::encode` once Tempo publishes the canonical layout.

### Public-input commitment

The TEE signs `keccak256(abi.encode_params(domain || public_inputs))`:

```
domain                       = "tempo-zone-tee-batch-v1"
portal_address               : address
sequencer_address            : address
tempo_block_number           : uint64
recent_tempo_block_number    : uint64
block_transition             : BlockTransition       // (prev, next) zone block hash
deposit_queue_transition     : DepositQueueTransition // (prev, next) hash + (prev, next) number
withdrawal_queue_hash        : bytes32
expected_withdrawal_batch_index : uint64
```

This mirrors the `PublicInputs` block in [spec.md §Witness Structure](../specs/spec.md#witness-structure)
and adds the portal address so a signature cannot be replayed across zones with
the same struct values.

### `verifierConfig` layout (`TeeVerifierConfig::encode`)

```
version       : u8                     // current = 1
format_tag    : u8                     // SEV-SNP=0x01, Nitro=0x02, TDX=0x03, Unconfirmed=0xff
domain_len    : u16 (big-endian)
domain        : [u8; domain_len]       // domain separator (e.g. portal || zone_id)
enclave_id_len: u16 (big-endian)
enclave_id    : [u8; enclave_id_len]   // MR_ENCLAVE / MEASUREMENT / equivalent
commitment    : [u8; 32]               // BatchPublicInputs::commitment()
```

### `proof` layout (`TeeAttestation::encode`)

```
version       : u8                     // current = 1
sig_len       : u16 (big-endian)
signature     : [u8; sig_len]          // enclave-key signature over `commitment`
quote_len     : u32 (big-endian)
quote         : [u8; quote_len]        // raw attestation document
```

Length-prefixed so the on-chain verifier can parse without knowing the
attestation flavour up front.

## Open questions for Tempo

These are the blockers preventing a working live submission. They map 1:1 to the
questions in issue #11.

### Batch / proof path

- [ ] Does Moderato currently support TEE-backed zone proof verification, or is
      only an empty-proof / ZK proof path live?
- [ ] What exact `verifierConfig` and `proof` bytes does the deployed verifier
      expect? Confirm the field order and whether the proposed length-prefixed
      shape above is acceptable.
- [ ] Is there a mock / dev verifier path that accepts empty proofs and is safe
      to use for private alpha smoke testing?
- [ ] How should enclave identity, domain separation, and version be encoded?
      Confirm the byte tags above (SEV-SNP=0x01, Nitro=0x02, TDX=0x03) or
      replace with Tempo's canonical IDs.
- [ ] Which TEE flavours does the verifier currently support?
- [ ] Is the public-input commitment the right thing to sign, or does Tempo
      expect the signature to cover the raw `IVerifier.verify(...)` argument
      tuple?

### Settlement / withdrawals

- [ ] Are there known `submitBatch` revert selectors we should decode beyond
      the four ZonePortal custom errors (`NotSequencer`, `InvalidProof`,
      `InvalidTempoBlockNumber`, `DepositPolicyForbids`)? Verifier-side errors
      currently surface as `unknown revert selector 0x…` once decoded.
- [ ] Recommended recovery path for a zone whose first batch never settled.
      Today the local resync re-anchors on `portal.blockHash() == 0`, but a
      portal-side replay tool would be safer if available.
- [ ] Constraints on `recentTempoBlockNumber` / EIP-2935 ancestry mode on
      Moderato — confirm the 8192-block window is still authoritative and that
      ancestry headers are validated identically.

### Wallet / RPC

These are split out of issue #11 and tracked separately from this doc but
included here so the runbook is complete:

- [ ] Chain metadata dapps should pass to Tempo Wallet / wagmi / viem.
- [ ] Required EIP-191 auth message format for private RPC.
- [ ] Should dapps submit raw signed txs to the zone RPC, or use a Tempo Wallet
      helper path?

## What landed in this repo

- `crates/tempo-zone/src/proof.rs` — types, providers, request/response
  envelopes (`TeeAttestationRequest`, `TeeAttestationResponse`), the
  configurable `HttpTeeAttestationProvider`, and the `ProofBackend` /
  `TeeProviderOptions` plumbing.
- `crates/tempo-zone/src/batch.rs` — `BatchSubmitter` takes a
  `SharedProofProvider`, builds public inputs, and runs preflight diagnostics
  before signing.
- `crates/tempo-zone/src/zonemonitor.rs` — improved revert decoder (logs the
  4-byte selector on unknown reverts; reports the decoded reason on every retry,
  not just after exhausting them).
- `crates/tempo-zone/src/cli.rs` — `--proof.backend` flag plus
  `--proof.tee.endpoint`, `--proof.tee.auth-bearer`, `--proof.tee.timeout-secs`,
  `--proof.tee.enclave-id`, `--proof.tee.domain`, `--proof.tee.format`.
- `xtask settlement-preflight` — read-only diagnostic that can be run against
  any zone to triage settlement before signing.
- `docs/RUNBOOK_FIRST_BATCH.md` — fresh-zone and stuck-zone runbook.

## What is intentionally not implemented

- The attestation service itself. `HttpTeeAttestationProvider` ships the
  client-side contract (request shape, response shape, fail-closed validation)
  but operators are responsible for running the enclave-side counterpart and
  pointing the sequencer at it via `--proof.tee.endpoint`. Until that endpoint
  is set, `--proof.backend=tee` stays diagnostic-only via
  `PendingTeeAttestationProvider`.
- Canonical `verifierConfig` / `proof` byte layouts. `HttpTeeAttestationProvider`
  forwards whatever the attestation service returns; the provider doesn't enforce
  a particular Moderato shape. Live Moderato success still requires Tempo
  confirming the canonical wire format and the attestation service emitting it.
- Verifier-side signature verification fixture. Pending an external test
  vector from Tempo.
- Ancestry-mode header inclusion in the verifier payload. The ancestry headers
  are already collected by the submitter but the proposed `proof` layout above
  only carries `(signature, quote)`; once the canonical encoding is published,
  extend `TeeAttestation::encode` to include `ancestry_headers`.
