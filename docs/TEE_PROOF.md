# TEE-Backed Batch Proofs for Tempo Settlement

Status: **integration in progress**. The Rust surface area in
`crates/tempo-zone/src/proof.rs` is wired through the submitter, but the
on-wire `verifierConfig` / `proof` layouts and the enclave runtime are still
external dependencies. See [Open questions](#open-questions-for-tempo) before
attempting a live Moderato submission.

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
| `StaticTeeProofProvider` | Test fixture | Replay / unit-test path with a pre-built payload. |
| `PendingTeeAttestationProvider` | Wiring complete, enclave pending | Selected via `--proof.backend=tee`. Computes the commitment that *would* be signed and logs the proposed `verifierConfig`, but still errors with `TempoIntegrationPending` until the enclave runtime is connected. |

### CLI

```
zone --sequencer \
    --proof.backend=tee \
    ...
```

Backends: `fail-fast` (default), `empty-legacy`, `tee`. Settings: `PROOF_BACKEND`
env var.

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

- `crates/tempo-zone/src/proof.rs` — types + providers + `ProofBackend` config.
- `crates/tempo-zone/src/batch.rs` — `BatchSubmitter` takes a
  `SharedProofProvider`, builds public inputs, and runs preflight diagnostics
  before signing.
- `crates/tempo-zone/src/zonemonitor.rs` — improved revert decoder (logs the
  4-byte selector on unknown reverts; reports the decoded reason on every retry,
  not just after exhausting them).
- `crates/tempo-zone/src/cli.rs` — `--proof.backend` flag.
- `xtask settlement-preflight` — read-only diagnostic that can be run against
  any zone to triage settlement before signing.
- `docs/RUNBOOK_FIRST_BATCH.md` — fresh-zone and stuck-zone runbook.

## What is intentionally not implemented

- Real enclave runtime (`PendingTeeAttestationProvider::build_proof` errors).
  Connecting it depends on resolving the Tempo questions above and on choosing
  the SEV-SNP / Nitro / TDX stack.
- Verifier-side signature verification fixture. Pending an external test
  vector from Tempo.
- Ancestry-mode header inclusion in the verifier payload. The ancestry headers
  are already collected by the submitter but the proposed `proof` layout above
  only carries `(signature, quote)`; once the canonical encoding is published,
  extend `TeeAttestation::encode` to include `ancestry_headers`.
