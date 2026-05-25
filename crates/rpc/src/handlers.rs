//! Private RPC method handlers.
//!
//! Each handler calls the underlying EthApi via the [`ZoneRpcApi`] trait,
//! which performs typed privacy redactions internally before serialization.

use std::str::FromStr;

use alloy_primitives::{Address, B256, Bytes, U64};
use alloy_rpc_types_eth::{BlockId, BlockNumberOrTag, Filter, FilterId, state::StateOverride};
use serde_json::{Value, value::RawValue};
use tempo_alloy::rpc::TempoTransactionRequest;
use tempo_contracts::precompiles::account_keychain::IAccountKeychain::KeyInfo;
use tracing::warn;

use crate::{
    auth::AuthContext,
    subscription::BoxWsSubscriptionFut,
    types::{
        BoxEyreFut, BoxFut, JsonRpcError, JsonRpcRequest, JsonRpcResponse, MarketPair, MethodTier,
        WithdrawalStatusQuery, classify_method,
    },
};

/// Interface to the underlying reth EthApi for the private zone RPC.
///
/// Implementations are responsible for:
/// - **Access control**: restricting responses based on the [`AuthContext`]
///   (e.g. returning `null` for transactions not owned by the caller).
/// - **Redaction**: scrubbing privacy-sensitive fields (e.g. zeroing
///   `logsBloom`, clearing transaction lists) on typed responses *before*
///   serializing to JSON.
pub trait ZoneRpcApi: Send + Sync + 'static {
    /// `AccountKeychain.getKey(account, keyId)` — returns the current keychain
    /// authorization for a recovered access key.
    fn get_keychain_key(&self, account: Address, key_id: Address) -> BoxEyreFut<'_, KeyInfo>;

    /// `eth_blockNumber` — returns the latest block number.
    fn block_number(&self) -> BoxFut<'_>;

    /// `eth_chainId` — returns the chain ID.
    fn chain_id(&self) -> BoxFut<'_>;

    /// `net_version` — returns the network ID as a decimal string.
    fn net_version(&self) -> BoxFut<'_>;

    /// `eth_gasPrice` — returns the current gas price.
    fn gas_price(&self) -> BoxFut<'_>;

    /// `eth_maxPriorityFeePerGas` — returns the current max priority fee.
    fn max_priority_fee_per_gas(&self) -> BoxFut<'_>;

    /// `eth_feeHistory(blockCount, newestBlock, rewardPercentiles)` — returns fee history.
    fn fee_history(
        &self,
        block_count: u64,
        newest_block: BlockNumberOrTag,
        reward_percentiles: Option<Vec<f64>>,
    ) -> BoxFut<'_>;

    /// `eth_getBalance(address, block)` — returns the balance of an account.
    ///
    /// Returns `0x0` for non-sequencer callers querying an address that does
    /// not match `auth.caller`.
    fn get_balance(
        &self,
        address: Address,
        block: Option<BlockId>,
        auth: AuthContext,
    ) -> BoxFut<'_>;

    /// `eth_getTransactionCount(address, block)` — returns the nonce.
    ///
    /// Returns `0x0` for non-sequencer callers querying an address that does
    /// not match `auth.caller`.
    fn get_transaction_count(
        &self,
        address: Address,
        block: Option<BlockId>,
        auth: AuthContext,
    ) -> BoxFut<'_>;

    /// `eth_getBlockByNumber(number, full)` — returns a block by number.
    fn block_by_number(
        &self,
        number: BlockNumberOrTag,
        full: bool,
        auth: AuthContext,
    ) -> BoxFut<'_>;

    /// `eth_getBlockByHash(hash, full)` — returns a block by hash.
    fn block_by_hash(&self, hash: B256, full: bool, auth: AuthContext) -> BoxFut<'_>;

    /// `eth_getTransactionByHash(hash)` — returns a transaction by hash.
    fn transaction_by_hash(&self, hash: B256, auth: AuthContext) -> BoxFut<'_>;

    /// `eth_getTransactionReceipt(hash)` — returns a transaction receipt.
    fn transaction_receipt(&self, hash: B256, auth: AuthContext) -> BoxFut<'_>;

    /// `eth_call(request, block, state_override)` — executes a call without
    /// creating a transaction.
    ///
    /// Enforces that `from` equals the authenticated account (sets it if omitted,
    /// rejects with `-32004` on mismatch). State/block overrides are rejected
    /// with `-32602` for non-sequencer callers.
    fn call(
        &self,
        request: TempoTransactionRequest,
        block: Option<BlockId>,
        state_override: Option<StateOverride>,
        auth: AuthContext,
    ) -> BoxFut<'_>;

    /// `eth_estimateGas(request, block, state_override)` — estimates gas for a transaction.
    ///
    /// Same `from`-enforcement as [`call`](Self::call). State overrides are
    /// rejected with `-32602` for non-sequencer callers.
    fn estimate_gas(
        &self,
        request: TempoTransactionRequest,
        block: Option<BlockId>,
        state_override: Option<StateOverride>,
        auth: AuthContext,
    ) -> BoxFut<'_>;

    /// `eth_sendRawTransaction(data)` — submits a signed transaction to the pool.
    ///
    /// Verifies that the recovered tx sender matches the authenticated account;
    /// rejects with `-32003` on mismatch.
    fn send_raw_transaction(&self, data: Bytes, auth: AuthContext) -> BoxFut<'_>;

    /// `eth_sendRawTransactionSync(data)` — submits a signed transaction and
    /// waits for inclusion, returning the receipt.
    ///
    /// Same sender verification as [`send_raw_transaction`](Self::send_raw_transaction).
    fn send_raw_transaction_sync(&self, data: Bytes, auth: AuthContext) -> BoxFut<'_>;

    /// `eth_fillTransaction(request)` — fills defaults on an unsigned transaction
    /// (nonce, gas limit, fees, chain ID) and returns the filled + RLP-encoded
    /// result without signing or submitting.
    ///
    /// Same `from`-enforcement as [`call`](Self::call).
    fn fill_transaction(&self, request: TempoTransactionRequest, auth: AuthContext) -> BoxFut<'_>;

    /// `eth_getLogs(filter)` — returns logs matching the filter, scoped to the caller.
    fn get_logs(&self, filter: Filter, auth: AuthContext) -> BoxFut<'_>;

    /// `eth_newFilter(filter)` — creates a new log filter, scoped to the caller.
    fn new_filter(&self, filter: Filter, auth: AuthContext) -> BoxFut<'_>;

    /// `eth_getFilterLogs(id)` — returns all logs for a filter.
    fn get_filter_logs(&self, id: FilterId, auth: AuthContext) -> BoxFut<'_>;

    /// `eth_getFilterChanges(id)` — returns new logs since last poll.
    fn get_filter_changes(&self, id: FilterId, auth: AuthContext) -> BoxFut<'_>;

    /// `eth_newBlockFilter` — creates a new block filter.
    fn new_block_filter(&self, auth: AuthContext) -> BoxFut<'_>;

    /// `eth_uninstallFilter(id)` — removes a filter.
    fn uninstall_filter(&self, id: FilterId, auth: AuthContext) -> BoxFut<'_>;

    /// `eth_subscribe("newHeads")` — opens a stream of new block headers.
    fn ws_subscribe_new_heads(&self, _auth: AuthContext) -> BoxWsSubscriptionFut<'_> {
        Box::pin(async { Err(JsonRpcError::method_disabled()) })
    }

    /// `eth_subscribe("logs", filter)` — opens a stream of matching logs.
    fn ws_subscribe_logs(&self, _filter: Filter, _auth: AuthContext) -> BoxWsSubscriptionFut<'_> {
        Box::pin(async { Err(JsonRpcError::method_disabled()) })
    }

    /// `eth_subscribe("newPendingTransactions", full?)` — opens a stream of
    /// pending transactions, returning either hashes or full transaction objects.
    fn ws_subscribe_pending_transactions(
        &self,
        _full: bool,
        _auth: AuthContext,
    ) -> BoxWsSubscriptionFut<'_> {
        Box::pin(async { Err(JsonRpcError::method_disabled()) })
    }

    /// `zone_getAuthorizationTokenInfo()` — returns the authenticated account
    /// and token expiry.
    fn zone_get_authorization_token_info(&self, auth: AuthContext) -> BoxFut<'_>;

    /// `zone_getZoneInfo()` — returns zone metadata.
    fn zone_get_zone_info(&self, auth: AuthContext) -> BoxFut<'_>;

    /// `zone_getDepositStatus(tempoBlockNumber)` — returns per-caller deposit
    /// processing state for a Tempo L1 block.
    fn zone_get_deposit_status(&self, tempo_block_number: u64, auth: AuthContext) -> BoxFut<'_>;

    /// `zone_listBatches(params)` — paginate public, aggregate-only batch
    /// history. The response is caller-agnostic and must never include
    /// owner-linked data.
    fn zone_list_batches(
        &self,
        _params: crate::types::ListBatchesParams,
        _auth: AuthContext,
    ) -> BoxFut<'_> {
        Box::pin(async { Err(JsonRpcError::method_disabled()) })
    }

    /// `zone_getBatch(batchNumber)` — return the public, aggregate-only
    /// summary for a single batch. Returns `null` when the batch is not on L1.
    fn zone_get_batch(&self, _batch_number: u64, _auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async { Err(JsonRpcError::method_disabled()) })
    }

    /// `zone_searchBatch(query)` — resolve a batch by batch number or L1
    /// settlement tx hash.
    fn zone_search_batch(&self, _query: String, _auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async { Err(JsonRpcError::method_disabled()) })
    }

    /// `zone_getMarketConfig()` — returns canonical market metadata.
    fn zone_get_market_config(&self, auth: AuthContext) -> BoxFut<'_>;

    /// `zone_getTopOfBook(pair)` — returns aggregate best bid/ask and midpoint.
    fn zone_get_top_of_book(&self, base: Address, quote: Address, auth: AuthContext) -> BoxFut<'_>;

    /// `zone_getMidpointHistory(pair, interval, limit, cursor?)` — returns
    /// aggregate midpoint samples for charting.
    fn zone_get_midpoint_history(
        &self,
        base: Address,
        quote: Address,
        interval: String,
        limit: u32,
        cursor: Option<String>,
        auth: AuthContext,
    ) -> BoxFut<'_>;

    /// `zone_getWithdrawalStatus(txHashOrWithdrawalIndex)` — returns lifecycle
    /// status of the caller's own withdrawal, joining zone L2 outbox events with
    /// L1 portal settlement events. Returns `null` if the withdrawal does not
    /// exist or is not owned by the authenticated caller.
    fn zone_get_withdrawal_status(
        &self,
        query: WithdrawalStatusQuery,
        auth: AuthContext,
    ) -> BoxFut<'_>;
}

/// Deserialize JSON-RPC params, returning an error response on failure.
#[allow(clippy::result_large_err)]
fn parse_params<T: serde::de::DeserializeOwned>(
    raw: &str,
    id: &Value,
    msg: &'static str,
) -> Result<T, JsonRpcResponse> {
    serde_json::from_str(raw)
        .map_err(|_| JsonRpcResponse::error(id.clone(), JsonRpcError::invalid_params(msg)))
}

/// Params for `eth_call` / `eth_estimateGas`: `[request, block?, stateOverride?]`.
///
/// Supports 1–3 element arrays with null-as-absent semantics for trailing optionals.
struct CallParams(
    TempoTransactionRequest,
    Option<BlockId>,
    Option<StateOverride>,
);

impl<'de> serde::Deserialize<'de> for CallParams {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Vis;
        impl<'de> serde::de::Visitor<'de> for Vis {
            type Value = CallParams;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("[request, block?, stateOverride?]")
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                let request = seq
                    .next_element()?
                    .ok_or_else(|| serde::de::Error::invalid_length(0, &self))?;
                let block = seq.next_element::<Option<BlockId>>()?.flatten();
                let state_override = seq.next_element::<Option<StateOverride>>()?.flatten();
                Ok(CallParams(request, block, state_override))
            }
        }
        deserializer.deserialize_seq(Vis)
    }
}

/// Convert an API result into a JSON-RPC response, logging failures.
fn api_result(
    id: Value,
    method: &str,
    res: Result<Box<RawValue>, JsonRpcError>,
) -> JsonRpcResponse {
    match res {
        Ok(v) => JsonRpcResponse::success(id, v),
        Err(e) => {
            warn!(target: "zone::rpc", err = %e, method, "RPC call failed");
            JsonRpcResponse::error(id, e)
        }
    }
}

/// Dispatch a single JSON-RPC request through the private zone RPC pipeline.
///
/// Enforces a strict whitelist of allowed methods (see [`classify_method`]) and
/// rejects anything unknown or disabled. Individual handlers may apply
/// additional per-method access checks.
pub async fn dispatch(
    req: &JsonRpcRequest,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let id = req.id.clone();

    let tier = match classify_method(&req.method) {
        Some(tier) => tier,
        None => return JsonRpcResponse::error(id, JsonRpcError::method_not_found()),
    };

    match tier {
        MethodTier::Disabled => {
            return JsonRpcResponse::error(id, JsonRpcError::method_disabled());
        }
        MethodTier::Restricted => {
            return JsonRpcResponse::error(id, JsonRpcError::sequencer_only());
        }
        MethodTier::UnsupportedAccountManagement => {
            return JsonRpcResponse::error(
                id,
                JsonRpcError::unsupported_account_method(&req.method),
            );
        }
        _ => {}
    }

    // Raw params JSON — handlers deserialize directly, no intermediate Vec<Value>.
    let raw = req.params.as_deref().map(|p| p.get()).unwrap_or("[]");

    match req.method.as_str() {
        // Simple passthrough methods (no params, no auth scoping)
        "eth_blockNumber" => api_result(id, "eth_blockNumber", api.block_number().await),
        "eth_chainId" => api_result(id, "eth_chainId", api.chain_id().await),
        "eth_gasPrice" => api_result(id, "eth_gasPrice", api.gas_price().await),
        "eth_maxPriorityFeePerGas" => api_result(
            id,
            "eth_maxPriorityFeePerGas",
            api.max_priority_fee_per_gas().await,
        ),
        "net_version" => api_result(id, "net_version", api.net_version().await),
        "net_listening" => api_result(id, "net_listening", crate::types::to_raw(&true)),
        "web3_clientVersion" => api_result(
            id,
            "web3_clientVersion",
            crate::types::to_raw(&"tempo-zone/v0.1.0"),
        ),

        // Fee history
        "eth_feeHistory" => handle_fee_history(id, raw, api).await,

        // Scoped state queries
        "eth_getBalance" => handle_get_balance(id, raw, auth, api).await,
        "eth_getTransactionCount" => handle_get_transaction_count(id, raw, auth, api).await,

        // Block queries
        "eth_getBlockByNumber" => handle_get_block_by_number(id, raw, auth, api).await,
        "eth_getBlockByHash" => handle_get_block_by_hash(id, raw, auth, api).await,

        // Transaction queries
        "eth_getTransactionByHash" => handle_get_transaction_by_hash(id, raw, auth, api).await,
        "eth_getTransactionReceipt" => handle_get_transaction_receipt(id, raw, auth, api).await,

        // Simulation
        "eth_call" => handle_call(id, raw, auth, api).await,
        "eth_estimateGas" => handle_estimate_gas(id, raw, auth, api).await,

        // Transaction preparation & submission
        "eth_fillTransaction" => handle_fill_transaction(id, raw, auth, api).await,
        "eth_sendRawTransaction" => handle_send_raw_transaction(id, raw, auth, api).await,
        "eth_sendRawTransactionSync" => handle_send_raw_transaction_sync(id, raw, auth, api).await,

        // Log & filter queries
        "eth_getLogs" => handle_get_logs(id, raw, auth, api).await,
        "eth_newFilter" => handle_new_filter(id, raw, auth, api).await,
        "eth_getFilterLogs" => handle_get_filter_logs(id, raw, auth, api).await,
        "eth_getFilterChanges" => handle_get_filter_changes(id, raw, auth, api).await,
        "eth_newBlockFilter" => handle_new_block_filter(id, auth, api).await,
        "eth_uninstallFilter" => handle_uninstall_filter(id, raw, auth, api).await,
        "zone_getAuthorizationTokenInfo" => api_result(
            id,
            "zone_getAuthorizationTokenInfo",
            api.zone_get_authorization_token_info(auth.clone()).await,
        ),
        "zone_getZoneInfo" => api_result(
            id,
            "zone_getZoneInfo",
            api.zone_get_zone_info(auth.clone()).await,
        ),
        "zone_getDepositStatus" => handle_zone_get_deposit_status(id, raw, auth, api).await,
        "zone_listBatches" => handle_zone_list_batches(id, raw, auth, api).await,
        "zone_getBatch" => handle_zone_get_batch(id, raw, auth, api).await,
        "zone_searchBatch" => handle_zone_search_batch(id, raw, auth, api).await,
        "zone_getMarketConfig" => api_result(
            id,
            "zone_getMarketConfig",
            api.zone_get_market_config(auth.clone()).await,
        ),
        "zone_getTopOfBook" => handle_zone_get_top_of_book(id, raw, auth, api).await,
        "zone_getMidpointHistory" => handle_zone_get_midpoint_history(id, raw, auth, api).await,
        "zone_getWithdrawalStatus" => handle_zone_get_withdrawal_status(id, raw, auth, api).await,
        _ => {
            // Method is whitelisted but not yet implemented via direct API
            JsonRpcResponse::error(
                id,
                JsonRpcError::internal("method not yet implemented in private RPC"),
            )
        }
    }
}

/// Handle `eth_getBlockByNumber`. Rejects `full=true` for non-sequencer callers.
async fn handle_get_block_by_number(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let (number, full) = match parse_params::<(BlockNumberOrTag, bool)>(
        raw,
        &id,
        "expected [blockNumberOrTag, full]",
    ) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let number = normalize_block_number(number);

    if full {
        return JsonRpcResponse::error(id, JsonRpcError::sequencer_only());
    }

    api_result(
        id,
        "eth_getBlockByNumber",
        api.block_by_number(number, full, auth.clone()).await,
    )
}

/// Handle `eth_getBlockByHash`. Rejects `full=true`.
async fn handle_get_block_by_hash(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let (hash, full) = match parse_params::<(B256, bool)>(raw, &id, "expected [blockHash, full]") {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    if full {
        return JsonRpcResponse::error(id, JsonRpcError::sequencer_only());
    }

    api_result(
        id,
        "eth_getBlockByHash",
        api.block_by_hash(hash, full, auth.clone()).await,
    )
}

/// Handle `eth_getTransactionByHash`. Access control is delegated to the API impl.
async fn handle_get_transaction_by_hash(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let (hash,) = match parse_params::<(B256,)>(raw, &id, "expected [txHash]") {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    api_result(
        id,
        "eth_getTransactionByHash",
        api.transaction_by_hash(hash, auth.clone()).await,
    )
}

/// Handle `eth_getTransactionReceipt`. Access control is delegated to the API impl.
async fn handle_get_transaction_receipt(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let (hash,) = match parse_params::<(B256,)>(raw, &id, "expected [txHash]") {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    api_result(
        id,
        "eth_getTransactionReceipt",
        api.transaction_receipt(hash, auth.clone()).await,
    )
}

/// Handle `eth_call`. Enforces `from` matches the authenticated account and
/// rejects state overrides for non-sequencer callers.
async fn handle_call(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let CallParams(request, block, state_override) =
        match parse_params(raw, &id, "expected [request, block?, stateOverride?]") {
            Ok(v) => v,
            Err(resp) => return resp,
        };

    if state_override.is_some() {
        return JsonRpcResponse::error(
            id,
            JsonRpcError::invalid_params("state overrides not allowed"),
        );
    }

    api_result(
        id,
        "eth_call",
        api.call(request, block, state_override, auth.clone()).await,
    )
}

/// Handle `eth_estimateGas`. Same `from`-enforcement as `eth_call`.
/// Rejects state overrides.
async fn handle_estimate_gas(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let CallParams(request, block, state_override) =
        match parse_params(raw, &id, "expected [request, block?, stateOverride?]") {
            Ok(v) => v,
            Err(resp) => return resp,
        };

    if state_override.is_some() {
        return JsonRpcResponse::error(
            id,
            JsonRpcError::invalid_params("state overrides not allowed"),
        );
    }

    api_result(
        id,
        "eth_estimateGas",
        api.estimate_gas(request, block, state_override, auth.clone())
            .await,
    )
}

/// Handle `eth_fillTransaction`. `from`-enforcement is delegated to the API impl.
async fn handle_fill_transaction(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let (request,) =
        match parse_params::<(TempoTransactionRequest,)>(raw, &id, "expected [request]") {
            Ok(v) => v,
            Err(resp) => return resp,
        };

    api_result(
        id,
        "eth_fillTransaction",
        api.fill_transaction(request, auth.clone()).await,
    )
}

/// Handle `eth_sendRawTransaction`. Sender verification is delegated to the API impl.
async fn handle_send_raw_transaction(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let (data,) = match parse_params::<(Bytes,)>(raw, &id, "expected [data]") {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    api_result(
        id,
        "eth_sendRawTransaction",
        api.send_raw_transaction(data, auth.clone()).await,
    )
}

/// Handle `eth_sendRawTransactionSync`. Sender verification is delegated to
/// the API impl.
async fn handle_send_raw_transaction_sync(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let (data,) = match parse_params::<(Bytes,)>(raw, &id, "expected [data]") {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    api_result(
        id,
        "eth_sendRawTransactionSync",
        api.send_raw_transaction_sync(data, auth.clone()).await,
    )
}

/// Handle `eth_feeHistory`. Public method, no auth scoping needed.
async fn handle_fee_history(id: Value, raw: &str, api: &dyn ZoneRpcApi) -> JsonRpcResponse {
    let (block_count, newest_block, reward_percentiles) =
        match parse_params::<(u64, BlockNumberOrTag, Option<Vec<f64>>)>(
            raw,
            &id,
            "expected [blockCount, newestBlock, rewardPercentiles?]",
        ) {
            Ok(v) => v,
            Err(resp) => return resp,
        };

    api_result(
        id,
        "eth_feeHistory",
        api.fee_history(block_count, newest_block, reward_percentiles)
            .await,
    )
}

/// Handle `eth_getBalance`. Returns `0x0` for non-sequencer callers querying
/// a different address (checked in API impl, no timing leak since check is pre-fetch).
async fn handle_get_balance(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let (address, block) =
        match parse_params::<(Address, Option<BlockId>)>(raw, &id, "expected [address, block?]") {
            Ok(v) => v,
            Err(resp) => return resp,
        };

    api_result(
        id,
        "eth_getBalance",
        api.get_balance(address, block, auth.clone()).await,
    )
}

/// Handle `eth_getTransactionCount`. Returns `0x0` for non-sequencer callers
/// querying a different address (checked in API impl, no timing leak).
async fn handle_get_transaction_count(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let (address, block) =
        match parse_params::<(Address, Option<BlockId>)>(raw, &id, "expected [address, block?]") {
            Ok(v) => v,
            Err(resp) => return resp,
        };

    api_result(
        id,
        "eth_getTransactionCount",
        api.get_transaction_count(address, block, auth.clone())
            .await,
    )
}

/// Handle `eth_getLogs`.
async fn handle_get_logs(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let (filter,) = match parse_params::<(Filter,)>(raw, &id, "expected [filter]") {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    api_result(id, "eth_getLogs", api.get_logs(filter, auth.clone()).await)
}

/// Handle `eth_newFilter`.
async fn handle_new_filter(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let (filter,) = match parse_params::<(Filter,)>(raw, &id, "expected [filter]") {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    api_result(
        id,
        "eth_newFilter",
        api.new_filter(filter, auth.clone()).await,
    )
}

/// Handle `eth_getFilterLogs`.
async fn handle_get_filter_logs(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let (filter_id,) = match parse_params::<(FilterId,)>(raw, &id, "expected [filterId]") {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    api_result(
        id,
        "eth_getFilterLogs",
        api.get_filter_logs(filter_id, auth.clone()).await,
    )
}

/// Handle `eth_getFilterChanges`.
async fn handle_get_filter_changes(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let (filter_id,) = match parse_params::<(FilterId,)>(raw, &id, "expected [filterId]") {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    api_result(
        id,
        "eth_getFilterChanges",
        api.get_filter_changes(filter_id, auth.clone()).await,
    )
}

/// Handle `eth_newBlockFilter`.
async fn handle_new_block_filter(
    id: Value,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    api_result(
        id,
        "eth_newBlockFilter",
        api.new_block_filter(auth.clone()).await,
    )
}

/// Handle `eth_uninstallFilter`.
async fn handle_uninstall_filter(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let (filter_id,) = match parse_params::<(FilterId,)>(raw, &id, "expected [filterId]") {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    api_result(
        id,
        "eth_uninstallFilter",
        api.uninstall_filter(filter_id, auth.clone()).await,
    )
}

/// Handle `zone_listBatches(params)`.
///
/// Accepts either `[]`, `[params]`, or `[limit, cursor]` for ergonomic clients.
async fn handle_zone_list_batches(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let params = match parse_list_batches_params(raw) {
        Ok(params) => params,
        Err(msg) => return JsonRpcResponse::error(id, JsonRpcError::invalid_params(msg)),
    };

    api_result(
        id,
        "zone_listBatches",
        api.zone_list_batches(params, auth.clone()).await,
    )
}

/// Parse `zone_listBatches` params into a [`ListBatchesParams`].
#[allow(clippy::result_large_err)]
fn parse_list_batches_params(raw: &str) -> Result<crate::types::ListBatchesParams, &'static str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "[]" {
        return Ok(crate::types::ListBatchesParams::default());
    }

    if let Ok((params,)) = serde_json::from_str::<(crate::types::ListBatchesParams,)>(raw) {
        return Ok(params);
    }

    if let Ok((limit, cursor)) = serde_json::from_str::<(Option<u32>, Option<U64>)>(raw) {
        return Ok(crate::types::ListBatchesParams { limit, cursor });
    }

    Err("expected [] or [params] for zone_listBatches")
}

/// Handle `zone_getBatch(batchNumber)`.
async fn handle_zone_get_batch(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let (batch_number,) = match parse_params::<(String,)>(raw, &id, "expected [batchNumber]") {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let batch_number = match U64::from_str(&batch_number) {
        Ok(value) => value.to(),
        Err(_) => {
            return JsonRpcResponse::error(
                id,
                JsonRpcError::invalid_params("expected [batchNumber]"),
            );
        }
    };

    api_result(
        id,
        "zone_getBatch",
        api.zone_get_batch(batch_number, auth.clone()).await,
    )
}

/// Handle `zone_searchBatch(query)`.
async fn handle_zone_search_batch(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let (query,) = match parse_params::<(String,)>(raw, &id, "expected [query]") {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    api_result(
        id,
        "zone_searchBatch",
        api.zone_search_batch(query, auth.clone()).await,
    )
}

/// Handle `zone_getTopOfBook(pair)`.
async fn handle_zone_get_top_of_book(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let (pair,) = match parse_params::<(MarketPair,)>(raw, &id, "expected [{base, quote}]") {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    api_result(
        id,
        "zone_getTopOfBook",
        api.zone_get_top_of_book(pair.base, pair.quote, auth.clone())
            .await,
    )
}

const DEFAULT_MIDPOINT_HISTORY_LIMIT: u32 = 500;
const MAX_MIDPOINT_HISTORY_LIMIT: u32 = 5_000;

/// Params for `zone_getMidpointHistory`: `[{base, quote}, interval, limit?, cursor?]`.
#[derive(serde::Deserialize)]
struct MidpointHistoryParams(
    MarketPair,
    String,
    #[serde(default)] Option<u32>,
    #[serde(default)] Option<String>,
);

/// Handle `zone_getMidpointHistory(pair, interval, limit?, cursor?)`.
async fn handle_zone_get_midpoint_history(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let MidpointHistoryParams(pair, interval, limit, cursor) = match parse_params(
        raw,
        &id,
        "expected [{base, quote}, interval, limit?, cursor?]",
    ) {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    let limit = limit
        .unwrap_or(DEFAULT_MIDPOINT_HISTORY_LIMIT)
        .min(MAX_MIDPOINT_HISTORY_LIMIT);

    api_result(
        id,
        "zone_getMidpointHistory",
        api.zone_get_midpoint_history(pair.base, pair.quote, interval, limit, cursor, auth.clone())
            .await,
    )
}

/// Handle `zone_getDepositStatus(tempoBlockNumber)`.
async fn handle_zone_get_deposit_status(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let (tempo_block_number,) =
        match parse_params::<(String,)>(raw, &id, "expected [tempoBlockNumber]") {
            Ok((tempo_block_number,)) => (tempo_block_number,),
            Err(resp) => return resp,
        };
    let tempo_block_number = match U64::from_str(&tempo_block_number) {
        Ok(tempo_block_number) => tempo_block_number.to(),
        Err(_) => {
            return JsonRpcResponse::error(
                id,
                JsonRpcError::invalid_params("expected [tempoBlockNumber]"),
            );
        }
    };

    api_result(
        id,
        "zone_getDepositStatus",
        api.zone_get_deposit_status(tempo_block_number, auth.clone())
            .await,
    )
}

/// Handle `zone_getWithdrawalStatus(txHashOrWithdrawalIndex)`.
///
/// Accepts a single hex-encoded string that is parsed either as a 32-byte
/// transaction hash or, failing that, as a `U64` quantity withdrawal index.
async fn handle_zone_get_withdrawal_status(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let (param,) = match parse_params::<(String,)>(raw, &id, "expected [txHashOrWithdrawalIndex]") {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    let query = match parse_withdrawal_status_query(&param) {
        Ok(q) => q,
        Err(err) => return JsonRpcResponse::error(id, err),
    };

    api_result(
        id,
        "zone_getWithdrawalStatus",
        api.zone_get_withdrawal_status(query, auth.clone()).await,
    )
}

/// Parse a `zone_getWithdrawalStatus` parameter into either a tx hash or a
/// withdrawal index.
///
/// A 32-byte hex string (`0x` + 64 hex chars) is treated as a transaction hash.
/// Any other valid hex quantity is treated as a `U64` withdrawal index.
fn parse_withdrawal_status_query(param: &str) -> Result<WithdrawalStatusQuery, JsonRpcError> {
    let stripped = param
        .strip_prefix("0x")
        .or_else(|| param.strip_prefix("0X"));
    if let Some(hex) = stripped
        && hex.len() == 64
        && hex.chars().all(|c| c.is_ascii_hexdigit())
        && let Ok(hash) = B256::from_str(param)
    {
        return Ok(WithdrawalStatusQuery::TxHash(hash));
    }

    U64::from_str(param)
        .map(|v| WithdrawalStatusQuery::WithdrawalIndex(v.to()))
        .map_err(|_| JsonRpcError::invalid_params("expected [txHashOrWithdrawalIndex]"))
}

/// Zones do not have a real pending block, so treat `pending` as `latest`.
fn normalize_block_number(number: BlockNumberOrTag) -> BlockNumberOrTag {
    if number.is_pending() {
        BlockNumberOrTag::Latest
    } else {
        number
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    };

    use alloy_primitives::Address;
    use serde_json::json;

    use super::*;
    use crate::types::to_raw;

    struct MockZoneRpcApi {
        last_tempo_block_number: AtomicU64,
        last_withdrawal_query: Mutex<Option<WithdrawalStatusQuery>>,
        last_midpoint_limit: AtomicU64,
        last_midpoint_cursor: Mutex<Option<String>>,
    }

    impl Default for MockZoneRpcApi {
        fn default() -> Self {
            Self {
                last_tempo_block_number: AtomicU64::new(0),
                last_withdrawal_query: Mutex::new(None),
                last_midpoint_limit: AtomicU64::new(0),
                last_midpoint_cursor: Mutex::new(None),
            }
        }
    }

    macro_rules! stub {
        ($method:ident $(, $arg:ident : $ty:ty)*) => {
            fn $method(&self $(, $arg: $ty)*) -> BoxFut<'_> {
                Box::pin(async { Err(JsonRpcError::internal("not implemented")) })
            }
        };
    }

    impl ZoneRpcApi for MockZoneRpcApi {
        fn get_keychain_key(&self, _account: Address, _key_id: Address) -> BoxEyreFut<'_, KeyInfo> {
            Box::pin(async { Err(eyre::eyre!("not implemented")) })
        }

        stub!(block_number);
        stub!(chain_id);
        stub!(net_version);
        stub!(gas_price);
        stub!(max_priority_fee_per_gas);
        stub!(fee_history, _block_count: u64, _newest_block: BlockNumberOrTag, _reward_percentiles: Option<Vec<f64>>);
        stub!(get_balance, _address: Address, _block: Option<BlockId>, _auth: AuthContext);
        stub!(get_transaction_count, _address: Address, _block: Option<BlockId>, _auth: AuthContext);
        stub!(block_by_number, _number: BlockNumberOrTag, _full: bool, _auth: AuthContext);
        stub!(block_by_hash, _hash: B256, _full: bool, _auth: AuthContext);
        stub!(transaction_by_hash, _hash: B256, _auth: AuthContext);
        stub!(transaction_receipt, _hash: B256, _auth: AuthContext);
        stub!(call, _request: TempoTransactionRequest, _block: Option<BlockId>, _state_override: Option<StateOverride>, _auth: AuthContext);
        stub!(estimate_gas, _request: TempoTransactionRequest, _block: Option<BlockId>, _state_override: Option<StateOverride>, _auth: AuthContext);
        stub!(send_raw_transaction, _data: Bytes, _auth: AuthContext);
        stub!(send_raw_transaction_sync, _data: Bytes, _auth: AuthContext);
        stub!(fill_transaction, _request: TempoTransactionRequest, _auth: AuthContext);
        stub!(get_logs, _filter: Filter, _auth: AuthContext);
        stub!(new_filter, _filter: Filter, _auth: AuthContext);
        stub!(get_filter_logs, _id: FilterId, _auth: AuthContext);
        stub!(get_filter_changes, _id: FilterId, _auth: AuthContext);
        stub!(new_block_filter, _auth: AuthContext);
        stub!(uninstall_filter, _id: FilterId, _auth: AuthContext);

        fn zone_get_authorization_token_info(&self, auth: AuthContext) -> BoxFut<'_> {
            Box::pin(async move {
                to_raw(&json!({
                    "account": auth.caller,
                    "expiresAt": alloy_primitives::U64::from(auth.expires_at),
                }))
            })
        }

        fn zone_get_zone_info(&self, _auth: AuthContext) -> BoxFut<'_> {
            Box::pin(async move {
                to_raw(&json!({
                    "zoneId": "0x1",
                    "zoneTokens": [format!("{:#x}", Address::repeat_byte(0x11))],
                    "chainId": "0x2a",
                }))
            })
        }

        fn zone_get_deposit_status(
            &self,
            tempo_block_number: u64,
            _auth: AuthContext,
        ) -> BoxFut<'_> {
            self.last_tempo_block_number
                .store(tempo_block_number, Ordering::Relaxed);
            Box::pin(async move {
                to_raw(&json!({
                    "tempoBlockNumber": alloy_primitives::U64::from(tempo_block_number),
                    "zoneProcessedThrough": alloy_primitives::U64::from(tempo_block_number),
                    "processed": true,
                    "deposits": [],
                }))
            })
        }

        fn zone_get_market_config(&self, _auth: AuthContext) -> BoxFut<'_> {
            Box::pin(async move {
                to_raw(&json!({
                    "darkpool": format!("{:#x}", Address::repeat_byte(0xdd)),
                    "markets": [],
                }))
            })
        }

        fn zone_get_top_of_book(
            &self,
            base: Address,
            quote: Address,
            _auth: AuthContext,
        ) -> BoxFut<'_> {
            Box::pin(async move {
                to_raw(&json!({
                    "pair": "MOCK/MOCK",
                    "base": base,
                    "quote": quote,
                    "bid": null,
                    "ask": null,
                    "midpoint": null,
                    "spread": null,
                    "asOfBlock": "0x0",
                }))
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
            self.last_midpoint_limit
                .store(limit as u64, Ordering::Relaxed);
            *self
                .last_midpoint_cursor
                .lock()
                .expect("mock lock poisoned") = cursor;
            Box::pin(async move {
                to_raw(&json!({
                    "pair": "MOCK/MOCK",
                    "base": base,
                    "quote": quote,
                    "interval": interval,
                    "samples": [],
                    "nextCursor": null,
                    "history": {
                        "enabled": false,
                        "reason": "mock",
                    },
                }))
            })
        }

        fn zone_get_withdrawal_status(
            &self,
            query: WithdrawalStatusQuery,
            _auth: AuthContext,
        ) -> BoxFut<'_> {
            self.last_withdrawal_query
                .lock()
                .expect("mock lock poisoned")
                .replace(query);
            Box::pin(async move {
                to_raw(&json!({
                    "status": "pending",
                }))
            })
        }
    }

    fn auth() -> AuthContext {
        AuthContext {
            caller: Address::repeat_byte(0xaa),
            expires_at: 1_700_000_000,
        }
    }

    fn request(method: &str, params: serde_json::Value) -> JsonRpcRequest {
        serde_json::from_value(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
            "id": 1
        }))
        .expect("request should deserialize")
    }

    #[tokio::test]
    async fn dispatches_zone_get_authorization_token_info() {
        let api = MockZoneRpcApi::default();
        let resp = dispatch(
            &request("zone_getAuthorizationTokenInfo", json!([])),
            &auth(),
            &api,
        )
        .await;

        assert!(resp.error.is_none());
        let body: serde_json::Value =
            serde_json::from_str(resp.result.as_ref().unwrap().get()).unwrap();
        assert_eq!(
            body["account"].as_str().unwrap(),
            format!("{:#x}", Address::repeat_byte(0xaa)),
        );
        assert_eq!(body["expiresAt"], "0x6553f100");
    }

    #[tokio::test]
    async fn dispatches_zone_get_zone_info() {
        let api = MockZoneRpcApi::default();
        let resp = dispatch(&request("zone_getZoneInfo", json!([])), &auth(), &api).await;

        assert!(resp.error.is_none());
        let body: serde_json::Value =
            serde_json::from_str(resp.result.as_ref().unwrap().get()).unwrap();
        assert_eq!(body["zoneId"], "0x1");
        assert_eq!(
            body["zoneTokens"][0],
            format!("{:#x}", Address::repeat_byte(0x11))
        );
        assert_eq!(body["chainId"], "0x2a");
    }

    #[tokio::test]
    async fn dispatches_zone_get_deposit_status_for_hex_quantity() {
        let api = MockZoneRpcApi::default();

        let resp = dispatch(
            &request("zone_getDepositStatus", json!(["0x2a"])),
            &auth(),
            &api,
        )
        .await;
        assert!(resp.error.is_none());
        assert_eq!(api.last_tempo_block_number.load(Ordering::Relaxed), 42);
    }

    #[tokio::test]
    async fn dispatches_zone_get_market_config() {
        let api = MockZoneRpcApi::default();
        let resp = dispatch(&request("zone_getMarketConfig", json!([])), &auth(), &api).await;

        assert!(resp.error.is_none());
        let body: serde_json::Value =
            serde_json::from_str(resp.result.as_ref().unwrap().get()).unwrap();
        assert_eq!(
            body["darkpool"],
            format!("{:#x}", Address::repeat_byte(0xdd))
        );
        assert!(body["markets"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn dispatches_zone_get_top_of_book_with_pair_object() {
        let api = MockZoneRpcApi::default();
        let base = format!("{:#x}", Address::repeat_byte(0x11));
        let quote = format!("{:#x}", Address::repeat_byte(0x22));

        let resp = dispatch(
            &request("zone_getTopOfBook", json!([{"base": base, "quote": quote}])),
            &auth(),
            &api,
        )
        .await;

        assert!(resp.error.is_none(), "{:?}", resp.error);
        let body: serde_json::Value =
            serde_json::from_str(resp.result.as_ref().unwrap().get()).unwrap();
        assert_eq!(body["base"], base);
        assert_eq!(body["quote"], quote);
        assert!(body["bid"].is_null());
        assert!(body["ask"].is_null());
    }

    #[tokio::test]
    async fn rejects_zone_get_top_of_book_missing_pair() {
        let api = MockZoneRpcApi::default();
        let resp = dispatch(&request("zone_getTopOfBook", json!([])), &auth(), &api).await;

        assert!(resp.result.is_none());
        let err = resp.error.expect("missing pair must error");
        assert_eq!(err.code, -32602);
        assert_eq!(err.message, "expected [{base, quote}]");
    }

    #[tokio::test]
    async fn dispatches_zone_get_midpoint_history_defaults_limit_when_omitted() {
        let api = MockZoneRpcApi::default();
        let base = format!("{:#x}", Address::repeat_byte(0x11));
        let quote = format!("{:#x}", Address::repeat_byte(0x22));

        let resp = dispatch(
            &request(
                "zone_getMidpointHistory",
                json!([{"base": base, "quote": quote}, "1m"]),
            ),
            &auth(),
            &api,
        )
        .await;
        assert!(resp.error.is_none(), "{:?}", resp.error);
        assert_eq!(
            api.last_midpoint_limit.load(Ordering::Relaxed),
            DEFAULT_MIDPOINT_HISTORY_LIMIT as u64,
        );
        assert!(api.last_midpoint_cursor.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn dispatches_zone_get_midpoint_history_caps_limit() {
        let api = MockZoneRpcApi::default();
        let base = format!("{:#x}", Address::repeat_byte(0x11));
        let quote = format!("{:#x}", Address::repeat_byte(0x22));

        let resp = dispatch(
            &request(
                "zone_getMidpointHistory",
                json!([
                    {"base": base, "quote": quote},
                    "1m",
                    MAX_MIDPOINT_HISTORY_LIMIT + 99,
                    "cur-1",
                ]),
            ),
            &auth(),
            &api,
        )
        .await;
        assert!(resp.error.is_none(), "{:?}", resp.error);
        assert_eq!(
            api.last_midpoint_limit.load(Ordering::Relaxed),
            MAX_MIDPOINT_HISTORY_LIMIT as u64,
        );
        assert_eq!(
            api.last_midpoint_cursor.lock().unwrap().as_deref(),
            Some("cur-1"),
        );
    }

    #[tokio::test]
    async fn rejects_numeric_zone_get_deposit_status_param() {
        let api = MockZoneRpcApi::default();

        let resp = dispatch(&request("zone_getDepositStatus", json!([7])), &auth(), &api).await;
        assert!(resp.result.is_none());
        assert_eq!(resp.error.as_ref().unwrap().code, -32602);
    }

    #[tokio::test]
    async fn dispatches_zone_get_withdrawal_status_for_tx_hash() {
        let api = MockZoneRpcApi::default();
        let hash = B256::repeat_byte(0x42);

        let resp = dispatch(
            &request("zone_getWithdrawalStatus", json!([format!("{:#x}", hash)])),
            &auth(),
            &api,
        )
        .await;
        assert!(resp.error.is_none());
        assert_eq!(
            *api.last_withdrawal_query.lock().unwrap(),
            Some(WithdrawalStatusQuery::TxHash(hash))
        );
    }

    #[tokio::test]
    async fn dispatches_zone_get_withdrawal_status_for_index() {
        let api = MockZoneRpcApi::default();

        let resp = dispatch(
            &request("zone_getWithdrawalStatus", json!(["0x2a"])),
            &auth(),
            &api,
        )
        .await;
        assert!(resp.error.is_none());
        assert_eq!(
            *api.last_withdrawal_query.lock().unwrap(),
            Some(WithdrawalStatusQuery::WithdrawalIndex(42))
        );
    }

    #[tokio::test]
    async fn rejects_non_hex_zone_get_withdrawal_status_param() {
        let api = MockZoneRpcApi::default();

        let resp = dispatch(
            &request("zone_getWithdrawalStatus", json!(["not-a-hex"])),
            &auth(),
            &api,
        )
        .await;
        assert!(resp.result.is_none());
        let err = resp.error.expect("invalid params should error");
        assert_eq!(err.code, -32602);
        assert_eq!(err.message, "expected [txHashOrWithdrawalIndex]");
    }

    #[test]
    fn parses_tx_hash_for_withdrawal_status_query() {
        let hash = B256::repeat_byte(0x42);
        let parsed = parse_withdrawal_status_query(&format!("{:#x}", hash)).unwrap();
        assert_eq!(parsed, WithdrawalStatusQuery::TxHash(hash));
    }

    #[test]
    fn parses_short_hex_quantity_as_withdrawal_index() {
        let parsed = parse_withdrawal_status_query("0x2a").unwrap();
        assert_eq!(parsed, WithdrawalStatusQuery::WithdrawalIndex(42));
    }

    #[test]
    fn rejects_non_hex_withdrawal_status_query() {
        let err = parse_withdrawal_status_query("not-a-hex").unwrap_err();
        assert_eq!(err.code, -32602);
        assert_eq!(err.message, "expected [txHashOrWithdrawalIndex]");
    }

    #[tokio::test]
    async fn rejects_numeric_zone_get_withdrawal_status_param() {
        let api = MockZoneRpcApi::default();

        let resp = dispatch(
            &request("zone_getWithdrawalStatus", json!([7])),
            &auth(),
            &api,
        )
        .await;
        assert!(resp.result.is_none());
        assert_eq!(resp.error.as_ref().unwrap().code, -32602);
    }

    #[tokio::test]
    async fn rejects_state_override_for_eth_call() {
        let api = MockZoneRpcApi::default();
        let resp = dispatch(
            &request(
                "eth_call",
                json!([
                    {"to": format!("{:#x}", Address::repeat_byte(0x11)), "data": "0x"},
                    "latest",
                    {}
                ]),
            ),
            &auth(),
            &api,
        )
        .await;

        assert!(resp.result.is_none());
        let err = resp.error.expect("should reject state overrides");
        assert_eq!(err.code, -32602);
        assert_eq!(err.message, "state overrides not allowed");
    }

    #[tokio::test]
    async fn rejects_state_override_for_estimate_gas() {
        let api = MockZoneRpcApi::default();
        let resp = dispatch(
            &request(
                "eth_estimateGas",
                json!([
                    {"to": format!("{:#x}", Address::repeat_byte(0x11)), "data": "0x"},
                    "latest",
                    {}
                ]),
            ),
            &auth(),
            &api,
        )
        .await;

        assert!(resp.result.is_none());
        let err = resp.error.expect("should reject state overrides");
        assert_eq!(err.code, -32602);
        assert_eq!(err.message, "state overrides not allowed");
    }

    #[tokio::test]
    async fn parses_list_batches_with_no_params() {
        let result = parse_list_batches_params("[]").expect("empty params should parse");
        assert!(result.limit.is_none());
        assert!(result.cursor.is_none());
    }

    #[tokio::test]
    async fn parses_list_batches_with_object_form() {
        let result = parse_list_batches_params("[{\"limit\":10,\"cursor\":\"0x2a\"}]")
            .expect("object form should parse");
        assert_eq!(result.limit, Some(10));
        assert_eq!(result.cursor, Some(U64::from(42)));
    }

    #[tokio::test]
    async fn parses_list_batches_with_tuple_form() {
        let result = parse_list_batches_params("[10, \"0x2a\"]").expect("tuple form should parse");
        assert_eq!(result.limit, Some(10));
        assert_eq!(result.cursor, Some(U64::from(42)));
    }

    #[tokio::test]
    async fn dispatches_zone_get_batch_for_hex_quantity() {
        let api = MockZoneRpcApi::default();
        let resp = dispatch(&request("zone_getBatch", json!(["0x2a"])), &auth(), &api).await;
        assert!(resp.result.is_none());
        let err = resp.error.as_ref().expect("default impl returns error");
        assert_eq!(err.code, -32006);
    }

    #[tokio::test]
    async fn dispatches_zone_list_batches_with_empty_params() {
        let api = MockZoneRpcApi::default();
        let resp = dispatch(&request("zone_listBatches", json!([])), &auth(), &api).await;
        let err = resp.error.as_ref().expect("default impl returns error");
        assert_eq!(err.code, -32006);
    }

    #[tokio::test]
    async fn rejects_numeric_zone_get_batch_param() {
        let api = MockZoneRpcApi::default();
        let resp = dispatch(&request("zone_getBatch", json!([7])), &auth(), &api).await;
        assert!(resp.result.is_none());
        assert_eq!(resp.error.as_ref().unwrap().code, -32602);
    }

    #[tokio::test]
    async fn rejects_eth_send_transaction_with_account_method_error() {
        let api = MockZoneRpcApi::default();
        let resp = dispatch(
            &request(
                "eth_sendTransaction",
                json!([{
                    "from": format!("{:#x}", Address::repeat_byte(0xaa)),
                    "to": format!("{:#x}", Address::repeat_byte(0x11)),
                    "data": "0x",
                }]),
            ),
            &auth(),
            &api,
        )
        .await;

        assert!(resp.result.is_none());
        let err = resp.error.expect("should reject eth_sendTransaction");
        assert_eq!(err.code, -32004);
        assert!(
            err.message.contains("eth_sendTransaction")
                && err.message.contains("eth_sendRawTransaction"),
            "error should point at the raw-tx path; got: {}",
            err.message,
        );
    }

    #[tokio::test]
    async fn rejects_eth_sign_transaction_with_account_method_error() {
        let api = MockZoneRpcApi::default();
        let resp = dispatch(
            &request(
                "eth_signTransaction",
                json!([{
                    "from": format!("{:#x}", Address::repeat_byte(0xaa)),
                    "to": format!("{:#x}", Address::repeat_byte(0x11)),
                    "data": "0x",
                }]),
            ),
            &auth(),
            &api,
        )
        .await;

        let err = resp.error.expect("should reject eth_signTransaction");
        assert_eq!(err.code, -32004);
        assert!(err.message.contains("eth_signTransaction"));
    }

    #[tokio::test]
    async fn rejects_extra_block_override_param_for_eth_call() {
        let api = MockZoneRpcApi::default();
        let resp = dispatch(
            &request(
                "eth_call",
                json!([
                    {"to": format!("{:#x}", Address::repeat_byte(0x11)), "data": "0x"},
                    "latest",
                    {},
                    {}
                ]),
            ),
            &auth(),
            &api,
        )
        .await;

        assert!(resp.result.is_none());
        let err = resp.error.expect("should reject extra simulation params");
        assert_eq!(err.code, -32602);
        assert_eq!(err.message, "expected [request, block?, stateOverride?]");
    }
}
