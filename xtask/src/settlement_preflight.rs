//! `settlement-preflight` — read-only diagnostic for first-batch (or stuck-zone)
//! settlement against a deployed Tempo portal.
//!
//! Triages whether a `submitBatch` call *would* succeed before any L1 transaction
//! is signed. Backs the runbooks in `docs/RUNBOOK_FIRST_BATCH.md`:
//!
//! - Fresh zone: portal `blockHash() == 0`, no batches submitted yet.
//! - Stuck zone: a prior `submitBatch` reverted and the operator wants to see
//!   which input is mismatched.
//!
//! The command only performs read RPC calls. It does **not** sign or broadcast
//! anything. The proof commitment hash is computed and printed so the same
//! value can be cross-checked against a future TEE attestation payload.

use std::sync::Arc;

use alloy::{
    primitives::{Address, B256},
    providers::{Provider, ProviderBuilder},
};
use eyre::{Context, Result};
use tempo_alloy::TempoNetwork;
use zone::{
    BatchData, BatchSubmitter, EmptyLegacyProofProvider,
    abi::{TEMPO_STATE_ADDRESS, TempoState, ZONE_INBOX_ADDRESS, ZoneInbox, ZonePortal},
};

#[derive(Debug, clap::Parser)]
pub(crate) struct SettlementPreflightCmd {
    /// Tempo L1 HTTP/WS RPC URL.
    #[arg(long, default_value = "https://rpc.moderato.tempo.xyz")]
    l1_rpc_url: String,

    /// Zone L2 HTTP RPC URL.
    #[arg(long, default_value = "http://127.0.0.1:8545")]
    zone_rpc_url: String,

    /// ZonePortal contract address on L1.
    #[arg(long)]
    portal_address: Address,

    /// Optional: ZoneInbox address on L2. Defaults to the standard predeploy.
    #[arg(long, default_value_t = ZONE_INBOX_ADDRESS)]
    inbox_address: Address,

    /// Optional: TempoState predeploy address on L2. Defaults to the standard predeploy.
    #[arg(long, default_value_t = TEMPO_STATE_ADDRESS)]
    tempo_state_address: Address,

    /// Optional: zone L2 block number to evaluate as the *next* batch target.
    /// Defaults to the current zone tip.
    #[arg(long)]
    next_zone_block: Option<u64>,
}

impl SettlementPreflightCmd {
    pub(crate) async fn run(self) -> Result<()> {
        let l1_provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect(&self.l1_rpc_url)
            .await
            .wrap_err_with(|| format!("failed to connect to L1 RPC at {}", self.l1_rpc_url))?
            .erased();
        let l1_portal_provider = ProviderBuilder::new()
            .connect(&self.l1_rpc_url)
            .await
            .wrap_err_with(|| format!("failed to connect to L1 RPC at {}", self.l1_rpc_url))?
            .erased();
        let zone_provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect(&self.zone_rpc_url)
            .await
            .wrap_err_with(|| format!("failed to connect to zone RPC at {}", self.zone_rpc_url))?
            .erased();

        let portal = ZonePortal::new(self.portal_address, l1_provider.clone());
        let genesis_tempo_block_number = portal
            .genesisTempoBlockNumber()
            .call()
            .await
            .wrap_err("failed to read genesisTempoBlockNumber from portal")?;
        let portal_block_hash = portal
            .blockHash()
            .call()
            .await
            .wrap_err("failed to read blockHash from portal")?;
        let last_synced_tempo_block_number = portal
            .lastSyncedTempoBlockNumber()
            .call()
            .await
            .wrap_err("failed to read lastSyncedTempoBlockNumber from portal")?;
        let sequencer = portal
            .sequencer()
            .call()
            .await
            .wrap_err("failed to read sequencer from portal")?;
        let verifier = portal
            .verifier()
            .call()
            .await
            .wrap_err("failed to read verifier from portal")?;
        let withdrawal_batch_index = portal
            .withdrawalBatchIndex()
            .call()
            .await
            .wrap_err("failed to read withdrawalBatchIndex from portal")?;
        let withdrawal_queue_head = portal
            .withdrawalQueueHead()
            .call()
            .await
            .wrap_err("failed to read withdrawalQueueHead from portal")?;
        let withdrawal_queue_tail = portal
            .withdrawalQueueTail()
            .call()
            .await
            .wrap_err("failed to read withdrawalQueueTail from portal")?;
        let last_processed_deposit_number = portal
            .lastProcessedDepositNumber()
            .call()
            .await
            .wrap_err("failed to read lastProcessedDepositNumber from portal")?;
        let current_deposit_queue_hash = portal
            .currentDepositQueueHash()
            .call()
            .await
            .wrap_err("failed to read currentDepositQueueHash from portal")?;

        println!("Portal {}", self.portal_address);
        println!("  L1 RPC:                 {}", self.l1_rpc_url);
        println!("  Zone RPC:               {}", self.zone_rpc_url);
        println!("  Sequencer:              {sequencer}");
        println!("  Verifier:               {verifier}");
        println!("  Genesis Tempo Block:    {genesis_tempo_block_number}");
        println!("  Block Hash:             {portal_block_hash}");
        println!("  Last Synced Tempo Blk:  {last_synced_tempo_block_number}");
        println!("  Withdrawal Batch Index: {withdrawal_batch_index}");
        println!(
            "  Withdrawal Queue:       head={withdrawal_queue_head} tail={withdrawal_queue_tail}"
        );
        println!(
            "  Last Processed Deposit: number={last_processed_deposit_number} \
             hash={current_deposit_queue_hash}"
        );

        let is_fresh_zone = portal_block_hash.is_zero() && withdrawal_batch_index == 0;
        println!(
            "\nZone state: {}",
            if is_fresh_zone {
                "FRESH — no batches submitted yet"
            } else {
                "ACTIVE — at least one batch already settled"
            }
        );

        // Build a candidate batch from current zone L2 state so we can run the
        // existing preflight + commitment plumbing without faking values.
        let target_zone_block = match self.next_zone_block {
            Some(n) => n,
            None => zone_provider
                .get_block_number()
                .await
                .wrap_err("failed to read current zone L2 block number")?,
        };
        println!("\nTarget zone block: {target_zone_block}");

        if target_zone_block == 0 {
            eyre::bail!("zone L2 has no committed blocks yet — nothing to settle");
        }

        let target_block = zone_provider
            .get_block_by_number(target_zone_block.into())
            .await
            .wrap_err_with(|| format!("failed to read zone L2 block {target_zone_block}"))?
            .ok_or_else(|| eyre::eyre!("zone L2 block {target_zone_block} not found"))?;
        let next_block_hash: B256 = target_block.header.hash;

        let inbox = ZoneInbox::new(self.inbox_address, zone_provider.clone());
        let tempo_state = TempoState::new(self.tempo_state_address, zone_provider.clone());

        let next_processed_deposit_hash = inbox
            .processedDepositQueueHash()
            .block(target_zone_block.into())
            .call()
            .await
            .wrap_err("failed to read processedDepositQueueHash at target block")?;
        let next_deposit_number = inbox
            .processedDepositNumber()
            .block(target_zone_block.into())
            .call()
            .await
            .wrap_err("failed to read processedDepositNumber at target block")?;
        let tempo_block_number = tempo_state
            .tempoBlockNumber()
            .block(target_zone_block.into())
            .call()
            .await
            .wrap_err("failed to read tempoBlockNumber at target block")?;

        // The portal accepts withdrawal_queue_hash = 0 for batches without
        // withdrawals; we leave it zero here to keep the preflight read-only
        // and decoupled from the outbox event walk.
        let batch = BatchData {
            tempo_block_number,
            prev_block_hash: portal_block_hash,
            next_block_hash,
            prev_processed_deposit_hash: current_deposit_queue_hash,
            next_processed_deposit_hash,
            prev_deposit_number: last_processed_deposit_number,
            next_deposit_number,
            withdrawal_queue_hash: B256::ZERO,
        };

        // EmptyLegacyProofProvider is fine here — settlement-preflight never
        // actually submits, it only triggers the diagnostic snapshot.
        let submitter = BatchSubmitter::new_with_proof_provider(
            self.portal_address,
            l1_provider,
            l1_portal_provider,
            genesis_tempo_block_number,
            Arc::new(EmptyLegacyProofProvider),
        );

        let preflight = submitter
            .preflight_report(&batch, 0)
            .await
            .wrap_err("portal preflight RPC reads failed")?;

        // recent_tempo_block_number=0 ⇒ direct EIP-2935 lookup; ancestry mode
        // is a separate runbook in docs/RUNBOOK_FIRST_BATCH.md.
        let public_inputs = preflight.public_inputs(&batch, 0);

        println!("\nProposed batch");
        println!(
            "  Tempo block:            {tempo_block_number} \
             (genesis {genesis_tempo_block_number})"
        );
        println!("  Prev block hash:        {}", batch.prev_block_hash);
        println!("  Next block hash:        {next_block_hash}");
        println!(
            "  Deposit transition:     {} → {} (number {} → {})",
            batch.prev_processed_deposit_hash,
            batch.next_processed_deposit_hash,
            batch.prev_deposit_number,
            batch.next_deposit_number,
        );
        println!(
            "  Expected wd batch idx:  {}",
            public_inputs.expected_withdrawal_batch_index
        );
        println!("  Public-input commit:    {}", public_inputs.commitment());

        let mut issues: Vec<String> = Vec::new();
        if portal_block_hash != batch.prev_block_hash {
            issues.push(format!(
                "portal.blockHash() = {portal_block_hash} but batch.prev_block_hash = {} \
                 — the zone is not anchored on the portal-confirmed block",
                batch.prev_block_hash
            ));
        }
        if last_processed_deposit_number != batch.prev_deposit_number {
            issues.push(format!(
                "portal.lastProcessedDepositNumber = {last_processed_deposit_number} \
                 but batch.prev_deposit_number = {} — deposit queue is desynced",
                batch.prev_deposit_number
            ));
        }
        if tempo_block_number < genesis_tempo_block_number {
            issues.push(format!(
                "batch.tempo_block_number = {tempo_block_number} is below \
                 genesisTempoBlockNumber = {genesis_tempo_block_number}"
            ));
        }
        if tempo_block_number > preflight.current_l1_block {
            issues.push(format!(
                "batch.tempo_block_number = {tempo_block_number} exceeds L1 tip = {} \
                 — zone has not yet observed the L1 anchor",
                preflight.current_l1_block
            ));
        }

        println!("\nDiagnostics");
        if issues.is_empty() {
            println!("  No portal-side mismatch detected.");
            println!("  Verifier acceptance still depends on the proof payload:");
            println!("    - Live Moderato verifier rejects empty proofs (issue #2).");
            println!("    - TEE attestation pipeline tracked in docs/TEE_PROOF.md.");
        } else {
            println!("  Found {} blocker(s):", issues.len());
            for (i, issue) in issues.iter().enumerate() {
                println!("    [{}] {issue}", i + 1);
            }
        }

        Ok(())
    }
}
