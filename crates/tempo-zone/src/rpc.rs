//! [`ZoneRpcApi`] implementation backed by reth's EthApi (in-process reth-backed).
//!
//! Re-exports the standalone `zone-rpc` crate so everything is accessible
//! via `zone::rpc::*`.

pub use zone_rpc::*;

use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::{Arc, Weak},
    time::Duration,
};

use alloy_consensus::{BlockHeader, Transaction as _, TxReceipt};
use alloy_network::{ReceiptResponse, TransactionResponse};
use alloy_primitives::{Address, B256, Bloom, Bytes, U64, U128, U256};
use alloy_provider::{DynProvider, Provider, ProviderBuilder};
use alloy_rpc_types_eth::{
    Block, BlockId, BlockNumberOrTag, BlockTransactions, Filter, FilterChanges, FilterId,
    TransactionRequest,
    state::{EvmOverrides, StateOverride},
};
use alloy_sol_types::{SolCall, SolEvent, SolEventInterface};
use eyre::WrapErr;
use futures::StreamExt;
use reth_provider::CanonStateSubscriptions;
use reth_rpc::{EthFilter, eth::filter::EthFilterError};
use reth_rpc_builder::EthHandlers;
use reth_rpc_eth_api::{
    EthApiTypes, EthFilterApiServer, RpcConvert,
    helpers::{EthApiSpec, EthBlocks, EthCall, EthFees, EthState, EthTransactions, FullEthApi},
};
use reth_rpc_eth_types::logs_utils;
use reth_transaction_pool::TransactionPool;
use tempo_alloy::{
    TempoNetwork,
    rpc::{TempoHeaderResponse, TempoTransactionRequest},
};
use tempo_contracts::precompiles::{
    ACCOUNT_KEYCHAIN_ADDRESS,
    account_keychain::IAccountKeychain::{self, KeyInfo, getKeyCall},
};
use tempo_primitives::{TempoHeader, TempoTxEnvelope};
use tokio::{
    sync::Mutex,
    time::{MissedTickBehavior, interval},
};
use zone_precompiles::DARKPOOL_ADDRESS;

use crate::abi::{
    DarkpoolReader, TEMPO_STATE_ADDRESS, ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS,
    ZONE_TOKEN_ADDRESS, ZoneInbox, ZoneOutbox, ZonePortal,
};
use zone_rpc::{
    auth::AuthContext,
    darkpool::{self as zone_darkpool, FillRole, HistoryQuery, Page, TransferQuery},
    types::{
        AuthorizationTokenInfoResponse, BatchListResponse, BatchStatus, BatchSummary, BoxEyreFut,
        BoxFut, DepositKind, DepositState, DepositStatusEntry, DepositStatusResponse,
        HistoryAvailability, JsonRpcError, LIST_BATCHES_DEFAULT_LIMIT, LIST_BATCHES_MAX_LIMIT,
        ListBatchesParams, MarketAction, MarketConfigResponse, MarketEntry, MarketToken,
        MidpointHistoryResponse, OrderLevel, TopOfBookResponse, WithdrawalState,
        WithdrawalStatusQuery, WithdrawalStatusResponse, ZoneInfoResponse, internal, raw_null,
        raw_zero, to_raw,
    },
};

type RpcBlock = Block<alloy_rpc_types_eth::Transaction<TempoTxEnvelope>, TempoHeaderResponse>;
const FILTER_OWNER_PRUNE_INTERVAL: Duration = Duration::from_secs(60);

/// Canonical alpha-launch market constants.
///
/// The darkpool's `bestBid(base)` / `bestAsk(base)` reads take only a base
/// address and implicitly resolve the quote via the base token's
/// `quoteToken()`. To keep response labels and on-chain reads in sync, the
/// alpha private RPC exposes exactly one pair: OALPHA/PATH.USD.
mod alpha {
    use alloy_primitives::{Address, address};

    pub(super) const BASE: Address = address!("0x20C000000000000000000000518dDADD37eD1d28");
    pub(super) const QUOTE: Address = address!("0x20C0000000000000000000000000000000000000");
    pub(super) const BASE_SYMBOL: &str = "OALPHA";
    pub(super) const QUOTE_SYMBOL: &str = "PATH.USD";
    pub(super) const DECIMALS: u8 = 6;
    pub(super) const PAIR_LABEL: &str = "OALPHA/PATH.USD";
}

fn filter_not_found_error() -> JsonRpcError {
    JsonRpcError::invalid_params("filter not found")
}

fn map_eth_filter_error(err: EthFilterError) -> JsonRpcError {
    match err {
        EthFilterError::FilterNotFound(_) => filter_not_found_error(),
        other => internal(other),
    }
}

fn stale_filter_owner_ids(
    owner_ids: impl IntoIterator<Item = FilterId>,
    active_ids: &HashSet<FilterId>,
) -> Vec<FilterId> {
    owner_ids
        .into_iter()
        .filter(|id| !active_ids.contains(id))
        .collect()
}

async fn prune_filter_owners<Api: EthApiTypes + 'static>(
    filter: &EthFilter<Api>,
    owners: &Mutex<HashMap<FilterId, Address>>,
) {
    let owner_ids = {
        let owners = owners.lock().await;
        owners.keys().cloned().collect::<Vec<_>>()
    };
    if owner_ids.is_empty() {
        return;
    }

    let active_ids = filter
        .active_filters()
        .ids()
        .await
        .into_iter()
        .collect::<HashSet<_>>();
    let stale_ids = stale_filter_owner_ids(owner_ids, &active_ids);
    if stale_ids.is_empty() {
        return;
    }

    let mut owners = owners.lock().await;
    for id in stale_ids {
        owners.remove(&id);
    }
}

/// [`ZoneRpcApi`] implementation backed by reth's [`EthHandlers`].
///
/// This is the privacy enforcement layer for the zone's JSON-RPC surface.
/// Only methods explicitly routed through [`ZoneRpcApi`] are reachable —
/// everything else is rejected by the dispatcher's [`classify_method`]
/// whitelist, so this struct effectively acts as an **enforced allowlist**
/// of Ethereum JSON-RPC endpoints.
///
/// For every allowed endpoint it applies typed privacy checks *before*
/// serializing to JSON:
///
/// - **Block redaction** — zeroing `logsBloom` and clearing transaction
///   lists for non-sequencer callers.
/// - **Sender-scoped access** — returning `null` for transactions and
///   receipts not owned by the authenticated caller.
/// - **`from`-enforcement** — `eth_call` / `eth_estimateGas` may only
///   simulate from the authenticated account (`-32004` on mismatch,
///   auto-set when omitted); state overrides are rejected for
///   non-sequencer callers (`-32602`).
/// - **Sender verification** — `eth_sendRawTransaction` checks that the
///   recovered transaction sender matches the authenticated account
///   (`-32003` on mismatch).
///
/// [`classify_method`]: zone_rpc::types::classify_method
pub struct TempoZoneRpc<Api: EthApiTypes> {
    eth: EthHandlers<Api>,
    config: zone_rpc::PrivateRpcConfig,
    l1_provider: DynProvider<TempoNetwork>,
    zone_provider: DynProvider<TempoNetwork>,
    tempo_state:
        crate::abi::TempoState::TempoStateInstance<DynProvider<TempoNetwork>, TempoNetwork>,
    /// Maps filter IDs to the authenticated account that created them.
    /// The reth filter registry remains the source of truth for filter liveness.
    filter_owners: Arc<Mutex<HashMap<FilterId, Address>>>,
}

impl<Api: EthApiTypes + 'static> TempoZoneRpc<Api> {
    /// Wrap reth's [`EthHandlers`] (api + filter + pubsub).
    pub async fn new(
        eth: EthHandlers<Api>,
        config: zone_rpc::PrivateRpcConfig,
    ) -> eyre::Result<Self> {
        let l1_rpc_url = config.l1_rpc_url.clone();
        let zone_rpc_url = config.zone_rpc_url.clone();
        let l1_provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_with_config(
                &l1_rpc_url,
                crate::rpc_connection_config(config.retry_connection_interval),
            )
            .await
            .wrap_err("failed to connect private RPC L1 provider")?
            .erased();
        let zone_provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_with_config(
                &zone_rpc_url,
                crate::rpc_connection_config(config.retry_connection_interval),
            )
            .await
            .wrap_err("failed to connect private RPC zone provider")?
            .erased();
        let tempo_state = crate::abi::TempoState::new(TEMPO_STATE_ADDRESS, zone_provider.clone());
        let rpc = Self {
            eth,
            config,
            l1_provider,
            zone_provider,
            tempo_state,
            filter_owners: Arc::new(Mutex::new(HashMap::new())),
        };
        rpc.spawn_filter_owner_pruner();
        Ok(rpc)
    }

    /// Returns a reference to the inner [`EthFilter`] handler.
    pub fn filter(&self) -> &EthFilter<Api> {
        &self.eth.filter
    }

    async fn filter_is_active(&self, id: &FilterId) -> bool {
        self.filter().active_filters().contains(id).await
    }

    fn spawn_filter_owner_pruner(&self)
    where
        Api: Send + Sync + 'static,
    {
        let filter = self.filter().clone();
        let owners: Weak<Mutex<HashMap<FilterId, Address>>> = Arc::downgrade(&self.filter_owners);
        tokio::spawn(async move {
            let mut prune_interval = interval(FILTER_OWNER_PRUNE_INTERVAL);
            prune_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

            loop {
                prune_interval.tick().await;

                let Some(owners) = owners.upgrade() else {
                    break;
                };

                prune_filter_owners(&filter, &owners).await;
            }
        });
    }

    /// Verify that the filter belongs to the authenticated caller.
    ///
    /// Returns `Ok(())` if the caller owns the filter or is the sequencer.
    /// Returns an error indistinguishable from "filter not found" to avoid
    /// leaking filter existence to non-owners.
    async fn ensure_filter_owner(
        &self,
        id: &FilterId,
        auth: &AuthContext,
    ) -> Result<(), JsonRpcError> {
        let owner_matches = {
            let owners = self.filter_owners.lock().await;
            matches!(owners.get(id), Some(owner) if *owner == auth.caller)
        };
        if !owner_matches {
            return Err(filter_not_found_error());
        }
        if self.filter_is_active(id).await {
            Ok(())
        } else {
            self.filter_owners.lock().await.remove(id);
            Err(filter_not_found_error())
        }
    }

    async fn portal_deposits_for_block(
        &self,
        tempo_block_number: u64,
    ) -> Result<Vec<PortalDepositRecord>, JsonRpcError> {
        if self.config.zone_portal.is_zero() {
            return Err(JsonRpcError::internal("zone portal not configured"));
        }

        let filter = Filter::new()
            .address(self.config.zone_portal)
            .from_block(tempo_block_number)
            .to_block(tempo_block_number)
            .event_signature(vec![
                ZonePortal::DepositMade::SIGNATURE_HASH,
                ZonePortal::EncryptedDepositMade::SIGNATURE_HASH,
            ]);

        let logs = self.l1_provider.get_logs(&filter).await.map_err(internal)?;
        let mut deposits = Vec::with_capacity(logs.len());

        for log in logs {
            match ZonePortal::ZonePortalEvents::decode_log(&log.inner)
                .map_err(internal)?
                .data
            {
                ZonePortal::ZonePortalEvents::DepositMade(event) => {
                    deposits.push(PortalDepositRecord::Regular {
                        deposit_hash: event.newCurrentDepositQueueHash,
                        sender: event.sender,
                        recipient: event.to,
                        token: event.token,
                        amount: event.netAmount,
                        memo: event.memo,
                    });
                }
                ZonePortal::ZonePortalEvents::EncryptedDepositMade(event) => {
                    deposits.push(PortalDepositRecord::Encrypted {
                        deposit_hash: event.newCurrentDepositQueueHash,
                        sender: event.sender,
                        token: event.token,
                        amount: event.netAmount,
                    });
                }
                _ => {}
            }
        }

        Ok(deposits)
    }

    async fn zone_tokens(&self) -> Result<Vec<Address>, JsonRpcError> {
        if self.config.zone_portal.is_zero() {
            return Ok(vec![ZONE_TOKEN_ADDRESS]);
        }

        ZonePortal::new(self.config.zone_portal, &self.l1_provider)
            .enabled_tokens()
            .await
            .map_err(internal)
    }

    /// Read the portal's `withdrawalBatchIndex`, the highest batch number
    /// currently observable on L1.
    async fn latest_batch_number(&self) -> Result<u64, JsonRpcError> {
        if self.config.zone_portal.is_zero() {
            return Err(JsonRpcError::internal("zone portal not configured"));
        }
        ZonePortal::new(self.config.zone_portal, self.l1_provider.clone())
            .withdrawalBatchIndex()
            .call()
            .await
            .map_err(internal)
    }

    /// Fetch a single `BatchSubmitted` log by indexed `withdrawalBatchIndex`.
    async fn fetch_batch_log(
        &self,
        batch_number: u64,
    ) -> Result<Option<alloy_rpc_types_eth::Log>, JsonRpcError> {
        if self.config.zone_portal.is_zero() {
            return Err(JsonRpcError::internal("zone portal not configured"));
        }
        let filter = Filter::new()
            .address(self.config.zone_portal)
            .event_signature(ZonePortal::BatchSubmitted::SIGNATURE_HASH)
            .topic1(batch_number_topic(batch_number))
            .from_block(0);
        let logs = self.l1_provider.get_logs(&filter).await.map_err(internal)?;
        Ok(logs.into_iter().next())
    }

    /// Fetch `BatchSubmitted` logs for inclusive batch range `[start, end]`.
    async fn fetch_batch_logs_in_range(
        &self,
        start: u64,
        end: u64,
    ) -> Result<Vec<alloy_rpc_types_eth::Log>, JsonRpcError> {
        if self.config.zone_portal.is_zero() {
            return Err(JsonRpcError::internal("zone portal not configured"));
        }
        if end < start {
            return Ok(Vec::new());
        }
        let topics: Vec<B256> = (start..=end).map(batch_number_topic).collect();
        let filter = Filter::new()
            .address(self.config.zone_portal)
            .event_signature(ZonePortal::BatchSubmitted::SIGNATURE_HASH)
            .topic1(topics)
            .from_block(0);
        let mut logs = self.l1_provider.get_logs(&filter).await.map_err(internal)?;
        logs.sort_by_key(|log| log_batch_index(log).unwrap_or(0));
        Ok(logs)
    }

    /// Resolve a `BatchSubmitted` log into the public, aggregate-only summary
    /// returned by the explorer methods.
    async fn build_batch_summary(
        &self,
        log: alloy_rpc_types_eth::Log,
    ) -> Result<BatchSummary, JsonRpcError> {
        let event = ZonePortal::BatchSubmitted::decode_log(&log.inner)
            .map_err(internal)?
            .data;
        let settlement_tx_hash = log
            .transaction_hash
            .ok_or_else(|| JsonRpcError::internal("BatchSubmitted log missing transaction hash"))?;
        let l1_block_number = log
            .block_number
            .ok_or_else(|| JsonRpcError::internal("BatchSubmitted log missing block number"))?;

        let (settled_at, tx, zone_block_to) = tokio::try_join!(
            async {
                self.l1_provider
                    .get_block_by_number(l1_block_number.into())
                    .await
                    .map(|opt| opt.as_ref().map(|b| b.header.timestamp()))
                    .map_err(internal)
            },
            async {
                self.l1_provider
                    .get_transaction_by_hash(settlement_tx_hash)
                    .await
                    .map_err(internal)?
                    .ok_or_else(|| {
                        JsonRpcError::internal("BatchSubmitted settlement tx not found on L1")
                    })
            },
            async {
                self.zone_provider
                    .get_block_by_hash(event.nextBlockHash)
                    .await
                    .map_err(internal)
            },
        )?;

        let call = ZonePortal::submitBatchCall::abi_decode(tx.input().as_ref()).map_err(|err| {
            JsonRpcError::internal(format!("failed to decode submitBatch calldata: {err}"))
        })?;

        let (zone_block_to_number, sealed_at) = match zone_block_to {
            Some(block) => (Some(block.number()), Some(block.header.timestamp())),
            None => (None, None),
        };

        let zone_block_from_number = if call.blockTransition.prevBlockHash.is_zero() {
            Some(0u64)
        } else {
            self.zone_provider
                .get_block_by_hash(call.blockTransition.prevBlockHash)
                .await
                .map_err(internal)?
                .map(|block| block.number())
        };

        Ok(map_batch_summary(
            &event,
            &call,
            settlement_tx_hash,
            settled_at,
            zone_block_from_number,
            zone_block_to_number,
            sealed_at,
        ))
    }

    /// Find the `WithdrawalRequested` event matching `query`, scoped to the
    /// authenticated caller. Returns `Ok(None)` when the withdrawal does not
    /// exist or is not owned by the caller — callers must treat both cases
    /// identically to avoid leaking existence to non-owners.
    async fn find_withdrawal_requested(
        &self,
        query: WithdrawalStatusQuery,
        caller: Address,
    ) -> Result<Option<WithdrawalRequestedRecord>, JsonRpcError> {
        let logs = match query {
            WithdrawalStatusQuery::TxHash(tx_hash) => {
                let receipt = self
                    .zone_provider
                    .get_transaction_receipt(tx_hash)
                    .await
                    .map_err(internal)?;
                let Some(receipt) = receipt else {
                    return Ok(None);
                };
                receipt
                    .inner
                    .inner
                    .logs()
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
            }
            WithdrawalStatusQuery::WithdrawalIndex(idx) => {
                let filter = Filter::new()
                    .address(ZONE_OUTBOX_ADDRESS)
                    .from_block(0)
                    .event_signature(ZoneOutbox::WithdrawalRequested::SIGNATURE_HASH)
                    .topic1(B256::from(U256::from(idx)));
                self.zone_provider
                    .get_logs(&filter)
                    .await
                    .map_err(internal)?
            }
        };

        for log in logs {
            if log.address() != ZONE_OUTBOX_ADDRESS {
                continue;
            }
            if log.topics().first().copied()
                != Some(ZoneOutbox::WithdrawalRequested::SIGNATURE_HASH)
            {
                continue;
            }
            let event = ZoneOutbox::WithdrawalRequested::decode_log(&log.inner)
                .map_err(internal)?
                .data;
            if event.sender != caller {
                continue;
            }
            let Some(zone_tx_hash) = log.transaction_hash else {
                continue;
            };
            let Some(zone_block_number) = log.block_number else {
                continue;
            };
            return Ok(Some(WithdrawalRequestedRecord {
                withdrawal_index: event.withdrawalIndex,
                token: event.token,
                to: event.to,
                amount: event.amount,
                fee: event.fee,
                memo: event.memo,
                gas_limit: event.gasLimit,
                fallback_recipient: event.fallbackRecipient,
                callback_data: event.data.clone(),
                zone_tx_hash,
                zone_block_number,
            }));
        }
        Ok(None)
    }

    /// Find the L2 `BatchFinalized` event sealing the zone block that contains
    /// the withdrawal. Returns `None` if the batch has not yet been sealed
    /// (still pending) or if the block sealed no withdrawals.
    async fn find_batch_finalized_for_block(
        &self,
        zone_block_number: u64,
    ) -> Result<Option<BatchFinalizedRecord>, JsonRpcError> {
        let filter = Filter::new()
            .address(ZONE_OUTBOX_ADDRESS)
            .from_block(zone_block_number)
            .to_block(zone_block_number)
            .event_signature(ZoneOutbox::BatchFinalized::SIGNATURE_HASH);
        let logs = self
            .zone_provider
            .get_logs(&filter)
            .await
            .map_err(internal)?;

        for log in logs {
            let event = ZoneOutbox::BatchFinalized::decode_log(&log.inner)
                .map_err(internal)?
                .data;
            if event.withdrawalQueueHash.is_zero() {
                continue;
            }
            return Ok(Some(BatchFinalizedRecord {
                withdrawal_batch_index: event.withdrawalBatchIndex,
                withdrawal_queue_hash: event.withdrawalQueueHash,
            }));
        }
        Ok(None)
    }

    /// Find the L1 `BatchSubmitted` event matching the zone-side batch index.
    ///
    /// The L1 portal increments its own `withdrawalBatchIndex` once per
    /// `submitBatch` call, so it tracks the L2 outbox batch index 1:1. Filter
    /// by indexed topic1 to skip an unbounded scan.
    async fn find_l1_batch_submitted(
        &self,
        withdrawal_batch_index: u64,
        expected_queue_hash: B256,
    ) -> Result<Option<BatchSubmittedRecord>, JsonRpcError> {
        if self.config.zone_portal.is_zero() {
            return Ok(None);
        }

        let filter = Filter::new()
            .address(self.config.zone_portal)
            .from_block(0)
            .event_signature(ZonePortal::BatchSubmitted::SIGNATURE_HASH)
            .topic1(B256::from(U256::from(withdrawal_batch_index)));
        let logs = self.l1_provider.get_logs(&filter).await.map_err(internal)?;

        for log in logs {
            let event = ZonePortal::BatchSubmitted::decode_log(&log.inner)
                .map_err(internal)?
                .data;
            if event.withdrawalQueueHash != expected_queue_hash {
                continue;
            }
            let Some(l1_tx_hash) = log.transaction_hash else {
                continue;
            };
            let Some(l1_block_number) = log.block_number else {
                continue;
            };
            return Ok(Some(BatchSubmittedRecord {
                l1_tx_hash,
                l1_block_number,
            }));
        }
        Ok(None)
    }

    /// Scan L1 from `from_block` for the `WithdrawalProcessed` event that
    /// settles this withdrawal.
    ///
    /// Matching by `(to, token, amount)` alone is ambiguous when the caller
    /// has multiple identical-shape withdrawals to the same recipient. To
    /// disambiguate, every candidate's L1 transaction is fetched and the
    /// `processWithdrawal(withdrawal, ...)` calldata is decoded; the inner
    /// `Withdrawal` struct is then compared field-by-field — including the
    /// authenticated `senderTag = keccak256(sender || zoneTxHash)` — against
    /// the originating zone request.
    ///
    /// Returns:
    /// - [`TerminalLookup::NotFound`]    — no candidate's calldata matches.
    /// - [`TerminalLookup::Single`]      — exactly one candidate matches; for
    ///   `callbackSuccess == false` an in-tx `BounceBack` probe disambiguates
    ///   `bounced` vs. `failed`.
    /// - [`TerminalLookup::Ambiguous`]   — more than one candidate matches.
    ///   The caller must keep the public status at `submitted` rather than
    ///   guess which terminal tx is correct.
    async fn find_l1_withdrawal_terminal(
        &self,
        withdrawal: &WithdrawalRequestedRecord,
        expected_sender_tag: B256,
        from_block: u64,
    ) -> Result<TerminalLookup, JsonRpcError> {
        if self.config.zone_portal.is_zero() {
            return Ok(TerminalLookup::NotFound);
        }

        let filter = Filter::new()
            .address(self.config.zone_portal)
            .from_block(from_block)
            .event_signature(ZonePortal::WithdrawalProcessed::SIGNATURE_HASH)
            .topic1(B256::left_padding_from(withdrawal.to.as_slice()));
        let logs = self.l1_provider.get_logs(&filter).await.map_err(internal)?;

        let mut candidates: Vec<CandidateTerminal> = Vec::new();
        for log in logs {
            let event = ZonePortal::WithdrawalProcessed::decode_log(&log.inner)
                .map_err(internal)?
                .data;
            if event.token != withdrawal.token || event.amount != withdrawal.amount {
                continue;
            }
            let Some(l1_tx_hash) = log.transaction_hash else {
                continue;
            };

            // Decode the full `processWithdrawal` payload from the L1 tx's
            // calldata so we can compare every field against the originating
            // zone request, not just the `(to, token, amount)` triple the
            // event exposes.
            let Some(tx) = self
                .l1_provider
                .get_transaction_by_hash(l1_tx_hash)
                .await
                .map_err(internal)?
            else {
                continue;
            };
            let call = match ZonePortal::processWithdrawalCall::abi_decode(tx.input().as_ref()) {
                Ok(call) => call,
                Err(_) => continue,
            };
            if !withdrawal_matches(&call.withdrawal, withdrawal, expected_sender_tag) {
                continue;
            }
            candidates.push(CandidateTerminal {
                l1_tx_hash,
                callback_success: event.callbackSuccess,
            });
        }

        match classify_terminal_candidates(candidates) {
            TerminalCandidateOutcome::NotFound => Ok(TerminalLookup::NotFound),
            TerminalCandidateOutcome::Ambiguous => Ok(TerminalLookup::Ambiguous),
            TerminalCandidateOutcome::Single(candidate) => {
                let bounced = if candidate.callback_success {
                    false
                } else {
                    self.bounce_back_in_tx(candidate.l1_tx_hash, withdrawal)
                        .await?
                };
                Ok(TerminalLookup::Single(TerminalWithdrawalEvent {
                    l1_tx_hash: candidate.l1_tx_hash,
                    callback_success: candidate.callback_success,
                    bounced,
                }))
            }
        }
    }

    /// Returns `true` if the given L1 transaction contains a `BounceBack`
    /// event for `withdrawal.fallback_recipient` with the same `(token, amount)`.
    async fn bounce_back_in_tx(
        &self,
        l1_tx_hash: B256,
        withdrawal: &WithdrawalRequestedRecord,
    ) -> Result<bool, JsonRpcError> {
        let receipt = self
            .l1_provider
            .get_transaction_receipt(l1_tx_hash)
            .await
            .map_err(internal)?;
        let Some(receipt) = receipt else {
            return Ok(false);
        };
        let logs = receipt.inner.inner.logs();
        for log in logs {
            if log.address() != self.config.zone_portal {
                continue;
            }
            if log.topics().first().copied() != Some(ZonePortal::BounceBack::SIGNATURE_HASH) {
                continue;
            }
            let event = ZonePortal::BounceBack::decode_log(&log.inner)
                .map_err(internal)?
                .data;
            if event.fallbackRecipient == withdrawal.fallback_recipient
                && event.token == withdrawal.token
                && event.amount == withdrawal.amount
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn terminal_event_for_deposit(
        &self,
        deposit_hash: B256,
    ) -> Result<Option<TerminalDepositEvent>, JsonRpcError> {
        let filter = Filter::new()
            .address(ZONE_INBOX_ADDRESS)
            .from_block(0)
            .event_signature(vec![
                ZoneInbox::DepositProcessed::SIGNATURE_HASH,
                ZoneInbox::EncryptedDepositProcessed::SIGNATURE_HASH,
                ZoneInbox::EncryptedDepositFailed::SIGNATURE_HASH,
            ])
            .topic1(deposit_hash);

        let logs = self
            .zone_provider
            .get_logs(&filter)
            .await
            .map_err(internal)?;
        let Some(log) = logs.last() else {
            return Ok(None);
        };

        let Some(signature) = log.topics().first().copied() else {
            return Ok(None);
        };

        if signature == ZoneInbox::DepositProcessed::SIGNATURE_HASH {
            ZoneInbox::DepositProcessed::decode_log(&log.inner).map_err(internal)?;
            return Ok(Some(TerminalDepositEvent::RegularProcessed));
        }

        if signature == ZoneInbox::EncryptedDepositProcessed::SIGNATURE_HASH {
            let event =
                ZoneInbox::EncryptedDepositProcessed::decode_log(&log.inner).map_err(internal)?;
            return Ok(Some(TerminalDepositEvent::EncryptedProcessed {
                recipient: event.to,
                memo: event.memo,
            }));
        }

        if signature == ZoneInbox::EncryptedDepositFailed::SIGNATURE_HASH {
            ZoneInbox::EncryptedDepositFailed::decode_log(&log.inner).map_err(internal)?;
            return Ok(Some(TerminalDepositEvent::EncryptedFailed));
        }

        Ok(None)
    }
}

impl<Api> zone_rpc::ZoneRpcApi for TempoZoneRpc<Api>
where
    Api: FullEthApi + EthApiTypes<NetworkTypes = TempoNetwork> + Send + Sync + 'static,
{
    fn get_keychain_key(&self, account: Address, key_id: Address) -> BoxEyreFut<'_, KeyInfo> {
        Box::pin(async move {
            let request = TempoTransactionRequest {
                inner: TransactionRequest {
                    to: Some(ACCOUNT_KEYCHAIN_ADDRESS.into()),
                    input: getKeyCall {
                        account,
                        keyId: key_id,
                    }
                    .abi_encode()
                    .into(),
                    ..Default::default()
                },
                ..Default::default()
            };

            let output = EthCall::call(&self.eth.api, request, None, EvmOverrides::default())
                .await
                .wrap_err("AccountKeychain.getKey eth_call failed")?;

            IAccountKeychain::getKeyCall::abi_decode_returns(output.as_ref()).map_err(Into::into)
        })
    }

    fn block_number(&self) -> BoxFut<'_> {
        Box::pin(async move {
            let info = EthApiSpec::chain_info(&self.eth.api).map_err(internal)?;
            to_raw(&U256::from(info.best_number))
        })
    }

    fn chain_id(&self) -> BoxFut<'_> {
        Box::pin(async move {
            let chain_id = EthApiSpec::chain_id(&self.eth.api);
            to_raw(&Some(chain_id))
        })
    }

    fn net_version(&self) -> BoxFut<'_> {
        Box::pin(async move {
            let chain_id = EthApiSpec::chain_id(&self.eth.api);
            to_raw(&chain_id.to_string())
        })
    }

    fn gas_price(&self) -> BoxFut<'_> {
        Box::pin(async move {
            let price = EthFees::gas_price(&self.eth.api).await.map_err(internal)?;
            to_raw(&price)
        })
    }

    fn max_priority_fee_per_gas(&self) -> BoxFut<'_> {
        Box::pin(async move {
            let fee = EthFees::suggested_priority_fee(&self.eth.api)
                .await
                .map_err(internal)?;
            to_raw(&fee)
        })
    }

    fn fee_history(
        &self,
        block_count: u64,
        newest_block: BlockNumberOrTag,
        reward_percentiles: Option<Vec<f64>>,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            let history =
                EthFees::fee_history(&self.eth.api, block_count, newest_block, reward_percentiles)
                    .await
                    .map_err(internal)?;
            to_raw(&history)
        })
    }

    fn get_balance(
        &self,
        address: Address,
        block: Option<BlockId>,
        auth: AuthContext,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            // Silent dummy: non-caller addresses get "0x0" to avoid leaking account existence.
            if address != auth.caller {
                return Ok(raw_zero());
            }
            let balance = EthState::balance(&self.eth.api, address, block)
                .await
                .map_err(internal)?;
            to_raw(&balance)
        })
    }

    fn get_transaction_count(
        &self,
        address: Address,
        block: Option<BlockId>,
        auth: AuthContext,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            // Silent dummy: non-caller addresses get "0x0" to avoid leaking account existence.
            if address != auth.caller {
                return Ok(raw_zero());
            }
            let count = EthState::transaction_count(&self.eth.api, address, block)
                .await
                .map_err(internal)?;
            to_raw(&count)
        })
    }

    fn block_by_number(
        &self,
        number: BlockNumberOrTag,
        full: bool,
        _auth: AuthContext,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            let block = EthBlocks::rpc_block(&self.eth.api, number.into(), full)
                .await
                .map_err(internal)?;

            let Some(mut block) = block else {
                return Ok(raw_null());
            };

            redact_block(&mut block);

            to_raw(&block)
        })
    }

    fn block_by_hash(&self, hash: B256, full: bool, _auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            let block = EthBlocks::rpc_block(&self.eth.api, hash.into(), full)
                .await
                .map_err(internal)?;

            let Some(mut block) = block else {
                return Ok(raw_null());
            };

            redact_block(&mut block);

            to_raw(&block)
        })
    }

    fn transaction_by_hash(&self, hash: B256, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            let tx = EthTransactions::transaction_by_hash(&self.eth.api, hash)
                .await
                .map_err(internal)?
                .map(|src| src.into_transaction(self.eth.api.converter()))
                .transpose()
                .map_err(internal)?;

            let Some(tx) = tx else { return Ok(raw_null()) };

            if tx.from() != auth.caller {
                return Ok(raw_null());
            }

            to_raw(&tx)
        })
    }

    fn transaction_receipt(&self, hash: B256, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            let receipt = EthTransactions::transaction_receipt(&self.eth.api, hash)
                .await
                .map_err(internal)?;

            let Some(mut receipt) = receipt else {
                return Ok(raw_null());
            };

            if receipt.from() != auth.caller {
                return Ok(raw_null());
            }

            receipt = zone_rpc::filter::filter_receipt_logs(receipt);

            to_raw(&receipt)
        })
    }

    fn call(
        &self,
        mut request: TempoTransactionRequest,
        block: Option<BlockId>,
        state_override: Option<StateOverride>,
        auth: AuthContext,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            if state_override.is_some() {
                return Err(JsonRpcError::invalid_params("state overrides not allowed"));
            }

            zone_rpc::policy::enforce_from(&mut request, &auth)?;
            zone_rpc::policy::enforce_no_contract_creation(&request)?;

            let result = EthCall::call(
                &self.eth.api,
                request,
                block,
                EvmOverrides::state(state_override),
            )
            .await
            .map_err(internal)?;
            to_raw(&result)
        })
    }

    fn estimate_gas(
        &self,
        mut request: TempoTransactionRequest,
        block: Option<BlockId>,
        state_override: Option<StateOverride>,
        auth: AuthContext,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            if state_override.is_some() {
                return Err(JsonRpcError::invalid_params("state overrides not allowed"));
            }

            zone_rpc::policy::enforce_from(&mut request, &auth)?;

            zone_rpc::policy::enforce_no_contract_creation(&request)?;

            let result = EthCall::estimate_gas_at(
                &self.eth.api,
                request,
                block.unwrap_or_default(),
                state_override,
            )
            .await
            .map_err(internal)?;
            to_raw(&result)
        })
    }

    fn send_raw_transaction(&self, data: Bytes, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            zone_rpc::policy::verify_raw_tx_sender(&data, &auth)?;

            let hash = EthTransactions::send_raw_transaction(&self.eth.api, data)
                .await
                .map_err(internal)?;
            to_raw(&hash)
        })
    }

    fn send_raw_transaction_sync(&self, data: Bytes, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            zone_rpc::policy::verify_raw_tx_sender(&data, &auth)?;

            let mut receipt = EthTransactions::send_raw_transaction_sync(&self.eth.api, data)
                .await
                .map_err(internal)?;

            receipt = zone_rpc::filter::filter_receipt_logs(receipt);

            to_raw(&receipt)
        })
    }

    fn fill_transaction(
        &self,
        mut request: TempoTransactionRequest,
        auth: AuthContext,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            zone_rpc::policy::enforce_from(&mut request, &auth)?;
            zone_rpc::policy::enforce_no_contract_creation(&request)?;

            let result = EthTransactions::fill_transaction(&self.eth.api, request)
                .await
                .map_err(internal)?;
            to_raw(&result)
        })
    }

    fn get_logs(&self, mut filter: Filter, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            zone_rpc::filter::scope_filter(&mut filter);
            let logs = EthFilterApiServer::logs(&self.eth.filter, filter)
                .await
                .map_err(internal)?;
            let filtered = zone_rpc::filter::filter_logs(logs, &auth.caller);
            to_raw(&filtered)
        })
    }

    fn new_filter(&self, mut filter: Filter, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            zone_rpc::filter::scope_filter(&mut filter);
            let id = EthFilterApiServer::new_filter(&self.eth.filter, filter)
                .await
                .map_err(internal)?;
            self.filter_owners
                .lock()
                .await
                .insert(id.clone(), auth.caller);
            to_raw(&id)
        })
    }

    fn get_filter_logs(&self, id: FilterId, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            self.ensure_filter_owner(&id, &auth).await?;

            let logs = self
                .filter()
                .filter_logs(id)
                .await
                .map_err(map_eth_filter_error)?;

            let filtered = zone_rpc::filter::filter_logs(logs, &auth.caller);
            to_raw(&filtered)
        })
    }

    fn get_filter_changes(&self, id: FilterId, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            self.ensure_filter_owner(&id, &auth).await?;

            let changes = self
                .filter()
                .filter_changes(id)
                .await
                .map_err(map_eth_filter_error)?;

            match changes {
                FilterChanges::Logs(logs) => {
                    let filtered = zone_rpc::filter::filter_logs(logs, &auth.caller);
                    to_raw(&FilterChanges::<
                        alloy_rpc_types_eth::Transaction<TempoTxEnvelope>,
                    >::Logs(filtered))
                }
                FilterChanges::Hashes(hashes) => to_raw(&FilterChanges::<
                    alloy_rpc_types_eth::Transaction<TempoTxEnvelope>,
                >::Hashes(hashes)),
                // Pending transaction filters are disabled — return empty if one somehow exists
                FilterChanges::Transactions(_) => to_raw(
                    &FilterChanges::<alloy_rpc_types_eth::Transaction<TempoTxEnvelope>>::Empty,
                ),
                FilterChanges::Empty => to_raw(
                    &FilterChanges::<alloy_rpc_types_eth::Transaction<TempoTxEnvelope>>::Empty,
                ),
            }
        })
    }

    fn new_block_filter(&self, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            let id = EthFilterApiServer::new_block_filter(&self.eth.filter)
                .await
                .map_err(internal)?;
            self.filter_owners
                .lock()
                .await
                .insert(id.clone(), auth.caller);
            to_raw(&id)
        })
    }

    fn uninstall_filter(&self, id: FilterId, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            self.ensure_filter_owner(&id, &auth).await?;

            let result = EthFilterApiServer::uninstall_filter(&self.eth.filter, id.clone())
                .await
                .map_err(internal)?;

            if result || !self.filter_is_active(&id).await {
                self.filter_owners.lock().await.remove(&id);
            }

            to_raw(&result)
        })
    }

    fn ws_subscribe_new_heads(&self, _auth: AuthContext) -> BoxWsSubscriptionFut<'_> {
        Box::pin(async move {
            let api = self.eth.api.clone();
            let provider = self.eth.api.provider().clone();
            let stream = provider
                .canonical_state_stream()
                .flat_map(move |new_chain| {
                    let api = api.clone();
                    let headers = new_chain
                        .committed()
                        .blocks_iter()
                        .filter_map(move |block| {
                            match api
                                .converter()
                                .convert_header(block.clone_sealed_header(), block.rlp_length())
                            {
                                Ok(header) => Some(header),
                                Err(err) => {
                                    tracing::error!(
                                        target: "rpc",
                                        %err,
                                        "Failed to convert header"
                                    );
                                    None
                                }
                            }
                        })
                        .collect::<Vec<_>>();
                    futures::stream::iter(headers)
                })
                .map(move |mut header| {
                    redact_ws_header(&mut header);
                    to_raw(&header)
                });
            let stream: zone_rpc::WsSubscriptionStream = Box::pin(stream);
            Ok(stream)
        })
    }

    fn ws_subscribe_logs(&self, mut filter: Filter, auth: AuthContext) -> BoxWsSubscriptionFut<'_> {
        Box::pin(async move {
            let provider = self.eth.api.provider().clone();
            let caller = auth.caller;

            zone_rpc::filter::scope_filter(&mut filter);

            let stream = provider
                .canonical_state_stream()
                .flat_map(|canon_state| futures::stream::iter(canon_state.block_receipts()))
                .flat_map(move |(block_receipts, removed)| {
                    let all_logs = logs_utils::matching_block_logs_with_tx_hashes(
                        &filter,
                        block_receipts.block,
                        block_receipts.timestamp,
                        block_receipts
                            .tx_receipts
                            .iter()
                            .map(|(tx, receipt)| (*tx, receipt)),
                        removed,
                    );
                    futures::stream::iter(all_logs)
                });

            let stream = stream.filter_map(move |log| {
                std::future::ready(
                    zone_rpc::filter::is_log_visible(&log, &caller).then(|| to_raw(&log)),
                )
            });
            let stream: zone_rpc::WsSubscriptionStream = Box::pin(stream);
            Ok(stream)
        })
    }

    fn ws_subscribe_pending_transactions(
        &self,
        full: bool,
        auth: AuthContext,
    ) -> BoxWsSubscriptionFut<'_> {
        Box::pin(async move {
            let caller = auth.caller;
            let pool = self.eth.api.pool().clone();

            if !full {
                let stream =
                    pool.new_pending_pool_transactions_listener()
                        .filter_map(move |pending_tx| {
                            std::future::ready(
                                (pending_tx.transaction.sender() == caller)
                                    .then(|| to_raw(pending_tx.transaction.hash())),
                            )
                        });
                let stream: zone_rpc::WsSubscriptionStream = Box::pin(stream);
                return Ok(stream);
            }

            let api = self.eth.api.clone();
            let stream =
                pool.new_pending_pool_transactions_listener()
                    .filter_map(move |pending_tx| {
                        std::future::ready((pending_tx.transaction.sender() == caller).then(|| {
                            api.converter()
                                .fill_pending(pending_tx.transaction.to_consensus())
                                .map_err(internal)
                                .and_then(|tx| to_raw(&tx))
                        }))
                    });
            let stream: zone_rpc::WsSubscriptionStream = Box::pin(stream);
            Ok(stream)
        })
    }

    fn zone_get_authorization_token_info(&self, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            to_raw(&AuthorizationTokenInfoResponse {
                account: auth.caller,
                expires_at: U64::from(auth.expires_at),
            })
        })
    }

    fn zone_get_zone_info(&self, _auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            let zone_tokens = self.zone_tokens().await?;
            to_raw(&ZoneInfoResponse {
                zone_id: U64::from(self.config.zone_id),
                zone_tokens,
                chain_id: U64::from(self.config.chain_id),
            })
        })
    }

    fn zone_list_batches(&self, params: ListBatchesParams, _auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            let limit = params
                .limit
                .unwrap_or(LIST_BATCHES_DEFAULT_LIMIT)
                .min(LIST_BATCHES_MAX_LIMIT)
                .max(1);

            let latest = self.latest_batch_number().await?;
            if latest == 0 {
                return to_raw(&BatchListResponse {
                    batches: Vec::new(),
                    next_cursor: None,
                });
            }

            let end = match params.cursor {
                Some(cursor) => {
                    let cursor: u64 = cursor.to();
                    if cursor == 0 {
                        return to_raw(&BatchListResponse {
                            batches: Vec::new(),
                            next_cursor: None,
                        });
                    }
                    cursor.saturating_sub(1).min(latest)
                }
                None => latest,
            };

            let start = end.saturating_sub((limit as u64).saturating_sub(1)).max(1);
            let logs = self.fetch_batch_logs_in_range(start, end).await?;

            let futures = logs
                .into_iter()
                .map(|log| self.build_batch_summary(log))
                .collect::<Vec<_>>();
            let mut batches = futures::future::try_join_all(futures).await?;
            batches.sort_by(|a, b| b.batch_number.cmp(&a.batch_number));

            let next_cursor = if start > 1 {
                Some(U64::from(start))
            } else {
                None
            };

            to_raw(&BatchListResponse {
                batches,
                next_cursor,
            })
        })
    }

    fn zone_get_batch(&self, batch_number: u64, _auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            if batch_number == 0 {
                return Ok(raw_null());
            }
            let log = match self.fetch_batch_log(batch_number).await? {
                Some(log) => log,
                None => return Ok(raw_null()),
            };
            let summary = self.build_batch_summary(log).await?;
            to_raw(&summary)
        })
    }

    fn zone_search_batch(&self, query: String, _auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            let trimmed = query.trim();
            if trimmed.is_empty() {
                return Err(JsonRpcError::invalid_params("query must not be empty"));
            }

            match classify_batch_query(trimmed) {
                BatchQuery::BatchNumber(0) => Ok(raw_null()),
                BatchQuery::BatchNumber(batch_number) => {
                    let log = match self.fetch_batch_log(batch_number).await? {
                        Some(log) => log,
                        None => return Ok(raw_null()),
                    };
                    let summary = self.build_batch_summary(log).await?;
                    to_raw(&summary)
                }
                BatchQuery::SettlementTxHash(tx_hash) => {
                    let receipt = match self
                        .l1_provider
                        .get_transaction_receipt(tx_hash)
                        .await
                        .map_err(internal)?
                    {
                        Some(receipt) => receipt,
                        None => return Ok(raw_null()),
                    };
                    let portal_address = self.config.zone_portal;
                    let event_topic = ZonePortal::BatchSubmitted::SIGNATURE_HASH;
                    let log = receipt
                        .inner
                        .logs()
                        .iter()
                        .find(|log| {
                            log.address() == portal_address
                                && log.topics().first() == Some(&event_topic)
                        })
                        .cloned();
                    let Some(log) = log else {
                        return Ok(raw_null());
                    };
                    let summary = self.build_batch_summary(log).await?;
                    to_raw(&summary)
                }
                BatchQuery::Invalid => Err(JsonRpcError::invalid_params(
                    "query must be a batch number (decimal or hex) or an L1 settlement tx hash",
                )),
            }
        })
    }

    fn zone_get_market_config(&self, _auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move { to_raw(&canonical_alpha_market_config()) })
    }

    fn zone_get_top_of_book(
        &self,
        base: Address,
        quote: Address,
        _auth: AuthContext,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            ensure_canonical_pair(base, quote)?;

            let darkpool = DarkpoolReader::new(DARKPOOL_ADDRESS, &self.zone_provider);
            let best_bid = darkpool.bestBid(base).call().await.map_err(internal)?;
            let best_ask = darkpool.bestAsk(base).call().await.map_err(internal)?;
            let as_of_block = self
                .zone_provider
                .get_block_number()
                .await
                .map_err(internal)?;

            let bid = level_from_response(best_bid.price, best_bid.quantity);
            let ask = level_from_response(best_ask.price, best_ask.quantity);
            let (midpoint, spread) = match (&bid, &ask) {
                (Some(b), Some(a)) => (
                    Some(U128::from(
                        (b.price.saturating_add(a.price)) / U128::from(2),
                    )),
                    Some(U128::from(a.price.saturating_sub(b.price))),
                ),
                _ => (None, None),
            };

            to_raw(&TopOfBookResponse {
                pair: alpha::PAIR_LABEL.to_string(),
                base,
                quote,
                bid,
                ask,
                midpoint,
                spread,
                as_of_block: U64::from(as_of_block),
            })
        })
    }

    fn zone_get_midpoint_history(
        &self,
        base: Address,
        quote: Address,
        interval: String,
        _limit: u32,
        _cursor: Option<String>,
        _auth: AuthContext,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            ensure_canonical_pair(base, quote)?;

            to_raw(&MidpointHistoryResponse {
                pair: alpha::PAIR_LABEL.to_string(),
                base,
                quote,
                interval,
                samples: Vec::new(),
                next_cursor: None,
                history: HistoryAvailability {
                    enabled: false,
                    reason: "midpoint history aggregation is not yet enabled for alpha; frontends should keep the chart disabled and use zone_getTopOfBook for live aggregate values"
                        .to_string(),
                },
            })
        })
    }

    fn zone_get_withdrawal_status(
        &self,
        query: WithdrawalStatusQuery,
        auth: AuthContext,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            // Step 1: resolve the WithdrawalRequested event scoped to the caller.
            //
            // The function returns `Ok(None)` both when the withdrawal does not
            // exist *and* when it exists but belongs to a different account, so
            // a non-owner cannot distinguish the two cases via this RPC.
            let Some(withdrawal) = self.find_withdrawal_requested(query, auth.caller).await? else {
                return Ok(raw_null());
            };

            let mut response = WithdrawalStatusResponse {
                withdrawal_index: U64::from(withdrawal.withdrawal_index),
                zone_tx_hash: withdrawal.zone_tx_hash,
                status: WithdrawalState::Pending,
                token: withdrawal.token,
                amount: U256::from(withdrawal.amount),
                to: withdrawal.to,
                fallback_recipient: withdrawal.fallback_recipient,
                memo: withdrawal.memo,
                zone_block_number: U64::from(withdrawal.zone_block_number),
                withdrawal_batch_index: None,
                portal_slot: None,
                l1_submit_batch_tx_hash: None,
                l1_process_withdrawal_tx_hash: None,
                callback_success: None,
                error: None,
            };

            // Step 2: check whether the zone outbox sealed the batch.
            let Some(batch_finalized) = self
                .find_batch_finalized_for_block(withdrawal.zone_block_number)
                .await?
            else {
                return to_raw(&response);
            };
            response.withdrawal_batch_index =
                Some(U64::from(batch_finalized.withdrawal_batch_index));
            response.status = WithdrawalState::Batched;

            // Step 3: check whether the batch landed on L1.
            let Some(batch_submitted) = self
                .find_l1_batch_submitted(
                    batch_finalized.withdrawal_batch_index,
                    batch_finalized.withdrawal_queue_hash,
                )
                .await?
            else {
                return to_raw(&response);
            };
            response.l1_submit_batch_tx_hash = Some(batch_submitted.l1_tx_hash);
            response.status = WithdrawalState::Submitted;

            // Step 4: check whether the L1 portal processed this withdrawal.
            //
            // `expected_sender_tag` binds the L1-side `Withdrawal.senderTag`
            // back to `(auth.caller, zone_tx_hash)`. Combined with the rest of
            // the calldata comparison this rules out look-alike withdrawals
            // belonging to the same caller in adjacent batches.
            let expected_sender_tag =
                crate::abi::Withdrawal::sender_tag(auth.caller, withdrawal.zone_tx_hash);
            let terminal = self
                .find_l1_withdrawal_terminal(
                    &withdrawal,
                    expected_sender_tag,
                    batch_submitted.l1_block_number,
                )
                .await?;
            match terminal {
                TerminalLookup::NotFound => return to_raw(&response),
                TerminalLookup::Ambiguous => {
                    // Multiple `processWithdrawal` calldata payloads matched
                    // the zone request. Returning either would be a guess, so
                    // keep the public status at `submitted` and surface the
                    // ambiguity via a stable, non-sensitive error code.
                    response.error = Some("ambiguous_terminal_match".to_string());
                    return to_raw(&response);
                }
                TerminalLookup::Single(terminal) => {
                    response.l1_process_withdrawal_tx_hash = Some(terminal.l1_tx_hash);
                    response.callback_success = Some(terminal.callback_success);
                    response.status = withdrawal_status_from_terminal(terminal);
                    if response.status == WithdrawalState::Failed {
                        response.error = Some("withdrawal callback reverted on L1".to_string());
                    } else if response.status == WithdrawalState::Bounced {
                        response.error =
                            Some("withdrawal bounced to fallback recipient on L1".to_string());
                    }
                }
            }

            to_raw(&response)
        })
    }

    fn zone_get_deposit_status(&self, tempo_block_number: u64, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            let zone_processed_through = self
                .tempo_state
                .tempoBlockNumber()
                .call()
                .await
                .map_err(internal)?;
            let portal_deposits = self.portal_deposits_for_block(tempo_block_number).await?;

            let mut deposits = Vec::new();
            for deposit in portal_deposits {
                match deposit {
                    PortalDepositRecord::Regular {
                        deposit_hash,
                        sender,
                        recipient,
                        token,
                        amount,
                        memo,
                    } => {
                        if sender != auth.caller && recipient != auth.caller {
                            continue;
                        }

                        let terminal = self.terminal_event_for_deposit(deposit_hash).await?;
                        let status = regular_deposit_status(terminal)?;

                        deposits.push(DepositStatusEntry {
                            deposit_hash,
                            kind: DepositKind::Regular,
                            token,
                            sender,
                            recipient: Some(recipient),
                            amount: U256::from(amount),
                            memo: Some(memo),
                            status,
                        });
                    }
                    PortalDepositRecord::Encrypted {
                        deposit_hash,
                        sender,
                        token,
                        amount,
                    } => {
                        let terminal = self.terminal_event_for_deposit(deposit_hash).await?;

                        let include = match (&terminal, sender == auth.caller) {
                            (_, true) => true,
                            (
                                Some(TerminalDepositEvent::EncryptedProcessed {
                                    recipient, ..
                                }),
                                false,
                            ) => *recipient == auth.caller,
                            _ => false,
                        };

                        if !include {
                            continue;
                        }

                        let (recipient, memo, status) = encrypted_deposit_details(terminal)?;

                        deposits.push(DepositStatusEntry {
                            deposit_hash,
                            kind: DepositKind::Encrypted,
                            token,
                            sender,
                            recipient,
                            amount: U256::from(amount),
                            memo,
                            status,
                        });
                    }
                }
            }

            let processed = zone_processed_through >= tempo_block_number
                && deposits
                    .iter()
                    .all(|deposit| deposit.status != DepositState::Pending);

            to_raw(&DepositStatusResponse {
                tempo_block_number: U64::from(tempo_block_number),
                zone_processed_through: U64::from(zone_processed_through),
                processed,
                deposits,
            })
        })
    }

    fn zone_get_my_orders(&self, query: HistoryQuery, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            let owner = zone_darkpool::require_owner(query.account, &auth.caller)?;
            let limit = zone_darkpool::clamp_limit(query.limit);
            let cursor = query
                .cursor
                .as_deref()
                .map(zone_darkpool::Cursor::decode)
                .transpose()?;
            let pair_filter = zone_darkpool::parse_pair_filter(query.pair.as_deref())?;

            let owner_topic = zone_darkpool::topic_for_address(&owner);
            let topics = vec![
                zone_darkpool::OrderSubmitted::SIGNATURE_HASH,
                zone_darkpool::OrderPlaced::SIGNATURE_HASH,
                zone_darkpool::OrderFilled::SIGNATURE_HASH,
                zone_darkpool::OrderCancelled::SIGNATURE_HASH,
            ];
            let filter = zone_darkpool::build_darkpool_filter(&topics, Some(owner_topic), cursor);
            let logs = EthFilterApiServer::logs(&self.eth.filter, filter)
                .await
                .map_err(internal)?;

            let mut orders = zone_darkpool::reconstruct_orders(
                logs.iter()
                    .filter(|log| zone_darkpool::caller_is_maker(log, &owner)),
            );

            if let Some(pair) = pair_filter {
                orders.retain(|o| o.base_token == pair.0 && o.quote_token == pair.1);
            }
            if let Some(status) = query.status {
                orders.retain(|o| o.status == status);
            }
            orders.sort_by(|a, b| {
                b.updated_at_block
                    .cmp(&a.updated_at_block)
                    .then_with(|| b.order_id.cmp(&a.order_id))
            });

            let next_cursor = zone_darkpool::next_order_cursor(&orders, limit);
            orders.truncate(limit as usize);

            to_raw(&Page {
                items: orders,
                next_cursor,
            })
        })
    }

    fn zone_get_my_fills(&self, query: HistoryQuery, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            let owner = zone_darkpool::require_owner(query.account, &auth.caller)?;
            let limit = zone_darkpool::clamp_limit(query.limit);
            let cursor = query
                .cursor
                .as_deref()
                .map(zone_darkpool::Cursor::decode)
                .transpose()?;
            let pair_filter = zone_darkpool::parse_pair_filter(query.pair.as_deref())?;

            let owner_topic = zone_darkpool::topic_for_address(&owner);
            let topics = vec![zone_darkpool::OrderFilled::SIGNATURE_HASH];

            // OrderSubmitted carries the only pair metadata. Scan from
            // genesis because a fill at `cursor` can reference an older
            // resting order. Foreign submissions are used only for pair
            // metadata; their order ids are never returned.
            let submitted_filter = zone_darkpool::build_darkpool_filter(
                &[zone_darkpool::OrderSubmitted::SIGNATURE_HASH],
                None,
                None,
            );

            let maker_filter =
                zone_darkpool::build_darkpool_filter(&topics, Some(owner_topic), cursor);
            let mut taker_filter = zone_darkpool::build_darkpool_filter(&topics, None, cursor);
            taker_filter.topics[3] = alloy_rpc_types_eth::FilterSet::from(owner_topic);

            let submitted_logs = EthFilterApiServer::logs(&self.eth.filter, submitted_filter)
                .await
                .map_err(internal)?;
            let maker_logs = EthFilterApiServer::logs(&self.eth.filter, maker_filter)
                .await
                .map_err(internal)?;
            let taker_logs = EthFilterApiServer::logs(&self.eth.filter, taker_filter)
                .await
                .map_err(internal)?;

            let pair_index = zone_darkpool::build_pair_index(submitted_logs.iter(), &owner);

            let mut fills: Vec<zone_darkpool::FillEntry> = maker_logs
                .iter()
                .filter(|log| zone_darkpool::caller_is_maker(log, &owner))
                .filter_map(|log| {
                    zone_darkpool::fill_entry_from_log(log, FillRole::Maker, &pair_index)
                })
                .chain(
                    taker_logs
                        .iter()
                        .filter(|log| zone_darkpool::caller_is_taker(log, &owner))
                        .filter_map(|log| {
                            zone_darkpool::fill_entry_from_log(log, FillRole::Taker, &pair_index)
                        }),
                )
                .collect();

            if let Some(pair) = pair_filter {
                fills.retain(|f| f.base_token == pair.0 && f.quote_token == pair.1);
            }
            fills.sort_by(|a, b| {
                b.block_number
                    .cmp(&a.block_number)
                    .then_with(|| b.log_index.cmp(&a.log_index))
            });
            fills.dedup_by(|a, b| {
                a.tx_hash == b.tx_hash && a.log_index == b.log_index && a.role == b.role
            });

            let next_cursor = zone_darkpool::next_fill_cursor(&fills, limit);
            fills.truncate(limit as usize);

            to_raw(&Page {
                items: fills,
                next_cursor,
            })
        })
    }

    fn zone_get_my_transfers(&self, query: TransferQuery, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            let owner = zone_darkpool::require_owner(query.account, &auth.caller)?;
            let limit = zone_darkpool::clamp_limit(query.limit);
            let cursor = query
                .cursor
                .as_deref()
                .map(zone_darkpool::Cursor::decode)
                .transpose()?;

            let owner_topic = zone_darkpool::topic_for_address(&owner);
            let transfer_topics = vec![
                zone_rpc::filter::TRANSFER_TOPIC,
                zone_rpc::filter::TRANSFER_WITH_MEMO_TOPIC,
                zone_rpc::filter::MINT_TOPIC,
                zone_rpc::filter::BURN_TOPIC,
            ];
            let from_filter = zone_darkpool::build_tip20_filter(
                &transfer_topics,
                Some(owner_topic),
                cursor,
                true,
            );
            let to_filter = zone_darkpool::build_tip20_filter(
                &transfer_topics,
                Some(owner_topic),
                cursor,
                false,
            );

            let from_logs = EthFilterApiServer::logs(&self.eth.filter, from_filter)
                .await
                .map_err(internal)?;
            let to_logs = EthFilterApiServer::logs(&self.eth.filter, to_filter)
                .await
                .map_err(internal)?;

            let mut transfers: Vec<zone_darkpool::TransferEntry> = from_logs
                .into_iter()
                .chain(to_logs)
                .filter(|log| zone_rpc::filter::is_log_visible(log, &owner))
                .filter_map(|log| zone_darkpool::transfer_entry_from_log(&log, &owner))
                .collect();

            transfers.sort_by(|a, b| {
                b.block_number
                    .cmp(&a.block_number)
                    .then_with(|| b.log_index.cmp(&a.log_index))
            });
            transfers.dedup_by(|a, b| a.tx_hash == b.tx_hash && a.log_index == b.log_index);

            let next_cursor = zone_darkpool::next_transfer_cursor(&transfers, limit);
            transfers.truncate(limit as usize);

            to_raw(&Page {
                items: transfers,
                next_cursor,
            })
        })
    }

    fn zone_get_order(&self, order_id: u128, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            let owner = auth.caller;
            let filter = zone_darkpool::build_order_filter(order_id, &owner);
            let logs = EthFilterApiServer::logs(&self.eth.filter, filter)
                .await
                .map_err(internal)?;
            let mut orders = zone_darkpool::reconstruct_orders(
                logs.iter()
                    .filter(|log| zone_darkpool::caller_is_maker(log, &owner)),
            );
            match orders.pop() {
                Some(order) if order.order_id == alloy_primitives::U128::from(order_id) => {
                    to_raw(&order)
                }
                _ => Ok(raw_null()),
            }
        })
    }
}

/// Zone-side data extracted from a `WithdrawalRequested` event plus its
/// transaction context. The `sender` is intentionally omitted: an instance of
/// this record only exists once the caller's identity has been confirmed to
/// match the event's sender inside [`find_withdrawal_requested`], so further
/// code paths must not re-derive ownership decisions from it.
///
/// The non-indexed fields (`fee`, `gas_limit`, `callback_data`, …) are kept so
/// the L1 terminal lookup can compare the full `processWithdrawal` calldata
/// payload against the original request, rather than the ambiguous
/// `(to, token, amount)` triple.
#[derive(Debug, Clone)]
struct WithdrawalRequestedRecord {
    withdrawal_index: u64,
    token: Address,
    to: Address,
    amount: u128,
    fee: u128,
    memo: B256,
    gas_limit: u64,
    fallback_recipient: Address,
    callback_data: Bytes,
    zone_tx_hash: B256,
    zone_block_number: u64,
}

/// L2 outbox batch-seal data for the zone block containing the withdrawal.
#[derive(Debug, Clone, Copy)]
struct BatchFinalizedRecord {
    withdrawal_batch_index: u64,
    withdrawal_queue_hash: B256,
}

/// L1 portal `BatchSubmitted` data for the matching withdrawal batch.
#[derive(Debug, Clone, Copy)]
struct BatchSubmittedRecord {
    l1_tx_hash: B256,
    l1_block_number: u64,
}

/// Terminal L1 settlement outcome for a single withdrawal.
#[derive(Debug, Clone, Copy)]
struct TerminalWithdrawalEvent {
    l1_tx_hash: B256,
    callback_success: bool,
    /// `true` when a `BounceBack` was emitted alongside the `WithdrawalProcessed`
    /// in the same L1 transaction.
    bounced: bool,
}

/// One settled-withdrawal candidate that passed every layer of disambiguation
/// (indexed-`to` filter, shallow `(token, amount)` event-field check, and
/// full `processWithdrawal` calldata equality via [`withdrawal_matches`]).
///
/// Reaching this struct means the candidate is *eligible* to be the terminal
/// event; [`classify_terminal_candidates`] decides whether it is actually
/// reportable based on how many other candidates also reached this point.
#[derive(Debug, Clone)]
struct CandidateTerminal {
    l1_tx_hash: B256,
    callback_success: bool,
}

/// Outcome of the L1 terminal lookup after exact-calldata disambiguation.
#[derive(Debug)]
enum TerminalLookup {
    /// No `WithdrawalProcessed` log decoded to a `processWithdrawal` payload
    /// matching the zone-side request.
    NotFound,
    /// Exactly one candidate matched the full payload.
    Single(TerminalWithdrawalEvent),
    /// More than one candidate matched. Reporting any of them would be a
    /// guess, so the caller should keep the public status at `submitted` and
    /// surface this state via a non-sensitive error code.
    Ambiguous,
}

/// Classification of a `zone_searchBatch` query string.
#[derive(Debug, PartialEq, Eq)]
enum BatchQuery {
    /// Decimal or hex batch number.
    BatchNumber(u64),
    /// Exactly 32-byte hex hash interpreted as the L1 settlement tx hash.
    SettlementTxHash(B256),
    /// Could not be parsed as either form.
    Invalid,
}

fn classify_batch_query(query: &str) -> BatchQuery {
    let hex_body = query
        .strip_prefix("0x")
        .or_else(|| query.strip_prefix("0X"));

    if let Some(body) = hex_body
        && body.len() == 64
        && let Ok(hash) = B256::from_str(query)
    {
        return BatchQuery::SettlementTxHash(hash);
    }

    if let Ok(value) = U64::from_str(query) {
        return BatchQuery::BatchNumber(value.to());
    }
    if let Ok(value) = query.parse::<u64>() {
        return BatchQuery::BatchNumber(value);
    }

    BatchQuery::Invalid
}

/// Encode `batch_number` as the topic-1 value used by indexed `uint64` filters.
fn batch_number_topic(batch_number: u64) -> B256 {
    B256::from(U256::from(batch_number).to_be_bytes::<32>())
}

/// Decode the indexed `withdrawalBatchIndex` from topic-1 of a `BatchSubmitted` log.
fn log_batch_index(log: &alloy_rpc_types_eth::Log) -> Option<u64> {
    let topic = log.topics().get(1)?;
    let bytes = topic.as_slice();
    let last_eight = bytes.get(24..32)?;
    let mut arr = [0u8; 8];
    arr.copy_from_slice(last_eight);
    Some(u64::from_be_bytes(arr))
}

/// Pure mapping: `BatchSubmitted` event + decoded `submitBatch` calldata +
/// timing data to aggregate-only [`BatchSummary`].
fn map_batch_summary(
    event: &ZonePortal::BatchSubmitted,
    call: &ZonePortal::submitBatchCall,
    settlement_tx_hash: B256,
    settled_at: Option<u64>,
    zone_block_from: Option<u64>,
    zone_block_to: Option<u64>,
    sealed_at: Option<u64>,
) -> BatchSummary {
    BatchSummary {
        batch_number: U64::from(event.withdrawalBatchIndex),
        zone_block_from: zone_block_from.map(U64::from),
        zone_block_to: zone_block_to.map(U64::from),
        tempo_block_number: U64::from(call.tempoBlockNumber),
        root: event.withdrawalQueueHash,
        prev_block_hash: call.blockTransition.prevBlockHash,
        next_block_hash: event.nextBlockHash,
        status: BatchStatus::Submitted,
        sealed_at: sealed_at.map(U64::from),
        settled_at: settled_at.map(U64::from),
        order_count: U64::ZERO,
        fill_count: U64::ZERO,
        aggregate_pairs: Vec::new(),
        aggregate_volume: Vec::new(),
        settlement_tx_hash,
        proof_ref: None,
    }
}

#[derive(Debug, Clone)]
enum PortalDepositRecord {
    Regular {
        deposit_hash: B256,
        sender: Address,
        recipient: Address,
        token: Address,
        amount: u128,
        memo: B256,
    },
    Encrypted {
        deposit_hash: B256,
        sender: Address,
        token: Address,
        amount: u128,
    },
}

#[derive(Debug, Clone)]
enum TerminalDepositEvent {
    RegularProcessed,
    EncryptedProcessed { recipient: Address, memo: B256 },
    EncryptedFailed,
}

/// Map a terminal L1 settlement event to the corresponding `WithdrawalState`.
///
/// - `callbackSuccess == true` → `Processed` (callback succeeded or absent).
/// - `callbackSuccess == false` and `BounceBack` emitted in same tx → `Bounced`.
/// - `callbackSuccess == false` and no `BounceBack` → `Failed`.
fn withdrawal_status_from_terminal(terminal: TerminalWithdrawalEvent) -> WithdrawalState {
    if terminal.callback_success {
        WithdrawalState::Processed
    } else if terminal.bounced {
        WithdrawalState::Bounced
    } else {
        WithdrawalState::Failed
    }
}

/// Returns `true` when an L1 `Withdrawal` payload decoded from a
/// `processWithdrawal` calldata exactly matches the originating zone
/// `WithdrawalRequested` event.
///
/// `expected_sender_tag` must equal `keccak256(sender || zoneTxHash)` for the
/// authenticated caller — the L1 sender tag is the only way to bind the
/// settlement payload back to a specific zone transaction without trusting
/// the recipient/token/amount triple alone.
///
/// `encryptedSender` is intentionally NOT compared: the sequencer attaches
/// it only at `finalizeWithdrawalBatch` time and the zone event does not
/// carry it. The remaining fields are sufficient to make a collision require
/// two byte-identical withdrawals from the same caller, in the same zone tx,
/// settled into different L1 batches — which is the corner case the caller's
/// [`TerminalLookup::Ambiguous`] branch handles.
fn withdrawal_matches(
    l1_withdrawal: &crate::abi::Withdrawal,
    requested: &WithdrawalRequestedRecord,
    expected_sender_tag: B256,
) -> bool {
    l1_withdrawal.token == requested.token
        && l1_withdrawal.senderTag == expected_sender_tag
        && l1_withdrawal.to == requested.to
        && l1_withdrawal.amount == requested.amount
        && l1_withdrawal.fee == requested.fee
        && l1_withdrawal.memo == requested.memo
        && l1_withdrawal.gasLimit == requested.gas_limit
        && l1_withdrawal.fallbackRecipient == requested.fallback_recipient
        && l1_withdrawal.callbackData == requested.callback_data
}

/// Output of the count-based classification step in
/// [`find_l1_withdrawal_terminal`]. Pulled out as a standalone enum so the
/// 0/1/N → outcome mapping can be tested without spinning up a provider.
#[derive(Debug)]
enum TerminalCandidateOutcome<T> {
    NotFound,
    Single(T),
    Ambiguous,
}

/// Decide between `NotFound` / `Single(_)` / `Ambiguous` based purely on the
/// number of already-filtered candidates.
fn classify_terminal_candidates<T>(mut candidates: Vec<T>) -> TerminalCandidateOutcome<T> {
    match candidates.len() {
        0 => TerminalCandidateOutcome::NotFound,
        1 => TerminalCandidateOutcome::Single(candidates.remove(0)),
        _ => TerminalCandidateOutcome::Ambiguous,
    }
}

fn regular_deposit_status(
    terminal: Option<TerminalDepositEvent>,
) -> Result<DepositState, JsonRpcError> {
    match terminal {
        Some(TerminalDepositEvent::RegularProcessed) => Ok(DepositState::Processed),
        Some(TerminalDepositEvent::EncryptedProcessed { .. }) => Err(JsonRpcError::internal(
            "encrypted deposit event matched regular deposit hash",
        )),
        Some(TerminalDepositEvent::EncryptedFailed) => Err(JsonRpcError::internal(
            "encrypted deposit failure matched regular deposit hash",
        )),
        None => Ok(DepositState::Pending),
    }
}

fn encrypted_deposit_details(
    terminal: Option<TerminalDepositEvent>,
) -> Result<(Option<Address>, Option<B256>, DepositState), JsonRpcError> {
    match terminal {
        Some(TerminalDepositEvent::EncryptedProcessed { recipient, memo }) => {
            Ok((Some(recipient), Some(memo), DepositState::Processed))
        }
        Some(TerminalDepositEvent::EncryptedFailed) => Ok((None, None, DepositState::Failed)),
        Some(TerminalDepositEvent::RegularProcessed) => Err(JsonRpcError::internal(
            "regular deposit event matched encrypted deposit hash",
        )),
        None => Ok((None, None, DepositState::Pending)),
    }
}

fn redact_tempo_header(header: &mut TempoHeader) {
    header.inner.logs_bloom = Bloom::ZERO;
}

fn redact_ws_header(header: &mut TempoHeaderResponse) {
    redact_tempo_header(&mut header.inner.inner);
}

/// Strip privacy-sensitive fields from a block for non-sequencer callers.
fn redact_block(block: &mut RpcBlock) {
    redact_tempo_header(&mut block.header.inner);
    block.transactions = BlockTransactions::Hashes(Vec::new());
}

fn level_from_response(price: u128, quantity: u128) -> Option<OrderLevel> {
    if price == 0 || quantity == 0 {
        return None;
    }
    Some(OrderLevel {
        price: U128::from(price),
        quantity: U128::from(quantity),
    })
}

fn ensure_canonical_pair(base: Address, quote: Address) -> Result<(), JsonRpcError> {
    if base == alpha::BASE && quote == alpha::QUOTE {
        Ok(())
    } else {
        Err(JsonRpcError::invalid_params(
            "unsupported pair; this build only exposes OALPHA/PATH.USD",
        ))
    }
}

fn canonical_alpha_market_config() -> MarketConfigResponse {
    MarketConfigResponse {
        darkpool: DARKPOOL_ADDRESS,
        markets: vec![MarketEntry {
            pair: alpha::PAIR_LABEL.to_string(),
            base: MarketToken {
                address: alpha::BASE,
                symbol: alpha::BASE_SYMBOL.to_string(),
                decimals: alpha::DECIMALS,
            },
            quote: MarketToken {
                address: alpha::QUOTE,
                symbol: alpha::QUOTE_SYMBOL.to_string(),
                decimals: alpha::DECIMALS,
            },
            min_order_amount: U128::from(zone_precompiles::orderbook::MIN_ORDER_AMOUNT),
            price_unit: "raw integer; quote = baseAmount * price".to_string(),
            allowed_actions: vec![
                MarketAction::MarketBuy,
                MarketAction::MarketSell,
                MarketAction::LimitBid,
                MarketAction::LimitAsk,
            ],
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regular_deposit_status_maps_terminal_events() {
        assert_eq!(
            regular_deposit_status(Some(TerminalDepositEvent::RegularProcessed)).unwrap(),
            DepositState::Processed
        );
        assert_eq!(regular_deposit_status(None).unwrap(), DepositState::Pending);
    }

    #[test]
    fn regular_deposit_status_rejects_encrypted_terminal_events() {
        let err = regular_deposit_status(Some(TerminalDepositEvent::EncryptedFailed)).unwrap_err();
        assert_eq!(
            err.message,
            "encrypted deposit failure matched regular deposit hash"
        );
    }

    #[test]
    fn encrypted_deposit_details_maps_terminal_events() {
        let recipient = Address::repeat_byte(0x11);
        let memo = B256::from([0x22; 32]);

        assert_eq!(
            encrypted_deposit_details(Some(TerminalDepositEvent::EncryptedProcessed {
                recipient,
                memo,
            }))
            .unwrap(),
            (Some(recipient), Some(memo), DepositState::Processed)
        );
        assert_eq!(
            encrypted_deposit_details(Some(TerminalDepositEvent::EncryptedFailed)).unwrap(),
            (None, None, DepositState::Failed)
        );
        assert_eq!(
            encrypted_deposit_details(None).unwrap(),
            (None, None, DepositState::Pending)
        );
    }

    #[test]
    fn encrypted_deposit_details_rejects_regular_terminal_events() {
        let err =
            encrypted_deposit_details(Some(TerminalDepositEvent::RegularProcessed)).unwrap_err();
        assert_eq!(
            err.message,
            "regular deposit event matched encrypted deposit hash"
        );
    }

    #[test]
    fn withdrawal_status_maps_callback_success_to_processed() {
        let terminal = TerminalWithdrawalEvent {
            l1_tx_hash: B256::repeat_byte(0x11),
            callback_success: true,
            bounced: false,
        };
        assert_eq!(
            withdrawal_status_from_terminal(terminal),
            WithdrawalState::Processed
        );
    }

    #[test]
    fn withdrawal_status_maps_callback_failure_with_bounce_to_bounced() {
        let terminal = TerminalWithdrawalEvent {
            l1_tx_hash: B256::repeat_byte(0x22),
            callback_success: false,
            bounced: true,
        };
        assert_eq!(
            withdrawal_status_from_terminal(terminal),
            WithdrawalState::Bounced
        );
    }

    #[test]
    fn withdrawal_status_maps_callback_failure_without_bounce_to_failed() {
        let terminal = TerminalWithdrawalEvent {
            l1_tx_hash: B256::repeat_byte(0x33),
            callback_success: false,
            bounced: false,
        };
        assert_eq!(
            withdrawal_status_from_terminal(terminal),
            WithdrawalState::Failed
        );
    }

    /// Build a paired `(WithdrawalRequestedRecord, L1 Withdrawal, sender_tag)`
    /// fixture where every field of the L1 payload is consistent with the
    /// zone-side record. Tests then mutate one field at a time to assert that
    /// [`withdrawal_matches`] rejects mismatches.
    fn matching_withdrawal_fixture() -> (
        WithdrawalRequestedRecord,
        crate::abi::Withdrawal,
        B256, // expected_sender_tag
    ) {
        let sender = Address::repeat_byte(0xaa);
        let zone_tx_hash = B256::repeat_byte(0x77);
        let sender_tag = crate::abi::Withdrawal::sender_tag(sender, zone_tx_hash);

        let requested = WithdrawalRequestedRecord {
            withdrawal_index: 1,
            token: Address::repeat_byte(0x10),
            to: Address::repeat_byte(0x20),
            amount: 1_000_000,
            fee: 250,
            memo: B256::repeat_byte(0x33),
            gas_limit: 50_000,
            fallback_recipient: Address::repeat_byte(0x40),
            callback_data: Bytes::from(vec![0xde, 0xad, 0xbe, 0xef]),
            zone_tx_hash,
            zone_block_number: 42,
        };

        let l1_withdrawal = crate::abi::Withdrawal {
            token: requested.token,
            senderTag: sender_tag,
            to: requested.to,
            amount: requested.amount,
            fee: requested.fee,
            memo: requested.memo,
            gasLimit: requested.gas_limit,
            fallbackRecipient: requested.fallback_recipient,
            callbackData: requested.callback_data.clone(),
            encryptedSender: Bytes::from(vec![0x01, 0x02]),
        };

        (requested, l1_withdrawal, sender_tag)
    }

    #[test]
    fn withdrawal_matches_accepts_identical_payload() {
        let (requested, l1_withdrawal, sender_tag) = matching_withdrawal_fixture();
        assert!(withdrawal_matches(&l1_withdrawal, &requested, sender_tag));
    }

    #[test]
    fn withdrawal_matches_rejects_wrong_sender_tag() {
        let (requested, l1_withdrawal, _sender_tag) = matching_withdrawal_fixture();
        let other_caller = Address::repeat_byte(0xbb);
        let other_tag = crate::abi::Withdrawal::sender_tag(other_caller, requested.zone_tx_hash);
        assert!(!withdrawal_matches(&l1_withdrawal, &requested, other_tag));
    }

    #[test]
    fn withdrawal_matches_rejects_per_field_divergence() {
        let (requested, base, sender_tag) = matching_withdrawal_fixture();

        let mut diff_token = base.clone();
        diff_token.token = Address::repeat_byte(0x11);
        assert!(!withdrawal_matches(&diff_token, &requested, sender_tag));

        let mut diff_to = base.clone();
        diff_to.to = Address::repeat_byte(0x21);
        assert!(!withdrawal_matches(&diff_to, &requested, sender_tag));

        let mut diff_amount = base.clone();
        diff_amount.amount = base.amount + 1;
        assert!(!withdrawal_matches(&diff_amount, &requested, sender_tag));

        let mut diff_fee = base.clone();
        diff_fee.fee = base.fee + 1;
        assert!(!withdrawal_matches(&diff_fee, &requested, sender_tag));

        let mut diff_memo = base.clone();
        diff_memo.memo = B256::repeat_byte(0x44);
        assert!(!withdrawal_matches(&diff_memo, &requested, sender_tag));

        let mut diff_gas = base.clone();
        diff_gas.gasLimit = base.gasLimit + 1;
        assert!(!withdrawal_matches(&diff_gas, &requested, sender_tag));

        let mut diff_fallback = base.clone();
        diff_fallback.fallbackRecipient = Address::repeat_byte(0x41);
        assert!(!withdrawal_matches(&diff_fallback, &requested, sender_tag));

        let mut diff_callback = base.clone();
        diff_callback.callbackData = Bytes::from(vec![0xff]);
        assert!(!withdrawal_matches(&diff_callback, &requested, sender_tag));
    }

    #[test]
    fn classify_terminal_candidates_routes_empty_to_not_found() {
        let out: TerminalCandidateOutcome<u32> = classify_terminal_candidates(Vec::new());
        assert!(matches!(out, TerminalCandidateOutcome::NotFound));
    }

    #[test]
    fn classify_terminal_candidates_routes_singleton_to_single() {
        let out = classify_terminal_candidates(vec![42u32]);
        match out {
            TerminalCandidateOutcome::Single(v) => assert_eq!(v, 42),
            other => panic!("expected Single, got {other:?}"),
        }
    }

    #[test]
    fn classify_terminal_candidates_routes_two_plus_to_ambiguous() {
        let out = classify_terminal_candidates(vec![1u32, 2u32]);
        assert!(matches!(out, TerminalCandidateOutcome::Ambiguous));

        let out = classify_terminal_candidates(vec![1u32, 2u32, 3u32]);
        assert!(matches!(out, TerminalCandidateOutcome::Ambiguous));
    }

    fn sample_batch_event(batch: u64) -> ZonePortal::BatchSubmitted {
        ZonePortal::BatchSubmitted {
            withdrawalBatchIndex: batch,
            nextProcessedDepositQueueHash: B256::repeat_byte(0x11),
            nextBlockHash: B256::repeat_byte(0x22),
            withdrawalQueueHash: B256::repeat_byte(0x33),
            lastProcessedDepositNumber: 7,
        }
    }

    fn sample_batch_call(tempo_block: u64) -> ZonePortal::submitBatchCall {
        ZonePortal::submitBatchCall {
            tempoBlockNumber: tempo_block,
            recentTempoBlockNumber: 0,
            blockTransition: crate::abi::BlockTransition {
                prevBlockHash: B256::repeat_byte(0x44),
                nextBlockHash: B256::repeat_byte(0x22),
            },
            depositQueueTransition: crate::abi::DepositQueueTransition {
                prevProcessedHash: B256::ZERO,
                nextProcessedHash: B256::repeat_byte(0x11),
                prevDepositNumber: 0,
                nextDepositNumber: 7,
            },
            withdrawalQueueHash: B256::repeat_byte(0x33),
            verifierConfig: Default::default(),
            proof: Default::default(),
        }
    }

    #[test]
    fn map_batch_summary_uses_event_and_call_fields() {
        let event = sample_batch_event(42);
        let call = sample_batch_call(1_000);
        let settlement_tx = B256::repeat_byte(0x55);

        let summary = map_batch_summary(
            &event,
            &call,
            settlement_tx,
            Some(123),
            Some(8),
            Some(20),
            Some(456),
        );

        assert_eq!(summary.batch_number, U64::from(42));
        assert_eq!(summary.zone_block_from, Some(U64::from(8)));
        assert_eq!(summary.zone_block_to, Some(U64::from(20)));
        assert_eq!(summary.tempo_block_number, U64::from(1_000));
        assert_eq!(summary.root, event.withdrawalQueueHash);
        assert_eq!(summary.prev_block_hash, call.blockTransition.prevBlockHash);
        assert_eq!(summary.next_block_hash, event.nextBlockHash);
        assert_eq!(summary.status, BatchStatus::Submitted);
        assert_eq!(summary.sealed_at, Some(U64::from(456)));
        assert_eq!(summary.settled_at, Some(U64::from(123)));
        assert_eq!(summary.settlement_tx_hash, settlement_tx);
        assert_eq!(summary.proof_ref, None);
    }

    #[test]
    fn map_batch_summary_emits_aggregate_only_fields() {
        let event = sample_batch_event(7);
        let call = sample_batch_call(50);
        let summary = map_batch_summary(
            &event,
            &call,
            B256::repeat_byte(0x66),
            None,
            None,
            None,
            None,
        );

        let json = serde_json::to_value(&summary).expect("summary should serialize");
        let obj = json.as_object().expect("summary must be a JSON object");
        for forbidden in [
            "from",
            "to",
            "sender",
            "recipient",
            "owner",
            "counterparty",
            "orderId",
            "fillId",
            "trader",
            "userAddress",
        ] {
            assert!(
                !obj.contains_key(forbidden),
                "batch summary leaked owner-linked field `{forbidden}`",
            );
        }

        assert_eq!(summary.order_count, U64::ZERO);
        assert_eq!(summary.fill_count, U64::ZERO);
        assert!(summary.aggregate_pairs.is_empty());
        assert!(summary.aggregate_volume.is_empty());
    }

    #[test]
    fn batch_number_topic_left_pads_uint64() {
        let topic = batch_number_topic(0x2a);
        assert_eq!(&topic.as_slice()[..24], &[0u8; 24]);
        assert_eq!(
            u64::from_be_bytes(topic.as_slice()[24..32].try_into().unwrap()),
            42
        );
    }

    #[test]
    fn log_batch_index_round_trips_through_topic() {
        let topics = vec![
            ZonePortal::BatchSubmitted::SIGNATURE_HASH,
            batch_number_topic(123),
        ];
        let log = alloy_rpc_types_eth::Log {
            inner: alloy_primitives::Log {
                address: Address::ZERO,
                data: alloy_primitives::LogData::new_unchecked(topics, Bytes::default()),
            },
            block_hash: None,
            block_number: None,
            block_timestamp: None,
            transaction_hash: None,
            transaction_index: None,
            log_index: None,
            removed: false,
        };
        assert_eq!(log_batch_index(&log), Some(123));
    }

    #[test]
    fn classify_batch_query_recognises_batch_numbers() {
        assert_eq!(classify_batch_query("42"), BatchQuery::BatchNumber(42));
        assert_eq!(classify_batch_query("0x2a"), BatchQuery::BatchNumber(42));
        assert_eq!(classify_batch_query("0X2A"), BatchQuery::BatchNumber(42));
    }

    #[test]
    fn classify_batch_query_recognises_settlement_tx_hash() {
        let hash = format!("0x{}", "11".repeat(32));
        match classify_batch_query(&hash) {
            BatchQuery::SettlementTxHash(parsed) => {
                assert_eq!(parsed, B256::repeat_byte(0x11));
            }
            other => panic!("expected SettlementTxHash, got {other:?}"),
        }
    }

    #[test]
    fn classify_batch_query_rejects_garbage() {
        assert_eq!(classify_batch_query("not-a-batch"), BatchQuery::Invalid);
        let too_short = format!("0x{}", "11".repeat(31));
        assert_eq!(classify_batch_query(&too_short), BatchQuery::Invalid);
    }

    #[test]
    fn stale_filter_owner_ids_removes_only_inactive_entries() {
        let active_ids = HashSet::from([
            FilterId::Str("0xactive".to_string()),
            FilterId::Str("0xkeep".to_string()),
        ]);
        let owner_ids = vec![
            FilterId::Str("0xactive".to_string()),
            FilterId::Str("0xstale".to_string()),
            FilterId::Str("0xkeep".to_string()),
        ];

        let stale_ids = stale_filter_owner_ids(owner_ids, &active_ids);

        assert_eq!(stale_ids, vec![FilterId::Str("0xstale".to_string())]);
    }

    #[test]
    fn stale_filter_owner_ids_is_noop_for_empty_owner_set() {
        let stale_ids = stale_filter_owner_ids(Vec::new(), &HashSet::new());

        assert!(stale_ids.is_empty());
    }

    #[test]
    fn level_from_response_treats_zero_as_empty_side() {
        assert!(level_from_response(0, 0).is_none());
        assert!(level_from_response(0, 100).is_none());
        assert!(level_from_response(100, 0).is_none());

        let level = level_from_response(1_000_000, 250_000).expect("level should be present");
        assert_eq!(level.price, U128::from(1_000_000u128));
        assert_eq!(level.quantity, U128::from(250_000u128));
    }

    #[test]
    fn ensure_canonical_pair_accepts_alpha_pair() {
        assert!(ensure_canonical_pair(alpha::BASE, alpha::QUOTE).is_ok());
    }

    #[test]
    fn ensure_canonical_pair_rejects_swapped_pair() {
        let err = ensure_canonical_pair(alpha::QUOTE, alpha::BASE)
            .expect_err("swapped pair must be rejected");
        assert_eq!(err.code, -32602);
        assert_eq!(
            err.message,
            "unsupported pair; this build only exposes OALPHA/PATH.USD"
        );
    }

    #[test]
    fn ensure_canonical_pair_rejects_wrong_base() {
        let err = ensure_canonical_pair(Address::repeat_byte(0x42), alpha::QUOTE)
            .expect_err("wrong base must be rejected");
        assert_eq!(err.code, -32602);
    }

    #[test]
    fn ensure_canonical_pair_rejects_wrong_quote() {
        let err = ensure_canonical_pair(alpha::BASE, Address::repeat_byte(0x42))
            .expect_err("wrong quote must be rejected");
        assert_eq!(err.code, -32602);
    }

    #[test]
    fn canonical_alpha_market_config_returns_only_the_alpha_pair() {
        let config = canonical_alpha_market_config();
        assert_eq!(config.darkpool, DARKPOOL_ADDRESS);
        assert_eq!(config.markets.len(), 1);

        let market = &config.markets[0];
        assert_eq!(market.pair, "OALPHA/PATH.USD");
        assert_eq!(market.base.address, alpha::BASE);
        assert_eq!(market.base.symbol, "OALPHA");
        assert_eq!(market.base.decimals, 6);
        assert_eq!(market.quote.address, alpha::QUOTE);
        assert_eq!(market.quote.symbol, "PATH.USD");
        assert_eq!(market.quote.decimals, 6);
        assert_eq!(
            market.min_order_amount,
            U128::from(zone_precompiles::orderbook::MIN_ORDER_AMOUNT)
        );
        assert_eq!(market.price_unit, "raw integer; quote = baseAmount * price");
        assert_eq!(
            market.allowed_actions,
            vec![
                MarketAction::MarketBuy,
                MarketAction::MarketSell,
                MarketAction::LimitBid,
                MarketAction::LimitAsk,
            ]
        );
    }

    #[test]
    fn canonical_alpha_addresses_match_task_constants() {
        assert_eq!(
            format!("{:#x}", alpha::BASE),
            "0x20c000000000000000000000518ddadd37ed1d28"
        );
        assert_eq!(
            format!("{:#x}", alpha::QUOTE),
            "0x20c0000000000000000000000000000000000000"
        );
    }
}
