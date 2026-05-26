//! TEE-backed batch proof types and providers for Tempo settlement.
//!
//! ## Why this module exists
//!
//! Before this module, [`BatchSubmitter`](crate::batch::BatchSubmitter) always submitted
//! empty `verifierConfig` and `proof` bytes to `ZonePortal.submitBatch`. That worked on
//! permissive dev verifiers but the live Tempo Moderato verifier rejects empty proofs and
//! the transaction reverts before the portal anchor advances.
//!
//! ## Direction
//!
//! Private alpha targets TEE-backed proving (not ZK). The sequencer executes — or attests
//! to — each batch's state transition inside a Trusted Execution Environment, signs the
//! resulting public-input commitment with an attested enclave key, and submits the
//! attestation and signature in the verifier payload.
//!
//! ## What is implemented here
//!
//! - [`BatchPublicInputs`] — the values the L1 verifier binds the attestation to. They
//!   mirror the public inputs documented in [`specs/spec.md`](../../specs/spec.md) §Proving
//!   System and feed [`Self::commitment`] which the TEE signs.
//! - [`TeeProofPayload`] — the `(verifierConfig, proof)` byte pair handed to `submitBatch`.
//! - [`TeeVerifierConfig`] — proposed `verifierConfig` layout (version, enclave identity,
//!   domain separator, attestation format tag).
//! - [`TeeAttestation`] — proposed `proof` layout (signature over the public-input
//!   commitment plus the raw enclave attestation document).
//! - [`BatchProofProvider`] — the trait the submitter calls per batch. Implementations:
//!   - [`FailFastProofProvider`] (default) — refuses to submit. Forces an explicit choice
//!     before any settlement traffic hits L1.
//!   - [`EmptyLegacyProofProvider`] — preserves the pre-TEE behaviour for permissive dev
//!     verifiers. **Not safe** for Moderato.
//!   - [`StaticTeeProofProvider`] — for tests and replay; serves a pre-built payload.
//!   - [`PendingTeeAttestationProvider`] — placeholder for the real enclave integration.
//!     Returns [`ProofProviderError::TempoIntegrationPending`] until the Tempo verifier
//!     format and the enclave attestation pipeline are both confirmed (see
//!     [`docs/TEE_PROOF.md`](../../docs/TEE_PROOF.md)).
//!
//! ## What is intentionally not implemented
//!
//! Generating a real enclave attestation requires:
//!
//! 1. The exact `verifierConfig` / `proof` encoding the Moderato `IVerifier` accepts.
//! 2. An enclave runtime (SEV-SNP, Nitro, TDX, ...) that runs `prove_zone_batch` over the
//!    batch witness and returns a signed attestation.
//! 3. The enclave key registered with Tempo (or carried inline via the attestation doc).
//!
//! All three are external dependencies tracked in
//! [`docs/TEE_PROOF.md`](../../docs/TEE_PROOF.md). This module ships the surface area so
//! the integration can be filled in without touching the submitter or monitor again, and
//! it fails closed in the meantime.

use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use alloy_primitives::{Address, B256, Bytes, keccak256};
use alloy_sol_types::SolValue;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use url::Url;

use crate::abi::{BlockTransition, DepositQueueTransition};

/// Domain-separation tag for the TEE attestation commitment hash.
///
/// Mixed into [`BatchPublicInputs::commitment`] so a signature over one zone's batch
/// cannot be replayed against another zone or another protocol that happens to share the
/// same struct layout.
pub const TEE_COMMITMENT_DOMAIN: &[u8] = b"tempo-zone-tee-batch-v1";

/// Current `verifierConfig` layout version emitted by [`TeeVerifierConfig::encode`].
pub const TEE_VERIFIER_CONFIG_VERSION: u8 = 1;

/// Current attestation envelope version emitted by [`TeeAttestation::encode`].
pub const TEE_ATTESTATION_VERSION: u8 = 1;

/// Public inputs the L1 verifier binds the TEE attestation to.
///
/// Mirrors the `PublicInputs` block documented in
/// [`specs/spec.md`](../../specs/spec.md) §Proving System with two additions surfaced for
/// settlement diagnostics: the portal address (so the same enclave signing key cannot be
/// confused across portals) and a snapshot of the deposit-queue transition (so the
/// commitment uniquely binds the batch the portal will execute).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchPublicInputs {
    /// ZonePortal contract address on Tempo L1.
    pub portal_address: Address,
    /// Registered sequencer that signed the batch.
    pub sequencer_address: Address,
    /// Tempo L1 block number the zone anchored against.
    pub tempo_block_number: u64,
    /// Recent Tempo L1 block number passed to `submitBatch` (0 in direct mode).
    pub recent_tempo_block_number: u64,
    /// Previous zone block hash (must match portal's current `blockHash`).
    pub prev_block_hash: B256,
    /// Zone block hash after this batch.
    pub next_block_hash: B256,
    /// Deposit queue cumulative hash at the start of processing.
    pub prev_processed_deposit_hash: B256,
    /// Deposit queue cumulative hash after processing.
    pub next_processed_deposit_hash: B256,
    /// Deposit counter at the start of processing.
    pub prev_deposit_number: u64,
    /// Deposit counter after processing.
    pub next_deposit_number: u64,
    /// Withdrawal queue hash for this batch (`B256::ZERO` if none).
    pub withdrawal_queue_hash: B256,
    /// `withdrawalBatchIndex + 1` — the portal slot this batch will occupy.
    pub expected_withdrawal_batch_index: u64,
}

impl BatchPublicInputs {
    /// Compute the commitment hash the TEE signs.
    ///
    /// Deterministic over the public inputs, domain-separated with
    /// [`TEE_COMMITMENT_DOMAIN`]. Layout is intentionally simple (ABI-encoded params) so a
    /// future enclave implementation in Rust *or* Solidity can recompute it without
    /// negotiating a custom serializer.
    pub fn commitment(&self) -> B256 {
        let encoded = (
            Bytes::from_static(TEE_COMMITMENT_DOMAIN),
            self.portal_address,
            self.sequencer_address,
            self.tempo_block_number,
            self.recent_tempo_block_number,
            BlockTransition {
                prevBlockHash: self.prev_block_hash,
                nextBlockHash: self.next_block_hash,
            },
            DepositQueueTransition {
                prevProcessedHash: self.prev_processed_deposit_hash,
                nextProcessedHash: self.next_processed_deposit_hash,
                prevDepositNumber: self.prev_deposit_number,
                nextDepositNumber: self.next_deposit_number,
            },
            self.withdrawal_queue_hash,
            self.expected_withdrawal_batch_index,
        )
            .abi_encode_params();

        keccak256(encoded)
    }
}

/// Bytes submitted alongside `ZonePortal.submitBatch`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeeProofPayload {
    /// Opaque verifier configuration. Encodes enclave identity, domain separation, and
    /// versioning the verifier needs in order to interpret [`Self::proof`].
    pub verifier_config: Bytes,
    /// Opaque attestation / signature payload the verifier validates.
    pub proof: Bytes,
}

impl TeeProofPayload {
    /// Build an empty `(0x, 0x)` payload — the historical pre-TEE behaviour.
    ///
    /// Reserved for legacy permissive dev verifiers; will revert on Moderato.
    pub fn empty_legacy() -> Self {
        Self {
            verifier_config: Bytes::new(),
            proof: Bytes::new(),
        }
    }
}

/// Identifier for the TEE flavour producing the attestation.
///
/// Encoded into [`TeeVerifierConfig`] so the verifier can route to the right enclave
/// quote parser without touching the rest of the payload.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TeeAttestationFormat {
    /// AMD SEV-SNP attestation report.
    SevSnp,
    /// AWS Nitro Enclaves attestation document.
    NitroEnclaves,
    /// Intel TDX quote.
    IntelTdx,
    /// Placeholder used by tests and `--proof.backend=tee` until Tempo confirms the
    /// canonical format. Encoded byte is `0xff` — easy to spot in logs.
    #[default]
    Unconfirmed,
}

impl TeeAttestationFormat {
    /// Stable tag emitted into the on-wire verifier config.
    pub const fn tag(self) -> u8 {
        match self {
            Self::SevSnp => 0x01,
            Self::NitroEnclaves => 0x02,
            Self::IntelTdx => 0x03,
            Self::Unconfirmed => 0xff,
        }
    }
}

impl std::str::FromStr for TeeAttestationFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "sev-snp" | "sev_snp" | "sevsnp" => Ok(Self::SevSnp),
            "nitro-enclaves" | "nitro_enclaves" | "nitro" => Ok(Self::NitroEnclaves),
            "intel-tdx" | "intel_tdx" | "tdx" => Ok(Self::IntelTdx),
            "unconfirmed" | "pending" => Ok(Self::Unconfirmed),
            other => Err(format!(
                "unknown TEE attestation format `{other}` (expected one of: \
                 sev-snp, nitro-enclaves, intel-tdx, unconfirmed)"
            )),
        }
    }
}

/// Proposed `verifierConfig` layout.
///
/// Compact, length-prefixed, big-endian. The actual layout the Moderato verifier expects
/// is **unconfirmed** — tracked in [`docs/TEE_PROOF.md`](../../docs/TEE_PROOF.md). When
/// Tempo publishes the canonical shape, swap [`Self::encode`] for the official codec and
/// keep the rest of the integration intact.
///
/// Layout:
///
/// ```text
/// version       : u8                       // TEE_VERIFIER_CONFIG_VERSION
/// format_tag    : u8                       // TeeAttestationFormat::tag()
/// domain_len    : u16                      // big-endian
/// domain        : [u8; domain_len]         // domain separator (e.g. portal address || zone id)
/// enclave_id_len: u16                      // big-endian
/// enclave_id    : [u8; enclave_id_len]     // enclave MR_ENCLAVE or equivalent identity hash
/// commitment    : [u8; 32]                 // BatchPublicInputs::commitment()
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeeVerifierConfig {
    /// Attestation flavour.
    pub format: TeeAttestationFormat,
    /// Domain separator bound into the attested message. Typically the portal address
    /// concatenated with the zone id; left opaque so the verifier owns the policy.
    pub domain: Bytes,
    /// Stable enclave identity (e.g. MR_ENCLAVE for SGX/TDX, measurement for SEV-SNP).
    pub enclave_id: Bytes,
    /// Public-input commitment the enclave signed.
    pub commitment: B256,
}

impl TeeVerifierConfig {
    /// Encode the config into the on-wire byte layout.
    ///
    /// Panics if `domain` or `enclave_id` exceeds `u16::MAX` — caller is responsible for
    /// keeping them small.
    pub fn encode(&self) -> Bytes {
        let domain_len: u16 = self
            .domain
            .len()
            .try_into()
            .expect("verifier domain separator must fit in u16");
        let enclave_id_len: u16 = self
            .enclave_id
            .len()
            .try_into()
            .expect("enclave identity must fit in u16");

        let mut buf =
            Vec::with_capacity(1 + 1 + 2 + self.domain.len() + 2 + self.enclave_id.len() + 32);
        buf.push(TEE_VERIFIER_CONFIG_VERSION);
        buf.push(self.format.tag());
        buf.extend_from_slice(&domain_len.to_be_bytes());
        buf.extend_from_slice(&self.domain);
        buf.extend_from_slice(&enclave_id_len.to_be_bytes());
        buf.extend_from_slice(&self.enclave_id);
        buf.extend_from_slice(self.commitment.as_slice());

        Bytes::from(buf)
    }
}

/// Proposed `proof` layout.
///
/// Same caveat as [`TeeVerifierConfig`]: the on-wire format is unconfirmed and tracked
/// in [`docs/TEE_PROOF.md`](../../docs/TEE_PROOF.md). Layout:
///
/// ```text
/// version       : u8                       // TEE_ATTESTATION_VERSION
/// sig_len       : u16                      // big-endian
/// signature     : [u8; sig_len]            // enclave signature over commitment
/// quote_len     : u32                      // big-endian
/// quote         : [u8; quote_len]          // raw attestation document
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeeAttestation {
    /// Signature produced by the enclave key over the public-input commitment.
    pub signature: Bytes,
    /// Raw attestation document (SEV-SNP report, Nitro doc, TDX quote, ...).
    pub attestation_quote: Bytes,
}

impl TeeAttestation {
    /// Encode the attestation into the on-wire byte layout.
    pub fn encode(&self) -> Bytes {
        let sig_len: u16 = self
            .signature
            .len()
            .try_into()
            .expect("enclave signature must fit in u16");
        let quote_len: u32 = self
            .attestation_quote
            .len()
            .try_into()
            .expect("attestation quote must fit in u32");

        let mut buf =
            Vec::with_capacity(1 + 2 + self.signature.len() + 4 + self.attestation_quote.len());
        buf.push(TEE_ATTESTATION_VERSION);
        buf.extend_from_slice(&sig_len.to_be_bytes());
        buf.extend_from_slice(&self.signature);
        buf.extend_from_slice(&quote_len.to_be_bytes());
        buf.extend_from_slice(&self.attestation_quote);

        Bytes::from(buf)
    }
}

/// Errors a [`BatchProofProvider`] can return without it being a transient RPC issue.
#[derive(Debug, thiserror::Error)]
pub enum ProofProviderError {
    /// The provider is intentionally refusing to produce a proof and the operator must
    /// pick a backend before settlement can proceed.
    #[error(
        "no proof backend configured — set --proof.backend=tee for Moderato or \
         --proof.backend=empty-legacy only on a dev verifier that accepts empty proofs"
    )]
    NoBackendConfigured,
    /// Operator chose the TEE backend but did not configure an attestation service endpoint.
    /// `PendingTeeAttestationProvider` covers diagnostic logging; this variant surfaces from
    /// `HttpTeeAttestationProvider` so the structured-log channel can distinguish "no
    /// backend at all" from "TEE backend selected but unaddressable".
    #[error(
        "TEE proof backend selected but no attestation service endpoint configured — \
         set --proof.tee.endpoint=<url> (env PROOF_TEE_ENDPOINT) to point the provider at \
         the attestation service that returns verifierConfig/proof"
    )]
    MissingAttestationEndpoint,
    /// TEE provider is wired in but the external Tempo / enclave details required to
    /// actually generate a valid proof are still outstanding. See
    /// `docs/TEE_PROOF.md` for the open questions.
    #[error("TEE proof integration pending Tempo confirmation (see docs/TEE_PROOF.md): {0}")]
    TempoIntegrationPending(&'static str),
    /// The configured attestation service was reachable but rejected the request or
    /// returned a non-success status. Carries the HTTP status and a short body excerpt so
    /// the failure is visible in structured logs without a second request.
    #[error(
        "TEE attestation service `{endpoint}` returned an error (status {status}): {detail}"
    )]
    RemoteAttestationFailed {
        /// Endpoint the request targeted.
        endpoint: String,
        /// HTTP status code.
        status: u16,
        /// Body excerpt or transport error description.
        detail: String,
    },
    /// The attestation service responded but the payload failed validation before any L1
    /// traffic could be signed. Refusing to forward malformed bytes to `submitBatch` keeps
    /// the portal anchor intact.
    #[error("TEE attestation service `{endpoint}` returned a malformed response: {reason}")]
    MalformedAttestationResponse {
        /// Endpoint the request targeted.
        endpoint: String,
        /// What went wrong (decoding, version mismatch, empty bytes, commitment drift).
        reason: String,
    },
}

/// Pinned, send-safe future returned by [`BatchProofProvider::build_proof`].
pub type ProofFuture<'a> = Pin<Box<dyn Future<Output = eyre::Result<TeeProofPayload>> + Send + 'a>>;

/// Plug point for batch proof generation.
///
/// Implementations build the `(verifierConfig, proof)` payload submitted alongside
/// `ZonePortal.submitBatch`. The submitter calls [`Self::build_proof`] once per batch and
/// surfaces any error before sending the L1 transaction, so a failing provider keeps the
/// portal state intact.
pub trait BatchProofProvider: Send + Sync + std::fmt::Debug {
    /// Short tag used in logs and diagnostics.
    fn name(&self) -> &'static str;

    /// Produce the verifier config and proof bytes for a single batch.
    fn build_proof<'a>(&'a self, inputs: &'a BatchPublicInputs) -> ProofFuture<'a>;
}

/// Shared, cheaply-clonable handle for plumbing a provider through the sequencer stack.
pub type SharedProofProvider = Arc<dyn BatchProofProvider>;

/// Default safe provider — refuses to submit anything.
///
/// Use when the operator has not made an explicit choice. Fails closed so a misconfigured
/// node cannot accidentally spam Moderato with reverting `submitBatch` calls.
#[derive(Debug, Default)]
pub struct FailFastProofProvider;

impl BatchProofProvider for FailFastProofProvider {
    fn name(&self) -> &'static str {
        "fail-fast"
    }

    fn build_proof<'a>(&'a self, _inputs: &'a BatchPublicInputs) -> ProofFuture<'a> {
        Box::pin(async { Err(eyre::Report::new(ProofProviderError::NoBackendConfigured)) })
    }
}

/// Submits empty `verifierConfig` / `proof` bytes — the pre-TEE behaviour.
///
/// Only safe against dev verifiers that explicitly accept empty proofs. Logs a warning
/// every call so operators notice if it accidentally ends up pointed at Moderato.
#[derive(Debug, Default)]
pub struct EmptyLegacyProofProvider;

impl BatchProofProvider for EmptyLegacyProofProvider {
    fn name(&self) -> &'static str {
        "empty-legacy"
    }

    fn build_proof<'a>(&'a self, inputs: &'a BatchPublicInputs) -> ProofFuture<'a> {
        Box::pin(async move {
            warn!(
                portal = %inputs.portal_address,
                tempo_block_number = inputs.tempo_block_number,
                "Using empty legacy proof bytes — only valid on permissive dev verifiers"
            );
            Ok(TeeProofPayload::empty_legacy())
        })
    }
}

/// Static provider used by tests / replay — always returns the same payload.
#[derive(Debug, Clone)]
pub struct StaticTeeProofProvider {
    payload: TeeProofPayload,
}

impl StaticTeeProofProvider {
    /// Construct a provider that always returns `payload`.
    pub fn new(payload: TeeProofPayload) -> Self {
        Self { payload }
    }
}

impl BatchProofProvider for StaticTeeProofProvider {
    fn name(&self) -> &'static str {
        "static-tee"
    }

    fn build_proof<'a>(&'a self, _inputs: &'a BatchPublicInputs) -> ProofFuture<'a> {
        let payload = self.payload.clone();
        Box::pin(async move { Ok(payload) })
    }
}

/// Configuration for [`PendingTeeAttestationProvider`].
///
/// The fields cover the operator-supplied knobs that *don't* depend on Tempo confirming
/// the wire format: the enclave identity hash to advertise, the domain separator to bind
/// against, and which attestation flavour the operator believes the verifier expects.
#[derive(Debug, Clone)]
pub struct PendingTeeProviderConfig {
    /// Stable enclave identity hash to advertise in [`TeeVerifierConfig::enclave_id`].
    pub enclave_id: Bytes,
    /// Domain separator (e.g. `portal_address || zone_id`) bound into the config.
    pub domain: Bytes,
    /// Best-known attestation flavour. Defaults to [`TeeAttestationFormat::Unconfirmed`].
    pub format: TeeAttestationFormat,
}

impl Default for PendingTeeProviderConfig {
    fn default() -> Self {
        Self {
            enclave_id: Bytes::new(),
            domain: Bytes::new(),
            format: TeeAttestationFormat::Unconfirmed,
        }
    }
}

/// Placeholder for the production TEE integration.
///
/// Holds the operator-supplied knobs (enclave identity, domain separator, attestation
/// format) and exposes [`Self::config_for`] / [`Self::commitment_for`] so the rest of the
/// system can already log and validate what *would* be submitted. [`Self::build_proof`]
/// still errors with [`ProofProviderError::TempoIntegrationPending`] until the enclave
/// runtime is connected — see `docs/TEE_PROOF.md`.
#[derive(Debug, Clone)]
pub struct PendingTeeAttestationProvider {
    config: PendingTeeProviderConfig,
}

impl PendingTeeAttestationProvider {
    /// Build a provider pre-loaded with the operator-supplied config.
    pub fn new(config: PendingTeeProviderConfig) -> Self {
        Self { config }
    }

    /// Return the commitment hash the enclave would sign for `inputs`.
    pub fn commitment_for(&self, inputs: &BatchPublicInputs) -> B256 {
        inputs.commitment()
    }

    /// Produce the `verifierConfig` bytes the provider would submit for `inputs` —
    /// useful for diagnostic logging even before the enclave signature is available.
    pub fn config_for(&self, inputs: &BatchPublicInputs) -> TeeVerifierConfig {
        TeeVerifierConfig {
            format: self.config.format,
            domain: self.config.domain.clone(),
            enclave_id: self.config.enclave_id.clone(),
            commitment: inputs.commitment(),
        }
    }
}

impl BatchProofProvider for PendingTeeAttestationProvider {
    fn name(&self) -> &'static str {
        "pending-tee-attestation"
    }

    fn build_proof<'a>(&'a self, inputs: &'a BatchPublicInputs) -> ProofFuture<'a> {
        Box::pin(async move {
            let proposed = self.config_for(inputs);
            warn!(
                portal = %inputs.portal_address,
                tempo_block_number = inputs.tempo_block_number,
                commitment = %proposed.commitment,
                format = ?proposed.format,
                "TEE provider invoked without a connected enclave runtime; refusing to submit"
            );
            Err(eyre::Report::new(
                ProofProviderError::TempoIntegrationPending(
                    "no enclave runtime wired in — fill in PendingTeeAttestationProvider \
                     once the Tempo verifier shape and the enclave signing pipeline are confirmed",
                ),
            ))
        })
    }
}

/// Wire-format envelope version emitted by [`TeeAttestationRequest`] and required on
/// [`TeeAttestationResponse`]. Bumped if the request/response schema changes in a way the
/// attestation service must opt into.
pub const TEE_ATTESTATION_SERVICE_VERSION: u8 = 1;

/// Default per-request timeout when the operator does not override it.
pub const DEFAULT_TEE_ATTESTATION_TIMEOUT: Duration = Duration::from_secs(15);

/// Public-input projection sent to the attestation service alongside the commitment.
///
/// The service needs the raw inputs (not just the digest) so it can reconstruct the
/// commitment inside the enclave and confirm the host did not silently rewrite any
/// field. Hashes/addresses are encoded as `0x`-prefixed hex strings — interoperable
/// with any HTTP-aware attestation runtime regardless of language.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeeAttestationPublicInputs {
    /// ZonePortal contract address on Tempo L1.
    pub portal_address: Address,
    /// Registered sequencer address.
    pub sequencer_address: Address,
    /// Tempo L1 block number the zone anchored against.
    pub tempo_block_number: u64,
    /// Recent Tempo L1 block number passed to `submitBatch` (0 in direct mode).
    pub recent_tempo_block_number: u64,
    /// Previous zone block hash.
    pub prev_block_hash: B256,
    /// Next zone block hash.
    pub next_block_hash: B256,
    /// Deposit queue cumulative hash at start of processing.
    pub prev_processed_deposit_hash: B256,
    /// Deposit queue cumulative hash after processing.
    pub next_processed_deposit_hash: B256,
    /// Deposit counter at start of processing.
    pub prev_deposit_number: u64,
    /// Deposit counter after processing.
    pub next_deposit_number: u64,
    /// Withdrawal queue hash for this batch (`0x000...0` if none).
    pub withdrawal_queue_hash: B256,
    /// Portal withdrawal slot the batch will occupy.
    pub expected_withdrawal_batch_index: u64,
}

impl From<&BatchPublicInputs> for TeeAttestationPublicInputs {
    fn from(inputs: &BatchPublicInputs) -> Self {
        Self {
            portal_address: inputs.portal_address,
            sequencer_address: inputs.sequencer_address,
            tempo_block_number: inputs.tempo_block_number,
            recent_tempo_block_number: inputs.recent_tempo_block_number,
            prev_block_hash: inputs.prev_block_hash,
            next_block_hash: inputs.next_block_hash,
            prev_processed_deposit_hash: inputs.prev_processed_deposit_hash,
            next_processed_deposit_hash: inputs.next_processed_deposit_hash,
            prev_deposit_number: inputs.prev_deposit_number,
            next_deposit_number: inputs.next_deposit_number,
            withdrawal_queue_hash: inputs.withdrawal_queue_hash,
            expected_withdrawal_batch_index: inputs.expected_withdrawal_batch_index,
        }
    }
}

/// Request body the sequencer POSTs to the configured attestation service per batch.
///
/// Carries the full [`BatchPublicInputs`] *and* the precomputed `commitment` so the
/// attestation service can either trust the commitment or recompute it from the inputs
/// before signing. The `enclave_id`, `domain`, and `format` echo the operator-configured
/// values so the service has everything required to fill in the canonical
/// [`TeeVerifierConfig`] layout — even though the live Moderato shape is still
/// unconfirmed (see [`docs/TEE_PROOF.md`](../../docs/TEE_PROOF.md)).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeeAttestationRequest {
    /// Request envelope version. Always [`TEE_ATTESTATION_SERVICE_VERSION`] today.
    pub version: u8,
    /// Tag identifying the wire protocol so misconfigured services fail loudly instead of
    /// silently returning bytes shaped for a different chain. Always
    /// [`TeeAttestationRequest::PROTOCOL_TAG`] today.
    pub protocol: String,
    /// Public-input commitment the enclave is expected to sign.
    pub commitment: B256,
    /// Full public inputs the commitment was derived from.
    pub public_inputs: TeeAttestationPublicInputs,
    /// Enclave identity to advertise in the returned `verifierConfig`.
    pub enclave_id: Bytes,
    /// Domain separator to advertise in the returned `verifierConfig`.
    pub domain: Bytes,
    /// Best-known attestation format.
    pub format: TeeAttestationFormat,
}

impl TeeAttestationRequest {
    /// Wire-format protocol tag. Embedded in every request so attestation services can
    /// refuse misrouted traffic instead of producing bytes shaped for a different chain.
    pub const PROTOCOL_TAG: &'static str = "tempo-zone-tee-batch-v1";
}

/// Response body the attestation service returns. `verifier_config` and `proof` are the
/// exact byte strings the sequencer forwards to `ZonePortal.submitBatch`.
///
/// The response also echoes the request `commitment` so the sequencer can refuse to
/// submit if the service signed a different value (e.g. attested to an old batch or
/// hashed the inputs incorrectly).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeeAttestationResponse {
    /// Response envelope version. Must equal [`TEE_ATTESTATION_SERVICE_VERSION`].
    pub version: u8,
    /// Public-input commitment the service signed. Validated against the request.
    pub commitment: B256,
    /// `verifierConfig` bytes for `submitBatch`.
    pub verifier_config: Bytes,
    /// `proof` bytes for `submitBatch`.
    pub proof: Bytes,
}

/// Operator-supplied configuration for [`HttpTeeAttestationProvider`].
///
/// The endpoint is the only required field — the rest mirror
/// [`PendingTeeProviderConfig`] so the sequencer can fall back to diagnostic-only
/// behaviour when the endpoint is intentionally left blank.
#[derive(Debug, Clone)]
pub struct TeeAttestationServiceConfig {
    /// HTTP(S) URL the provider POSTs each [`TeeAttestationRequest`] to.
    pub endpoint: Url,
    /// Optional bearer token. When set, sent as `Authorization: Bearer <token>`.
    pub bearer_token: Option<String>,
    /// Per-request timeout. Applied independently of any reqwest-client default so the
    /// sequencer never blocks settlement on a misbehaving attestation service.
    pub request_timeout: Duration,
    /// Stable enclave identity hash to forward in the request.
    pub enclave_id: Bytes,
    /// Domain separator to forward in the request.
    pub domain: Bytes,
    /// Best-known attestation flavour to forward in the request.
    pub format: TeeAttestationFormat,
}

impl TeeAttestationServiceConfig {
    /// Build a config from an endpoint URL alone — uses default timeout and no auth.
    pub fn new(endpoint: Url) -> Self {
        Self {
            endpoint,
            bearer_token: None,
            request_timeout: DEFAULT_TEE_ATTESTATION_TIMEOUT,
            enclave_id: Bytes::new(),
            domain: Bytes::new(),
            format: TeeAttestationFormat::Unconfirmed,
        }
    }
}

/// HTTP-backed [`BatchProofProvider`] that delegates attestation to an external service.
///
/// The provider is intentionally a thin client — it does not validate signatures or parse
/// attestation quotes. It enforces the integration contract:
///
/// 1. **Fail closed when unconfigured.** If the operator selects `--proof.backend=tee`
///    without an endpoint, the provider returns
///    [`ProofProviderError::MissingAttestationEndpoint`] before any L1 traffic.
/// 2. **Surface transport errors structurally.** Non-2xx responses and reqwest failures
///    surface as [`ProofProviderError::RemoteAttestationFailed`] so operators can tell
///    "service unreachable" apart from "service returned junk".
/// 3. **Refuse malformed payloads.** Empty `verifier_config`/`proof`, wrong version, or a
///    commitment mismatch return [`ProofProviderError::MalformedAttestationResponse`]
///    before `submitBatch` is signed.
///
/// The provider does **not** know whether the bytes the service returned are accepted by
/// Moderato — that depends on Tempo confirming the canonical
/// `verifierConfig`/`proof` layout described in
/// [`docs/TEE_PROOF.md`](../../docs/TEE_PROOF.md).
#[derive(Debug, Clone)]
pub struct HttpTeeAttestationProvider {
    client: reqwest::Client,
    config: TeeAttestationServiceConfig,
}

impl HttpTeeAttestationProvider {
    /// Build a provider with a default reqwest client and the operator-supplied config.
    pub fn new(config: TeeAttestationServiceConfig) -> eyre::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .build()
            .map_err(|err| eyre::eyre!("failed to build attestation HTTP client: {err}"))?;
        Ok(Self { client, config })
    }

    /// Build a provider using an explicit reqwest client. Useful for tests that want to
    /// inject a custom transport (e.g. with the test server's CA root).
    pub fn with_client(client: reqwest::Client, config: TeeAttestationServiceConfig) -> Self {
        Self { client, config }
    }

    /// Endpoint the provider will POST to — exposed for diagnostics and tests.
    pub fn endpoint(&self) -> &Url {
        &self.config.endpoint
    }

    fn build_request(&self, inputs: &BatchPublicInputs) -> TeeAttestationRequest {
        TeeAttestationRequest {
            version: TEE_ATTESTATION_SERVICE_VERSION,
            protocol: TeeAttestationRequest::PROTOCOL_TAG.to_owned(),
            commitment: inputs.commitment(),
            public_inputs: inputs.into(),
            enclave_id: self.config.enclave_id.clone(),
            domain: self.config.domain.clone(),
            format: self.config.format,
        }
    }

    /// Validate an already-deserialized [`TeeAttestationResponse`] against the request.
    ///
    /// Exposed so tests can exercise the validation logic without a live HTTP server. The
    /// `endpoint` argument is purely cosmetic — it shapes the error message so logs match
    /// what an operator would see in production.
    pub fn validate_response(
        endpoint: &str,
        request_commitment: B256,
        response: &TeeAttestationResponse,
    ) -> Result<TeeProofPayload, ProofProviderError> {
        if response.version != TEE_ATTESTATION_SERVICE_VERSION {
            return Err(ProofProviderError::MalformedAttestationResponse {
                endpoint: endpoint.to_owned(),
                reason: format!(
                    "unexpected response version {} (expected {})",
                    response.version, TEE_ATTESTATION_SERVICE_VERSION
                ),
            });
        }
        if response.commitment != request_commitment {
            return Err(ProofProviderError::MalformedAttestationResponse {
                endpoint: endpoint.to_owned(),
                reason: format!(
                    "commitment mismatch: requested {} but service signed {}",
                    request_commitment, response.commitment
                ),
            });
        }
        if response.verifier_config.is_empty() {
            return Err(ProofProviderError::MalformedAttestationResponse {
                endpoint: endpoint.to_owned(),
                reason: "verifier_config bytes are empty".to_owned(),
            });
        }
        if response.proof.is_empty() {
            return Err(ProofProviderError::MalformedAttestationResponse {
                endpoint: endpoint.to_owned(),
                reason: "proof bytes are empty".to_owned(),
            });
        }
        Ok(TeeProofPayload {
            verifier_config: response.verifier_config.clone(),
            proof: response.proof.clone(),
        })
    }
}

impl BatchProofProvider for HttpTeeAttestationProvider {
    fn name(&self) -> &'static str {
        "http-tee-attestation"
    }

    fn build_proof<'a>(&'a self, inputs: &'a BatchPublicInputs) -> ProofFuture<'a> {
        Box::pin(async move {
            let request = self.build_request(inputs);
            let endpoint = self.config.endpoint.as_str().to_owned();

            info!(
                portal = %inputs.portal_address,
                tempo_block_number = inputs.tempo_block_number,
                commitment = %request.commitment,
                format = ?request.format,
                endpoint = %endpoint,
                "Requesting batch attestation from configured TEE service"
            );

            let mut req = self.client.post(self.config.endpoint.clone()).json(&request);
            if let Some(token) = self.config.bearer_token.as_deref() {
                req = req.bearer_auth(token);
            }

            let response = match req.send().await {
                Ok(resp) => resp,
                Err(err) => {
                    let status = err.status().map(|s| s.as_u16()).unwrap_or(0);
                    warn!(
                        endpoint = %endpoint,
                        status,
                        error = %err,
                        "TEE attestation service request failed before producing a response"
                    );
                    return Err(eyre::Report::new(ProofProviderError::RemoteAttestationFailed {
                        endpoint,
                        status,
                        detail: err.to_string(),
                    }));
                }
            };

            let status = response.status();
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                let detail = if body.is_empty() {
                    "(no response body)".to_owned()
                } else {
                    truncate_for_log(&body, 256)
                };
                warn!(
                    endpoint = %endpoint,
                    %status,
                    detail = %detail,
                    "TEE attestation service returned non-success status"
                );
                return Err(eyre::Report::new(ProofProviderError::RemoteAttestationFailed {
                    endpoint,
                    status: status.as_u16(),
                    detail,
                }));
            }

            let parsed: TeeAttestationResponse = match response.json().await {
                Ok(parsed) => parsed,
                Err(err) => {
                    warn!(
                        endpoint = %endpoint,
                        error = %err,
                        "TEE attestation service returned a body that did not decode as JSON"
                    );
                    return Err(eyre::Report::new(
                        ProofProviderError::MalformedAttestationResponse {
                            endpoint,
                            reason: format!("response body did not decode: {err}"),
                        },
                    ));
                }
            };

            let payload =
                Self::validate_response(&endpoint, request.commitment, &parsed).map_err(|err| {
                    warn!(
                        endpoint = %endpoint,
                        error = %err,
                        "TEE attestation service returned a malformed response"
                    );
                    eyre::Report::new(err)
                })?;

            info!(
                endpoint = %endpoint,
                verifier_config_len = payload.verifier_config.len(),
                proof_len = payload.proof.len(),
                "TEE attestation service returned a verifier payload"
            );

            Ok(payload)
        })
    }
}

fn truncate_for_log(value: &str, max_len: usize) -> String {
    if value.len() <= max_len {
        value.to_owned()
    } else {
        let mut truncated = value[..max_len].to_owned();
        truncated.push_str("... (truncated)");
        truncated
    }
}

/// Operator-facing selector for the proof backend.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProofBackend {
    /// [`FailFastProofProvider`] — default. No L1 traffic until an operator opts in.
    #[default]
    FailFast,
    /// [`EmptyLegacyProofProvider`] — submits empty bytes. Dev only.
    EmptyLegacy,
    /// TEE attestation provider. Resolved to [`HttpTeeAttestationProvider`] when an
    /// endpoint is configured, or [`PendingTeeAttestationProvider`] (diagnostic-only)
    /// when no endpoint is set.
    Tee,
}

/// Operator-supplied knobs that customise the TEE backend at runtime.
///
/// All fields are optional from the CLI's perspective — defaulting to
/// `PendingTeeAttestationProvider` (no endpoint, no auth, unconfirmed format) preserves
/// the prior fail-closed posture for operators who have not yet wired the attestation
/// service.
#[derive(Debug, Clone, Default)]
pub struct TeeProviderOptions {
    /// Attestation service URL.
    pub endpoint: Option<Url>,
    /// Optional bearer token.
    pub bearer_token: Option<String>,
    /// Per-request timeout override.
    pub request_timeout: Option<Duration>,
    /// Enclave identity to advertise.
    pub enclave_id: Bytes,
    /// Domain separator to bind.
    pub domain: Bytes,
    /// Best-known attestation flavour.
    pub format: TeeAttestationFormat,
}

impl TeeProviderOptions {
    fn pending_config(&self) -> PendingTeeProviderConfig {
        PendingTeeProviderConfig {
            enclave_id: self.enclave_id.clone(),
            domain: self.domain.clone(),
            format: self.format,
        }
    }

    fn service_config(&self, endpoint: Url) -> TeeAttestationServiceConfig {
        TeeAttestationServiceConfig {
            endpoint,
            bearer_token: self.bearer_token.clone(),
            request_timeout: self
                .request_timeout
                .unwrap_or(DEFAULT_TEE_ATTESTATION_TIMEOUT),
            enclave_id: self.enclave_id.clone(),
            domain: self.domain.clone(),
            format: self.format,
        }
    }
}

impl ProofBackend {
    /// Construct a [`SharedProofProvider`] for the chosen backend.
    ///
    /// For [`Self::Tee`] the provider is resolved at runtime:
    /// - With `options.endpoint = Some(_)` → [`HttpTeeAttestationProvider`].
    /// - With `options.endpoint = None`    → [`PendingTeeAttestationProvider`] (logs the
    ///   commitment that *would* be signed; refuses to submit until an endpoint is set).
    pub fn into_provider(self, options: TeeProviderOptions) -> eyre::Result<SharedProofProvider> {
        match self {
            Self::FailFast => Ok(Arc::new(FailFastProofProvider)),
            Self::EmptyLegacy => Ok(Arc::new(EmptyLegacyProofProvider)),
            Self::Tee => match options.endpoint.clone() {
                Some(endpoint) => Ok(Arc::new(HttpTeeAttestationProvider::new(
                    options.service_config(endpoint),
                )?)),
                None => Ok(Arc::new(PendingTeeAttestationProvider::new(
                    options.pending_config(),
                ))),
            },
        }
    }
}

impl std::str::FromStr for ProofBackend {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "fail-fast" | "failfast" | "reject" => Ok(Self::FailFast),
            "empty-legacy" | "empty" | "legacy" => Ok(Self::EmptyLegacy),
            "tee" | "pending-tee" => Ok(Self::Tee),
            other => Err(format!(
                "unknown proof backend `{other}` (expected one of: fail-fast, empty-legacy, tee)"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256, address};

    fn sample_inputs() -> BatchPublicInputs {
        BatchPublicInputs {
            portal_address: address!("0x00000000000000000000000000000000000000aa"),
            sequencer_address: address!("0x00000000000000000000000000000000000000bb"),
            tempo_block_number: 1234,
            recent_tempo_block_number: 0,
            prev_block_hash: B256::repeat_byte(0x11),
            next_block_hash: B256::repeat_byte(0x22),
            prev_processed_deposit_hash: B256::repeat_byte(0x33),
            next_processed_deposit_hash: B256::repeat_byte(0x44),
            prev_deposit_number: 7,
            next_deposit_number: 9,
            withdrawal_queue_hash: B256::repeat_byte(0x55),
            expected_withdrawal_batch_index: 12,
        }
    }

    #[test]
    fn commitment_is_deterministic() {
        let inputs = sample_inputs();
        assert_eq!(inputs.commitment(), inputs.commitment());
    }

    #[test]
    fn commitment_changes_when_any_input_changes() {
        let base = sample_inputs();
        let mut mutated = base.clone();
        mutated.next_block_hash = B256::repeat_byte(0xff);
        assert_ne!(base.commitment(), mutated.commitment());

        let mut mutated_index = base.clone();
        mutated_index.expected_withdrawal_batch_index += 1;
        assert_ne!(base.commitment(), mutated_index.commitment());

        let mut mutated_portal = base.clone();
        mutated_portal.portal_address = Address::repeat_byte(0xcc);
        assert_ne!(base.commitment(), mutated_portal.commitment());
    }

    #[test]
    fn empty_payload_is_zero_bytes() {
        let payload = TeeProofPayload::empty_legacy();
        assert!(payload.verifier_config.is_empty());
        assert!(payload.proof.is_empty());
    }

    #[test]
    fn verifier_config_encoding_is_length_prefixed() {
        let config = TeeVerifierConfig {
            format: TeeAttestationFormat::Unconfirmed,
            domain: Bytes::from_static(b"portal-domain"),
            enclave_id: Bytes::from_static(b"enclave-id"),
            commitment: B256::repeat_byte(0xab),
        };
        let encoded = config.encode();
        assert_eq!(encoded[0], TEE_VERIFIER_CONFIG_VERSION);
        assert_eq!(encoded[1], TeeAttestationFormat::Unconfirmed.tag());
        let domain_len = u16::from_be_bytes([encoded[2], encoded[3]]);
        assert_eq!(usize::from(domain_len), config.domain.len());
        let domain_end = 4 + usize::from(domain_len);
        assert_eq!(&encoded[4..domain_end], &config.domain[..]);
        let enclave_len = u16::from_be_bytes([encoded[domain_end], encoded[domain_end + 1]]);
        assert_eq!(usize::from(enclave_len), config.enclave_id.len());
        let enclave_end = domain_end + 2 + usize::from(enclave_len);
        assert_eq!(
            &encoded[domain_end + 2..enclave_end],
            &config.enclave_id[..]
        );
        assert_eq!(&encoded[enclave_end..], config.commitment.as_slice());
    }

    #[test]
    fn attestation_encoding_is_length_prefixed() {
        let att = TeeAttestation {
            signature: Bytes::from_static(b"sig"),
            attestation_quote: Bytes::from_static(b"quote-bytes"),
        };
        let encoded = att.encode();
        assert_eq!(encoded[0], TEE_ATTESTATION_VERSION);
        let sig_len = u16::from_be_bytes([encoded[1], encoded[2]]);
        assert_eq!(usize::from(sig_len), att.signature.len());
        let sig_end = 3 + usize::from(sig_len);
        assert_eq!(&encoded[3..sig_end], &att.signature[..]);
        let quote_len = u32::from_be_bytes([
            encoded[sig_end],
            encoded[sig_end + 1],
            encoded[sig_end + 2],
            encoded[sig_end + 3],
        ]);
        assert_eq!(quote_len as usize, att.attestation_quote.len());
        assert_eq!(&encoded[sig_end + 4..], &att.attestation_quote[..]);
    }

    #[tokio::test]
    async fn fail_fast_provider_errors_with_no_backend_configured() {
        let provider = FailFastProofProvider;
        let err = provider
            .build_proof(&sample_inputs())
            .await
            .expect_err("fail-fast provider must error");
        assert!(
            err.downcast_ref::<ProofProviderError>()
                .map(|e| matches!(e, ProofProviderError::NoBackendConfigured))
                .unwrap_or(false),
            "expected NoBackendConfigured, got: {err}"
        );
    }

    #[tokio::test]
    async fn empty_legacy_provider_returns_empty_payload() {
        let provider = EmptyLegacyProofProvider;
        let payload = provider.build_proof(&sample_inputs()).await.unwrap();
        assert_eq!(payload, TeeProofPayload::empty_legacy());
    }

    #[tokio::test]
    async fn pending_tee_provider_errors_with_integration_pending() {
        let provider = PendingTeeAttestationProvider::new(PendingTeeProviderConfig::default());
        let err = provider
            .build_proof(&sample_inputs())
            .await
            .expect_err("pending tee provider must error");
        assert!(
            err.downcast_ref::<ProofProviderError>()
                .map(|e| matches!(e, ProofProviderError::TempoIntegrationPending(_)))
                .unwrap_or(false),
            "expected TempoIntegrationPending, got: {err}"
        );
    }

    #[test]
    fn proof_backend_parses_known_variants() {
        use std::str::FromStr;
        assert_eq!(
            ProofBackend::from_str("fail-fast").unwrap(),
            ProofBackend::FailFast
        );
        assert_eq!(
            ProofBackend::from_str("EMPTY").unwrap(),
            ProofBackend::EmptyLegacy
        );
        assert_eq!(ProofBackend::from_str("tee").unwrap(), ProofBackend::Tee);
        assert!(ProofBackend::from_str("noop").is_err());
    }

    #[test]
    fn proof_backend_default_is_fail_fast() {
        assert_eq!(ProofBackend::default(), ProofBackend::FailFast);
    }

    fn sample_response(commitment: B256) -> TeeAttestationResponse {
        TeeAttestationResponse {
            version: TEE_ATTESTATION_SERVICE_VERSION,
            commitment,
            verifier_config: Bytes::from_static(b"verifier-config"),
            proof: Bytes::from_static(b"attestation-proof"),
        }
    }

    #[test]
    fn http_provider_validate_response_accepts_well_formed_payload() {
        let inputs = sample_inputs();
        let commitment = inputs.commitment();
        let response = sample_response(commitment);
        let payload =
            HttpTeeAttestationProvider::validate_response("http://test", commitment, &response)
                .expect("valid response must accept");
        assert_eq!(payload.verifier_config, response.verifier_config);
        assert_eq!(payload.proof, response.proof);
    }

    #[test]
    fn http_provider_validate_response_rejects_wrong_version() {
        let inputs = sample_inputs();
        let commitment = inputs.commitment();
        let mut response = sample_response(commitment);
        response.version = TEE_ATTESTATION_SERVICE_VERSION + 1;
        let err = HttpTeeAttestationProvider::validate_response("http://test", commitment, &response)
            .expect_err("wrong version must fail");
        assert!(
            matches!(err, ProofProviderError::MalformedAttestationResponse { .. }),
            "expected MalformedAttestationResponse, got: {err}"
        );
    }

    #[test]
    fn http_provider_validate_response_rejects_commitment_drift() {
        let inputs = sample_inputs();
        let request_commitment = inputs.commitment();
        let mut response = sample_response(request_commitment);
        response.commitment = B256::repeat_byte(0xee);
        let err = HttpTeeAttestationProvider::validate_response(
            "http://test",
            request_commitment,
            &response,
        )
        .expect_err("commitment mismatch must fail");
        match err {
            ProofProviderError::MalformedAttestationResponse { reason, .. } => {
                assert!(reason.contains("commitment mismatch"), "reason: {reason}");
            }
            other => panic!("expected MalformedAttestationResponse, got: {other}"),
        }
    }

    #[test]
    fn http_provider_validate_response_rejects_empty_verifier_config() {
        let inputs = sample_inputs();
        let commitment = inputs.commitment();
        let mut response = sample_response(commitment);
        response.verifier_config = Bytes::new();
        let err = HttpTeeAttestationProvider::validate_response("http://test", commitment, &response)
            .expect_err("empty verifier_config must fail");
        match err {
            ProofProviderError::MalformedAttestationResponse { reason, .. } => {
                assert!(reason.contains("verifier_config"), "reason: {reason}");
            }
            other => panic!("expected MalformedAttestationResponse, got: {other}"),
        }
    }

    #[test]
    fn http_provider_validate_response_rejects_empty_proof() {
        let inputs = sample_inputs();
        let commitment = inputs.commitment();
        let mut response = sample_response(commitment);
        response.proof = Bytes::new();
        let err = HttpTeeAttestationProvider::validate_response("http://test", commitment, &response)
            .expect_err("empty proof must fail");
        match err {
            ProofProviderError::MalformedAttestationResponse { reason, .. } => {
                assert!(reason.contains("proof"), "reason: {reason}");
            }
            other => panic!("expected MalformedAttestationResponse, got: {other}"),
        }
    }

    #[test]
    fn http_provider_build_request_embeds_commitment_and_inputs() {
        let inputs = sample_inputs();
        let endpoint = Url::parse("https://example.invalid/attest").unwrap();
        let provider = HttpTeeAttestationProvider::new(TeeAttestationServiceConfig {
            endpoint,
            bearer_token: None,
            request_timeout: DEFAULT_TEE_ATTESTATION_TIMEOUT,
            enclave_id: Bytes::from_static(b"enclave-id"),
            domain: Bytes::from_static(b"domain"),
            format: TeeAttestationFormat::Unconfirmed,
        })
        .unwrap();

        let request = provider.build_request(&inputs);
        assert_eq!(request.version, TEE_ATTESTATION_SERVICE_VERSION);
        assert_eq!(request.commitment, inputs.commitment());
        assert_eq!(request.public_inputs.portal_address, inputs.portal_address);
        assert_eq!(
            request.public_inputs.expected_withdrawal_batch_index,
            inputs.expected_withdrawal_batch_index
        );
        assert_eq!(request.enclave_id, Bytes::from_static(b"enclave-id"));
        assert_eq!(request.domain, Bytes::from_static(b"domain"));
        assert_eq!(request.format, TeeAttestationFormat::Unconfirmed);
        assert_eq!(request.protocol, TeeAttestationRequest::PROTOCOL_TAG);
    }

    #[tokio::test]
    async fn http_provider_returns_remote_failure_when_unreachable() {
        // Reserved TEST-NET-1 (RFC 5737) — never reachable. Short timeout makes the test
        // fast without depending on the OS connect-refused timing.
        let endpoint = Url::parse("http://192.0.2.1:1/attest").unwrap();
        let provider = HttpTeeAttestationProvider::new(TeeAttestationServiceConfig {
            endpoint,
            bearer_token: None,
            request_timeout: Duration::from_millis(100),
            enclave_id: Bytes::new(),
            domain: Bytes::new(),
            format: TeeAttestationFormat::Unconfirmed,
        })
        .unwrap();

        let err = provider
            .build_proof(&sample_inputs())
            .await
            .expect_err("unreachable endpoint must error");
        let downcast = err
            .downcast_ref::<ProofProviderError>()
            .expect("error should downcast to ProofProviderError");
        assert!(
            matches!(downcast, ProofProviderError::RemoteAttestationFailed { .. }),
            "expected RemoteAttestationFailed, got: {downcast}"
        );
    }

    #[tokio::test]
    async fn proof_backend_tee_without_endpoint_falls_back_to_pending() {
        let provider = ProofBackend::Tee
            .into_provider(TeeProviderOptions::default())
            .unwrap();
        assert_eq!(provider.name(), "pending-tee-attestation");
        let err = provider
            .build_proof(&sample_inputs())
            .await
            .expect_err("pending provider must error");
        assert!(
            err.downcast_ref::<ProofProviderError>()
                .map(|e| matches!(e, ProofProviderError::TempoIntegrationPending(_)))
                .unwrap_or(false),
            "expected TempoIntegrationPending, got: {err}"
        );
    }

    #[tokio::test]
    async fn proof_backend_tee_with_endpoint_resolves_to_http_provider() {
        let options = TeeProviderOptions {
            endpoint: Some(Url::parse("https://attestation.invalid/sign").unwrap()),
            ..TeeProviderOptions::default()
        };
        let provider = ProofBackend::Tee.into_provider(options).unwrap();
        assert_eq!(provider.name(), "http-tee-attestation");
    }

    #[tokio::test]
    async fn http_provider_round_trips_against_local_attestation_service() {
        use std::sync::Mutex;

        use axum::{Json, Router, extract::State, routing::post};

        #[derive(Default)]
        struct CapturedRequest {
            last: Option<TeeAttestationRequest>,
        }

        async fn handler(
            State(captured): State<Arc<Mutex<CapturedRequest>>>,
            Json(req): Json<TeeAttestationRequest>,
        ) -> Json<TeeAttestationResponse> {
            let commitment = req.commitment;
            captured.lock().unwrap().last = Some(req);
            Json(TeeAttestationResponse {
                version: TEE_ATTESTATION_SERVICE_VERSION,
                commitment,
                verifier_config: Bytes::from_static(b"verifier-config-from-service"),
                proof: Bytes::from_static(b"proof-from-service"),
            })
        }

        let captured = Arc::new(Mutex::new(CapturedRequest::default()));
        let app = Router::new()
            .route("/attest", post(handler))
            .with_state(captured.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let endpoint = Url::parse(&format!("http://{local_addr}/attest")).unwrap();
        let provider = HttpTeeAttestationProvider::new(TeeAttestationServiceConfig {
            endpoint,
            bearer_token: Some("test-bearer".to_owned()),
            request_timeout: Duration::from_secs(5),
            enclave_id: Bytes::from_static(b"enclave-id"),
            domain: Bytes::from_static(b"domain"),
            format: TeeAttestationFormat::Unconfirmed,
        })
        .unwrap();

        let inputs = sample_inputs();
        let payload = provider.build_proof(&inputs).await.unwrap();

        assert_eq!(
            payload.verifier_config,
            Bytes::from_static(b"verifier-config-from-service")
        );
        assert_eq!(payload.proof, Bytes::from_static(b"proof-from-service"));

        let last = captured.lock().unwrap().last.clone().expect("server saw a request");
        assert_eq!(last.commitment, inputs.commitment());
        assert_eq!(last.public_inputs.portal_address, inputs.portal_address);
        assert_eq!(last.protocol, TeeAttestationRequest::PROTOCOL_TAG);
        assert_eq!(last.enclave_id, Bytes::from_static(b"enclave-id"));

        server.abort();
    }

    #[tokio::test]
    async fn http_provider_rejects_commitment_drift_from_local_service() {
        use axum::{Json, Router, routing::post};

        async fn handler(
            Json(req): Json<TeeAttestationRequest>,
        ) -> Json<TeeAttestationResponse> {
            // Sign a *different* commitment than the request — exactly the failure mode
            // `validate_response` is meant to refuse.
            let _ = req;
            Json(TeeAttestationResponse {
                version: TEE_ATTESTATION_SERVICE_VERSION,
                commitment: B256::repeat_byte(0xee),
                verifier_config: Bytes::from_static(b"verifier-config"),
                proof: Bytes::from_static(b"proof"),
            })
        }

        let app = Router::new().route("/attest", post(handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let endpoint = Url::parse(&format!("http://{local_addr}/attest")).unwrap();
        let provider = HttpTeeAttestationProvider::new(TeeAttestationServiceConfig {
            endpoint,
            bearer_token: None,
            request_timeout: Duration::from_secs(5),
            enclave_id: Bytes::new(),
            domain: Bytes::new(),
            format: TeeAttestationFormat::Unconfirmed,
        })
        .unwrap();

        let err = provider
            .build_proof(&sample_inputs())
            .await
            .expect_err("commitment mismatch must fail");
        let downcast = err
            .downcast_ref::<ProofProviderError>()
            .expect("error should downcast to ProofProviderError");
        match downcast {
            ProofProviderError::MalformedAttestationResponse { reason, .. } => {
                assert!(reason.contains("commitment mismatch"), "reason: {reason}");
            }
            other => panic!("expected MalformedAttestationResponse, got: {other}"),
        }

        server.abort();
    }

    #[tokio::test]
    async fn http_provider_propagates_remote_5xx_as_remote_attestation_failed() {
        use axum::{Router, http::StatusCode, response::IntoResponse, routing::post};

        async fn handler() -> impl IntoResponse {
            (StatusCode::INTERNAL_SERVER_ERROR, "enclave key rotation in progress")
        }

        let app = Router::new().route("/attest", post(handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let endpoint = Url::parse(&format!("http://{local_addr}/attest")).unwrap();
        let provider = HttpTeeAttestationProvider::new(TeeAttestationServiceConfig {
            endpoint,
            bearer_token: None,
            request_timeout: Duration::from_secs(5),
            enclave_id: Bytes::new(),
            domain: Bytes::new(),
            format: TeeAttestationFormat::Unconfirmed,
        })
        .unwrap();

        let err = provider
            .build_proof(&sample_inputs())
            .await
            .expect_err("5xx must surface as RemoteAttestationFailed");
        let downcast = err
            .downcast_ref::<ProofProviderError>()
            .expect("error should downcast to ProofProviderError");
        match downcast {
            ProofProviderError::RemoteAttestationFailed { status, detail, .. } => {
                assert_eq!(*status, 500);
                assert!(detail.contains("enclave key rotation"), "detail: {detail}");
            }
            other => panic!("expected RemoteAttestationFailed, got: {other}"),
        }

        server.abort();
    }

    #[tokio::test]
    async fn fixture_provider_bytes_survive_submit_batch_calldata_roundtrip() {
        use alloy_sol_types::SolCall;

        use crate::abi::ZonePortal::submitBatchCall;

        let inputs = sample_inputs();
        let fixture_verifier_config = Bytes::from_static(b"verifier-config-fixture-bytes");
        let fixture_proof = Bytes::from_static(b"proof-fixture-bytes");

        let provider = StaticTeeProofProvider::new(TeeProofPayload {
            verifier_config: fixture_verifier_config.clone(),
            proof: fixture_proof.clone(),
        });

        // The fixture provider must produce the exact bytes the operator handed it.
        let first = provider.build_proof(&inputs).await.unwrap();
        let second = provider.build_proof(&inputs).await.unwrap();
        assert_eq!(first, second, "fixture provider must be deterministic");
        assert_eq!(first.verifier_config, fixture_verifier_config);
        assert_eq!(first.proof, fixture_proof);

        // Constructing a submitBatchCall with these bytes (the exact shape
        // `BatchSubmitter::submit_batch` builds) and round-tripping through ABI
        // encoding/decoding must preserve them byte-for-byte. This is the
        // contract `BatchSubmitter` relies on when it forwards
        // `(verifier_config, proof)` into `ZonePortal.submitBatch`.
        let call = submitBatchCall {
            tempoBlockNumber: inputs.tempo_block_number,
            recentTempoBlockNumber: inputs.recent_tempo_block_number,
            blockTransition: crate::abi::BlockTransition {
                prevBlockHash: inputs.prev_block_hash,
                nextBlockHash: inputs.next_block_hash,
            },
            depositQueueTransition: crate::abi::DepositQueueTransition {
                prevProcessedHash: inputs.prev_processed_deposit_hash,
                nextProcessedHash: inputs.next_processed_deposit_hash,
                prevDepositNumber: inputs.prev_deposit_number,
                nextDepositNumber: inputs.next_deposit_number,
            },
            withdrawalQueueHash: inputs.withdrawal_queue_hash,
            verifierConfig: first.verifier_config.clone(),
            proof: first.proof.clone(),
        };

        let encoded = call.abi_encode();
        let decoded = submitBatchCall::abi_decode(&encoded)
            .expect("submitBatch calldata must round-trip");

        assert_eq!(decoded.verifierConfig, fixture_verifier_config);
        assert_eq!(decoded.proof, fixture_proof);
        assert_eq!(decoded.tempoBlockNumber, inputs.tempo_block_number);
        assert_eq!(
            decoded.withdrawalQueueHash, inputs.withdrawal_queue_hash,
            "the fields adjacent to the proof bytes also survive"
        );
    }
}
