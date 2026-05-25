//! Sequencer background task orchestration.

use std::{sync::Arc, time::Duration};

use alloy_primitives::Address;
use alloy_provider::{DynProvider, Provider, ProviderBuilder};
use alloy_signer_local::PrivateKeySigner;
use tempo_alloy::{TempoNetwork, provider::ext::TempoProviderBuilderExt};
use tokio::sync::Notify;

use crate::{
    SharedWithdrawalStore, WithdrawalProcessorConfig, ZoneMonitorConfig,
    proof::{FailFastProofProvider, SharedProofProvider},
    spawn_zone_monitor, withdrawals,
};

/// Configuration for all zone sequencer background tasks.
#[derive(Clone)]
pub struct ZoneSequencerConfig {
    /// ZonePortal contract address on Tempo L1.
    pub portal_address: Address,
    /// Tempo L1 RPC URL.
    pub l1_rpc_url: String,
    /// Interval between WebSocket reconnection attempts for long-lived RPC clients.
    pub retry_connection_interval: Duration,
    /// How often the withdrawal processor polls the L1 queue.
    pub withdrawal_poll_interval: Duration,
    /// ZoneOutbox contract address on Zone L2.
    pub outbox_address: Address,
    /// ZoneInbox contract address on Zone L2.
    pub inbox_address: Address,
    /// TempoState predeploy address on Zone L2.
    pub tempo_state_address: Address,
    /// Zone L2 RPC URL.
    pub zone_rpc_url: String,
    /// How often the zone monitor polls for new L2 blocks.
    pub zone_poll_interval: Duration,
    /// Maximum time to accumulate zone blocks before submitting a batch to L1.
    pub batch_interval: Duration,
    /// Builds `(verifierConfig, proof)` for each batch. Defaults to
    /// [`FailFastProofProvider`] — production deployments must opt into a real
    /// backend explicitly.
    pub proof_provider: SharedProofProvider,
}

impl std::fmt::Debug for ZoneSequencerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZoneSequencerConfig")
            .field("portal_address", &self.portal_address)
            .field("l1_rpc_url", &self.l1_rpc_url)
            .field("retry_connection_interval", &self.retry_connection_interval)
            .field("withdrawal_poll_interval", &self.withdrawal_poll_interval)
            .field("outbox_address", &self.outbox_address)
            .field("inbox_address", &self.inbox_address)
            .field("tempo_state_address", &self.tempo_state_address)
            .field("zone_rpc_url", &self.zone_rpc_url)
            .field("zone_poll_interval", &self.zone_poll_interval)
            .field("batch_interval", &self.batch_interval)
            .field("proof_provider", &self.proof_provider.name())
            .finish()
    }
}

impl ZoneSequencerConfig {
    /// Construct a sequencer config with the default fail-fast proof provider.
    /// Callers should attach a real backend before connecting to a non-permissive
    /// verifier.
    pub fn new_with_defaults(
        portal_address: Address,
        l1_rpc_url: String,
        retry_connection_interval: Duration,
        withdrawal_poll_interval: Duration,
        outbox_address: Address,
        inbox_address: Address,
        tempo_state_address: Address,
        zone_rpc_url: String,
        zone_poll_interval: Duration,
        batch_interval: Duration,
    ) -> Self {
        Self {
            portal_address,
            l1_rpc_url,
            retry_connection_interval,
            withdrawal_poll_interval,
            outbox_address,
            inbox_address,
            tempo_state_address,
            zone_rpc_url,
            zone_poll_interval,
            batch_interval,
            proof_provider: std::sync::Arc::new(FailFastProofProvider),
        }
    }
}

/// Handles returned by [`spawn_zone_sequencer`] for managing background tasks.
pub struct ZoneSequencerHandle {
    /// Join handle for the withdrawal processor task.
    pub withdrawal_handle: tokio::task::JoinHandle<()>,
    /// Join handle for the zone monitor task (which also handles batch submission).
    pub monitor_handle: tokio::task::JoinHandle<()>,
}

/// Spawn all zone sequencer background tasks.
///
/// This is the top-level POC entrypoint that starts:
/// - **Zone monitor** — polls the Zone L2 for new blocks, extracts withdrawal events into the
///   shared store, builds [`crate::BatchData`], and submits each batch synchronously to the
///   ZonePortal on Tempo L1. Local state only advances on successful submission.
/// - **Withdrawal processor** — polls the ZonePortal withdrawal queue on Tempo L1 and calls
///   `processWithdrawal` for each pending withdrawal.
///
/// Both tasks share a single L1 provider and nonce manager to prevent signing/nonce contention
/// when submitting concurrent L1 transactions.
pub async fn spawn_zone_sequencer(
    config: ZoneSequencerConfig,
    signer: PrivateKeySigner,
) -> ZoneSequencerHandle {
    // Build a single shared L1 provider with the sequencer wallet.
    // Both the batch submitter (inside the zone monitor) and the withdrawal
    // processor use this provider, ensuring nonces are tracked in one place.
    let wallet = alloy_network::EthereumWallet::from(signer);
    let l1_provider: DynProvider<TempoNetwork> =
        ProviderBuilder::new_with_network::<TempoNetwork>()
            .with_nonce_key_filler()
            .wallet(wallet)
            .connect(&config.l1_rpc_url)
            .await
            .expect("valid L1 RPC URL")
            .erased();

    let withdrawal_store: SharedWithdrawalStore = Default::default();
    let withdrawal_notify = Arc::new(Notify::new());
    let withdrawal_repair_notify = Arc::new(Notify::new());

    let withdrawal_config = WithdrawalProcessorConfig {
        portal_address: config.portal_address,
        l1_rpc_url: config.l1_rpc_url.clone(),
        fallback_poll_interval: config.withdrawal_poll_interval,
    };

    let monitor_config = ZoneMonitorConfig {
        outbox_address: config.outbox_address,
        inbox_address: config.inbox_address,
        tempo_state_address: config.tempo_state_address,
        zone_rpc_url: config.zone_rpc_url,
        retry_connection_interval: config.retry_connection_interval,
        poll_interval: config.zone_poll_interval,
        batch_interval: config.batch_interval,
        portal_address: config.portal_address,
    };

    let withdrawal_handle = withdrawals::spawn_withdrawal_processor(
        withdrawal_config,
        l1_provider.clone(),
        withdrawal_store.clone(),
        withdrawal_notify.clone(),
        withdrawal_repair_notify.clone(),
    );
    let monitor_handle = spawn_zone_monitor(
        monitor_config,
        l1_provider,
        withdrawal_store,
        withdrawal_notify,
        withdrawal_repair_notify,
        config.proof_provider.clone(),
    );

    ZoneSequencerHandle {
        withdrawal_handle,
        monitor_handle,
    }
}
