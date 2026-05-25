//! JSON-RPC types for the private zone RPC.

use std::{future::Future, pin::Pin};

use alloy_primitives::{Address, B256, U64, U128, U256};
use serde::{Deserialize, Serialize};
use serde_json::{Value, value::RawValue};

/// Shorthand for the boxed future returned by [`ZoneRpcApi`](crate::handlers::ZoneRpcApi) methods.
///
/// Returns pre-serialized JSON ([`RawValue`]) to avoid an intermediate
/// `serde_json::Value` allocation — the result is embedded verbatim in
/// the JSON-RPC response.
pub type BoxFut<'a> =
    Pin<Box<dyn Future<Output = Result<Box<RawValue>, JsonRpcError>> + Send + 'a>>;

/// Shorthand for typed boxed futures returned by internal async helpers.
pub type BoxEyreFut<'a, T> = Pin<Box<dyn Future<Output = eyre::Result<T>> + Send + 'a>>;

/// A JSON-RPC 2.0 request.
#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcRequest {
    /// The JSON-RPC version (must be "2.0").
    pub jsonrpc: String,
    /// The method name.
    pub method: String,
    /// The parameters (raw JSON).
    pub params: Option<Box<serde_json::value::RawValue>>,
    /// The request ID.
    pub id: Value,
}

/// A JSON-RPC 2.0 response.
#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcResponse {
    /// The JSON-RPC version.
    pub jsonrpc: &'static str,
    /// The result, if successful (embedded as pre-serialized JSON).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Box<RawValue>>,
    /// The error, if failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
    /// The request ID.
    pub id: Value,
}

impl JsonRpcResponse {
    /// Create a successful response from a pre-serialized result.
    pub fn success(id: Value, result: Box<RawValue>) -> Self {
        Self {
            jsonrpc: "2.0",
            result: Some(result),
            error: None,
            id,
        }
    }

    /// Create an error response.
    pub fn error(id: Value, error: JsonRpcError) -> Self {
        Self {
            jsonrpc: "2.0",
            result: None,
            error: Some(error),
            id,
        }
    }
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    /// The error code.
    pub code: i64,
    /// The error message.
    pub message: String,
    /// Optional additional data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl std::fmt::Display for JsonRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (code {})", self.message, self.code)
    }
}

impl JsonRpcError {
    /// Method not found (-32601).
    pub fn method_not_found() -> Self {
        Self {
            code: -32601,
            message: "Method not found".to_string(),
            data: None,
        }
    }

    /// Method disabled (-32006).
    pub fn method_disabled() -> Self {
        Self {
            code: -32006,
            message: "Method disabled".to_string(),
            data: None,
        }
    }

    /// Sequencer-only method (-32005).
    pub fn sequencer_only() -> Self {
        Self {
            code: -32005,
            message: "Sequencer only".to_string(),
            data: None,
        }
    }

    /// Invalid params (-32602).
    pub fn invalid_params(msg: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: msg.into(),
            data: None,
        }
    }

    /// Transaction rejected — sender mismatch (-32003).
    pub fn transaction_rejected() -> Self {
        Self {
            code: -32003,
            message: "Transaction rejected".to_string(),
            data: None,
        }
    }

    /// Account mismatch — `from` does not match authenticated account (-32004).
    pub fn account_mismatch() -> Self {
        Self {
            code: -32004,
            message: "Account mismatch".to_string(),
            data: None,
        }
    }

    /// Unsupported account-management method (-32004).
    ///
    /// Returned for JSON-RPC methods that assume the node owns the caller's
    /// signing key (e.g. `eth_sendTransaction`). The private zone RPC never
    /// holds caller keys — clients must sign the transaction locally and
    /// submit it via `eth_sendRawTransaction` or `eth_sendRawTransactionSync`.
    pub fn unsupported_account_method(method: &str) -> Self {
        Self {
            code: -32004,
            message: format!(
                "{method} is not supported: the private zone RPC does not hold \
                 caller signing keys. Sign the transaction client-side and \
                 submit it via eth_sendRawTransaction or eth_sendRawTransactionSync."
            ),
            data: None,
        }
    }

    /// Parse error — invalid JSON (-32700).
    pub fn parse_error(msg: impl Into<String>) -> Self {
        Self {
            code: -32700,
            message: msg.into(),
            data: None,
        }
    }

    /// Internal error (-32603).
    pub fn internal(msg: impl Into<String>) -> Self {
        Self {
            code: -32603,
            message: msg.into(),
            data: None,
        }
    }
}

/// Response payload for `zone_getAuthorizationTokenInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorizationTokenInfoResponse {
    /// Authenticated account derived from the authorization token.
    pub account: Address,
    /// Expiration timestamp encoded as a JSON-RPC quantity.
    pub expires_at: U64,
}

/// Response payload for `zone_getZoneInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ZoneInfoResponse {
    /// The zone's numeric identifier.
    pub zone_id: U64,
    /// The enabled zone token contract addresses.
    pub zone_tokens: Vec<Address>,
    /// The zone chain ID.
    pub chain_id: U64,
}

/// Response payload for `zone_getDepositStatus`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DepositStatusResponse {
    /// The Tempo block number queried by the caller.
    pub tempo_block_number: U64,
    /// The latest Tempo block number processed on the zone.
    pub zone_processed_through: U64,
    /// Whether every relevant deposit for `tempo_block_number` has reached a terminal state.
    pub processed: bool,
    /// Deposits relevant to the authenticated caller.
    pub deposits: Vec<DepositStatusEntry>,
}

/// Per-deposit status entry returned by `zone_getDepositStatus`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DepositStatusEntry {
    /// The deposit queue hash used to correlate portal and inbox events.
    pub deposit_hash: B256,
    /// Whether the deposit is regular or encrypted.
    pub kind: DepositKind,
    /// The deposited token address.
    pub token: Address,
    /// The L1 sender who initiated the deposit.
    pub sender: Address,
    /// The revealed recipient, if visible to the caller.
    pub recipient: Option<Address>,
    /// The deposited amount.
    pub amount: U256,
    /// The revealed memo, if visible to the caller.
    pub memo: Option<B256>,
    /// The current terminal or pending state of the deposit.
    pub status: DepositState,
}

/// Deposit kind returned by `zone_getDepositStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DepositKind {
    /// A plaintext deposit emitted by the portal.
    Regular,
    /// A deposit whose recipient and memo remain hidden until revealed on L2.
    Encrypted,
}

/// Processing state returned by `zone_getDepositStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DepositState {
    /// The deposit has not yet reached a terminal L2 inbox event.
    Pending,
    /// The deposit was processed successfully on L2.
    Processed,
    /// The encrypted deposit reached an explicit failure event on L2.
    Failed,
}

/// Canonical market token metadata returned by `zone_getMarketConfig`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketToken {
    /// The token contract address.
    pub address: Address,
    /// Canonical display symbol.
    pub symbol: String,
    /// TIP-20 decimals.
    pub decimals: u8,
}

/// Order action supported by a market.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MarketAction {
    /// Taker buy at the best available ask price.
    MarketBuy,
    /// Taker sell at the best available bid price.
    MarketSell,
    /// Resting buy order at a fixed limit price.
    LimitBid,
    /// Resting sell order at a fixed limit price.
    LimitAsk,
}

/// Per-pair market entry returned by `zone_getMarketConfig`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketEntry {
    /// Display label `"<base>/<quote>"`.
    pub pair: String,
    /// Base token metadata.
    pub base: MarketToken,
    /// Quote token metadata.
    pub quote: MarketToken,
    /// Minimum order quantity in base-token units.
    pub min_order_amount: U128,
    /// Human-readable description of the price representation.
    pub price_unit: String,
    /// Order actions the darkpool supports for this pair.
    pub allowed_actions: Vec<MarketAction>,
}

/// Response payload for `zone_getMarketConfig`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketConfigResponse {
    /// The darkpool orderbook contract address.
    pub darkpool: Address,
    /// Markets currently exposed to the frontend.
    pub markets: Vec<MarketEntry>,
}

/// Pair selector accepted by market RPCs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketPair {
    /// Base token address.
    pub base: Address,
    /// Quote token address.
    pub quote: Address,
}

/// Aggregate price/quantity level at one side of the book.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderLevel {
    /// Price in raw integer units.
    pub price: U128,
    /// Aggregate resting quantity at this price level.
    pub quantity: U128,
}

/// Response payload for `zone_getTopOfBook`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TopOfBookResponse {
    /// Display label for the pair.
    pub pair: String,
    /// Base token address.
    pub base: Address,
    /// Quote token address.
    pub quote: Address,
    /// Best resting bid, or `null` when the book has no bids.
    pub bid: Option<OrderLevel>,
    /// Best resting ask, or `null` when the book has no asks.
    pub ask: Option<OrderLevel>,
    /// Arithmetic midpoint when both sides exist.
    pub midpoint: Option<U128>,
    /// Spread when both sides exist.
    pub spread: Option<U128>,
    /// Zone L2 block number used to read the book.
    pub as_of_block: U64,
}

/// History availability tag returned by `zone_getMidpointHistory`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryAvailability {
    /// Whether the backend currently emits midpoint samples.
    pub enabled: bool,
    /// Human-readable rationale when disabled.
    pub reason: String,
}

/// Single midpoint sample.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MidpointSample {
    /// Bucket end timestamp.
    pub timestamp: U64,
    /// Midpoint price for the bucket in raw integer units.
    pub midpoint: U128,
}

/// Response payload for `zone_getMidpointHistory`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MidpointHistoryResponse {
    /// Display label for the pair.
    pub pair: String,
    /// Base token address.
    pub base: Address,
    /// Quote token address.
    pub quote: Address,
    /// Bucket interval echoed from the request.
    pub interval: String,
    /// Aggregated samples.
    pub samples: Vec<MidpointSample>,
    /// Cursor for paginating older samples.
    pub next_cursor: Option<String>,
    /// Backend availability flag.
    pub history: HistoryAvailability,
}

/// Query parameter for `zone_getWithdrawalStatus`.
///
/// Callers identify a withdrawal either by the zone L2 transaction hash that
/// emitted `WithdrawalRequested` or by the global withdrawal index assigned by
/// the outbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WithdrawalStatusQuery {
    /// The zone L2 transaction hash that emitted `WithdrawalRequested`.
    TxHash(B256),
    /// The global withdrawal index assigned by `ZoneOutbox.requestWithdrawal`.
    WithdrawalIndex(u64),
}

/// Lifecycle state returned by `zone_getWithdrawalStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WithdrawalState {
    /// `WithdrawalRequested` emitted on L2, no `BatchFinalized` yet.
    Pending,
    /// `BatchFinalized` emitted on L2, batch has not landed on L1.
    Batched,
    /// `BatchSubmitted` emitted on L1, withdrawal not yet processed.
    Submitted,
    /// `WithdrawalProcessed` emitted on L1 with `callbackSuccess = true`.
    Processed,
    /// `WithdrawalProcessed` emitted on L1 with `callbackSuccess = false`
    /// but no accompanying `BounceBack`.
    Failed,
    /// `BounceBack` emitted on L1 — funds returned to `fallbackRecipient`.
    Bounced,
}

/// Response payload for `zone_getWithdrawalStatus`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WithdrawalStatusResponse {
    /// Global withdrawal index from `ZoneOutbox.WithdrawalRequested.withdrawalIndex`.
    pub withdrawal_index: U64,
    /// Zone L2 transaction hash that emitted `WithdrawalRequested`.
    pub zone_tx_hash: B256,
    /// Current lifecycle state.
    pub status: WithdrawalState,
    /// Token being withdrawn.
    pub token: Address,
    /// Withdrawn amount.
    pub amount: U256,
    /// L1 recipient of the withdrawal.
    pub to: Address,
    /// L1 fallback recipient used if the callback reverts.
    pub fallback_recipient: Address,
    /// Memo attached to the withdrawal.
    pub memo: B256,
    /// Zone L2 block number containing the `WithdrawalRequested` event.
    pub zone_block_number: U64,
    /// Zone-side `withdrawalBatchIndex` from `BatchFinalized`, if the batch was sealed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub withdrawal_batch_index: Option<U64>,
    /// L1 portal queue slot assigned to this batch, if it has landed on L1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub portal_slot: Option<U64>,
    /// L1 transaction hash of the `submitBatch` that landed the batch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub l1_submit_batch_tx_hash: Option<B256>,
    /// L1 transaction hash of the `processWithdrawal` that settled this withdrawal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub l1_process_withdrawal_tx_hash: Option<B256>,
    /// Whether the L1 callback executed without reverting, when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callback_success: Option<bool>,
    /// Human-readable error description for terminal failure states.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Settlement state of a sequencer batch returned by the batch explorer methods.
///
/// The batch explorer methods (`zone_listBatches`, `zone_getBatch`,
/// `zone_searchBatch`) only return public, aggregate-only batch metadata. They
/// never include per-user data; caller-scoped data belongs in private methods
/// such as `zone_getDepositStatus` and `zone_getWithdrawalStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BatchStatus {
    /// Batch was sealed by the sequencer but no L1 `BatchSubmitted` event has
    /// been observed yet. Reserved; listing endpoints only surface L1 batches.
    Pending,
    /// `BatchSubmitted` was observed on L1.
    Submitted,
    /// Reserved for future use once meaningful proof verification is enforced.
    Verified,
    /// L1 settlement attempt failed. Reserved; current explorer reads only
    /// successful events.
    Failed,
}

/// Aggregate per-token settled volume for a batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchAggregateVolume {
    /// Settled token address.
    pub token: Address,
    /// Aggregate amount settled for `token` in this batch.
    pub amount: U256,
}

/// Aggregate-only summary of a single sequencer batch.
///
/// **Privacy:** This response is intentionally caller-agnostic. It must not
/// include owner-linked fields, per-order ids, per-fill data, or counterparty
/// information.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchSummary {
    /// L1 portal `withdrawalBatchIndex` for this batch.
    pub batch_number: U64,
    /// First zone L2 block included in the batch, if resolvable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zone_block_from: Option<U64>,
    /// Last zone L2 block included in the batch, if resolvable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zone_block_to: Option<U64>,
    /// Tempo L1 block number anchored by `submitBatch`.
    pub tempo_block_number: U64,
    /// Withdrawal queue hash for this batch.
    pub root: B256,
    /// Portal `blockHash` before this batch was applied.
    pub prev_block_hash: B256,
    /// Portal `blockHash` after this batch was applied.
    pub next_block_hash: B256,
    /// Settlement state.
    pub status: BatchStatus,
    /// Zone block timestamp at `zone_block_to`, when resolvable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sealed_at: Option<U64>,
    /// L1 block timestamp of the `BatchSubmitted` event.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settled_at: Option<U64>,
    /// Aggregate order count. Reserved for higher-layer indexing; zero today.
    pub order_count: U64,
    /// Aggregate fill count. Reserved for higher-layer indexing; zero today.
    pub fill_count: U64,
    /// Aggregate trading pair tags settled in the batch.
    pub aggregate_pairs: Vec<String>,
    /// Aggregate per-token volume settled by the batch.
    pub aggregate_volume: Vec<BatchAggregateVolume>,
    /// L1 transaction hash that emitted `BatchSubmitted`.
    pub settlement_tx_hash: B256,
    /// Reference to the settlement proof, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proof_ref: Option<String>,
}

/// Pagination parameters for `zone_listBatches`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListBatchesParams {
    /// Maximum number of summaries to return.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Exclusive cursor batch number. Omit to start from the newest batch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<U64>,
}

/// Default page size for `zone_listBatches`.
pub const LIST_BATCHES_DEFAULT_LIMIT: u32 = 20;

/// Maximum page size for `zone_listBatches`.
pub const LIST_BATCHES_MAX_LIMIT: u32 = 100;

/// Response payload for `zone_listBatches`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchListResponse {
    /// Returned batches in descending `batchNumber` order.
    pub batches: Vec<BatchSummary>,
    /// Cursor for the next page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<U64>,
}

/// Method access tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodTier {
    /// Available to all authenticated callers.
    Public,
    /// Only available to the sequencer.
    Restricted,
    /// Disabled on the private RPC.
    Disabled,
    /// The method assumes the node owns a caller signing key, which the
    /// private zone RPC never does. Returns a clear pointer to the
    /// `eth_sendRawTransaction` path.
    UnsupportedAccountManagement,
}

/// Classify a JSON-RPC method into its access tier.
///
/// Returns `None` if the method is unknown.
pub fn classify_method(method: &str) -> Option<MethodTier> {
    match method {
        // Public read methods — no privacy redaction needed
        "eth_blockNumber"
        | "eth_chainId"
        | "eth_gasPrice"
        | "eth_getBalance"
        | "eth_getTransactionCount"
        | "eth_call"
        | "eth_estimateGas"
        | "eth_feeHistory"
        | "eth_maxPriorityFeePerGas"
        | "eth_getBlockByNumber"
        | "eth_getBlockByHash"
        | "eth_syncing"
        | "eth_coinbase"
        | "net_version"
        | "net_listening"
        | "web3_clientVersion"
        | "web3_sha3"
        | "zone_getAuthorizationTokenInfo"
        | "zone_getZoneInfo"
        | "zone_getDepositStatus"
        | "zone_getWithdrawalStatus"
        | "zone_listBatches"
        | "zone_getBatch"
        | "zone_searchBatch"
        | "zone_getMarketConfig"
        | "zone_getTopOfBook"
        | "zone_getMidpointHistory"
        | "zone_getMyOrders"
        | "zone_getMyFills"
        | "zone_getMyTransfers"
        | "zone_getOrder" => Some(MethodTier::Public),

        // Fetch-then-check: public but redacted based on caller identity
        "eth_getTransactionByHash"
        | "eth_getTransactionReceipt"
        | "eth_getLogs"
        | "eth_getFilterLogs"
        | "eth_getFilterChanges"
        | "eth_newFilter"
        | "eth_newBlockFilter"
        | "eth_uninstallFilter" => Some(MethodTier::Public),

        // Transaction preparation: public (scoped to caller's account)
        "eth_fillTransaction" => Some(MethodTier::Public),

        // Transaction submission: public (caller sends their own txs)
        "eth_sendRawTransaction" | "eth_sendRawTransactionSync" => Some(MethodTier::Public),

        // Unsupported account-management methods — the node never holds caller
        // signing keys, so these always fail. Dispatch intercepts them with a
        // dedicated error message pointing wallets at `eth_sendRawTransaction`.
        "eth_sendTransaction" | "eth_signTransaction" => {
            Some(MethodTier::UnsupportedAccountManagement)
        }

        // Sequencer-only — raw state inspection and full block data bypass privacy scoping
        "eth_getCode"
        | "eth_getStorageAt"
        | "eth_getBlockReceipts"
        | "debug_traceTransaction"
        | "debug_traceBlockByNumber"
        | "debug_traceBlockByHash"
        | "eth_createAccessList"
        | "eth_getBlockTransactionCountByNumber"
        | "eth_getBlockTransactionCountByHash"
        | "eth_getTransactionByBlockNumberAndIndex"
        | "eth_getTransactionByBlockHashAndIndex"
        | "eth_getUncleCountByBlockNumber"
        | "eth_getUncleCountByBlockHash"
        | "txpool_content"
        | "txpool_status"
        | "txpool_inspect" => Some(MethodTier::Restricted),

        // Disabled (mining, subscriptions not supported via HTTP proxy)
        "eth_mining" | "eth_hashrate" | "eth_submitWork" | "eth_submitHashrate"
        | "eth_subscribe" | "eth_unsubscribe" => Some(MethodTier::Disabled),

        _ if method.starts_with("admin_") => Some(MethodTier::Restricted),
        _ => None,
    }
}

/// Pre-serialized JSON `null`.
pub fn raw_null() -> Box<RawValue> {
    RawValue::from_string("null".to_string()).unwrap()
}

/// Pre-serialized JSON `"0x0"` — returned as a silent dummy for scoped queries
/// about non-caller accounts (e.g. `eth_getBalance`, `eth_getTransactionCount`).
pub fn raw_zero() -> Box<RawValue> {
    serde_json::value::to_raw_value(&U256::ZERO).unwrap()
}

/// Serialize a value directly to [`RawValue`], skipping the intermediate
/// `serde_json::Value` allocation.
pub fn to_raw<T: serde::Serialize>(value: &T) -> Result<Box<RawValue>, JsonRpcError> {
    serde_json::value::to_raw_value(value).map_err(|e| JsonRpcError::internal(e.to_string()))
}

/// Shorthand for wrapping any `Display` error into a [`JsonRpcError::internal`].
pub fn internal(e: impl std::fmt::Display) -> JsonRpcError {
    JsonRpcError::internal(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn alpha_market_entry() -> MarketEntry {
        MarketEntry {
            pair: "OALPHA/PATH.USD".to_string(),
            base: MarketToken {
                address: "0x20C000000000000000000000518dDADD37eD1d28"
                    .parse()
                    .unwrap(),
                symbol: "OALPHA".to_string(),
                decimals: 6,
            },
            quote: MarketToken {
                address: "0x20C0000000000000000000000000000000000000"
                    .parse()
                    .unwrap(),
                symbol: "PATH.USD".to_string(),
                decimals: 6,
            },
            min_order_amount: U128::from(100u128),
            price_unit: "raw integer; quote = baseAmount * price".to_string(),
            allowed_actions: vec![
                MarketAction::MarketBuy,
                MarketAction::MarketSell,
                MarketAction::LimitBid,
                MarketAction::LimitAsk,
            ],
        }
    }

    #[test]
    fn market_config_serializes_canonical_alpha_addresses() {
        let response = MarketConfigResponse {
            darkpool: "0x0B00000000000000000000000000000000000001"
                .parse()
                .unwrap(),
            markets: vec![alpha_market_entry()],
        };

        let value = serde_json::to_value(&response).unwrap();
        let market = &value["markets"][0];
        assert_eq!(
            value["darkpool"],
            "0x0b00000000000000000000000000000000000001"
        );
        assert_eq!(market["pair"], "OALPHA/PATH.USD");
        assert_eq!(
            market["base"]["address"],
            "0x20c000000000000000000000518ddadd37ed1d28"
        );
        assert_eq!(
            market["allowedActions"],
            json!(["marketBuy", "marketSell", "limitBid", "limitAsk"])
        );
    }

    #[test]
    fn market_pair_deserializes_from_object() {
        let raw = json!({
            "base":  "0x20C000000000000000000000518dDADD37eD1d28",
            "quote": "0x20C0000000000000000000000000000000000000",
        });
        let pair: MarketPair = serde_json::from_value(raw).unwrap();
        assert_eq!(
            pair.base,
            "0x20C000000000000000000000518dDADD37eD1d28"
                .parse::<Address>()
                .unwrap()
        );
    }

    #[test]
    fn new_market_methods_are_in_the_public_allowlist() {
        assert!(matches!(
            classify_method("zone_getMarketConfig"),
            Some(MethodTier::Public)
        ));
        assert!(matches!(
            classify_method("zone_getTopOfBook"),
            Some(MethodTier::Public)
        ));
        assert!(matches!(
            classify_method("zone_getMidpointHistory"),
            Some(MethodTier::Public)
        ));
    }
}
