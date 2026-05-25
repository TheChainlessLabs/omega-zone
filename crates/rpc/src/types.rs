//! JSON-RPC types for the private zone RPC.

use std::{future::Future, pin::Pin};

use alloy_primitives::{Address, B256, U64, U256};
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

/// Settlement state of a sequencer batch returned by the batch explorer methods.
///
/// The batch explorer methods (`zone_listBatches`, `zone_getBatch`,
/// `zone_searchBatch`) only return public, aggregate-only batch metadata. They
/// never include per-user data — that is the responsibility of the private,
/// caller-scoped methods such as `zone_getDepositStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BatchStatus {
    /// Batch was sealed by the sequencer but no `BatchSubmitted` event has been
    /// observed on L1 yet. Reserved — listing endpoints only surface batches
    /// already on L1, so this value is not emitted today.
    Pending,
    /// `BatchSubmitted` was observed on L1 (the verifier accepted the proof and
    /// the portal state advanced). For v1 — where the verifier accepts an empty
    /// proof — this is the terminal happy-path state.
    Submitted,
    /// Reserved for future use once meaningful proof verification is enforced
    /// on L1. Not emitted today.
    Verified,
    /// L1 settlement attempt failed (tx reverted or otherwise rejected).
    /// Reserved — the current explorer reads only events emitted on success,
    /// so this value is not emitted today.
    Failed,
}

/// Aggregate per-token settled volume for a batch.
///
/// Sums withdrawal amounts that the L1 portal exposed via aggregate hash-chain
/// events. Never includes per-sender or per-recipient information.
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
/// **Privacy:** This response is intentionally caller-agnostic. It MUST NOT
/// include owner-linked fields (user addresses, per-order ids, per-fill data,
/// counterparty information). Anything that would distinguish one caller's
/// experience from another's belongs in a private, scoped RPC method.
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
    /// Tempo L1 block number anchored by `submitBatch`'s `tempoBlockNumber`
    /// argument (decoded from the L1 calldata).
    pub tempo_block_number: U64,
    /// Withdrawal queue hash for this batch — the cryptographic anchor that
    /// commits the batch's aggregated withdrawal set.
    pub root: B256,
    /// Portal `blockHash` value before this batch was applied.
    pub prev_block_hash: B256,
    /// Portal `blockHash` value after this batch was applied.
    pub next_block_hash: B256,
    /// Settlement state. See [`BatchStatus`].
    pub status: BatchStatus,
    /// Zone block timestamp at `zone_block_to`, when resolvable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sealed_at: Option<U64>,
    /// L1 block timestamp at `tempo_block_number` of the `BatchSubmitted` event.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settled_at: Option<U64>,
    /// Aggregate count of orders included in the batch.
    ///
    /// "Order" granularity is not directly tracked on-chain at the zone layer;
    /// today this surfaces the on-chain withdrawal count (which is a safe
    /// public aggregate). Set to `0x0` when no withdrawals settle in the batch.
    pub order_count: U64,
    /// Aggregate count of fills. Reserved for future use once an explicit Fill
    /// concept is plumbed through; the zone layer does not track fills today.
    pub fill_count: U64,
    /// Aggregate trading pair tags settled in the batch. The zone layer does
    /// not have pair semantics, so this is always empty here; higher-layer
    /// services may populate it before exposing to the explorer UI.
    pub aggregate_pairs: Vec<String>,
    /// Aggregate per-token volume settled by the batch. Empty in v1 — the
    /// portal exposes only the withdrawal hash chain, not a per-token sum.
    pub aggregate_volume: Vec<BatchAggregateVolume>,
    /// L1 transaction hash that emitted `BatchSubmitted` for this batch.
    pub settlement_tx_hash: B256,
    /// Reference to the settlement proof (e.g. `tee:<attestation-id>`), when
    /// known. `None` in v1 because the verifier accepts an empty proof and no
    /// attestation is recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proof_ref: Option<String>,
}

/// Pagination parameters for `zone_listBatches`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListBatchesParams {
    /// Maximum number of summaries to return. Server caps at
    /// [`LIST_BATCHES_MAX_LIMIT`]; defaults to [`LIST_BATCHES_DEFAULT_LIMIT`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Batch number to page from — the server returns batches strictly older
    /// than `cursor`. Omit to start from the newest batch.
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
    /// Cursor for the next page. Pass this as `cursor` in the next call.
    /// `None` when no older batches remain.
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
        | "zone_listBatches"
        | "zone_getBatch"
        | "zone_searchBatch" => Some(MethodTier::Public),

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

        // Sequencer-only — raw state inspection and full block data bypass privacy scoping
        "eth_getCode"
        | "eth_getStorageAt"
        | "eth_getBlockReceipts"
        | "eth_sendTransaction"
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
