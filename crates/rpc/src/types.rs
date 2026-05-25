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
    /// Canonical display symbol (e.g. `"OALPHA"`, `"PATH.USD"`).
    pub symbol: String,
    /// TIP-20 decimals (alpha tokens are both 6-decimal).
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
    /// Display label `"<base>/<quote>"` built from the on-chain symbols.
    pub pair: String,
    /// Base token metadata.
    pub base: MarketToken,
    /// Quote token metadata.
    pub quote: MarketToken,
    /// Minimum order quantity in base-token units.
    pub min_order_amount: U128,
    /// Human-readable description of the price representation used by the darkpool.
    pub price_unit: String,
    /// Order actions the darkpool supports for this pair.
    pub allowed_actions: Vec<MarketAction>,
}

/// Response payload for `zone_getMarketConfig`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketConfigResponse {
    /// The darkpool orderbook contract address that hosts these markets.
    pub darkpool: Address,
    /// Markets currently exposed to the frontend.
    pub markets: Vec<MarketEntry>,
}

/// Pair selector accepted by `zone_getTopOfBook` and `zone_getMidpointHistory`.
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
    /// Price in raw integer units (quote = baseAmount * price).
    pub price: U128,
    /// Aggregate resting quantity at this price level.
    pub quantity: U128,
}

/// Response payload for `zone_getTopOfBook`.
///
/// Aggregate-only: never exposes individual order owners, counterparties, or
/// order identifiers — only the best bid/ask price and resting depth at that
/// level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TopOfBookResponse {
    /// Display label for the pair (`"<base>/<quote>"`).
    pub pair: String,
    /// Base token address.
    pub base: Address,
    /// Quote token address.
    pub quote: Address,
    /// Best resting bid, or `null` when the book has no bids.
    pub bid: Option<OrderLevel>,
    /// Best resting ask, or `null` when the book has no asks.
    pub ask: Option<OrderLevel>,
    /// Arithmetic midpoint `(bid.price + ask.price) / 2` when both sides exist.
    pub midpoint: Option<U128>,
    /// Spread `ask.price - bid.price` when both sides exist.
    pub spread: Option<U128>,
    /// Zone L2 block number used to read the book.
    pub as_of_block: U64,
}

/// History availability tag returned by `zone_getMidpointHistory`.
///
/// Alpha launches without a midpoint aggregator — this field tells the frontend
/// to keep the chart disabled rather than synthesize one client-side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryAvailability {
    /// Whether the backend currently emits midpoint samples.
    pub enabled: bool,
    /// Human-readable rationale shown to operators when `enabled` is `false`.
    pub reason: String,
}

/// Single midpoint sample.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MidpointSample {
    /// Bucket end timestamp in seconds since the Unix epoch.
    pub timestamp: U64,
    /// Midpoint price for the bucket in raw integer units.
    pub midpoint: U128,
}

/// Response payload for `zone_getMidpointHistory`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MidpointHistoryResponse {
    /// Display label for the pair (`"<base>/<quote>"`).
    pub pair: String,
    /// Base token address.
    pub base: Address,
    /// Quote token address.
    pub quote: Address,
    /// Bucket interval echoed from the request (e.g. `"1m"`, `"5m"`).
    pub interval: String,
    /// Aggregated samples — empty while [`HistoryAvailability::enabled`] is `false`.
    pub samples: Vec<MidpointSample>,
    /// Cursor for paginating older samples, or `null` when none more exist.
    pub next_cursor: Option<String>,
    /// Backend availability flag for the chart.
    pub history: HistoryAvailability,
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
        | "zone_getMarketConfig"
        | "zone_getTopOfBook"
        | "zone_getMidpointHistory" => Some(MethodTier::Public),

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
    fn market_config_serializes_camel_case_with_canonical_alpha_addresses() {
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
        assert_eq!(market["base"]["symbol"], "OALPHA");
        assert_eq!(market["base"]["decimals"], 6);
        assert_eq!(
            market["quote"]["address"],
            "0x20c0000000000000000000000000000000000000"
        );
        assert_eq!(market["quote"]["symbol"], "PATH.USD");
        assert_eq!(market["quote"]["decimals"], 6);
        assert_eq!(market["minOrderAmount"], "0x64");
        assert_eq!(
            market["priceUnit"],
            "raw integer; quote = baseAmount * price"
        );
        assert_eq!(
            market["allowedActions"],
            json!(["marketBuy", "marketSell", "limitBid", "limitAsk"])
        );
    }

    #[test]
    fn market_config_roundtrips_through_serde() {
        let original = MarketConfigResponse {
            darkpool: "0x0B00000000000000000000000000000000000001"
                .parse()
                .unwrap(),
            markets: vec![alpha_market_entry()],
        };

        let json = serde_json::to_string(&original).unwrap();
        let parsed: MarketConfigResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn unit_price_one_displays_as_one_for_matched_decimals() {
        // Per the issue: with both tokens at 6 decimals, the raw integer
        // price `1` represents `1.000000` because
        //   quote_raw = base_raw * price
        //   quote_display = quote_raw / 10^quote_decimals
        //   base_display  = base_raw  / 10^base_decimals
        // so for equal decimals the displayed ratio equals `price` exactly.
        let base_decimals: i32 = 6;
        let quote_decimals: i32 = 6;
        let base_raw: u128 = 1_000_000; // 1.000000 base
        let price_raw: u128 = 1;

        let quote_raw = base_raw * price_raw;
        let base_display = base_raw as f64 / 10f64.powi(base_decimals);
        let quote_display = quote_raw as f64 / 10f64.powi(quote_decimals);
        assert_eq!(quote_display, base_display);
        assert_eq!(quote_display / base_display, 1.0);
    }

    #[test]
    fn top_of_book_serializes_null_sides_when_book_is_empty() {
        let response = TopOfBookResponse {
            pair: "OALPHA/PATH.USD".to_string(),
            base: Address::repeat_byte(0xaa),
            quote: Address::repeat_byte(0xbb),
            bid: None,
            ask: None,
            midpoint: None,
            spread: None,
            as_of_block: U64::from(42u64),
        };

        let value = serde_json::to_value(&response).unwrap();
        assert!(value["bid"].is_null());
        assert!(value["ask"].is_null());
        assert!(value["midpoint"].is_null());
        assert!(value["spread"].is_null());
        assert_eq!(value["asOfBlock"], "0x2a");
    }

    #[test]
    fn top_of_book_serializes_populated_sides() {
        let response = TopOfBookResponse {
            pair: "OALPHA/PATH.USD".to_string(),
            base: Address::repeat_byte(0xaa),
            quote: Address::repeat_byte(0xbb),
            bid: Some(OrderLevel {
                price: U128::from(99u128),
                quantity: U128::from(1_000_000u128),
            }),
            ask: Some(OrderLevel {
                price: U128::from(101u128),
                quantity: U128::from(750_000u128),
            }),
            midpoint: Some(U128::from(100u128)),
            spread: Some(U128::from(2u128)),
            as_of_block: U64::from(7u64),
        };

        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(value["bid"]["price"], "0x63");
        assert_eq!(value["bid"]["quantity"], "0xf4240");
        assert_eq!(value["ask"]["price"], "0x65");
        assert_eq!(value["midpoint"], "0x64");
        assert_eq!(value["spread"], "0x2");
    }

    #[test]
    fn midpoint_history_signals_disabled_with_empty_samples_for_alpha() {
        let response = MidpointHistoryResponse {
            pair: "OALPHA/PATH.USD".to_string(),
            base: Address::repeat_byte(0xaa),
            quote: Address::repeat_byte(0xbb),
            interval: "1m".to_string(),
            samples: Vec::new(),
            next_cursor: None,
            history: HistoryAvailability {
                enabled: false,
                reason: "alpha".to_string(),
            },
        };

        let value = serde_json::to_value(&response).unwrap();
        assert_eq!(value["history"]["enabled"], false);
        assert_eq!(value["history"]["reason"], "alpha");
        assert!(value["samples"].as_array().unwrap().is_empty());
        assert!(value["nextCursor"].is_null());
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
        assert_eq!(
            pair.quote,
            "0x20C0000000000000000000000000000000000000"
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
