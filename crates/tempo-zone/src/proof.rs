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

use std::{future::Future, pin::Pin, sync::Arc};

use alloy_primitives::{Address, B256, Bytes, keccak256};
use alloy_sol_types::SolValue;
use serde::{Deserialize, Serialize};
use tracing::warn;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    /// TEE provider is wired in but the external Tempo / enclave details required to
    /// actually generate a valid proof are still outstanding. See
    /// `docs/TEE_PROOF.md` for the open questions.
    #[error("TEE proof integration pending Tempo confirmation (see docs/TEE_PROOF.md): {0}")]
    TempoIntegrationPending(&'static str),
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

/// Operator-facing selector for the proof backend.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProofBackend {
    /// [`FailFastProofProvider`] — default. No L1 traffic until an operator opts in.
    #[default]
    FailFast,
    /// [`EmptyLegacyProofProvider`] — submits empty bytes. Dev only.
    EmptyLegacy,
    /// [`PendingTeeAttestationProvider`] with a default config. Logs what *would* be
    /// submitted and refuses until the enclave runtime is connected.
    Tee,
}

impl ProofBackend {
    /// Construct a [`SharedProofProvider`] for the chosen backend.
    pub fn into_provider(self) -> SharedProofProvider {
        match self {
            Self::FailFast => Arc::new(FailFastProofProvider),
            Self::EmptyLegacy => Arc::new(EmptyLegacyProofProvider),
            Self::Tee => Arc::new(PendingTeeAttestationProvider::new(
                PendingTeeProviderConfig::default(),
            )),
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
}
