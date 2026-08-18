//! [`ZoneRpcApi`] implementation backed by reth's EthApi.
//!
//! Re-exports the standalone `zone-rpc` crate so everything is accessible
//! via `zone_node::rpc::*`.

pub use zone_rpc::*;

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    str::FromStr,
    sync::{Arc, Weak},
    time::Duration,
};

use alloy_consensus::{BlockHeader, Transaction as _, TxReceipt};
use alloy_network::{ReceiptResponse, TransactionBuilder, TransactionResponse};
use alloy_primitives::{Address, B256, Bloom, Bytes, U64, U128, U256};
use alloy_provider::{DynProvider, Provider, ProviderBuilder};
use alloy_rpc_types_eth::{
    Block, BlockId, BlockNumberOrTag, BlockTransactions, FeeHistory, Filter, FilterChanges,
    FilterId, TransactionRequest,
    state::{EvmOverrides, StateOverride},
};
use alloy_sol_types::{SolCall, SolEvent, SolEventInterface};
use eyre::WrapErr;
use futures::StreamExt;
use parking_lot::RwLock;
use reth_provider::CanonStateSubscriptions;
use reth_rpc::{EthFilter, eth::filter::EthFilterError};
use reth_rpc_builder::EthHandlers;
use reth_rpc_eth_api::{
    EthApiTypes, EthFilterApiServer, RpcConvert,
    helpers::{EthApiSpec, EthBlocks, EthCall, EthFees, EthState, EthTransactions, FullEthApi},
};
use reth_rpc_eth_types::logs_utils;
use tempo_alloy::{
    TempoNetwork,
    rpc::{TempoHeaderResponse, TempoTransactionRequest},
};
use tempo_chainspec::spec::TEMPO_T0_BASE_FEE;
use tempo_contracts::precompiles::{
    ACCOUNT_KEYCHAIN_ADDRESS, ITIP20,
    account_keychain::IAccountKeychain::{self, KeyInfo, getKeyCall},
};
use tempo_primitives::TempoTxEnvelope;
use tokio::{
    sync::Mutex,
    time::{MissedTickBehavior, interval},
};
use zone_precompiles::DARKPOOL_ADDRESS;

use crate::abi::{
    DarkpoolReader, TEMPO_STATE_ADDRESS, ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS,
    ZONE_TOKEN_ADDRESS, ZoneInbox, ZoneOutbox, ZonePortal,
};
use alloy_rpc_client::ConnectionConfig;
use tempo_zone_contracts::DepositType;
use zone_precompiles::refprice::{ReferencePrice, ReferencePriceGuard};
use zone_rpc::{
    auth::AuthContext,
    darkpool::{self as zone_darkpool, FillRole, HistoryQuery, Page, TransferQuery},
    refprice as zone_refprice,
    types::{
        AuthorizationTokenInfoResponse, BatchAggregateVolume, BatchListResponse, BatchStatus,
        BatchSummary, BoxEyreFut, BoxFut, DepositKind, DepositState, DepositStatusEntry,
        DepositStatusResponse, HistoryAvailability, JsonRpcError, LIST_BATCHES_DEFAULT_LIMIT,
        LIST_BATCHES_MAX_LIMIT, ListBatchesParams, MarketAction, MarketConfigResponse, MarketEntry,
        MarketToken, MidpointHistoryResponse, MidpointSample, OrderLevel,
        REFERENCE_PRICE_DISCLAIMER, REFERENCE_PRICE_UNIT, ReferencePriceResponse,
        TopOfBookResponse, WithdrawalState, WithdrawalStatusQuery, WithdrawalStatusResponse,
        ZoneInfoResponse, internal, raw_null, raw_zero, to_raw,
    },
};

use crate::midpoint::{
    MIDPOINT_RETENTION, MIDPOINT_SAMPLE_INTERVAL, MidpointHistory, RawSample, SUPPORTED_INTERVALS,
    interval_seconds,
};

type RpcBlock = Block<alloy_rpc_types_eth::Transaction<TempoTxEnvelope>, TempoHeaderResponse>;
const FILTER_OWNER_PRUNE_INTERVAL: Duration = Duration::from_secs(60);
/// Keep L1 log requests comfortably below public-provider block-range limits.
const L1_BATCH_LOG_QUERY_MAX_BLOCKS: u64 = 50_000;
/// Keep recent immutable submitted summaries in memory for explorer reuse.
const BATCH_SUMMARY_CACHE_CAPACITY: usize = 2_048;

type MarketKey = (Address, Address);
type MidpointHistories = RwLock<HashMap<MarketKey, Arc<MidpointHistory>>>;
type BatchSummaryCache = RwLock<BTreeMap<u64, BatchSummary>>;

#[cfg(test)]
mod test_market {
    use alloy_primitives::{Address, address};

    pub(super) const BASE: Address = address!("0x20C0000000000000000000000000000000000001");
    pub(super) const QUOTE: Address = address!("0x20C0000000000000000000000000000000000000");
    pub(super) const PAIR_LABEL: &str =
        "0x20c0000000000000000000000000000000000001/0x20c0000000000000000000000000000000000000";
    pub(super) const DISPLAY_LABEL: &str = "ALPHAUSD/PATHUSD";
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
pub struct ZoneRpc<Api: EthApiTypes> {
    eth: EthHandlers<Api>,
    config: zone_rpc::PrivateRpcConfig,
    l1_provider: DynProvider<TempoNetwork>,
    zone_provider: DynProvider<TempoNetwork>,
    tempo_state: tempo_zone_contracts::TempoState::TempoStateInstance<
        DynProvider<TempoNetwork>,
        TempoNetwork,
    >,
    /// Maps filter IDs to the authenticated account that created them.
    /// The reth filter registry remains the source of truth for filter liveness.
    filter_owners: Arc<Mutex<HashMap<FilterId, Address>>>,
    /// In-memory aggregate midpoint history backing
    /// [`zone_get_midpoint_history`](Self::zone_get_midpoint_history).
    /// Written by a background sampler; never sees owner data.
    midpoint_histories: Arc<MidpointHistories>,
    /// Recent submitted batches. Once emitted on L1 these summaries are immutable.
    batch_summaries: Arc<BatchSummaryCache>,
    /// Serializes batch-explorer L1 reads across concurrent private-RPC requests.
    batch_query_lock: Arc<Mutex<()>>,
    /// Unix timestamp at which the (static) reference-price snapshot was
    /// loaded. Used to compute snapshot age for `zone_getReferencePrice`.
    ref_price_loaded_at: u64,
}

impl<Api: EthApiTypes + 'static> ZoneRpc<Api> {
    /// Wrap reth's [`EthHandlers`] (api + filter + pubsub).
    pub async fn new(
        eth: EthHandlers<Api>,
        config: zone_rpc::PrivateRpcConfig,
    ) -> eyre::Result<Self> {
        let l1_rpc_url = l1_read_rpc_url(&config.l1_rpc_url)?;
        let zone_rpc_url = config.zone_rpc_url.clone();
        let l1_provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_with_config(
                l1_rpc_url.as_str(),
                rpc_connection_config(config.retry_connection_interval),
            )
            .await
            .wrap_err("failed to connect private RPC L1 provider")?
            .erased();
        let zone_provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_with_config(
                &zone_rpc_url,
                rpc_connection_config(config.retry_connection_interval),
            )
            .await
            .wrap_err("failed to connect private RPC zone provider")?
            .erased();
        let tempo_state = crate::abi::TempoState::new(TEMPO_STATE_ADDRESS, zone_provider.clone());
        let ref_price_loaded_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let rpc = Self {
            eth,
            config,
            l1_provider,
            zone_provider,
            tempo_state,
            filter_owners: Arc::new(Mutex::new(HashMap::new())),
            midpoint_histories: Arc::new(RwLock::new(HashMap::new())),
            batch_summaries: Arc::new(RwLock::new(BTreeMap::new())),
            batch_query_lock: Arc::new(Mutex::new(())),
            ref_price_loaded_at,
        };
        rpc.spawn_filter_owner_pruner();
        rpc.spawn_midpoint_sampler();
        Ok(rpc)
    }

    /// Returns a reference to the inner [`EthFilter`] handler.
    pub fn filter(&self) -> &EthFilter<Api> {
        &self.eth.filter
    }

    async fn ensure_darkpool_market(
        &self,
        base: Address,
        quote: Address,
    ) -> Result<(), JsonRpcError> {
        let darkpool = DarkpoolReader::new(DARKPOOL_ADDRESS, &self.zone_provider);
        let exists = darkpool
            .pairExists(base, quote)
            .call()
            .await
            .map_err(internal)?;
        if exists {
            Ok(())
        } else {
            Err(JsonRpcError::invalid_params(format!(
                "market {base:#x}/{quote:#x} does not exist in the darkpool",
            )))
        }
    }

    async fn market_token(&self, address: Address) -> Result<MarketToken, JsonRpcError> {
        let token = ITIP20::new(address, &self.zone_provider);
        let symbol = token.symbol().call().await.map_err(internal)?;
        let decimals = token.decimals().call().await.map_err(internal)?;
        Ok(MarketToken {
            address,
            symbol,
            decimals,
        })
    }

    async fn market_label(&self, base: Address, quote: Address) -> Result<String, JsonRpcError> {
        let base = self.market_token(base).await?;
        let quote = self.market_token(quote).await?;
        Ok(format!("{}/{}", base.symbol, quote.symbol))
    }

    async fn darkpool_pairs(&self) -> Result<Vec<MarketKey>, JsonRpcError> {
        let darkpool = DarkpoolReader::new(DARKPOOL_ADDRESS, &self.zone_provider);
        let pair_count = darkpool.pairCount().call().await.map_err(internal)?;
        let pair_count = usize::try_from(pair_count)
            .map_err(|_| JsonRpcError::internal("darkpool pair count exceeds platform limits"))?;
        let mut pairs = Vec::with_capacity(pair_count);
        for index in 0..pair_count {
            let pair = darkpool
                .pairAt(U256::from(index))
                .call()
                .await
                .map_err(internal)?;
            pairs.push((pair.base, pair.quote));
        }
        Ok(pairs)
    }

    async fn darkpool_market_config(&self) -> Result<MarketConfigResponse, JsonRpcError> {
        let darkpool = DarkpoolReader::new(DARKPOOL_ADDRESS, &self.zone_provider);
        let min_order_amount = darkpool.MIN_ORDER_AMOUNT().call().await.map_err(internal)?;
        let mut markets = Vec::new();
        for (base, quote) in self.darkpool_pairs().await? {
            let base = self.market_token(base).await?;
            let quote = self.market_token(quote).await?;
            markets.push(MarketEntry {
                pair: format!("{}/{}", base.symbol, quote.symbol),
                base,
                quote,
                min_order_amount: U128::from(min_order_amount),
                price_unit: "raw integer; quote = baseAmount * price".to_string(),
                allowed_actions: vec![
                    MarketAction::MarketBuy,
                    MarketAction::MarketSell,
                    MarketAction::LimitBid,
                    MarketAction::LimitAsk,
                ],
            });
        }
        Ok(MarketConfigResponse {
            darkpool: DARKPOOL_ADDRESS,
            markets,
        })
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

    /// Spawn the background midpoint sampler. It polls the darkpool
    /// precompile's aggregate top-of-book at [`MIDPOINT_SAMPLE_INTERVAL`]
    /// and records a midpoint sample whenever both sides of the book are
    /// non-empty. Reads only aggregate values — no owner data crosses this
    /// boundary.
    fn spawn_midpoint_sampler(&self)
    where
        Api: Send + Sync + 'static,
    {
        let provider = self.zone_provider.clone();
        let histories: Weak<MidpointHistories> = Arc::downgrade(&self.midpoint_histories);
        tokio::spawn(async move {
            let mut tick = interval(MIDPOINT_SAMPLE_INTERVAL);
            tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

            loop {
                tick.tick().await;

                let Some(histories) = histories.upgrade() else {
                    break;
                };

                let darkpool = DarkpoolReader::new(DARKPOOL_ADDRESS, &provider);
                let Ok(pair_count) = darkpool.pairCount().call().await else {
                    continue;
                };
                let Ok(pair_count) = usize::try_from(pair_count) else {
                    continue;
                };
                for index in 0..pair_count {
                    let Ok(pair) = darkpool.pairAt(U256::from(index)).call().await else {
                        continue;
                    };
                    let Ok(best_bid) = darkpool.bestBid(pair.base, pair.quote).call().await else {
                        continue;
                    };
                    let Ok(best_ask) = darkpool.bestAsk(pair.base, pair.quote).call().await else {
                        continue;
                    };

                    if best_bid.price == 0
                        || best_bid.quantity == 0
                        || best_ask.price == 0
                        || best_ask.quantity == 0
                    {
                        continue;
                    }

                    let history = histories
                        .write()
                        .entry((pair.base, pair.quote))
                        .or_insert_with(|| Arc::new(MidpointHistory::new(MIDPOINT_RETENTION)))
                        .clone();
                    history.record(RawSample {
                        timestamp: unix_now_secs(),
                        midpoint: best_bid.price.saturating_add(best_ask.price) / 2,
                    });
                }
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
                        bounceback_recipient: event.bouncebackRecipient,
                        token: event.token,
                        amount: event.netAmount,
                        memo: event.memo,
                    });
                }
                ZonePortal::ZonePortalEvents::EncryptedDepositMade(event) => {
                    deposits.push(PortalDepositRecord::Encrypted {
                        deposit_hash: event.newCurrentDepositQueueHash,
                        sender: event.sender,
                        bounceback_recipient: event.bouncebackRecipient,
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

    async fn zone_sequencer(&self) -> Result<Address, JsonRpcError> {
        if self.config.zone_portal.is_zero() {
            return Ok(Address::ZERO);
        }

        ZonePortal::new(self.config.zone_portal, &self.l1_provider)
            .sequencer()
            .call()
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

    /// Read the portal's currently accepted zone block hash.
    async fn portal_block_hash(&self) -> Result<B256, JsonRpcError> {
        if self.config.zone_portal.is_zero() {
            return Err(JsonRpcError::internal("zone portal not configured"));
        }
        ZonePortal::new(self.config.zone_portal, self.l1_provider.clone())
            .blockHash()
            .call()
            .await
            .map_err(internal)
    }

    /// Return the newest local zone block as `(number, hash)`.
    async fn latest_zone_block(&self) -> Result<Option<(u64, B256)>, JsonRpcError> {
        let block = self
            .zone_provider
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await
            .map_err(internal)?;
        Ok(block.map(|block| (block.number(), block.header.hash)))
    }

    /// Resolve the L1 portal's accepted zone hash back to a local zone block.
    async fn submitted_zone_block_number(
        &self,
        portal_block_hash: B256,
    ) -> Result<Option<u64>, JsonRpcError> {
        if portal_block_hash.is_zero() {
            return Ok(Some(0));
        }
        self.zone_provider
            .get_block_by_hash(portal_block_hash)
            .await
            .map(|block| block.map(|block| block.number()))
            .map_err(internal)
    }

    /// Build a public pending batch row from local zone blocks that have not
    /// landed on L1 yet.
    async fn pending_batch_summary(
        &self,
        latest_batch_number: u64,
    ) -> Result<Option<BatchSummary>, JsonRpcError> {
        let (portal_block_hash, latest_zone_block, tempo_block_number) =
            tokio::try_join!(self.portal_block_hash(), self.latest_zone_block(), async {
                self.tempo_state
                    .tempoBlockNumber()
                    .call()
                    .await
                    .map_err(internal)
            },)?;

        let Some((latest_zone_block_number, latest_zone_block_hash)) = latest_zone_block else {
            return Ok(None);
        };

        let submitted_zone_block_number = self
            .submitted_zone_block_number(portal_block_hash)
            .await?
            .filter(|submitted| latest_zone_block_number > *submitted);

        let Some(submitted_zone_block_number) = submitted_zone_block_number else {
            return Ok(None);
        };

        Ok(Some(map_pending_batch_summary(
            latest_batch_number.saturating_add(1),
            submitted_zone_block_number.checked_add(1),
            latest_zone_block_number,
            tempo_block_number,
            portal_block_hash,
            latest_zone_block_hash,
            BatchAggregates::default(),
        )))
    }

    /// Fetch a single `BatchSubmitted` log by indexed `withdrawalBatchIndex`.
    async fn fetch_batch_log(
        &self,
        batch_number: u64,
    ) -> Result<Option<alloy_rpc_types_eth::Log>, JsonRpcError> {
        let logs = self
            .fetch_batch_logs_by_topics(&[batch_number_topic(batch_number)])
            .await?;
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
        self.fetch_batch_logs_by_topics(&topics).await
    }

    fn cached_batch_summary(&self, batch_number: u64) -> Option<BatchSummary> {
        self.batch_summaries.read().get(&batch_number).cloned()
    }

    fn cache_batch_summary(&self, batch_number: u64, summary: BatchSummary) {
        insert_batch_summary(&mut self.batch_summaries.write(), batch_number, summary);
    }

    /// Fetch the requested `BatchSubmitted` topics without issuing an unbounded
    /// `eth_getLogs` request. Public L1 providers cap the block span even when
    /// indexed topics are supplied, so scan backwards from the current tip in
    /// bounded windows and stop once every requested batch has been found.
    async fn fetch_batch_logs_by_topics(
        &self,
        topics: &[B256],
    ) -> Result<Vec<alloy_rpc_types_eth::Log>, JsonRpcError> {
        if self.config.zone_portal.is_zero() {
            return Err(JsonRpcError::internal("zone portal not configured"));
        }
        if topics.is_empty() {
            return Ok(Vec::new());
        }

        let portal = ZonePortal::new(self.config.zone_portal, self.l1_provider.clone());
        let genesis_block = portal
            .genesisTempoBlockNumber()
            .call()
            .await
            .map_err(internal)?;
        let latest_block = self
            .l1_provider
            .get_block_number()
            .await
            .map_err(internal)?;
        if latest_block < genesis_block {
            return Ok(Vec::new());
        }

        let requested = topics.iter().copied().collect::<HashSet<_>>();
        let mut found = HashSet::with_capacity(requested.len());
        let mut logs = Vec::with_capacity(requested.len());

        for (from_block, to_block) in reverse_inclusive_block_ranges(
            genesis_block,
            latest_block,
            L1_BATCH_LOG_QUERY_MAX_BLOCKS,
        ) {
            let filter = Filter::new()
                .address(self.config.zone_portal)
                .event_signature(ZonePortal::BatchSubmitted::SIGNATURE_HASH)
                .topic1(topics.to_vec())
                .from_block(from_block)
                .to_block(to_block);
            let chunk = self.l1_provider.get_logs(&filter).await.map_err(internal)?;
            for log in chunk {
                if let Some(topic) = log.topics().get(1)
                    && requested.contains(topic)
                    && found.insert(*topic)
                {
                    logs.push(log);
                }
            }
            if found.len() == requested.len() {
                break;
            }
        }

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
        if let Some(summary) = self.cached_batch_summary(event.withdrawalBatchIndex) {
            return Ok(summary);
        }
        let settlement_tx_hash = log
            .transaction_hash
            .ok_or_else(|| JsonRpcError::internal("BatchSubmitted log missing transaction hash"))?;
        let l1_block_number = log
            .block_number
            .ok_or_else(|| JsonRpcError::internal("BatchSubmitted log missing block number"))?;

        // Tempo's public endpoint enforces a low per-IP connection limit. Keep
        // explorer hydration strictly serial so one page cannot open a burst of
        // block/transaction connections alongside the sequencer's own L1 work.
        let zone_block_to = self
            .zone_provider
            .get_block_by_hash(event.nextBlockHash)
            .await
            .map_err(internal)?;
        let settled_at = self
            .l1_provider
            .get_block_by_number(l1_block_number.into())
            .await
            .map(|opt| opt.as_ref().map(|b| b.header.timestamp()))
            .map_err(internal)?;
        let tx = self
            .l1_provider
            .get_transaction_by_hash(settlement_tx_hash)
            .await
            .map_err(internal)?
            .ok_or_else(|| {
                JsonRpcError::internal("BatchSubmitted settlement tx not found on L1")
            })?;

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

        let aggregates = match (zone_block_from_number, zone_block_to_number) {
            (Some(prev_or_zero), Some(end)) => {
                // The reported `zone_block_from` is the *prev* block — already
                // included in the preceding batch — so the new blocks added by
                // this batch start at prev+1 (or 0 for the genesis batch).
                let inclusive_start = if call.blockTransition.prevBlockHash.is_zero() {
                    0
                } else {
                    prev_or_zero.saturating_add(1)
                };
                self.fetch_batch_aggregates(inclusive_start, end).await?
            }
            _ => BatchAggregates::default(),
        };

        let summary = map_batch_summary(
            &event,
            &call,
            settlement_tx_hash,
            settled_at,
            zone_block_from_number,
            zone_block_to_number,
            sealed_at,
            aggregates,
        );
        self.cache_batch_summary(event.withdrawalBatchIndex, summary.clone());
        Ok(summary)
    }

    /// Fetch darkpool `OrderSubmitted` / `OrderFilled` logs covering the
    /// inclusive zone-block range `[from, to]` (plus any `OrderSubmitted`
    /// emitted in earlier blocks, so taker fills against orders placed in a
    /// prior batch still resolve to the correct trading pair) and reduce them
    /// to the public, aggregate-only [`BatchAggregates`].
    async fn fetch_batch_aggregates(
        &self,
        from: u64,
        to: u64,
    ) -> Result<BatchAggregates, JsonRpcError> {
        if to < from {
            return Ok(BatchAggregates::default());
        }
        let filter = Filter::new()
            .address(DARKPOOL_ADDRESS)
            .from_block(0)
            .to_block(to)
            .event_signature(vec![
                zone_darkpool::OrderSubmitted::SIGNATURE_HASH,
                zone_darkpool::OrderFilled::SIGNATURE_HASH,
            ]);
        let logs = self
            .zone_provider
            .get_logs(&filter)
            .await
            .map_err(internal)?;
        Ok(aggregate_batch_events(&logs, (from, to)))
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

        let logs = self
            .fetch_batch_logs_by_topics(&[batch_number_topic(withdrawal_batch_index)])
            .await?;

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
            if log.topics().first().copied()
                != Some(ZonePortal::WithdrawalBounceBack::SIGNATURE_HASH)
            {
                continue;
            }
            let event = ZonePortal::WithdrawalBounceBack::decode_log(&log.inner)
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

    async fn enforce_authorized(
        &self,
        request: &mut TempoTransactionRequest,
        auth: &AuthContext,
    ) -> Result<(), JsonRpcError> {
        let caller = auth.caller;
        zone_rpc::policy::enforce_authorized(request, auth, async {
            Ok(self.zone_sequencer().await? == caller)
        })
        .await
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
                ZoneInbox::DepositFailed::SIGNATURE_HASH,
                ZoneInbox::EncryptedDepositProcessed::SIGNATURE_HASH,
                ZoneInbox::EncryptedDepositFailed::SIGNATURE_HASH,
                ZoneInbox::DepositRejected::SIGNATURE_HASH,
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

        if signature == ZoneInbox::DepositFailed::SIGNATURE_HASH {
            ZoneInbox::DepositFailed::decode_log(&log.inner).map_err(internal)?;
            return Ok(Some(TerminalDepositEvent::RegularFailed));
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

        if signature == ZoneInbox::DepositRejected::SIGNATURE_HASH {
            let event = ZoneInbox::DepositRejected::decode_log(&log.inner).map_err(internal)?;
            return match event.depositType {
                DepositType::Regular => Ok(Some(TerminalDepositEvent::RegularRejected)),
                DepositType::Encrypted => Ok(Some(TerminalDepositEvent::EncryptedRejected)),
                _ => Ok(None),
            };
        }

        Ok(None)
    }
}

impl<Api> zone_rpc::ZoneRpcApi for ZoneRpc<Api>
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

    fn syncing(&self) -> BoxFut<'_> {
        Box::pin(async move {
            let status = EthApiSpec::sync_status(&self.eth.api).map_err(internal)?;
            to_raw(&status)
        })
    }

    fn coinbase(&self) -> BoxFut<'_> {
        Box::pin(async move { to_raw(&self.zone_sequencer().await?) })
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
            let mut history =
                EthFees::fee_history(&self.eth.api, block_count, newest_block, reward_percentiles)
                    .await
                    .map_err(internal)?;
            // Redact gas fields (like `gas_used_ratio`) that can be used to guess tx counts
            redact_fee_history(&mut history);
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

            let Some(mut tx) = tx else {
                return Ok(raw_null());
            };

            if tx.from() != auth.caller {
                return Ok(raw_null());
            }

            // transaction_index leaks how many txns were in this block, so redact
            tx.transaction_index = Some(0);

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

            self.enforce_authorized(&mut request, &auth).await?;

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

            self.enforce_authorized(&mut request, &auth).await?;

            let result = EthCall::estimate_gas_at(
                &self.eth.api,
                request,
                block.unwrap_or_default(),
                EvmOverrides::state(state_override),
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

            let mut receipt = EthTransactions::send_raw_transaction_sync(&self.eth.api, data, None)
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
            self.enforce_authorized(&mut request, &auth).await?;

            // Prefill the users request so the `fill_transaction` doesnt leak dynamic fee estimates via
            // missing fee fields.
            apply_public_fee_policy(&mut request);

            let result = EthTransactions::fill_transaction(&self.eth.api, request)
                .await
                .map_err(internal)?;
            to_raw(&result)
        })
    }

    fn get_logs(&self, mut filter: Filter, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            let zone_tokens = self.zone_tokens().await?;
            zone_rpc::filter::scope_filter_addresses(&mut filter, &zone_tokens)?;
            zone_rpc::filter::scope_filter_for_caller(&mut filter, &auth.caller)?;
            let logs = EthFilterApiServer::logs(&self.eth.filter, filter)
                .await
                .map_err(internal)?;
            let filtered = zone_rpc::filter::filter_logs(logs, &auth.caller);
            to_raw(&filtered)
        })
    }

    fn new_filter(&self, mut filter: Filter, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            let zone_tokens = self.zone_tokens().await?;
            zone_rpc::filter::scope_filter_addresses(&mut filter, &zone_tokens)?;
            zone_rpc::filter::scope_filter_for_caller(&mut filter, &auth.caller)?;
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
                    redact_header(&mut header);
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

            let zone_tokens = self.zone_tokens().await?;
            zone_rpc::filter::scope_filter_addresses(&mut filter, &zone_tokens)?;
            zone_rpc::filter::scope_filter_for_caller(&mut filter, &caller)?;

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

            // Renumber `log_index` per-transaction so a log seen live over the
            // subscription carries the same `(transactionHash, logIndex)` it would
            // via `eth_getLogs`/`eth_getTransactionReceipt`.
            // Logs arrive in block order grouped by tx, which is what `LogOrderingRedactor` needs.
            let mut log_redactor = zone_rpc::filter::LogOrderingRedactor::default();
            let stream = stream.filter_map(move |log| {
                std::future::ready(
                    zone_rpc::filter::is_log_visible(&log, &caller)
                        .then(|| to_raw(&log_redactor.redact(log))),
                )
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
            let sequencer = self.zone_sequencer().await?;
            to_raw(&ZoneInfoResponse {
                zone_id: U64::from(self.config.zone_id),
                zone_tokens,
                sequencer,
                chain_id: U64::from(self.config.chain_id),
            })
        })
    }

    fn zone_list_batches(&self, params: ListBatchesParams, _auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            // The public L1 endpoint rejects connection bursts by source IP.
            // Queue explorer pages so concurrent callers cannot multiply the
            // serial hydration work performed below.
            let _query_guard = self.batch_query_lock.lock().await;
            let limit = params
                .limit
                .unwrap_or(LIST_BATCHES_DEFAULT_LIMIT)
                .min(LIST_BATCHES_MAX_LIMIT)
                .max(1);

            let latest = self.latest_batch_number().await?;
            let mut batches = Vec::new();
            let include_pending = params.cursor.is_none();
            if include_pending && let Some(pending) = self.pending_batch_summary(latest).await? {
                batches.push(pending);
            }

            let remaining_limit = limit.saturating_sub(batches.len() as u32);
            if latest == 0 || remaining_limit == 0 {
                let next_cursor = if latest > 0 && remaining_limit == 0 {
                    Some(U64::from(latest.saturating_add(1)))
                } else {
                    None
                };
                return to_raw(&BatchListResponse {
                    batches,
                    next_cursor,
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

            let start = end
                .saturating_sub((remaining_limit as u64).saturating_sub(1))
                .max(1);
            let expected_count = end.saturating_sub(start).saturating_add(1) as usize;
            let mut submitted_batches = (start..=end)
                .filter_map(|batch_number| self.cached_batch_summary(batch_number))
                .collect::<Vec<_>>();
            if submitted_batches.len() != expected_count {
                submitted_batches.clear();
                for log in self.fetch_batch_logs_in_range(start, end).await? {
                    submitted_batches.push(self.build_batch_summary(log).await?);
                }
            }
            submitted_batches.sort_by(|a, b| b.batch_number.cmp(&a.batch_number));
            batches.extend(submitted_batches);

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
            if let Some(summary) = self.cached_batch_summary(batch_number) {
                return to_raw(&summary);
            }
            let _query_guard = self.batch_query_lock.lock().await;
            if let Some(summary) = self.cached_batch_summary(batch_number) {
                return to_raw(&summary);
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
                    if let Some(summary) = self.cached_batch_summary(batch_number) {
                        return to_raw(&summary);
                    }
                    let _query_guard = self.batch_query_lock.lock().await;
                    if let Some(summary) = self.cached_batch_summary(batch_number) {
                        return to_raw(&summary);
                    }
                    let log = match self.fetch_batch_log(batch_number).await? {
                        Some(log) => log,
                        None => return Ok(raw_null()),
                    };
                    let summary = self.build_batch_summary(log).await?;
                    to_raw(&summary)
                }
                BatchQuery::SettlementTxHash(tx_hash) => {
                    let _query_guard = self.batch_query_lock.lock().await;
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
        Box::pin(async move { to_raw(&self.darkpool_market_config().await?) })
    }

    fn zone_get_reference_price(
        &self,
        base: Address,
        quote: Address,
        _auth: AuthContext,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            self.ensure_darkpool_market(base, quote).await?;
            let pair = self.market_label(base, quote).await?;

            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let as_of_block = self
                .zone_provider
                .get_block_number()
                .await
                .map_err(internal)?;
            let response = build_reference_price_response(
                self.config.ref_price_provider.as_ref(),
                self.ref_price_loaded_at,
                now_secs,
                as_of_block,
                pair,
                base,
                quote,
            );
            to_raw(&response)
        })
    }

    fn zone_get_top_of_book(
        &self,
        base: Address,
        quote: Address,
        _auth: AuthContext,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            self.ensure_darkpool_market(base, quote).await?;
            let pair = self.market_label(base, quote).await?;

            let darkpool = DarkpoolReader::new(DARKPOOL_ADDRESS, &self.zone_provider);
            let best_bid = darkpool
                .bestBid(base, quote)
                .call()
                .await
                .map_err(internal)?;
            let best_ask = darkpool
                .bestAsk(base, quote)
                .call()
                .await
                .map_err(internal)?;
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
                pair,
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
        limit: u32,
        cursor: Option<String>,
        _auth: AuthContext,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            self.ensure_darkpool_market(base, quote).await?;
            let pair = self.market_label(base, quote).await?;

            let interval_secs = interval_seconds(&interval).ok_or_else(|| {
                JsonRpcError::invalid_params(format!(
                    "unsupported interval `{interval}`; expected one of: {}",
                    SUPPORTED_INTERVALS.join(", "),
                ))
            })?;

            let cursor_ts = parse_midpoint_cursor(cursor.as_deref())?;

            let history = self.midpoint_histories.read().get(&(base, quote)).cloned();
            let (page, next_cursor) = history
                .as_deref()
                .map(|history| history.query(interval_secs, limit, cursor_ts))
                .unwrap_or_default();

            let samples = page
                .into_iter()
                .map(|s| MidpointSample {
                    timestamp: U64::from(s.bucket_end),
                    midpoint: U128::from(s.midpoint),
                })
                .collect();

            to_raw(&build_midpoint_history_response(
                pair,
                base,
                quote,
                interval,
                samples,
                next_cursor,
            ))
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
                        bounceback_recipient,
                        token,
                        amount,
                        memo,
                    } => {
                        if sender != auth.caller
                            && recipient != auth.caller
                            && bounceback_recipient != auth.caller
                        {
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
                        bounceback_recipient,
                        token,
                        amount,
                    } => {
                        let terminal = self.terminal_event_for_deposit(deposit_hash).await?;

                        let include = match (
                            &terminal,
                            sender == auth.caller || bounceback_recipient == auth.caller,
                        ) {
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

fn insert_batch_summary(
    cache: &mut BTreeMap<u64, BatchSummary>,
    batch_number: u64,
    summary: BatchSummary,
) {
    cache.insert(batch_number, summary);
    while cache.len() > BATCH_SUMMARY_CACHE_CAPACITY {
        cache.pop_first();
    }
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

/// Split `[first, last]` into non-overlapping inclusive ranges ordered newest
/// first. Every range contains at most `max_blocks` blocks.
fn reverse_inclusive_block_ranges(first: u64, last: u64, max_blocks: u64) -> Vec<(u64, u64)> {
    if last < first || max_blocks == 0 {
        return Vec::new();
    }

    let mut ranges = Vec::new();
    let mut end = last;
    loop {
        let start = end.saturating_sub(max_blocks.saturating_sub(1)).max(first);
        ranges.push((start, end));
        if start == first {
            break;
        }
        end = start - 1;
    }
    ranges
}

/// Pure mapping: `BatchSubmitted` event + decoded `submitBatch` calldata +
/// timing data + precomputed darkpool aggregates → aggregate-only
/// [`BatchSummary`].
fn map_batch_summary(
    event: &ZonePortal::BatchSubmitted,
    call: &ZonePortal::submitBatchCall,
    settlement_tx_hash: B256,
    settled_at: Option<u64>,
    zone_block_from: Option<u64>,
    zone_block_to: Option<u64>,
    sealed_at: Option<u64>,
    aggregates: BatchAggregates,
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
        order_count: U64::from(aggregates.order_count),
        fill_count: U64::from(aggregates.fill_count),
        aggregate_pairs: aggregates.pair_labels,
        aggregate_volume: aggregates.volume_by_token,
        settlement_tx_hash: Some(settlement_tx_hash),
        proof_ref: None,
    }
}

/// Pure mapping for the local zone blocks that have been produced but have not
/// landed in a `BatchSubmitted` L1 event yet.
fn map_pending_batch_summary(
    batch_number: u64,
    zone_block_from: Option<u64>,
    zone_block_to: u64,
    tempo_block_number: u64,
    prev_block_hash: B256,
    next_block_hash: B256,
    aggregates: BatchAggregates,
) -> BatchSummary {
    BatchSummary {
        batch_number: U64::from(batch_number),
        zone_block_from: zone_block_from.map(U64::from),
        zone_block_to: Some(U64::from(zone_block_to)),
        tempo_block_number: U64::from(tempo_block_number),
        root: B256::ZERO,
        prev_block_hash,
        next_block_hash,
        status: BatchStatus::Pending,
        sealed_at: None,
        settled_at: None,
        order_count: U64::from(aggregates.order_count),
        fill_count: U64::from(aggregates.fill_count),
        aggregate_pairs: aggregates.pair_labels,
        aggregate_volume: aggregates.volume_by_token,
        settlement_tx_hash: None,
        proof_ref: None,
    }
}

/// Aggregate-only darkpool statistics for a sequencer batch.
///
/// Constructed by [`aggregate_batch_events`] from the raw `OrderSubmitted` /
/// `OrderFilled` logs covering the batch's zone-block range. No owner-,
/// order-, or fill-id-linked fields are retained.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct BatchAggregates {
    order_count: u64,
    fill_count: u64,
    pair_labels: Vec<String>,
    volume_by_token: Vec<BatchAggregateVolume>,
}

/// Stable address-based label for `(base, quote)` used by the synchronous
/// batch-event reducer. Market RPCs resolve human-readable token symbols from
/// on-chain TIP-20 metadata instead.
fn pair_label(base: Address, quote: Address) -> String {
    format!("{base:#x}/{quote:#x}")
}

/// Pure reduction: a slice of darkpool logs spanning `[0, block_range.1]` →
/// public aggregate counts and per-token settled volume for the inclusive
/// range `block_range`.
///
/// `OrderSubmitted` logs are scanned across the full slice to build an
/// order-id → pair index, so an `OrderFilled` in `block_range` whose maker
/// placed the order in an earlier batch still attributes its volume to the
/// right pair and tokens.
///
/// **Privacy:** the returned aggregates never include maker, taker, owner,
/// order id, or fill id fields — only counts, pair labels, and per-token
/// totals.
fn aggregate_batch_events(
    darkpool_logs: &[alloy_rpc_types_eth::Log],
    block_range: (u64, u64),
) -> BatchAggregates {
    use std::collections::{BTreeMap, BTreeSet};

    let (from, to) = block_range;
    if to < from {
        return BatchAggregates::default();
    }

    let mut pair_by_order_id: BTreeMap<u128, (Address, Address)> = BTreeMap::new();
    for log in darkpool_logs {
        if log.topic0().copied() != Some(zone_darkpool::OrderSubmitted::SIGNATURE_HASH) {
            continue;
        }
        let Some(block) = log.block_number else {
            continue;
        };
        if block > to {
            continue;
        }
        if let Ok(decoded) = zone_darkpool::OrderSubmitted::decode_log(&log.inner) {
            pair_by_order_id.insert(decoded.orderId, (decoded.base, decoded.quote));
        }
    }

    let mut order_count: u64 = 0;
    let mut fill_count: u64 = 0;
    let mut pair_set: BTreeSet<(Address, Address)> = BTreeSet::new();
    let mut volume_by_token: BTreeMap<Address, U256> = BTreeMap::new();

    for log in darkpool_logs {
        let Some(block) = log.block_number else {
            continue;
        };
        if block < from || block > to {
            continue;
        }
        let Some(topic0) = log.topic0().copied() else {
            continue;
        };

        if topic0 == zone_darkpool::OrderSubmitted::SIGNATURE_HASH {
            order_count = order_count.saturating_add(1);
            if let Ok(decoded) = zone_darkpool::OrderSubmitted::decode_log(&log.inner) {
                pair_set.insert((decoded.base, decoded.quote));
            }
        } else if topic0 == zone_darkpool::OrderFilled::SIGNATURE_HASH {
            fill_count = fill_count.saturating_add(1);
            if let Ok(decoded) = zone_darkpool::OrderFilled::decode_log(&log.inner)
                && let Some(&(base, quote)) = pair_by_order_id.get(&decoded.orderId)
            {
                pair_set.insert((base, quote));
                let base_amount = U256::from(decoded.amountFilled);
                // `quote = baseAmount * price` per the darkpool price model.
                let quote_amount = base_amount.saturating_mul(U256::from(decoded.price));
                volume_by_token
                    .entry(base)
                    .and_modify(|v| *v = v.saturating_add(base_amount))
                    .or_insert(base_amount);
                volume_by_token
                    .entry(quote)
                    .and_modify(|v| *v = v.saturating_add(quote_amount))
                    .or_insert(quote_amount);
            }
        }
    }

    let mut pair_labels: Vec<String> = pair_set
        .iter()
        .map(|&(base, quote)| pair_label(base, quote))
        .collect();
    pair_labels.sort();
    pair_labels.dedup();

    let volume_by_token = volume_by_token
        .into_iter()
        .map(|(token, amount)| BatchAggregateVolume { token, amount })
        .collect();

    BatchAggregates {
        order_count,
        fill_count,
        pair_labels,
        volume_by_token,
    }
}

#[derive(Debug, Clone)]
enum PortalDepositRecord {
    Regular {
        deposit_hash: B256,
        sender: Address,
        recipient: Address,
        bounceback_recipient: Address,
        token: Address,
        amount: u128,
        memo: B256,
    },
    Encrypted {
        deposit_hash: B256,
        sender: Address,
        bounceback_recipient: Address,
        token: Address,
        amount: u128,
    },
}

#[derive(Debug, Clone)]
enum TerminalDepositEvent {
    RegularProcessed,
    RegularFailed,
    RegularRejected,
    EncryptedProcessed { recipient: Address, memo: B256 },
    EncryptedFailed,
    EncryptedRejected,
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
        Some(TerminalDepositEvent::RegularFailed | TerminalDepositEvent::RegularRejected) => {
            Ok(DepositState::Failed)
        }
        Some(TerminalDepositEvent::EncryptedProcessed { .. }) => Err(JsonRpcError::internal(
            "encrypted deposit event matched regular deposit hash",
        )),
        Some(TerminalDepositEvent::EncryptedFailed | TerminalDepositEvent::EncryptedRejected) => {
            Err(JsonRpcError::internal(
                "encrypted deposit failure matched regular deposit hash",
            ))
        }
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
        Some(TerminalDepositEvent::EncryptedFailed | TerminalDepositEvent::EncryptedRejected) => {
            Ok((None, None, DepositState::Failed))
        }
        Some(
            TerminalDepositEvent::RegularProcessed
            | TerminalDepositEvent::RegularFailed
            | TerminalDepositEvent::RegularRejected,
        ) => Err(JsonRpcError::internal(
            "regular deposit event matched encrypted deposit hash",
        )),
        None => Ok((None, None, DepositState::Pending)),
    }
}

/// Clear RPC header fields that reveal private execution state from the header
fn redact_header(header: &mut TempoHeaderResponse) {
    header.inner.size = header.inner.size.map(|_| U256::ZERO);
    let inner = &mut header.inner.inner.inner;
    inner.gas_used = 0;
    inner.logs_bloom = Bloom::ZERO;
    inner.blob_gas_used = inner.blob_gas_used.map(|_| 0);
    inner.excess_blob_gas = inner.excess_blob_gas.map(|_| 0);
}

/// Clear gas related fields that leak the size (and therefore tx counts)
fn redact_fee_history(history: &mut FeeHistory) {
    history.base_fee_per_gas.fill(u128::from(TEMPO_T0_BASE_FEE));
    history.gas_used_ratio.fill(0.0);
    history.base_fee_per_blob_gas.fill(0);
    history.blob_gas_used_ratio.fill(0.0);
    if let Some(rewards) = &mut history.reward {
        for block_rewards in rewards {
            block_rewards.fill(0);
        }
    }
}

/// Prefill missing transaction fee fields with public, deterministic values before calling reth's
/// transaction filler, so `eth_fillTransaction` does not expose dynamic fee estimates derived from
/// private zone activity.
fn apply_public_fee_policy(request: &mut TempoTransactionRequest) {
    if request.inner.has_eip4844_fields() && request.inner.max_fee_per_blob_gas.is_none() {
        request.inner.max_fee_per_blob_gas = Some(0);
    }

    if request.gas_price().is_some() {
        return;
    }

    if matches!(request.inner.transaction_type, Some(0 | 1)) {
        request.set_gas_price(u128::from(TEMPO_T0_BASE_FEE));
        return;
    }

    let priority_fee = request.max_priority_fee_per_gas().unwrap_or(0);
    if request.max_priority_fee_per_gas().is_none() {
        request.set_max_priority_fee_per_gas(0);
    }
    if request.max_fee_per_gas().is_none() {
        request.set_max_fee_per_gas(u128::from(TEMPO_T0_BASE_FEE) + priority_fee);
    }
}

/// Strip privacy-sensitive fields from a block for non-sequencer callers.
fn redact_block(block: &mut RpcBlock) {
    redact_header(&mut block.header);
    block.transactions = BlockTransactions::Hashes(Vec::new());
    block.withdrawals = block.withdrawals.take().map(|_| Default::default());
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

fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn parse_midpoint_cursor(cursor: Option<&str>) -> Result<Option<u64>, JsonRpcError> {
    match cursor {
        None => Ok(None),
        Some(raw) => U64::from_str(raw)
            .map(|u| Some(u.to::<u64>()))
            .map_err(|_| JsonRpcError::invalid_params("cursor must be a hex quantity")),
    }
}

fn build_midpoint_history_response(
    pair: String,
    base: Address,
    quote: Address,
    interval: String,
    samples: Vec<MidpointSample>,
    next_cursor: Option<u64>,
) -> MidpointHistoryResponse {
    MidpointHistoryResponse {
        pair,
        base,
        quote,
        interval,
        samples,
        next_cursor: next_cursor.map(|ts| format!("0x{ts:x}")),
        history: HistoryAvailability {
            enabled: true,
            reason: format!(
                "in-process midpoint sampler online; polls top-of-book every {}s, retains \
                 ~{} raw samples",
                MIDPOINT_SAMPLE_INTERVAL.as_secs(),
                MIDPOINT_RETENTION,
            ),
        },
    }
}

/// Build the `zone_getReferencePrice` response payload from the configured
/// provider (if any) and the current zone clock.
///
/// Pure function so the response shape is unit-testable without spinning up
/// reth providers. For a static provider, `as_of_block` is 0 (the value does
/// not anchor to a specific zone block) and `as_of_timestamp` is the unix
/// second at which the snapshot was loaded into the node, so freshness ages
/// linearly with node uptime.
fn build_reference_price_response(
    provider: Option<&zone_refprice::ReferencePriceProviderConfig>,
    loaded_at_secs: u64,
    now_secs: u64,
    _as_of_block: u64,
    pair: String,
    base: Address,
    quote: Address,
) -> ReferencePriceResponse {
    let Some(provider) = provider else {
        return ReferencePriceResponse {
            enabled: false,
            pair,
            base,
            quote,
            price: None,
            source: None,
            as_of_block: None,
            as_of_timestamp: None,
            fresh: None,
            age_secs: None,
            max_deviation_bps: None,
            max_staleness_secs: None,
            price_unit: REFERENCE_PRICE_UNIT.to_string(),
            disclaimer: REFERENCE_PRICE_DISCLAIMER.to_string(),
            reason: Some("reference-price provider not configured".to_string()),
        };
    };

    let (price, source) = match &provider.kind {
        zone_refprice::ReferencePriceProviderKind::Static { price, source } => {
            (*price, source.clone())
        }
    };

    let snapshot = ReferencePrice {
        price,
        source: source.clone(),
        as_of_block: 0,
        as_of_timestamp: loaded_at_secs,
    };
    let guard = ReferencePriceGuard {
        max_deviation_bps: provider.max_deviation_bps,
        max_staleness_secs: provider.max_staleness_secs,
    };
    let fresh = guard.is_fresh(&snapshot, now_secs);
    let age = ReferencePriceGuard::age_secs(&snapshot, now_secs);

    ReferencePriceResponse {
        enabled: true,
        pair,
        base,
        quote,
        price: Some(U128::from(price)),
        source: Some(source),
        as_of_block: Some(U64::from(0u64)),
        as_of_timestamp: Some(U64::from(loaded_at_secs)),
        fresh: Some(fresh),
        age_secs: Some(U64::from(age)),
        max_deviation_bps: Some(provider.max_deviation_bps),
        max_staleness_secs: Some(provider.max_staleness_secs),
        price_unit: REFERENCE_PRICE_UNIT.to_string(),
        disclaimer: REFERENCE_PRICE_DISCLAIMER.to_string(),
        reason: None,
    }
}

pub(crate) fn rpc_connection_config(retry_connection_interval: Duration) -> ConnectionConfig {
    ConnectionConfig::new()
        .with_max_retries(u32::MAX)
        .with_retry_interval(retry_connection_interval)
}

/// Return the HTTP(S) endpoint used by private-RPC L1 reads.
///
/// The zone's main L1 subscriber still uses the configured WS(S) endpoint for
/// new-head subscriptions. Explorer and status methods only perform request /
/// response reads, so keeping them off the pubsub transport prevents a broken
/// websocket reconnect loop from stalling authenticated RPC requests.
fn l1_read_rpc_url(l1_rpc_url: &str) -> eyre::Result<url::Url> {
    let mut url: url::Url = l1_rpc_url.parse().wrap_err("invalid private RPC L1 URL")?;
    let http_scheme = match url.scheme() {
        "http" | "https" => return Ok(url),
        "ws" => "http",
        "wss" => "https",
        scheme => eyre::bail!("unsupported private RPC L1 URL scheme `{scheme}`"),
    };
    url.set_scheme(http_scheme)
        .map_err(|_| eyre::eyre!("failed to set private RPC L1 URL scheme"))?;
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l1_read_rpc_url_uses_http_for_request_response_reads() {
        assert_eq!(
            l1_read_rpc_url("wss://rpc.example.test/rpc?key=value")
                .unwrap()
                .as_str(),
            "https://rpc.example.test/rpc?key=value",
        );
        assert_eq!(
            l1_read_rpc_url("ws://127.0.0.1:8545").unwrap().as_str(),
            "http://127.0.0.1:8545/",
        );
    }

    #[test]
    fn l1_read_rpc_url_preserves_existing_http_endpoints() {
        assert_eq!(
            l1_read_rpc_url("https://rpc.example.test")
                .unwrap()
                .as_str(),
            "https://rpc.example.test/",
        );
        assert_eq!(
            l1_read_rpc_url("http://127.0.0.1:8545").unwrap().as_str(),
            "http://127.0.0.1:8545/",
        );
    }

    #[test]
    fn l1_read_rpc_url_rejects_unsupported_or_invalid_urls() {
        assert!(l1_read_rpc_url("ftp://rpc.example.test").is_err());
        assert!(l1_read_rpc_url("not a URL").is_err());
    }

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
            withdrawalQueueIndex: U256::from(batch),
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
    fn batch_log_ranges_are_bounded_newest_first_without_gaps() {
        assert_eq!(
            reverse_inclusive_block_ranges(100, 349, 100),
            vec![(250, 349), (150, 249), (100, 149)]
        );
    }

    #[test]
    fn batch_log_ranges_handle_short_empty_and_invalid_spans() {
        assert_eq!(
            reverse_inclusive_block_ranges(24798757, 24799061, 50_000),
            vec![(24798757, 24799061)]
        );
        assert!(reverse_inclusive_block_ranges(2, 1, 50_000).is_empty());
        assert!(reverse_inclusive_block_ranges(1, 2, 0).is_empty());
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
            BatchAggregates::default(),
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
        assert_eq!(summary.settlement_tx_hash, Some(settlement_tx));
        assert_eq!(summary.proof_ref, None);
    }

    #[test]
    fn map_batch_summary_with_default_aggregates_is_zeroed_and_owner_free() {
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
            BatchAggregates::default(),
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
            "maker",
            "taker",
            "account",
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
    fn map_batch_summary_propagates_aggregates_without_leaking_owners() {
        let event = sample_batch_event(99);
        let call = sample_batch_call(2_000);
        let aggregates = BatchAggregates {
            order_count: 3,
            fill_count: 2,
            pair_labels: vec![test_market::PAIR_LABEL.to_string()],
            volume_by_token: vec![
                BatchAggregateVolume {
                    token: test_market::BASE,
                    amount: U256::from(500u64),
                },
                BatchAggregateVolume {
                    token: test_market::QUOTE,
                    amount: U256::from(2_500u64),
                },
            ],
        };
        let summary = map_batch_summary(
            &event,
            &call,
            B256::repeat_byte(0x77),
            Some(10),
            Some(0),
            Some(99),
            Some(20),
            aggregates,
        );

        assert_eq!(summary.order_count, U64::from(3));
        assert_eq!(summary.fill_count, U64::from(2));
        assert_eq!(
            summary.aggregate_pairs,
            vec![test_market::PAIR_LABEL.to_string()]
        );
        assert_eq!(summary.aggregate_volume.len(), 2);

        let json = serde_json::to_value(&summary).expect("summary should serialize");
        let obj = json.as_object().expect("summary must be a JSON object");
        for forbidden in [
            "maker",
            "taker",
            "account",
            "owner",
            "orderId",
            "fillId",
            "counterparty",
            "trader",
            "userAddress",
            "sender",
            "recipient",
        ] {
            assert!(
                !obj.contains_key(forbidden),
                "batch summary leaked owner-linked field `{forbidden}`",
            );
        }
    }

    #[test]
    fn map_pending_batch_summary_omits_l1_settlement_fields() {
        let aggregates = BatchAggregates {
            order_count: 2,
            fill_count: 1,
            pair_labels: vec![test_market::PAIR_LABEL.to_string()],
            volume_by_token: vec![BatchAggregateVolume {
                token: test_market::BASE,
                amount: U256::from(500u64),
            }],
        };

        let summary = map_pending_batch_summary(
            43,
            Some(101),
            150,
            2_500,
            B256::repeat_byte(0x11),
            B256::repeat_byte(0x22),
            aggregates,
        );

        assert_eq!(summary.batch_number, U64::from(43));
        assert_eq!(summary.zone_block_from, Some(U64::from(101)));
        assert_eq!(summary.zone_block_to, Some(U64::from(150)));
        assert_eq!(summary.tempo_block_number, U64::from(2_500));
        assert_eq!(summary.root, B256::ZERO);
        assert_eq!(summary.prev_block_hash, B256::repeat_byte(0x11));
        assert_eq!(summary.next_block_hash, B256::repeat_byte(0x22));
        assert_eq!(summary.status, BatchStatus::Pending);
        assert_eq!(summary.sealed_at, None);
        assert_eq!(summary.settled_at, None);
        assert_eq!(summary.order_count, U64::from(2));
        assert_eq!(summary.fill_count, U64::from(1));
        assert_eq!(
            summary.aggregate_pairs,
            vec![test_market::PAIR_LABEL.to_string()]
        );
        assert_eq!(summary.aggregate_volume.len(), 1);
        assert_eq!(summary.settlement_tx_hash, None);
        assert_eq!(summary.proof_ref, None);

        let json = serde_json::to_value(&summary).expect("summary should serialize");
        let obj = json.as_object().expect("summary must be a JSON object");
        assert!(!obj.contains_key("settlementTxHash"));
        assert!(!obj.contains_key("proofRef"));
    }

    #[test]
    fn batch_summary_cache_keeps_only_the_newest_submitted_batches() {
        let mut cache = BTreeMap::new();
        for batch_number in 1..=(BATCH_SUMMARY_CACHE_CAPACITY as u64 + 1) {
            let summary = map_pending_batch_summary(
                batch_number,
                None,
                batch_number,
                batch_number,
                B256::ZERO,
                B256::repeat_byte(0x11),
                BatchAggregates::default(),
            );
            insert_batch_summary(&mut cache, batch_number, summary);
        }

        assert_eq!(cache.len(), BATCH_SUMMARY_CACHE_CAPACITY);
        assert!(!cache.contains_key(&1));
        assert!(cache.contains_key(&2));
        assert!(cache.contains_key(&(BATCH_SUMMARY_CACHE_CAPACITY as u64 + 1)));
    }

    /// Build a darkpool [`alloy_rpc_types_eth::Log`] for an `OrderSubmitted`
    /// event at `block` with the test pair, used to seed the pair index in
    /// aggregation tests.
    fn alpha_submitted_log(
        block: u64,
        order_id: u128,
        amount: u128,
        price: u128,
    ) -> alloy_rpc_types_eth::Log {
        let event = zone_darkpool::OrderSubmitted {
            orderId: order_id,
            maker: Address::repeat_byte(0xaa),
            base: test_market::BASE,
            quote: test_market::QUOTE,
            amount,
            price,
            isBid: true,
        };
        wrap_log(DARKPOOL_ADDRESS, event.encode_log_data(), block)
    }

    fn alpha_filled_log(
        block: u64,
        order_id: u128,
        amount: u128,
        price: u128,
    ) -> alloy_rpc_types_eth::Log {
        let event = zone_darkpool::OrderFilled {
            orderId: order_id,
            maker: Address::repeat_byte(0xaa),
            taker: Address::repeat_byte(0xbb),
            amountFilled: amount,
            price,
        };
        wrap_log(DARKPOOL_ADDRESS, event.encode_log_data(), block)
    }

    fn wrap_log(
        address: Address,
        data: alloy_primitives::LogData,
        block: u64,
    ) -> alloy_rpc_types_eth::Log {
        alloy_rpc_types_eth::Log {
            inner: alloy_primitives::Log { address, data },
            block_hash: None,
            block_number: Some(block),
            block_timestamp: None,
            transaction_hash: None,
            transaction_index: None,
            log_index: None,
            removed: false,
        }
    }

    #[test]
    fn aggregate_batch_events_counts_orders_and_fills_in_range() {
        let logs = vec![
            alpha_submitted_log(10, 1, 1_000, 5),
            alpha_filled_log(10, 1, 400, 5),
            alpha_submitted_log(11, 2, 800, 6),
            alpha_filled_log(11, 2, 800, 6),
        ];
        let agg = aggregate_batch_events(&logs, (10, 11));

        assert_eq!(agg.order_count, 2, "two OrderSubmitted events in range");
        assert_eq!(agg.fill_count, 2, "two OrderFilled events in range");
        assert_eq!(agg.pair_labels, vec![test_market::PAIR_LABEL.to_string()]);
    }

    #[test]
    fn aggregate_batch_events_includes_address_pair_when_traded() {
        let logs = vec![
            alpha_submitted_log(5, 1, 1_000, 7),
            alpha_filled_log(5, 1, 1_000, 7),
        ];
        let agg = aggregate_batch_events(&logs, (5, 5));
        assert_eq!(agg.pair_labels, vec![test_market::PAIR_LABEL.to_string()]);
    }

    #[test]
    fn aggregate_batch_events_aggregates_volume_per_token() {
        // Two fills on the test pair: 400 base @ price 5 → +2_000 quote;
        // 800 base @ price 6 → +4_800 quote. Totals: base 1_200, quote 6_800.
        let logs = vec![
            alpha_submitted_log(10, 1, 1_000, 5),
            alpha_filled_log(10, 1, 400, 5),
            alpha_submitted_log(11, 2, 800, 6),
            alpha_filled_log(11, 2, 800, 6),
        ];
        let agg = aggregate_batch_events(&logs, (10, 11));

        let base_volume = agg
            .volume_by_token
            .iter()
            .find(|v| v.token == test_market::BASE)
            .expect("base volume present");
        let quote_volume = agg
            .volume_by_token
            .iter()
            .find(|v| v.token == test_market::QUOTE)
            .expect("quote volume present");
        assert_eq!(base_volume.amount, U256::from(1_200u64));
        assert_eq!(quote_volume.amount, U256::from(6_800u64));
    }

    #[test]
    fn aggregate_batch_events_uses_earlier_order_submitted_for_pair_lookup() {
        // OrderSubmitted lives in block 5; OrderFilled hits in block 12 — the
        // pair index must still resolve the fill to the test pair even
        // though the submission is outside the [from, to] range.
        let logs = vec![
            alpha_submitted_log(5, 42, 1_000, 4),
            alpha_filled_log(12, 42, 1_000, 4),
        ];
        let agg = aggregate_batch_events(&logs, (10, 15));

        assert_eq!(agg.order_count, 0, "submission is outside [from, to]");
        assert_eq!(agg.fill_count, 1, "fill is inside [from, to]");
        assert_eq!(agg.pair_labels, vec![test_market::PAIR_LABEL.to_string()]);
        let base_volume = agg
            .volume_by_token
            .iter()
            .find(|v| v.token == test_market::BASE)
            .expect("base volume present");
        assert_eq!(base_volume.amount, U256::from(1_000u64));
    }

    #[test]
    fn aggregate_batch_events_skips_events_outside_range() {
        let logs = vec![
            alpha_submitted_log(1, 1, 1_000, 5),
            alpha_filled_log(1, 1, 1_000, 5),
            alpha_submitted_log(100, 2, 500, 3),
            alpha_filled_log(100, 2, 500, 3),
        ];
        let agg = aggregate_batch_events(&logs, (10, 20));

        assert_eq!(agg.order_count, 0);
        assert_eq!(agg.fill_count, 0);
        assert!(agg.pair_labels.is_empty());
        assert!(agg.volume_by_token.is_empty());
    }

    #[test]
    fn aggregate_batch_events_returns_default_for_inverted_range() {
        let logs = vec![alpha_submitted_log(10, 1, 1_000, 5)];
        let agg = aggregate_batch_events(&logs, (10, 5));
        assert_eq!(agg, BatchAggregates::default());
    }

    #[test]
    fn pair_label_uses_address_form_for_every_pair() {
        assert_eq!(
            pair_label(test_market::BASE, test_market::QUOTE),
            test_market::PAIR_LABEL
        );
    }

    #[test]
    fn pair_label_falls_back_to_hex_form_for_unknown_pairs() {
        let base = Address::repeat_byte(0x12);
        let quote = Address::repeat_byte(0x34);
        let label = pair_label(base, quote);
        assert!(label.contains("0x121212"));
        assert!(label.contains("0x343434"));
        assert!(label.contains('/'));
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
    fn redact_fee_history_preserves_shape_and_public_values() {
        let mut history = FeeHistory {
            base_fee_per_gas: vec![1, 2, 3],
            gas_used_ratio: vec![0.25, 0.75],
            base_fee_per_blob_gas: vec![4, 5, 6],
            blob_gas_used_ratio: vec![0.5, 1.0],
            oldest_block: 42,
            reward: Some(vec![vec![7, 8], vec![9, 10]]),
        };

        redact_fee_history(&mut history);

        assert_eq!(history.oldest_block, 42);
        assert_eq!(
            history.base_fee_per_gas,
            vec![u128::from(TEMPO_T0_BASE_FEE); 3]
        );
        assert_eq!(history.gas_used_ratio, vec![0.0; 2]);
        assert_eq!(history.base_fee_per_blob_gas, vec![0; 3]);
        assert_eq!(history.blob_gas_used_ratio, vec![0.0; 2]);
        assert_eq!(history.reward, Some(vec![vec![0, 0], vec![0, 0]]));
    }

    #[test]
    fn apply_public_fee_policy_prefills_missing_fees() {
        let mut request = TempoTransactionRequest::default();

        apply_public_fee_policy(&mut request);

        assert_eq!(request.gas_price(), None);
        assert_eq!(
            request.max_fee_per_gas(),
            Some(u128::from(TEMPO_T0_BASE_FEE))
        );
        assert_eq!(request.max_priority_fee_per_gas(), Some(0));
    }

    #[test]
    fn apply_public_fee_policy_prefills_legacy_gas_price() {
        let mut request = TempoTransactionRequest::default();
        request.inner.transaction_type = Some(0);

        apply_public_fee_policy(&mut request);

        assert_eq!(request.gas_price(), Some(u128::from(TEMPO_T0_BASE_FEE)));
        assert_eq!(request.max_fee_per_gas(), None);
        assert_eq!(request.max_priority_fee_per_gas(), None);
    }

    #[test]
    fn apply_public_fee_policy_preserves_supplied_priority_fee() {
        let mut request = TempoTransactionRequest::default();
        request.set_max_priority_fee_per_gas(7);

        apply_public_fee_policy(&mut request);

        assert_eq!(request.max_priority_fee_per_gas(), Some(7));
        assert_eq!(
            request.max_fee_per_gas(),
            Some(u128::from(TEMPO_T0_BASE_FEE) + 7)
        );
    }

    #[test]
    fn apply_public_fee_policy_prefills_blob_fee() {
        let mut request = TempoTransactionRequest::default();
        request.inner.blob_versioned_hashes = Some(Vec::new());

        apply_public_fee_policy(&mut request);

        assert_eq!(request.inner.max_fee_per_blob_gas, Some(0));
    }

    #[test]
    fn redact_header_clears_activity_metadata() {
        let mut header = TempoHeaderResponse {
            inner: alloy_rpc_types_eth::Header {
                hash: B256::with_last_byte(7),
                inner: tempo_primitives::TempoHeader {
                    inner: alloy_consensus::Header {
                        gas_used: 123,
                        state_root: B256::with_last_byte(1),
                        transactions_root: B256::with_last_byte(2),
                        receipts_root: B256::with_last_byte(3),
                        extra_data: Bytes::from_static(b"private"),
                        blob_gas_used: Some(4),
                        excess_blob_gas: Some(5),
                        withdrawals_root: Some(B256::with_last_byte(6)),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                size: Some(U256::from(8)),
                ..Default::default()
            },
            timestamp_millis: 123_000,
        };

        redact_header(&mut header);

        let inner = &header.inner.inner.inner;
        assert_eq!(header.inner.hash, B256::with_last_byte(7));
        assert_eq!(header.inner.size, Some(U256::ZERO));
        assert_eq!(header.timestamp_millis, 123_000);
        assert_eq!(inner.gas_used, 0);
        assert_eq!(inner.logs_bloom, Bloom::ZERO);
        assert_eq!(inner.blob_gas_used, Some(0));
        assert_eq!(inner.excess_blob_gas, Some(0));
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
    fn rpc_midpoint_cursor_accepts_hex_and_decimal() {
        assert_eq!(parse_midpoint_cursor(None).unwrap(), None);
        assert_eq!(parse_midpoint_cursor(Some("0x180")).unwrap(), Some(384));
        assert_eq!(parse_midpoint_cursor(Some("180")).unwrap(), Some(180));
    }

    #[test]
    fn rpc_midpoint_cursor_rejects_garbage_with_invalid_params() {
        let err =
            parse_midpoint_cursor(Some("not-a-number")).expect_err("garbage cursor must error");
        assert_eq!(err.code, -32602);
        assert_eq!(err.message, "cursor must be a hex quantity");
    }

    #[test]
    fn rpc_midpoint_history_response_advertises_enabled_sampler() {
        let response = build_midpoint_history_response(
            test_market::DISPLAY_LABEL.to_string(),
            test_market::BASE,
            test_market::QUOTE,
            "1m".to_string(),
            Vec::new(),
            None,
        );

        assert_eq!(response.pair, test_market::DISPLAY_LABEL);
        assert_eq!(response.base, test_market::BASE);
        assert_eq!(response.quote, test_market::QUOTE);
        assert_eq!(response.interval, "1m");
        assert!(response.samples.is_empty());
        assert!(response.next_cursor.is_none());
        assert!(
            response.history.enabled,
            "sampler must advertise enabled=true once the backing store exists"
        );
        assert!(
            response.history.reason.contains("sampler"),
            "reason should document the sampler; got {:?}",
            response.history.reason
        );
    }

    #[test]
    fn rpc_midpoint_history_response_encodes_next_cursor_as_hex() {
        let response = build_midpoint_history_response(
            test_market::DISPLAY_LABEL.to_string(),
            test_market::BASE,
            test_market::QUOTE,
            "5m".to_string(),
            vec![MidpointSample {
                timestamp: U64::from(1_700u64),
                midpoint: U128::from(42u128),
            }],
            Some(0x180),
        );

        assert_eq!(response.next_cursor.as_deref(), Some("0x180"));
    }

    #[test]
    fn rpc_midpoint_history_response_emits_aggregate_only_fields() {
        let response = build_midpoint_history_response(
            test_market::DISPLAY_LABEL.to_string(),
            test_market::BASE,
            test_market::QUOTE,
            "1m".to_string(),
            vec![
                MidpointSample {
                    timestamp: U64::from(120u64),
                    midpoint: U128::from(100u128),
                },
                MidpointSample {
                    timestamp: U64::from(180u64),
                    midpoint: U128::from(110u128),
                },
            ],
            None,
        );

        let json = serde_json::to_value(&response).expect("response must serialize");
        let obj = json.as_object().expect("response must be a JSON object");
        for forbidden in [
            "account",
            "owner",
            "trader",
            "maker",
            "taker",
            "user",
            "userAddress",
            "counterparty",
            "fill",
            "fillId",
            "orderId",
            "from",
            "to",
            "sender",
            "recipient",
        ] {
            assert!(
                !obj.contains_key(forbidden),
                "midpoint history leaked owner-linked field `{forbidden}`",
            );
        }

        let samples = json["samples"]
            .as_array()
            .expect("samples must be a JSON array");
        for sample in samples {
            let sample = sample.as_object().expect("sample must be a JSON object");
            assert_eq!(sample.len(), 2);
            assert!(sample.contains_key("timestamp"));
            assert!(sample.contains_key("midpoint"));
        }
    }

    fn static_alpha_provider(price: u128) -> zone_refprice::ReferencePriceProviderConfig {
        zone_refprice::ReferencePriceProviderConfig {
            max_deviation_bps: 1_000,
            max_staleness_secs: 0,
            kind: zone_refprice::ReferencePriceProviderKind::Static {
                price,
                source: "static:alpha".to_string(),
            },
        }
    }

    fn build_test_reference_price_response(
        provider: Option<&zone_refprice::ReferencePriceProviderConfig>,
        loaded_at_secs: u64,
        now_secs: u64,
        as_of_block: u64,
    ) -> ReferencePriceResponse {
        build_reference_price_response(
            provider,
            loaded_at_secs,
            now_secs,
            as_of_block,
            test_market::DISPLAY_LABEL.to_string(),
            test_market::BASE,
            test_market::QUOTE,
        )
    }

    #[test]
    fn market_reference_price_disabled_returns_explicit_disabled_response() {
        let response = build_test_reference_price_response(None, 1_700_000_000, 1_700_000_100, 42);

        assert!(!response.enabled);
        assert_eq!(response.pair, test_market::DISPLAY_LABEL);
        assert_eq!(response.base, test_market::BASE);
        assert_eq!(response.quote, test_market::QUOTE);
        assert!(response.price.is_none());
        assert!(response.source.is_none());
        assert!(response.as_of_block.is_none());
        assert!(response.as_of_timestamp.is_none());
        assert!(response.fresh.is_none());
        assert!(response.age_secs.is_none());
        assert!(response.max_deviation_bps.is_none());
        assert!(response.max_staleness_secs.is_none());
        assert_eq!(
            response.price_unit,
            "raw integer; quote = baseAmount * price"
        );
        assert_eq!(
            response.disclaimer,
            "alpha infrastructure; not a production oracle"
        );
        assert_eq!(
            response.reason.as_deref(),
            Some("reference-price provider not configured"),
        );
    }

    #[test]
    fn market_reference_price_static_provider_returns_price_source_and_freshness() {
        let provider = static_alpha_provider(1_000_000);
        let response =
            build_test_reference_price_response(Some(&provider), 1_700_000_000, 1_700_000_010, 99);

        assert!(response.enabled);
        assert_eq!(response.pair, test_market::DISPLAY_LABEL);
        assert_eq!(response.price, Some(U128::from(1_000_000u128)));
        assert_eq!(response.source.as_deref(), Some("static:alpha"));
        // Static providers do not anchor to a block; expose the sentinel.
        assert_eq!(response.as_of_block, Some(U64::from(0u64)));
        assert_eq!(response.as_of_timestamp, Some(U64::from(1_700_000_000u64)));
        assert_eq!(response.fresh, Some(true));
        assert_eq!(response.age_secs, Some(U64::from(10u64)));
        assert_eq!(response.max_deviation_bps, Some(1_000));
        assert_eq!(response.max_staleness_secs, Some(0));
        assert!(response.reason.is_none());
    }

    #[test]
    fn market_reference_price_static_provider_marks_stale_after_max_staleness() {
        let mut provider = static_alpha_provider(1_000_000);
        provider.max_staleness_secs = 60;
        let response =
            build_test_reference_price_response(Some(&provider), 1_700_000_000, 1_700_000_120, 1);

        assert!(response.enabled);
        assert_eq!(response.fresh, Some(false));
        assert_eq!(response.age_secs, Some(U64::from(120u64)));
        assert_eq!(response.max_staleness_secs, Some(60));
    }

    #[test]
    fn market_reference_price_response_serializes_with_camel_case_keys() {
        let provider = static_alpha_provider(2_500_000);
        let response =
            build_test_reference_price_response(Some(&provider), 1_700_000_000, 1_700_000_005, 7);

        let json = serde_json::to_value(&response).expect("response must serialize");
        let obj = json.as_object().expect("response must be a JSON object");
        for required in [
            "enabled",
            "pair",
            "base",
            "quote",
            "price",
            "source",
            "asOfBlock",
            "asOfTimestamp",
            "fresh",
            "ageSecs",
            "maxDeviationBps",
            "maxStalenessSecs",
            "priceUnit",
            "disclaimer",
        ] {
            assert!(obj.contains_key(required), "missing field `{required}`");
        }
        // When enabled the disabled-only `reason` must be absent.
        assert!(
            !obj.contains_key("reason"),
            "enabled responses must not surface `reason`",
        );
    }

    #[test]
    fn market_reference_price_disabled_response_omits_snapshot_fields_in_json() {
        let response = build_test_reference_price_response(None, 1_700_000_000, 1_700_000_100, 5);
        let json = serde_json::to_value(&response).expect("response must serialize");
        let obj = json.as_object().expect("response must be a JSON object");
        for omitted in [
            "price",
            "source",
            "asOfBlock",
            "asOfTimestamp",
            "fresh",
            "ageSecs",
            "maxDeviationBps",
            "maxStalenessSecs",
        ] {
            assert!(
                !obj.contains_key(omitted),
                "disabled responses must omit `{omitted}`",
            );
        }
        assert_eq!(obj["enabled"], false);
        assert!(obj["reason"].is_string());
    }
}
