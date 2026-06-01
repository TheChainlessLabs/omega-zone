//! Owner-scoped darkpool history types and helpers.
//!
//! The darkpool orderbook precompile emits `OrderSubmitted`, `OrderPlaced`,
//! `OrderFilled`, `OrderMatched`, and `OrderCancelled` events. These events
//! always carry the maker (and for fills, the taker) address as an indexed
//! topic. This module wraps that wire format in stable, owner-scoped response
//! types so the frontend can reconstruct an authenticated user's order, fill,
//! and transfer history without trusting client-side localStorage.
//!
//! All access in this module is gated on the authenticated caller: when an
//! `account` parameter is provided in the JSON-RPC request, handlers must
//! reject the call unless it matches the auth context.
//!
//! The address and topic constants here MUST be kept in sync with
//! `crates/precompiles/src/orderbook.rs`. The cross-crate equality is
//! asserted in `crates/tempo-zone/tests/it/private_rpc.rs` to catch drift.

use alloy_primitives::{Address, B256, U128, U256};
use alloy_rpc_types_eth::{Filter, FilterSet, Log};
use alloy_sol_types::{SolEvent, sol};
use serde::{Deserialize, Serialize};

use crate::{filter, types::JsonRpcError};

/// Address of the darkpool orderbook precompile on a zone.
///
/// Mirrors `zone_precompiles::DARKPOOL_ADDRESS`.
pub const DARKPOOL_ADDRESS: Address = Address::new([
    0x0B, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x01,
]);

sol! {
    /// `OrderSubmitted` — fires on every accepted limit-order submission, even
    /// one that fills fully. Indexed: orderId (topic1), maker (topic2).
    event OrderSubmitted(
        uint128 indexed orderId,
        address indexed maker,
        address base,
        address quote,
        uint128 amount,
        uint128 price,
        bool isBid
    );

    /// `OrderPlaced` — fires when a residual quantity rests on the book.
    /// Indexed: orderId (topic1), maker (topic2).
    event OrderPlaced(
        uint128 indexed orderId,
        address indexed maker,
        address base,
        address quote,
        uint128 amount,
        uint128 price,
        bool isBid
    );

    /// `OrderFilled` — fires for each resting-order leg consumed by a taker.
    /// Indexed: orderId (topic1), maker (topic2), taker (topic3).
    event OrderFilled(
        uint128 indexed orderId,
        address indexed maker,
        address indexed taker,
        uint128 amountFilled,
        uint128 price
    );

    /// `OrderMatched` — fires alongside `OrderFilled` for limit matches.
    /// Indexed: makerOrderId (topic1), takerOrderId (topic2), maker (topic3).
    event OrderMatched(
        uint128 indexed makerOrderId,
        uint128 indexed takerOrderId,
        address indexed maker,
        address taker,
        uint128 amountFilled,
        uint128 price
    );

    /// `OrderCancelled` — fires when a maker cancels their resting order.
    /// Indexed: orderId (topic1), maker (topic2).
    event OrderCancelled(
        uint128 indexed orderId,
        address indexed maker
    );
}

/// All darkpool event topic hashes, derived from the sol! definitions above.
pub const DARKPOOL_TOPICS: [B256; 5] = [
    OrderSubmitted::SIGNATURE_HASH,
    OrderPlaced::SIGNATURE_HASH,
    OrderFilled::SIGNATURE_HASH,
    OrderMatched::SIGNATURE_HASH,
    OrderCancelled::SIGNATURE_HASH,
];

/// Maximum number of items returned per page from a `zone_getMy*` call. The
/// upstream `eth_getLogs` scan is bounded separately by `from_block`/`to_block`.
pub const MAX_PAGE_LIMIT: u32 = 500;

/// Default page size when the caller does not specify `limit`.
pub const DEFAULT_PAGE_LIMIT: u32 = 100;

/// Side of an order from the maker's perspective.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    /// Buying base in exchange for quote.
    Bid,
    /// Selling base in exchange for quote.
    Ask,
}

/// Status of an order at the time the response is returned.
///
/// `Open` and `PartiallyFilled` only appear when a residual quantity still
/// rests on the book; `Filled` and `Cancelled` are terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OrderStatus {
    /// Order is resting with its full original amount remaining.
    Open,
    /// Order has been partially filled and still has residual quantity.
    PartiallyFilled,
    /// Order has been fully filled. Removed from the live book.
    Filled,
    /// Order was cancelled by the maker. Removed from the live book.
    Cancelled,
}

/// Role of the authenticated caller in a fill row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FillRole {
    /// Caller was the resting-order maker.
    Maker,
    /// Caller was the incoming taker.
    Taker,
}

/// Direction of a transfer relative to the authenticated caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransferDirection {
    /// Caller is the recipient.
    In,
    /// Caller is the sender.
    Out,
}

/// A single darkpool order belonging to the authenticated caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderEntry {
    /// Darkpool order id.
    pub order_id: U128,
    /// Caller's side of the book.
    pub side: Side,
    /// Current status reconstructed from the event stream.
    pub status: OrderStatus,
    /// Base TIP-20 token.
    pub base_token: Address,
    /// Quote TIP-20 token.
    pub quote_token: Address,
    /// Original submitted amount (in base-token units).
    pub amount: U128,
    /// Remaining unfilled amount. `0` when status is `filled` or `cancelled`.
    pub remaining: U128,
    /// Total filled amount. `amount - remaining` for open / partially-filled.
    pub filled: U128,
    /// Limit price (raw integer units, quote-per-base; the frontend handles
    /// the decimals).
    pub price: U128,
    /// Block in which `OrderSubmitted` was observed.
    pub created_at_block: U256,
    /// Block of the most recent state-changing event.
    pub updated_at_block: U256,
    /// Tx hash of the submission.
    pub created_tx_hash: B256,
    /// Tx hash of the `OrderCancelled` event, if cancelled.
    pub cancel_tx_hash: Option<B256>,
}

/// A single darkpool fill belonging to the authenticated caller.
///
/// `order_id` is the caller-owned order id when one exists in the event
/// stream:
/// - **Maker-side fill**: the caller's resting order id, carried directly in
///   `OrderFilled.orderId`.
/// - **Taker-side limit fill**: the caller's incoming order id, resolved by
///   correlating the fill tx with the caller's same-tx `OrderSubmitted`.
/// - **Taker-side market fill**: `None`, because market orders do not emit
///   `OrderSubmitted` and `OrderFilled.orderId` is the counterparty's resting
///   order id.
///
/// This response intentionally never exposes a counterparty order id; the
/// API is owner-scoped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FillEntry {
    /// Caller-owned darkpool order id for this fill, if the precompile emitted
    /// one. `None` for market-order taker fills.
    pub order_id: Option<U128>,
    /// Caller's role in this fill.
    pub role: FillRole,
    /// Base token of the pair.
    pub base_token: Address,
    /// Quote token of the pair.
    pub quote_token: Address,
    /// Filled amount in base-token units.
    pub amount_filled: U128,
    /// Fill price (raw integer units, quote-per-base).
    pub price: U128,
    /// Block of the fill.
    pub block_number: U256,
    /// Tx hash of the fill.
    pub tx_hash: B256,
    /// Per-block log index, for stable ordering and cursor pagination.
    pub log_index: U256,
}

/// A single TIP-20 transfer involving the authenticated caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferEntry {
    /// TIP-20 token contract.
    pub token: Address,
    /// Counterparty (the other side of the transfer).
    pub counterparty: Address,
    /// Transfer amount in raw token units.
    pub amount: U256,
    /// Direction relative to the caller.
    pub direction: TransferDirection,
    /// Block of the transfer.
    pub block_number: U256,
    /// Tx hash of the transfer.
    pub tx_hash: B256,
    /// Per-block log index, for stable cursor pagination.
    pub log_index: U256,
}

/// Paginated response envelope used by every `zone_getMy*` method.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Page<T: Serialize> {
    /// Items in this page, ordered most-recent-first.
    pub items: Vec<T>,
    /// Opaque cursor for the next page, or `null` when no more items remain.
    pub next_cursor: Option<String>,
}

/// Query params shared by `zone_getMyOrders` and `zone_getMyFills`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryQuery {
    /// Optional account override. Must equal the authenticated caller.
    pub account: Option<Address>,
    /// Optional `0xBASE/0xQUOTE` pair restriction.
    pub pair: Option<String>,
    /// Optional order-status filter (orders only).
    pub status: Option<OrderStatus>,
    /// Opaque cursor returned by a previous page.
    pub cursor: Option<String>,
    /// Caller-supplied page size, clamped to `MAX_PAGE_LIMIT`.
    pub limit: Option<u32>,
}

/// Query params for `zone_getMyTransfers`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransferQuery {
    /// Optional account override. Must equal the authenticated caller.
    pub account: Option<Address>,
    /// Opaque cursor returned by a previous page.
    pub cursor: Option<String>,
    /// Caller-supplied page size, clamped to `MAX_PAGE_LIMIT`.
    pub limit: Option<u32>,
}

/// Verify the optional caller-supplied `account` matches the authenticated caller.
///
/// Returns an `Account mismatch` error on conflict. Returning the JSON-RPC
/// error without leaking whether the requested account exists keeps the
/// privacy contract symmetric with the rest of the private RPC.
#[allow(clippy::result_large_err)]
pub fn require_owner(
    requested: Option<Address>,
    caller: &Address,
) -> Result<Address, JsonRpcError> {
    match requested {
        Some(addr) if &addr != caller => Err(JsonRpcError::account_mismatch()),
        _ => Ok(*caller),
    }
}

/// Clamp a caller-supplied page size to `[1, MAX_PAGE_LIMIT]`.
pub fn clamp_limit(limit: Option<u32>) -> u32 {
    limit.unwrap_or(DEFAULT_PAGE_LIMIT).clamp(1, MAX_PAGE_LIMIT)
}

/// Encoded `(block_number, log_index)` cursor.
///
/// Cursors are opaque to the client; we use `block:logIndex` (both decimal)
/// so a future server-side change can keep the format stable while the
/// underlying scan strategy evolves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    /// Block number the next page should resume from.
    pub block_number: u64,
    /// Log index within that block to skip past.
    pub log_index: u64,
}

impl Cursor {
    /// Encode this cursor as `"<block>:<logIndex>"`.
    pub fn encode(&self) -> String {
        format!("{}:{}", self.block_number, self.log_index)
    }

    /// Parse a cursor produced by [`Cursor::encode`].
    #[allow(clippy::result_large_err)]
    pub fn decode(s: &str) -> Result<Self, JsonRpcError> {
        let (block, idx) = s
            .split_once(':')
            .ok_or_else(|| JsonRpcError::invalid_params("malformed cursor"))?;
        let block_number = block
            .parse::<u64>()
            .map_err(|_| JsonRpcError::invalid_params("malformed cursor"))?;
        let log_index = idx
            .parse::<u64>()
            .map_err(|_| JsonRpcError::invalid_params("malformed cursor"))?;
        Ok(Self {
            block_number,
            log_index,
        })
    }
}

/// Returns `true` if the caller is the `maker` recorded in this darkpool event.
///
/// Maker sits at topic2 for `OrderSubmitted`/`OrderPlaced`/`OrderCancelled` and
/// for the resting-side address on `OrderFilled`; for `OrderMatched` it shifts
/// to topic3 because the first two indexed topics are the two order ids.
pub fn caller_is_maker(log: &Log, caller: &Address) -> bool {
    let topics = log.topics();
    let Some(topic0) = topics.first() else {
        return false;
    };
    if !DARKPOOL_TOPICS.contains(topic0) {
        return false;
    }
    let caller_word = B256::left_padding_from(caller.as_slice());
    if *topic0 == OrderMatched::SIGNATURE_HASH {
        topics.get(3) == Some(&caller_word)
    } else {
        topics.get(2) == Some(&caller_word)
    }
}

/// Returns `true` if the caller is the `taker` recorded in this darkpool fill event.
///
/// `OrderFilled` indexes `taker` at topic3; `OrderMatched` carries `taker`
/// in the non-indexed body, so this check applies only to `OrderFilled`.
pub fn caller_is_taker(log: &Log, caller: &Address) -> bool {
    let topics = log.topics();
    let Some(topic0) = topics.first() else {
        return false;
    };
    if *topic0 != OrderFilled::SIGNATURE_HASH {
        return false;
    }
    let caller_word = B256::left_padding_from(caller.as_slice());
    topics.get(3) == Some(&caller_word)
}

/// Returns `true` if the caller is either the maker or the taker of this event.
pub fn caller_is_party(log: &Log, caller: &Address) -> bool {
    caller_is_maker(log, caller) || caller_is_taker(log, caller)
}

/// Encode a 20-byte address as a left-padded 32-byte topic word.
pub fn topic_for_address(addr: &Address) -> B256 {
    B256::left_padding_from(addr.as_slice())
}

/// Encode a `uint128` order id as a left-padded 32-byte topic word.
pub fn order_id_topic(order_id: u128) -> B256 {
    B256::left_padding_from(&order_id.to_be_bytes())
}

/// Resolve `pair` from `"0xBASE/0xQUOTE"` form. Returns `None` if the caller
/// did not pass a pair filter at all.
#[allow(clippy::result_large_err)]
pub fn parse_pair_filter(pair: Option<&str>) -> Result<Option<(Address, Address)>, JsonRpcError> {
    let Some(pair) = pair else { return Ok(None) };
    let (base, quote) = pair
        .split_once('/')
        .ok_or_else(|| JsonRpcError::invalid_params("pair must be \"0xBASE/0xQUOTE\""))?;
    let base: Address = base
        .parse()
        .map_err(|_| JsonRpcError::invalid_params("pair base must be a 0x address"))?;
    let quote: Address = quote
        .parse()
        .map_err(|_| JsonRpcError::invalid_params("pair quote must be a 0x address"))?;
    Ok(Some((base, quote)))
}

/// Build a darkpool-scoped `eth_getLogs` filter with the given topic0 set and
/// optional maker topic. `cursor.block_number` (if any) becomes the
/// `from_block`; otherwise the scan starts at genesis.
pub fn build_darkpool_filter(
    topic0: &[B256],
    maker_topic: Option<B256>,
    cursor: Option<Cursor>,
) -> Filter {
    use alloy_rpc_types_eth::BlockNumberOrTag;
    let mut filter = Filter::default();
    filter.address = FilterSet::from(DARKPOOL_ADDRESS);
    filter.topics[0] = FilterSet::from(topic0.to_vec());
    if let Some(topic) = maker_topic {
        filter.topics[2] = FilterSet::from(topic);
    }
    if let Some(cursor) = cursor {
        filter = filter.from_block(BlockNumberOrTag::Number(cursor.block_number));
    }
    filter
}

/// Build a darkpool filter for `OrderFilled` scoped to a single order id and
/// owner. Used by `zone_getOrder`.
pub fn build_order_filter(order_id: u128, owner: &Address) -> Filter {
    let mut filter = Filter::default();
    filter.address = FilterSet::from(DARKPOOL_ADDRESS);
    filter.topics[0] = FilterSet::from(DARKPOOL_TOPICS.to_vec());
    filter.topics[1] = FilterSet::from(order_id_topic(order_id));
    filter.topics[2] = FilterSet::from(topic_for_address(owner));
    filter
}

/// Build a TIP-20 filter scoped to the caller as `from` (topic1) or `to`
/// (topic2). The address field is left open because every enabled zone TIP-20
/// emits the same topics; the post-filter ([`filter::is_log_visible`]) keeps
/// the privacy invariant.
pub fn build_tip20_filter(
    topic0: &[B256],
    owner_topic: Option<B256>,
    cursor: Option<Cursor>,
    as_from: bool,
) -> Filter {
    use alloy_rpc_types_eth::BlockNumberOrTag;
    let mut filter = Filter::default();
    filter.topics[0] = FilterSet::from(topic0.to_vec());
    if let Some(topic) = owner_topic {
        if as_from {
            filter.topics[1] = FilterSet::from(topic);
        } else {
            filter.topics[2] = FilterSet::from(topic);
        }
    }
    if let Some(cursor) = cursor {
        filter = filter.from_block(BlockNumberOrTag::Number(cursor.block_number));
    }
    filter
}

/// Reconstruct caller-owned orders from an iterator of darkpool logs.
///
/// Walks the events in block / log-index order and folds them into per-id
/// state. Events targeting orders the caller did not author are ignored at
/// the call site — this function trusts its input.
pub fn reconstruct_orders<'a, I: IntoIterator<Item = &'a Log>>(logs: I) -> Vec<OrderEntry> {
    use std::collections::BTreeMap;

    let mut by_id: BTreeMap<u128, OrderEntry> = BTreeMap::new();
    let mut sorted: Vec<&Log> = logs.into_iter().collect();
    sorted.sort_by_key(|l| (l.block_number.unwrap_or(0), l.log_index.unwrap_or(0)));

    for log in sorted {
        let Some(topic0) = log.topic0().copied() else {
            continue;
        };
        let block = log.block_number.map(U256::from).unwrap_or(U256::ZERO);
        let tx_hash = log.transaction_hash.unwrap_or_default();

        if topic0 == OrderSubmitted::SIGNATURE_HASH {
            if let Ok(decoded) = OrderSubmitted::decode_log(&log.inner) {
                let entry = by_id.entry(decoded.orderId).or_insert_with(|| OrderEntry {
                    order_id: U128::from(decoded.orderId),
                    side: if decoded.isBid { Side::Bid } else { Side::Ask },
                    status: OrderStatus::Open,
                    base_token: decoded.base,
                    quote_token: decoded.quote,
                    amount: U128::from(decoded.amount),
                    remaining: U128::from(decoded.amount),
                    filled: U128::ZERO,
                    price: U128::from(decoded.price),
                    created_at_block: block,
                    updated_at_block: block,
                    created_tx_hash: tx_hash,
                    cancel_tx_hash: None,
                });
                entry.updated_at_block = block;
            }
        } else if topic0 == OrderPlaced::SIGNATURE_HASH {
            if let Ok(decoded) = OrderPlaced::decode_log(&log.inner) {
                // OrderPlaced fires with the *residual* amount after eager
                // crossing. The original submitted amount lives in
                // OrderSubmitted, so back-fill from there if available.
                let entry = by_id.entry(decoded.orderId).or_insert_with(|| OrderEntry {
                    order_id: U128::from(decoded.orderId),
                    side: if decoded.isBid { Side::Bid } else { Side::Ask },
                    status: OrderStatus::Open,
                    base_token: decoded.base,
                    quote_token: decoded.quote,
                    amount: U128::from(decoded.amount),
                    remaining: U128::from(decoded.amount),
                    filled: U128::ZERO,
                    price: U128::from(decoded.price),
                    created_at_block: block,
                    updated_at_block: block,
                    created_tx_hash: tx_hash,
                    cancel_tx_hash: None,
                });
                entry.remaining = U128::from(decoded.amount);
                if entry.amount < entry.remaining {
                    entry.amount = entry.remaining;
                }
                entry.filled = entry.amount.saturating_sub(entry.remaining);
                entry.status = if entry.filled == U128::ZERO {
                    OrderStatus::Open
                } else if entry.remaining > U128::ZERO {
                    OrderStatus::PartiallyFilled
                } else {
                    OrderStatus::Filled
                };
                entry.updated_at_block = block;
            }
        } else if topic0 == OrderFilled::SIGNATURE_HASH {
            if let Ok(decoded) = OrderFilled::decode_log(&log.inner) {
                if let Some(entry) = by_id.get_mut(&decoded.orderId) {
                    entry.filled = entry
                        .filled
                        .saturating_add(U128::from(decoded.amountFilled));
                    entry.remaining = entry.amount.saturating_sub(entry.filled);
                    entry.status = if entry.remaining == U128::ZERO {
                        OrderStatus::Filled
                    } else {
                        OrderStatus::PartiallyFilled
                    };
                    entry.updated_at_block = block;
                }
            }
        } else if topic0 == OrderCancelled::SIGNATURE_HASH {
            if let Ok(decoded) = OrderCancelled::decode_log(&log.inner) {
                if let Some(entry) = by_id.get_mut(&decoded.orderId) {
                    entry.status = OrderStatus::Cancelled;
                    entry.remaining = U128::ZERO;
                    entry.cancel_tx_hash = Some(tx_hash);
                    entry.updated_at_block = block;
                }
            }
        }
        // OrderMatched is informational only — every match also emits
        // OrderFilled, which is the single source of truth for fill state.
    }

    by_id.into_values().collect()
}

/// Index of `OrderSubmitted` events, used to attach pair metadata to fills
/// (whose own event payload lacks it) and to derive the caller's incoming
/// order id for taker-side limit fills.
///
/// Lookups are keyed three ways:
/// - `pair_by_order_id` — global resting-order id → pair. Used internally for
///   taker market fills where the only available pair reference is the resting
///   order id; the id is never returned.
/// - `own_pair_by_order_id` — caller's order id → pair. Used for maker fills.
/// - `own_by_tx_hash` — for taker limit fills, the matching `OrderFilled` is
///   emitted in the same precompile call as the caller's `OrderSubmitted`,
///   so the transaction hash links them. The map stores the caller's own
///   incoming order id and pair.
#[derive(Debug, Default, Clone)]
pub struct PairIndex {
    pair_by_order_id: std::collections::BTreeMap<u128, (Address, Address)>,
    own_pair_by_order_id: std::collections::BTreeMap<u128, (Address, Address)>,
    own_by_tx_hash: std::collections::BTreeMap<B256, (u128, Address, Address)>,
}

impl PairIndex {
    /// Any resting order id → `(base, quote)`. Used only for metadata.
    pub fn pair_for_order(&self, order_id: u128) -> Option<(Address, Address)> {
        self.pair_by_order_id.get(&order_id).copied()
    }

    /// Caller's own order id → `(base, quote)`.
    pub fn own_pair_for_order(&self, order_id: u128) -> Option<(Address, Address)> {
        self.own_pair_by_order_id.get(&order_id).copied()
    }

    /// Caller's submission tx → `(own incoming order id, base, quote)`.
    pub fn taker_context_for_tx(&self, tx_hash: &B256) -> Option<(u128, Address, Address)> {
        self.own_by_tx_hash.get(tx_hash).copied()
    }
}

/// Build a [`PairIndex`] from an iterator of `OrderSubmitted` logs.
///
/// Non-`OrderSubmitted` logs are silently ignored, so callers can pass a raw
/// `eth_getLogs` response without pre-filtering. Logs owned by `owner` are
/// additionally indexed as caller-owned; foreign logs are used only for pair
/// metadata keyed by resting order id and never surface in the response.
pub fn build_pair_index<'a, I: IntoIterator<Item = &'a Log>>(
    logs: I,
    owner: &Address,
) -> PairIndex {
    let mut index = PairIndex::default();
    for log in logs {
        let Some(topic0) = log.topic0() else { continue };
        if *topic0 != OrderSubmitted::SIGNATURE_HASH {
            continue;
        }
        let Ok(decoded) = OrderSubmitted::decode_log(&log.inner) else {
            continue;
        };
        index
            .pair_by_order_id
            .insert(decoded.orderId, (decoded.base, decoded.quote));

        if !caller_is_maker(log, owner) {
            continue;
        }
        index
            .own_pair_by_order_id
            .insert(decoded.orderId, (decoded.base, decoded.quote));
        if let Some(tx) = log.transaction_hash {
            index
                .own_by_tx_hash
                .insert(tx, (decoded.orderId, decoded.base, decoded.quote));
        }
    }
    index
}

/// Build a [`FillEntry`] from an `OrderFilled` log using the caller's
/// [`PairIndex`] to attach pair metadata (and, for taker-side limit fills, to
/// resolve the caller's own incoming order id).
///
/// Returns `None` when:
/// - the log is not `OrderFilled`,
/// - the maker-side fill's order id is not in `pair_index.own_pair_by_order_id`
///   (the caller's submission history is incomplete — should not happen
///   for the caller's own activity),
/// - the taker-side fill has neither a caller same-tx `OrderSubmitted` nor a
///   resting-order pair lookup.
///
/// `OrderMatched` logs are intentionally **not** consumed here: every match
/// also emits `OrderFilled`, so handling both would double-count limit
/// fills, and the counterparty order id from `OrderMatched.makerOrderId` /
/// `takerOrderId` would leak counterparty information into an owner-scoped
/// response.
pub fn fill_entry_from_log(log: &Log, role: FillRole, pair_index: &PairIndex) -> Option<FillEntry> {
    let topic0 = log.topic0().copied()?;
    if topic0 != OrderFilled::SIGNATURE_HASH {
        return None;
    }
    let decoded = OrderFilled::decode_log(&log.inner).ok()?;
    let block = log.block_number.map(U256::from).unwrap_or(U256::ZERO);
    let tx_hash = log.transaction_hash.unwrap_or_default();

    let (order_id, base, quote) = match role {
        FillRole::Maker => {
            // OrderFilled.orderId is the caller's own resting order id; the
            // pair comes from the caller's own OrderSubmitted index.
            let (base, quote) = pair_index.own_pair_for_order(decoded.orderId)?;
            (Some(decoded.orderId), base, quote)
        }
        FillRole::Taker => {
            // OrderFilled.orderId is the *counterparty's* resting order id
            // and must not be surfaced. Limit takers have a same-tx
            // OrderSubmitted for their incoming order; market takers do not,
            // so we return order_id=None but still derive pair metadata from
            // the resting order id.
            if let Some((own_id, base, quote)) = pair_index.taker_context_for_tx(&tx_hash) {
                (Some(own_id), base, quote)
            } else {
                let (base, quote) = pair_index.pair_for_order(decoded.orderId)?;
                (None, base, quote)
            }
        }
    };

    Some(FillEntry {
        order_id: order_id.map(U128::from),
        role,
        base_token: base,
        quote_token: quote,
        amount_filled: U128::from(decoded.amountFilled),
        price: U128::from(decoded.price),
        block_number: block,
        tx_hash,
        log_index: log.log_index.map(U256::from).unwrap_or(U256::ZERO),
    })
}

/// Build a [`TransferEntry`] from a TIP-20 Transfer / TransferWithMemo /
/// Mint / Burn log, picking the direction based on whether the caller is
/// the `from` or `to` side.
pub fn transfer_entry_from_log(log: &Log, owner: &Address) -> Option<TransferEntry> {
    let topics = log.topics();
    let topic0 = *topics.first()?;
    let owner_topic = topic_for_address(owner);

    let (counterparty_topic, direction) = if topics.get(1) == Some(&owner_topic) {
        (
            topics.get(2).copied().unwrap_or(B256::ZERO),
            TransferDirection::Out,
        )
    } else if topics.get(2) == Some(&owner_topic) {
        (
            topics.get(1).copied().unwrap_or(B256::ZERO),
            TransferDirection::In,
        )
    } else if topic0 == filter::MINT_TOPIC && topics.get(1) == Some(&owner_topic) {
        (B256::ZERO, TransferDirection::In)
    } else if topic0 == filter::BURN_TOPIC && topics.get(1) == Some(&owner_topic) {
        (B256::ZERO, TransferDirection::Out)
    } else {
        return None;
    };

    let counterparty = Address::from_slice(&counterparty_topic.as_slice()[12..]);
    let data = log.data().data.as_ref();
    let amount = if data.len() >= 32 {
        U256::from_be_slice(&data[..32])
    } else {
        U256::ZERO
    };

    Some(TransferEntry {
        token: log.address(),
        counterparty,
        amount,
        direction,
        block_number: log.block_number.map(U256::from).unwrap_or(U256::ZERO),
        tx_hash: log.transaction_hash.unwrap_or_default(),
        log_index: log.log_index.map(U256::from).unwrap_or(U256::ZERO),
    })
}

/// Compute the next-page cursor for an order page. Returns `None` if the
/// page is shorter than `limit`.
pub fn next_order_cursor(orders: &[OrderEntry], limit: u32) -> Option<String> {
    if orders.len() <= limit as usize {
        return None;
    }
    let block: u64 = orders[limit as usize - 1]
        .updated_at_block
        .try_into()
        .unwrap_or(0);
    Some(
        Cursor {
            block_number: block,
            log_index: 0,
        }
        .encode(),
    )
}

/// Compute the next-page cursor for a fill page.
pub fn next_fill_cursor(fills: &[FillEntry], limit: u32) -> Option<String> {
    if fills.len() <= limit as usize {
        return None;
    }
    let block: u64 = fills[limit as usize - 1]
        .block_number
        .try_into()
        .unwrap_or(0);
    let log_index: u64 = fills[limit as usize - 1].log_index.try_into().unwrap_or(0);
    Some(
        Cursor {
            block_number: block,
            log_index,
        }
        .encode(),
    )
}

/// Compute the next-page cursor for a transfer page.
pub fn next_transfer_cursor(transfers: &[TransferEntry], limit: u32) -> Option<String> {
    if transfers.len() <= limit as usize {
        return None;
    }
    let block: u64 = transfers[limit as usize - 1]
        .block_number
        .try_into()
        .unwrap_or(0);
    let log_index: u64 = transfers[limit as usize - 1]
        .log_index
        .try_into()
        .unwrap_or(0);
    Some(
        Cursor {
            block_number: block,
            log_index,
        }
        .encode(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{B256, Bytes, LogData, address, b256, keccak256};
    use alloy_sol_types::SolValue;

    fn make_log(emitter: Address, topics: Vec<B256>) -> Log {
        Log {
            inner: alloy_primitives::Log {
                address: emitter,
                data: LogData::new_unchecked(topics, Bytes::new()),
            },
            block_hash: None,
            block_number: None,
            block_timestamp: None,
            transaction_hash: None,
            transaction_index: None,
            log_index: None,
            removed: false,
        }
    }

    fn make_log_full(
        emitter: Address,
        topics: Vec<B256>,
        data: Bytes,
        block_number: Option<u64>,
        tx_hash: Option<B256>,
    ) -> Log {
        Log {
            inner: alloy_primitives::Log {
                address: emitter,
                data: LogData::new_unchecked(topics, data),
            },
            block_hash: None,
            block_number,
            block_timestamp: None,
            transaction_hash: tx_hash,
            transaction_index: Some(0),
            log_index: Some(0),
            removed: false,
        }
    }

    fn topic_addr(addr: &Address) -> B256 {
        B256::left_padding_from(addr.as_slice())
    }

    fn order_id_topic(order_id: u128) -> B256 {
        B256::left_padding_from(&order_id.to_be_bytes())
    }

    /// Encode the non-indexed body of an `OrderSubmitted` event:
    /// `(address base, address quote, uint128 amount, uint128 price, bool isBid)`.
    fn order_submitted_body(
        base: Address,
        quote: Address,
        amount: u128,
        price: u128,
        is_bid: bool,
    ) -> Bytes {
        (base, quote, amount, price, is_bid).abi_encode().into()
    }

    /// Encode the non-indexed body of an `OrderFilled` event:
    /// `(uint128 amountFilled, uint128 price)`.
    fn order_filled_body(amount_filled: u128, price: u128) -> Bytes {
        (amount_filled, price).abi_encode().into()
    }

    fn make_order_submitted_log(
        maker: Address,
        order_id: u128,
        base: Address,
        quote: Address,
        amount: u128,
        price: u128,
        is_bid: bool,
        block: u64,
        tx_hash: B256,
    ) -> Log {
        make_log_full(
            DARKPOOL_ADDRESS,
            vec![
                OrderSubmitted::SIGNATURE_HASH,
                order_id_topic(order_id),
                topic_addr(&maker),
            ],
            order_submitted_body(base, quote, amount, price, is_bid),
            Some(block),
            Some(tx_hash),
        )
    }

    fn make_order_filled_log(
        resting_order_id: u128,
        resting_maker: Address,
        taker: Address,
        amount_filled: u128,
        price: u128,
        block: u64,
        tx_hash: B256,
    ) -> Log {
        make_log_full(
            DARKPOOL_ADDRESS,
            vec![
                OrderFilled::SIGNATURE_HASH,
                order_id_topic(resting_order_id),
                topic_addr(&resting_maker),
                topic_addr(&taker),
            ],
            order_filled_body(amount_filled, price),
            Some(block),
            Some(tx_hash),
        )
    }

    fn make_order_cancelled_log(maker: Address, order_id: u128, block: u64, tx_hash: B256) -> Log {
        make_log_full(
            DARKPOOL_ADDRESS,
            vec![
                OrderCancelled::SIGNATURE_HASH,
                order_id_topic(order_id),
                topic_addr(&maker),
            ],
            Bytes::new(),
            Some(block),
            Some(tx_hash),
        )
    }

    fn with_log_index(mut log: Log, log_index: u64) -> Log {
        log.log_index = Some(log_index);
        log
    }

    /// Cross-check the alloy-derived constants against keccak256 of the
    /// canonical event signature strings. This is the same gate `filter.rs`
    /// uses for TIP-20 topics.
    #[test]
    fn topic_hashes_match_signatures() {
        assert_eq!(
            OrderSubmitted::SIGNATURE_HASH,
            keccak256(b"OrderSubmitted(uint128,address,address,address,uint128,uint128,bool)")
        );
        assert_eq!(
            OrderPlaced::SIGNATURE_HASH,
            keccak256(b"OrderPlaced(uint128,address,address,address,uint128,uint128,bool)")
        );
        assert_eq!(
            OrderFilled::SIGNATURE_HASH,
            keccak256(b"OrderFilled(uint128,address,address,uint128,uint128)")
        );
        assert_eq!(
            OrderMatched::SIGNATURE_HASH,
            keccak256(b"OrderMatched(uint128,uint128,address,address,uint128,uint128)")
        );
        assert_eq!(
            OrderCancelled::SIGNATURE_HASH,
            keccak256(b"OrderCancelled(uint128,address)")
        );
    }

    #[test]
    fn darkpool_address_constant_matches_precompile_layout() {
        // Last byte = 0x01, all other bytes in the high 12 = 0x0B then zeros.
        assert_eq!(DARKPOOL_ADDRESS.as_slice()[0], 0x0B);
        assert_eq!(DARKPOOL_ADDRESS.as_slice()[19], 0x01);
        assert!(DARKPOOL_ADDRESS.as_slice()[1..19].iter().all(|b| *b == 0));
    }

    #[test]
    fn require_owner_accepts_match() {
        let caller = Address::repeat_byte(0xaa);
        assert_eq!(require_owner(Some(caller), &caller).unwrap(), caller);
        assert_eq!(require_owner(None, &caller).unwrap(), caller);
    }

    #[test]
    fn require_owner_rejects_mismatch() {
        let caller = Address::repeat_byte(0xaa);
        let other = Address::repeat_byte(0xbb);
        let err = require_owner(Some(other), &caller).expect_err("should reject");
        assert_eq!(err.code, -32004);
    }

    #[test]
    fn clamp_limit_applies_default_and_max() {
        assert_eq!(clamp_limit(None), DEFAULT_PAGE_LIMIT);
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(MAX_PAGE_LIMIT + 100)), MAX_PAGE_LIMIT);
        assert_eq!(clamp_limit(Some(50)), 50);
    }

    #[test]
    fn cursor_roundtrip() {
        let cursor = Cursor {
            block_number: 12345,
            log_index: 7,
        };
        let encoded = cursor.encode();
        assert_eq!(encoded, "12345:7");
        assert_eq!(Cursor::decode(&encoded).unwrap(), cursor);
    }

    #[test]
    fn cursor_decode_rejects_garbage() {
        assert!(Cursor::decode("not-a-cursor").is_err());
        assert!(Cursor::decode("abc:def").is_err());
        assert!(Cursor::decode("12345").is_err());
    }

    #[test]
    fn caller_is_maker_matches_topic_2_for_submitted() {
        let caller = address!("0x000000000000000000000000000000000000beef");
        let other = address!("0x000000000000000000000000000000000000c0de");
        let mine = make_log(
            DARKPOOL_ADDRESS,
            vec![
                OrderSubmitted::SIGNATURE_HASH,
                order_id_topic(1),
                topic_addr(&caller),
            ],
        );
        let theirs = make_log(
            DARKPOOL_ADDRESS,
            vec![
                OrderSubmitted::SIGNATURE_HASH,
                order_id_topic(2),
                topic_addr(&other),
            ],
        );
        assert!(caller_is_maker(&mine, &caller));
        assert!(!caller_is_maker(&theirs, &caller));
    }

    #[test]
    fn caller_is_maker_matches_topic_3_for_matched() {
        let caller = address!("0x000000000000000000000000000000000000beef");
        let other = address!("0x000000000000000000000000000000000000c0de");
        let mine_as_maker = make_log(
            DARKPOOL_ADDRESS,
            vec![
                OrderMatched::SIGNATURE_HASH,
                order_id_topic(1),
                order_id_topic(2),
                topic_addr(&caller),
            ],
        );
        let mine_as_taker_only = make_log(
            DARKPOOL_ADDRESS,
            vec![
                OrderMatched::SIGNATURE_HASH,
                order_id_topic(1),
                order_id_topic(2),
                topic_addr(&other),
            ],
        );
        assert!(caller_is_maker(&mine_as_maker, &caller));
        assert!(!caller_is_maker(&mine_as_taker_only, &caller));
    }

    #[test]
    fn caller_is_taker_only_matches_order_filled() {
        let caller = address!("0x000000000000000000000000000000000000beef");
        let other = address!("0x000000000000000000000000000000000000c0de");
        let mine_as_taker = make_log(
            DARKPOOL_ADDRESS,
            vec![
                OrderFilled::SIGNATURE_HASH,
                order_id_topic(1),
                topic_addr(&other),
                topic_addr(&caller),
            ],
        );
        let unrelated = make_log(
            DARKPOOL_ADDRESS,
            vec![
                OrderMatched::SIGNATURE_HASH,
                order_id_topic(1),
                order_id_topic(2),
                topic_addr(&caller),
            ],
        );
        assert!(caller_is_taker(&mine_as_taker, &caller));
        assert!(!caller_is_taker(&unrelated, &caller));
    }

    #[test]
    fn caller_is_party_unions_maker_and_taker() {
        let caller = address!("0x000000000000000000000000000000000000beef");
        let other = address!("0x000000000000000000000000000000000000c0de");
        let as_maker = make_log(
            DARKPOOL_ADDRESS,
            vec![
                OrderSubmitted::SIGNATURE_HASH,
                order_id_topic(1),
                topic_addr(&caller),
            ],
        );
        let as_taker = make_log(
            DARKPOOL_ADDRESS,
            vec![
                OrderFilled::SIGNATURE_HASH,
                order_id_topic(1),
                topic_addr(&other),
                topic_addr(&caller),
            ],
        );
        let other_party = make_log(
            DARKPOOL_ADDRESS,
            vec![
                OrderSubmitted::SIGNATURE_HASH,
                order_id_topic(1),
                topic_addr(&other),
            ],
        );
        assert!(caller_is_party(&as_maker, &caller));
        assert!(caller_is_party(&as_taker, &caller));
        assert!(!caller_is_party(&other_party, &caller));
    }

    #[test]
    fn caller_is_party_ignores_non_darkpool_topics() {
        let caller = address!("0x000000000000000000000000000000000000beef");
        let bogus = b256!("0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff");
        let bogus_log = make_log(DARKPOOL_ADDRESS, vec![bogus, topic_addr(&caller)]);
        assert!(!caller_is_party(&bogus_log, &caller));
    }

    #[test]
    fn no_topics_is_not_visible() {
        let caller = address!("0x000000000000000000000000000000000000beef");
        let empty = make_log(DARKPOOL_ADDRESS, vec![]);
        assert!(!caller_is_maker(&empty, &caller));
        assert!(!caller_is_taker(&empty, &caller));
        assert!(!caller_is_party(&empty, &caller));
    }

    // -----------------------------------------------------------------
    // PairIndex / fill_entry_from_log
    // -----------------------------------------------------------------

    #[test]
    fn fill_entry_has_no_counterparty_order_id_field() {
        // Compile-time guard: serialized FillEntry must NOT include
        // counterpartyOrderId. The privacy review explicitly removed it.
        let entry = FillEntry {
            order_id: Some(U128::from(1u128)),
            role: FillRole::Maker,
            base_token: Address::ZERO,
            quote_token: Address::ZERO,
            amount_filled: U128::ZERO,
            price: U128::ZERO,
            block_number: U256::ZERO,
            tx_hash: B256::ZERO,
            log_index: U256::ZERO,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(
            !json.contains("counterpartyOrderId"),
            "FillEntry must not expose counterparty order ids: {json}"
        );
    }

    #[test]
    fn build_pair_index_indexes_caller_submissions_by_order_id_and_tx() {
        let caller = address!("0x000000000000000000000000000000000000beef");
        let base = address!("0x0000000000000000000000000000000000ba51e1");
        let quote = address!("0x0000000000000000000000000000000000600073");

        let submitted = make_order_submitted_log(
            caller,
            7,
            base,
            quote,
            1_000_000,
            5,
            true,
            10,
            B256::with_last_byte(0xa1),
        );

        let index = build_pair_index(std::iter::once(&submitted), &caller);
        assert_eq!(index.pair_for_order(7), Some((base, quote)));
        assert_eq!(index.own_pair_for_order(7), Some((base, quote)));
        assert_eq!(
            index.taker_context_for_tx(&B256::with_last_byte(0xa1)),
            Some((7, base, quote)),
            "tx-hash lookup must return the caller's own incoming order id and pair"
        );
        assert_eq!(index.pair_for_order(99), None);
    }

    #[test]
    fn build_pair_index_skips_non_submitted_logs() {
        let caller = address!("0x000000000000000000000000000000000000beef");
        let filled = make_order_filled_log(
            1,
            caller,
            address!("0x000000000000000000000000000000000000c0de"),
            500,
            5,
            11,
            B256::with_last_byte(0xa2),
        );
        let index = build_pair_index(std::iter::once(&filled), &caller);
        assert!(index.pair_by_order_id.is_empty());
        assert!(index.own_pair_by_order_id.is_empty());
        assert!(index.own_by_tx_hash.is_empty());
    }

    #[test]
    fn fill_entry_maker_side_populates_pair_from_caller_index() {
        // The reviewer-blocking bug: previously this returned base=ZERO,
        // quote=ZERO. Now it must resolve to the caller's submitted pair.
        let caller = address!("0x000000000000000000000000000000000000beef");
        let other = address!("0x000000000000000000000000000000000000c0de");
        let base = address!("0x0000000000000000000000000000000000ba51e1");
        let quote = address!("0x0000000000000000000000000000000000600073");

        let submitted = make_order_submitted_log(
            caller,
            1,
            base,
            quote,
            1_000_000,
            5,
            true,
            10,
            B256::with_last_byte(0xa1),
        );
        let index = build_pair_index(std::iter::once(&submitted), &caller);

        // Someone else takes the caller's resting bid (orderId=1, caller is maker).
        let filled =
            make_order_filled_log(1, caller, other, 250_000, 5, 12, B256::with_last_byte(0xa2));

        let entry =
            fill_entry_from_log(&filled, FillRole::Maker, &index).expect("maker fill resolves");
        assert_eq!(entry.order_id, Some(U128::from(1u128)));
        assert_eq!(
            entry.base_token, base,
            "maker fill must carry the caller's order pair base"
        );
        assert_eq!(entry.quote_token, quote);
        assert_eq!(entry.amount_filled, U128::from(250_000u128));
        assert_eq!(entry.price, U128::from(5u128));
        assert_eq!(entry.tx_hash, B256::with_last_byte(0xa2));
    }

    #[test]
    fn reconstruct_orders_tracks_partial_fill_then_cancel_tx_hash() {
        let maker = address!("0x000000000000000000000000000000000000beef");
        let taker_one = address!("0x0000000000000000000000000000000000000001");
        let taker_two = address!("0x0000000000000000000000000000000000000002");
        let base = address!("0x0000000000000000000000000000000000ba51e1");
        let quote = address!("0x0000000000000000000000000000000000600073");
        let submit_tx = B256::with_last_byte(0xa1);
        let fill_one_tx = B256::with_last_byte(0xa2);
        let fill_two_tx = B256::with_last_byte(0xa3);
        let cancel_tx = B256::with_last_byte(0xa4);

        let logs = vec![
            make_order_submitted_log(maker, 7, base, quote, 1_000_000, 2, false, 10, submit_tx),
            make_order_filled_log(7, maker, taker_one, 300_000, 2, 11, fill_one_tx),
            make_order_filled_log(7, maker, taker_two, 400_000, 2, 12, fill_two_tx),
            make_order_cancelled_log(maker, 7, 13, cancel_tx),
        ];

        let orders = reconstruct_orders(logs.iter());
        assert_eq!(orders.len(), 1);
        let order = &orders[0];
        assert_eq!(order.order_id, U128::from(7u128));
        assert_eq!(order.side, Side::Ask);
        assert_eq!(order.status, OrderStatus::Cancelled);
        assert_eq!(order.amount, U128::from(1_000_000u128));
        assert_eq!(order.filled, U128::from(700_000u128));
        assert_eq!(
            order.remaining,
            U128::ZERO,
            "cancelled orders should not retain a resting remainder"
        );
        assert_eq!(order.cancel_tx_hash, Some(cancel_tx));
        assert_eq!(order.created_tx_hash, submit_tx);
        assert_eq!(order.updated_at_block, U256::from(13u64));
    }

    #[test]
    fn fill_entry_taker_side_resolves_callers_own_order_id_via_tx_hash() {
        // For taker fills, OrderFilled.orderId is the counterparty's resting
        // order — exposing it would leak counterparty info. The caller's
        // own incoming order id comes from the OrderSubmitted emitted by
        // the caller's same-tx limit order.
        let caller = address!("0x000000000000000000000000000000000000beef");
        let other = address!("0x000000000000000000000000000000000000c0de");
        let base = address!("0x0000000000000000000000000000000000ba51e1");
        let quote = address!("0x0000000000000000000000000000000000600073");

        let same_tx = B256::with_last_byte(0xb3);

        // Caller's incoming taker order — orderId=42, same tx as the fill.
        let submitted =
            make_order_submitted_log(caller, 42, base, quote, 200_000, 5, false, 13, same_tx);
        let index = build_pair_index(std::iter::once(&submitted), &caller);

        // Counterparty's resting bid (orderId=99) is taken by the caller.
        let filled = make_order_filled_log(99, other, caller, 200_000, 5, 13, same_tx);

        let entry =
            fill_entry_from_log(&filled, FillRole::Taker, &index).expect("taker fill resolves");
        assert_eq!(
            entry.order_id,
            Some(U128::from(42u128)),
            "taker fill order_id must be the caller's own incoming order id, not the counterparty's"
        );
        assert_ne!(
            entry.order_id,
            Some(U128::from(99u128)),
            "taker fill must not surface the counterparty resting order id"
        );
        assert_eq!(entry.base_token, base);
        assert_eq!(entry.quote_token, quote);
    }

    #[test]
    fn fill_entry_preserves_log_index_for_same_tx_multi_fill_rows() {
        let caller = address!("0x000000000000000000000000000000000000beef");
        let maker_one = address!("0x0000000000000000000000000000000000000001");
        let maker_two = address!("0x0000000000000000000000000000000000000002");
        let base = address!("0x0000000000000000000000000000000000ba51e1");
        let quote = address!("0x0000000000000000000000000000000000600073");
        let same_tx = B256::with_last_byte(0xc3);

        let taker_submission =
            make_order_submitted_log(caller, 42, base, quote, 700_000, 2, true, 20, same_tx);
        let maker_one_submission = make_order_submitted_log(
            maker_one,
            1,
            base,
            quote,
            300_000,
            2,
            false,
            10,
            B256::with_last_byte(0xa1),
        );
        let maker_two_submission = make_order_submitted_log(
            maker_two,
            2,
            base,
            quote,
            400_000,
            2,
            false,
            11,
            B256::with_last_byte(0xa2),
        );
        let index = build_pair_index(
            [
                &taker_submission,
                &maker_one_submission,
                &maker_two_submission,
            ],
            &caller,
        );

        let fill_one = with_log_index(
            make_order_filled_log(1, maker_one, caller, 300_000, 2, 20, same_tx),
            4,
        );
        let fill_two = with_log_index(
            make_order_filled_log(2, maker_two, caller, 400_000, 2, 20, same_tx),
            6,
        );

        let entry_one =
            fill_entry_from_log(&fill_one, FillRole::Taker, &index).expect("first fill resolves");
        let entry_two =
            fill_entry_from_log(&fill_two, FillRole::Taker, &index).expect("second fill resolves");

        assert_eq!(entry_one.order_id, Some(U128::from(42u128)));
        assert_eq!(entry_two.order_id, Some(U128::from(42u128)));
        assert_eq!(entry_one.tx_hash, same_tx);
        assert_eq!(entry_two.tx_hash, same_tx);
        assert_ne!(
            entry_one.log_index, entry_two.log_index,
            "log index keeps same-tx fill rows distinct"
        );
        assert_eq!(entry_one.log_index, U256::from(4u64));
        assert_eq!(entry_two.log_index, U256::from(6u64));
    }

    #[test]
    fn fill_entry_taker_market_fill_uses_resting_pair_without_order_id() {
        // Market orders do not emit OrderSubmitted for the taker. We can still
        // return the fill with pair metadata by using the resting order's
        // submitted pair, but the counterparty resting id is not surfaced.
        let caller = address!("0x000000000000000000000000000000000000beef");
        let other = address!("0x000000000000000000000000000000000000c0de");
        let base = address!("0x0000000000000000000000000000000000ba51e1");
        let quote = address!("0x0000000000000000000000000000000000600073");

        let resting_submitted = make_order_submitted_log(
            other,
            99,
            base,
            quote,
            1_000_000,
            5,
            true,
            10,
            B256::with_last_byte(0xa1),
        );
        let index = build_pair_index(std::iter::once(&resting_submitted), &caller);
        let filled = make_order_filled_log(
            99,
            other,
            caller,
            200_000,
            5,
            13,
            B256::with_last_byte(0xb3),
        );

        let entry =
            fill_entry_from_log(&filled, FillRole::Taker, &index).expect("market fill resolves");
        assert_eq!(entry.order_id, None);
        assert_eq!(entry.base_token, base);
        assert_eq!(entry.quote_token, quote);
    }

    #[test]
    fn fill_entry_taker_returns_none_when_caller_has_no_same_tx_submission() {
        // Defensive: if upstream returns a taker-side OrderFilled but the
        // caller's OrderSubmitted index has no matching tx (shouldn't
        // happen for the caller's own activity), drop the fill rather
        // than fabricate a pair.
        let other = address!("0x000000000000000000000000000000000000c0de");
        let caller = address!("0x000000000000000000000000000000000000beef");

        let index = PairIndex::default();
        let filled =
            make_order_filled_log(99, other, caller, 100, 5, 13, B256::with_last_byte(0xb3));

        assert!(fill_entry_from_log(&filled, FillRole::Taker, &index).is_none());
    }

    #[test]
    fn fill_entry_maker_returns_none_when_pair_index_missing() {
        let caller = address!("0x000000000000000000000000000000000000beef");
        let other = address!("0x000000000000000000000000000000000000c0de");

        let index = PairIndex::default();
        let filled =
            make_order_filled_log(1, caller, other, 100, 5, 12, B256::with_last_byte(0xa2));

        assert!(fill_entry_from_log(&filled, FillRole::Maker, &index).is_none());
    }

    #[test]
    fn fill_entry_rejects_non_order_filled_topic() {
        let caller = address!("0x000000000000000000000000000000000000beef");
        let index = PairIndex::default();
        let bogus = make_log(
            DARKPOOL_ADDRESS,
            vec![
                OrderMatched::SIGNATURE_HASH,
                order_id_topic(1),
                order_id_topic(2),
                topic_addr(&caller),
            ],
        );
        assert!(
            fill_entry_from_log(&bogus, FillRole::Maker, &index).is_none(),
            "OrderMatched must not produce a FillEntry — it would double-count"
        );
    }
}
