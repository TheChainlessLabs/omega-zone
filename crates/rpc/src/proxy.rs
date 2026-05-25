//! HTTP proxy implementation of [`ZoneRpcApi`].
//!
//! [`ProxyZoneRpc`] forwards JSON-RPC requests to an upstream zone node and
//! applies privacy redactions on the responses. This allows the private RPC
//! service to run as a standalone process without linking against reth.

use std::{collections::HashMap, sync::Arc};

use alloy_network::ReceiptResponse;
use alloy_primitives::{Address, Bytes, U128, hex};
use alloy_rpc_types_eth::{BlockId, BlockNumberOrTag, Filter, FilterId, Log, state::StateOverride};
use alloy_sol_types::{SolCall, SolEvent};
use eyre::WrapErr;
use serde::Deserialize;
use serde_json::value::RawValue;
use tempo_alloy::rpc::{TempoTransactionReceipt, TempoTransactionRequest};
use tempo_contracts::precompiles::{
    ACCOUNT_KEYCHAIN_ADDRESS,
    account_keychain::IAccountKeychain::{self, KeyInfo, getKeyCall},
};
use tokio::sync::Mutex;

use crate::{
    auth::AuthContext,
    darkpool::{self, Cursor, FillRole, HistoryQuery, OrderFilled, Page, TransferQuery},
    filter,
    handlers::ZoneRpcApi,
    policy,
    types::{BoxEyreFut, BoxFut, JsonRpcError, internal, raw_null, raw_zero, to_raw},
};

/// Upstream JSON-RPC response envelope.
#[derive(Deserialize)]
struct UpstreamResponse {
    result: Option<Box<RawValue>>,
    error: Option<JsonRpcError>,
}

/// HTTP proxy implementation of [`ZoneRpcApi`].
///
/// Forwards requests to an upstream zone node's standard (non-private) RPC
/// endpoint and applies per-caller privacy redactions on the responses.
pub struct ProxyZoneRpc {
    client: reqwest::Client,
    upstream_url: String,
    /// Maps filter IDs to the authenticated account that created them.
    filter_owners: Arc<Mutex<HashMap<FilterId, Address>>>,
}

impl ProxyZoneRpc {
    /// Create a new proxy targeting the given upstream RPC URL.
    pub fn new(upstream_url: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            upstream_url,
            filter_owners: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Forward a JSON-RPC call to the upstream node.
    async fn forward(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<Box<RawValue>, JsonRpcError> {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
            "id": 1
        });

        let response = self
            .client
            .post(&self.upstream_url)
            .json(&request)
            .send()
            .await
            .map_err(|e| JsonRpcError::internal(e.to_string()))?;

        let upstream: UpstreamResponse = response
            .json()
            .await
            .map_err(|e| JsonRpcError::internal(e.to_string()))?;

        if let Some(err) = upstream.error {
            return Err(err);
        }

        upstream
            .result
            .ok_or_else(|| JsonRpcError::internal("missing result in upstream response"))
    }

    /// Verify that the filter belongs to the authenticated caller.
    async fn ensure_filter_owner(
        &self,
        id: &FilterId,
        auth: &AuthContext,
    ) -> Result<(), JsonRpcError> {
        let owners = self.filter_owners.lock().await;
        match owners.get(id) {
            Some(owner) if *owner == auth.caller => Ok(()),
            _ => Err(JsonRpcError::invalid_params("filter not found")),
        }
    }
}

/// Strip privacy-sensitive fields from a block JSON object for non-sequencer callers.
///
/// Zeroes `logsBloom` and replaces `transactions` with an empty array.
fn redact_block_json(value: &mut serde_json::Value) {
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "logsBloom".to_string(),
            serde_json::Value::String(format!("0x{}", "0".repeat(512))),
        );
        obj.insert("transactions".to_string(), serde_json::Value::Array(vec![]));
    }
}

/// Extract the `from` address from a JSON transaction or receipt object.
fn json_from(value: &serde_json::Value) -> Option<Address> {
    value.get("from")?.as_str()?.parse().ok()
}

impl ZoneRpcApi for ProxyZoneRpc {
    fn get_keychain_key(&self, account: Address, key_id: Address) -> BoxEyreFut<'_, KeyInfo> {
        Box::pin(async move {
            let call_data = getKeyCall {
                account,
                keyId: key_id,
            }
            .abi_encode();

            let result = self
                .forward(
                    "eth_call",
                    serde_json::json!([
                        {
                            "to": format!("{ACCOUNT_KEYCHAIN_ADDRESS:#x}"),
                            "input": format!("0x{}", hex::encode(call_data)),
                        },
                        "latest"
                    ]),
                )
                .await
                .map_err(|err| eyre::eyre!("AccountKeychain.getKey eth_call failed: {err}"))?;
            let output: Bytes = serde_json::from_str(result.get())
                .wrap_err("AccountKeychain.getKey returned invalid bytes")?;

            IAccountKeychain::getKeyCall::abi_decode_returns(output.as_ref()).map_err(Into::into)
        })
    }

    fn block_number(&self) -> BoxFut<'_> {
        Box::pin(async move { self.forward("eth_blockNumber", serde_json::json!([])).await })
    }

    fn chain_id(&self) -> BoxFut<'_> {
        Box::pin(async move { self.forward("eth_chainId", serde_json::json!([])).await })
    }

    fn net_version(&self) -> BoxFut<'_> {
        Box::pin(async move { self.forward("net_version", serde_json::json!([])).await })
    }

    fn gas_price(&self) -> BoxFut<'_> {
        Box::pin(async move { self.forward("eth_gasPrice", serde_json::json!([])).await })
    }

    fn max_priority_fee_per_gas(&self) -> BoxFut<'_> {
        Box::pin(async move {
            self.forward("eth_maxPriorityFeePerGas", serde_json::json!([]))
                .await
        })
    }

    fn fee_history(
        &self,
        block_count: u64,
        newest_block: BlockNumberOrTag,
        reward_percentiles: Option<Vec<f64>>,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            self.forward(
                "eth_feeHistory",
                serde_json::json!([block_count, newest_block, reward_percentiles]),
            )
            .await
        })
    }

    fn get_balance(
        &self,
        address: Address,
        block: Option<BlockId>,
        auth: AuthContext,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            if address != auth.caller {
                return Ok(raw_zero());
            }
            self.forward("eth_getBalance", serde_json::json!([address, block]))
                .await
        })
    }

    fn get_transaction_count(
        &self,
        address: Address,
        block: Option<BlockId>,
        auth: AuthContext,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            if address != auth.caller {
                return Ok(raw_zero());
            }
            self.forward(
                "eth_getTransactionCount",
                serde_json::json!([address, block]),
            )
            .await
        })
    }

    fn block_by_number(
        &self,
        number: BlockNumberOrTag,
        full: bool,
        _auth: AuthContext,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            let result = self
                .forward("eth_getBlockByNumber", serde_json::json!([number, full]))
                .await?;

            let mut block: serde_json::Value =
                serde_json::from_str(result.get()).map_err(internal)?;

            if block.is_null() {
                return Ok(result);
            }

            redact_block_json(&mut block);
            to_raw(&block)
        })
    }

    fn block_by_hash(
        &self,
        hash: alloy_primitives::B256,
        full: bool,
        _auth: AuthContext,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            let result = self
                .forward("eth_getBlockByHash", serde_json::json!([hash, full]))
                .await?;

            let mut block: serde_json::Value =
                serde_json::from_str(result.get()).map_err(internal)?;

            if block.is_null() {
                return Ok(result);
            }

            redact_block_json(&mut block);
            to_raw(&block)
        })
    }

    fn transaction_by_hash(&self, hash: alloy_primitives::B256, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            let result = self
                .forward("eth_getTransactionByHash", serde_json::json!([hash]))
                .await?;

            let tx: serde_json::Value = serde_json::from_str(result.get()).map_err(internal)?;

            if tx.is_null() {
                return Ok(result);
            }

            if json_from(&tx) != Some(auth.caller) {
                return Ok(raw_null());
            }

            Ok(result)
        })
    }

    fn transaction_receipt(&self, hash: alloy_primitives::B256, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            let result = self
                .forward("eth_getTransactionReceipt", serde_json::json!([hash]))
                .await?;

            let receipt: Option<TempoTransactionReceipt> =
                serde_json::from_str(result.get()).map_err(internal)?;

            let Some(receipt) = receipt else {
                return Ok(result);
            };

            if receipt.from() != auth.caller {
                return Ok(raw_null());
            }

            to_raw(&filter::filter_receipt_logs(receipt))
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

            policy::enforce_from(&mut request, &auth)?;
            policy::enforce_no_contract_creation(&request)?;

            self.forward(
                "eth_call",
                serde_json::json!([request, block, state_override]),
            )
            .await
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

            policy::enforce_from(&mut request, &auth)?;
            policy::enforce_no_contract_creation(&request)?;

            self.forward(
                "eth_estimateGas",
                serde_json::json!([request, block, state_override]),
            )
            .await
        })
    }

    fn send_raw_transaction(&self, data: Bytes, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            policy::verify_raw_tx_sender(&data, &auth)?;

            self.forward("eth_sendRawTransaction", serde_json::json!([data]))
                .await
        })
    }

    fn send_raw_transaction_sync(&self, data: Bytes, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            policy::verify_raw_tx_sender(&data, &auth)?;

            let result = self
                .forward("eth_sendRawTransactionSync", serde_json::json!([data]))
                .await?;

            let receipt: TempoTransactionReceipt =
                serde_json::from_str(result.get()).map_err(internal)?;
            to_raw(&filter::filter_receipt_logs(receipt))
        })
    }

    fn fill_transaction(
        &self,
        mut request: TempoTransactionRequest,
        auth: AuthContext,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            policy::enforce_from(&mut request, &auth)?;

            policy::enforce_no_contract_creation(&request)?;

            self.forward("eth_fillTransaction", serde_json::json!([request]))
                .await
        })
    }

    fn get_logs(&self, mut filter: Filter, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            filter::scope_filter(&mut filter);
            let result = self
                .forward("eth_getLogs", serde_json::json!([filter]))
                .await?;
            let logs: Vec<Log> = serde_json::from_str(result.get()).map_err(internal)?;
            let filtered = filter::filter_logs(logs, &auth.caller);
            to_raw(&filtered)
        })
    }

    fn new_filter(&self, mut filter: Filter, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            filter::scope_filter(&mut filter);
            let result = self
                .forward("eth_newFilter", serde_json::json!([filter]))
                .await?;
            let id: FilterId = serde_json::from_str(result.get()).map_err(internal)?;
            self.filter_owners.lock().await.insert(id, auth.caller);
            Ok(result)
        })
    }

    fn get_filter_logs(&self, id: FilterId, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            self.ensure_filter_owner(&id, &auth).await?;

            let result = self
                .forward("eth_getFilterLogs", serde_json::json!([id]))
                .await?;

            let logs: Vec<Log> = serde_json::from_str(result.get()).map_err(internal)?;
            let filtered = filter::filter_logs(logs, &auth.caller);
            to_raw(&filtered)
        })
    }

    fn get_filter_changes(&self, id: FilterId, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            self.ensure_filter_owner(&id, &auth).await?;

            let result = self
                .forward("eth_getFilterChanges", serde_json::json!([id]))
                .await?;

            // Try to parse as logs for filtering. If the result is block hashes
            // (from a block filter) or empty, the parse will fail and we pass through.
            if let Ok(logs) = serde_json::from_str::<Vec<Log>>(result.get()) {
                let filtered = filter::filter_logs(logs, &auth.caller);
                return to_raw(&filtered);
            }

            Ok(result)
        })
    }

    fn new_block_filter(&self, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            let result = self
                .forward("eth_newBlockFilter", serde_json::json!([]))
                .await?;
            let id: FilterId = serde_json::from_str(result.get()).map_err(internal)?;
            self.filter_owners.lock().await.insert(id, auth.caller);
            Ok(result)
        })
    }

    fn uninstall_filter(&self, id: FilterId, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            self.ensure_filter_owner(&id, &auth).await?;

            let result = self
                .forward("eth_uninstallFilter", serde_json::json!([id]))
                .await?;

            self.filter_owners.lock().await.remove(&id);

            Ok(result)
        })
    }

    fn zone_get_authorization_token_info(&self, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            to_raw(&serde_json::json!({
                "account": auth.caller,
                "expiresAt": alloy_primitives::U64::from(auth.expires_at),
            }))
        })
    }

    fn zone_get_zone_info(&self, _auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            Err(JsonRpcError::internal(
                "zone-specific methods are not supported by the proxy backend",
            ))
        })
    }

    fn zone_get_deposit_status(&self, _tempo_block_number: u64, _auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            Err(JsonRpcError::internal(
                "zone-specific methods are not supported by the proxy backend",
            ))
        })
    }

    fn zone_get_my_orders(&self, query: HistoryQuery, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            let owner = darkpool::require_owner(query.account, &auth.caller)?;
            let limit = darkpool::clamp_limit(query.limit);
            let cursor = query.cursor.as_deref().map(Cursor::decode).transpose()?;
            let pair_filter = darkpool::parse_pair_filter(query.pair.as_deref())?;

            let owner_topic = darkpool::topic_for_address(&owner);
            let topics_filter = vec![
                darkpool::OrderSubmitted::SIGNATURE_HASH,
                darkpool::OrderPlaced::SIGNATURE_HASH,
                darkpool::OrderFilled::SIGNATURE_HASH,
                darkpool::OrderCancelled::SIGNATURE_HASH,
            ];
            let filter = darkpool::build_darkpool_filter(&topics_filter, Some(owner_topic), cursor);
            let logs = self.fetch_logs(filter).await?;

            // Defence-in-depth: re-check ownership client-side in case the
            // upstream node ignored the topic constraint.
            let mut orders = darkpool::reconstruct_orders(
                logs.iter()
                    .filter(|log| darkpool::caller_is_maker(log, &owner)),
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

            let next_cursor = darkpool::next_order_cursor(&orders, limit);
            orders.truncate(limit as usize);

            to_raw(&Page {
                items: orders,
                next_cursor,
            })
        })
    }

    fn zone_get_my_fills(&self, query: HistoryQuery, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            let owner = darkpool::require_owner(query.account, &auth.caller)?;
            let limit = darkpool::clamp_limit(query.limit);
            let cursor = query.cursor.as_deref().map(Cursor::decode).transpose()?;
            let pair_filter = darkpool::parse_pair_filter(query.pair.as_deref())?;

            let owner_topic = darkpool::topic_for_address(&owner);
            let topics = vec![OrderFilled::SIGNATURE_HASH];

            // OrderSubmitted is the only place that carries (base, quote).
            // We scan from genesis because a fill at the current cursor can
            // reference an older resting order. Foreign submissions are used
            // only for pair metadata; their order ids are never returned.
            let submitted_filter = darkpool::build_darkpool_filter(
                &[darkpool::OrderSubmitted::SIGNATURE_HASH],
                None,
                None,
            );

            let maker_filter = darkpool::build_darkpool_filter(&topics, Some(owner_topic), cursor);
            let mut taker_filter = darkpool::build_darkpool_filter(&topics, None, cursor);
            taker_filter.topics[3] = alloy_rpc_types_eth::FilterSet::from(owner_topic);

            let (submitted_logs, maker_logs, taker_logs) = tokio::try_join!(
                self.fetch_logs(submitted_filter),
                self.fetch_logs(maker_filter),
                self.fetch_logs(taker_filter),
            )?;

            let pair_index = darkpool::build_pair_index(submitted_logs.iter(), &owner);

            let mut fills: Vec<darkpool::FillEntry> = maker_logs
                .iter()
                .filter(|log| darkpool::caller_is_maker(log, &owner))
                .filter_map(|log| darkpool::fill_entry_from_log(log, FillRole::Maker, &pair_index))
                .chain(
                    taker_logs
                        .iter()
                        .filter(|log| darkpool::caller_is_taker(log, &owner))
                        .filter_map(|log| {
                            darkpool::fill_entry_from_log(log, FillRole::Taker, &pair_index)
                        }),
                )
                .collect();

            if let Some(pair) = pair_filter {
                fills.retain(|f| f.base_token == pair.0 && f.quote_token == pair.1);
            }

            fills.sort_by(|a, b| {
                b.block_number
                    .cmp(&a.block_number)
                    .then_with(|| b.tx_hash.cmp(&a.tx_hash))
            });
            fills.dedup_by(|a, b| {
                a.tx_hash == b.tx_hash && a.order_id == b.order_id && a.role == b.role
            });

            let next_cursor = darkpool::next_fill_cursor(&fills, limit);
            fills.truncate(limit as usize);

            to_raw(&Page {
                items: fills,
                next_cursor,
            })
        })
    }

    fn zone_get_my_transfers(&self, query: TransferQuery, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            let owner = darkpool::require_owner(query.account, &auth.caller)?;
            let limit = darkpool::clamp_limit(query.limit);
            let cursor = query.cursor.as_deref().map(Cursor::decode).transpose()?;

            let owner_topic = darkpool::topic_for_address(&owner);
            let transfer_topics = vec![
                filter::TRANSFER_TOPIC,
                filter::TRANSFER_WITH_MEMO_TOPIC,
                filter::MINT_TOPIC,
                filter::BURN_TOPIC,
            ];
            let from_filter =
                darkpool::build_tip20_filter(&transfer_topics, Some(owner_topic), cursor, true);
            let to_filter =
                darkpool::build_tip20_filter(&transfer_topics, Some(owner_topic), cursor, false);

            let (from_logs, to_logs) =
                tokio::try_join!(self.fetch_logs(from_filter), self.fetch_logs(to_filter))?;

            let mut transfers: Vec<darkpool::TransferEntry> = from_logs
                .into_iter()
                .chain(to_logs)
                .filter(|log| filter::is_log_visible(log, &owner))
                .filter_map(|log| darkpool::transfer_entry_from_log(&log, &owner))
                .collect();

            transfers.sort_by(|a, b| {
                b.block_number
                    .cmp(&a.block_number)
                    .then_with(|| b.log_index.cmp(&a.log_index))
            });
            transfers.dedup_by(|a, b| a.tx_hash == b.tx_hash && a.log_index == b.log_index);

            let next_cursor = darkpool::next_transfer_cursor(&transfers, limit);
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
            let filter = darkpool::build_order_filter(order_id, &owner);
            let logs = self.fetch_logs(filter).await?;
            let mut orders = darkpool::reconstruct_orders(
                logs.iter()
                    .filter(|log| darkpool::caller_is_maker(log, &owner)),
            );
            match orders.pop() {
                Some(order) if order.order_id == U128::from(order_id) => to_raw(&order),
                _ => Ok(raw_null()),
            }
        })
    }
}

impl ProxyZoneRpc {
    /// Forward an `eth_getLogs` query and decode the response into a `Vec<Log>`.
    async fn fetch_logs(&self, filter: Filter) -> Result<Vec<Log>, JsonRpcError> {
        let raw = self
            .forward("eth_getLogs", serde_json::json!([filter]))
            .await?;
        let logs: Vec<Log> = serde_json::from_str(raw.get()).map_err(internal)?;
        Ok(logs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::ReceiptWithBloom;
    use alloy_primitives::{B256, Bytes as PrimitiveBytes, LogData, TxHash, address};
    use alloy_rpc_types_eth::TransactionReceipt;
    use axum::{Json, Router, routing::post};
    use tempo_primitives::{TempoReceipt, TempoTxType};

    fn make_log(emitter: Address, topics: Vec<B256>) -> Log {
        Log {
            inner: alloy_primitives::Log {
                address: emitter,
                data: LogData::new_unchecked(topics, PrimitiveBytes::new()),
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

    fn caller_word(addr: &Address) -> B256 {
        B256::left_padding_from(addr.as_slice())
    }

    fn make_receipt(from: Address, logs: Vec<Log>) -> TempoTransactionReceipt {
        let receipt = TempoReceipt {
            tx_type: TempoTxType::Legacy,
            success: true,
            cumulative_gas_used: 21_000,
            logs,
        };

        TempoTransactionReceipt {
            inner: TransactionReceipt {
                inner: ReceiptWithBloom::from(receipt),
                transaction_hash: TxHash::with_last_byte(1),
                transaction_index: Some(0),
                block_hash: Some(B256::with_last_byte(2)),
                block_number: Some(1),
                gas_used: 21_000,
                effective_gas_price: 1,
                blob_gas_used: None,
                blob_gas_price: None,
                from,
                to: Some(Address::ZERO),
                contract_address: None,
            },
            fee_token: None,
            fee_payer: from,
        }
    }

    async fn spawn_upstream(result: serde_json::Value) -> String {
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": result,
        });

        let app = Router::new().route(
            "/",
            post(move || {
                let response = response.clone();
                async move { Json(response) }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind upstream test server");
        let addr = listener.local_addr().expect("read upstream addr");
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve upstream test server");
        });

        format!("http://{addr}")
    }

    /// Build a log decorated with block / log_index metadata so cursors and
    /// dedup paths behave correctly in tests.
    fn make_log_at(
        emitter: Address,
        topics: Vec<B256>,
        data: PrimitiveBytes,
        block_number: u64,
        log_index: u64,
        tx_hash: B256,
    ) -> Log {
        Log {
            inner: alloy_primitives::Log {
                address: emitter,
                data: LogData::new_unchecked(topics, data),
            },
            block_hash: None,
            block_number: Some(block_number),
            block_timestamp: None,
            transaction_hash: Some(tx_hash),
            transaction_index: Some(0),
            log_index: Some(log_index),
            removed: false,
        }
    }

    fn order_submitted_data(
        base: Address,
        quote: Address,
        amount: u128,
        price: u128,
        is_bid: bool,
    ) -> PrimitiveBytes {
        use alloy_sol_types::SolValue;
        (base, quote, amount, price, is_bid).abi_encode().into()
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
        log_index: u64,
        tx_hash: B256,
    ) -> Log {
        make_log_at(
            darkpool::DARKPOOL_ADDRESS,
            vec![
                darkpool::OrderSubmitted::SIGNATURE_HASH,
                darkpool::order_id_topic(order_id),
                darkpool::topic_for_address(&maker),
            ],
            order_submitted_data(base, quote, amount, price, is_bid),
            block,
            log_index,
            tx_hash,
        )
    }

    /// Spin up an upstream that responds to every JSON-RPC method with the
    /// same `result` array. Sufficient for the orders pass which only issues
    /// one `eth_getLogs` call.
    async fn spawn_upstream_logs(result: serde_json::Value) -> String {
        spawn_upstream(result).await
    }

    #[tokio::test]
    async fn zone_get_my_orders_only_returns_callers_logs() {
        // Two makers, one shared order book. The mock upstream returns both
        // logs even though our request-side topic filter only asked for the
        // caller — proves the proxy's defence-in-depth post-filter actually
        // drops the foreign log.
        let caller = address!("0x000000000000000000000000000000000000beef");
        let foreign = address!("0x000000000000000000000000000000000000c0de");
        let base = address!("0x0000000000000000000000000000000000ba51e1");
        let quote = address!("0x0000000000000000000000000000000000600073");

        let mine = make_order_submitted_log(
            caller,
            1,
            base,
            quote,
            1_000_000,
            5,
            true,
            10,
            0,
            B256::with_last_byte(0xa1),
        );
        let theirs = make_order_submitted_log(
            foreign,
            2,
            base,
            quote,
            2_000_000,
            6,
            false,
            11,
            0,
            B256::with_last_byte(0xa2),
        );

        let upstream =
            spawn_upstream_logs(serde_json::to_value(vec![mine.clone(), theirs.clone()]).unwrap())
                .await;
        let proxy = ProxyZoneRpc::new(upstream);

        let raw = proxy
            .zone_get_my_orders(
                darkpool::HistoryQuery::default(),
                AuthContext {
                    caller,
                    expires_at: u64::MAX,
                },
            )
            .await
            .expect("orders request should succeed");

        let page: darkpool::Page<darkpool::OrderEntry> =
            serde_json::from_str(raw.get()).expect("deserialize orders page");
        assert_eq!(page.items.len(), 1, "only caller-owned order returned");
        assert_eq!(page.items[0].order_id, U128::from(1u128));
        assert_eq!(page.items[0].base_token, base);
        assert_eq!(page.items[0].quote_token, quote);
        assert_eq!(page.items[0].amount, U128::from(1_000_000u128));
        assert!(matches!(page.items[0].side, darkpool::Side::Bid));
        assert!(matches!(page.items[0].status, darkpool::OrderStatus::Open));
        assert!(page.next_cursor.is_none());
    }

    #[tokio::test]
    async fn zone_get_my_orders_rejects_foreign_account_param() {
        let caller = address!("0x000000000000000000000000000000000000beef");
        let foreign = address!("0x000000000000000000000000000000000000c0de");

        let upstream = spawn_upstream_logs(serde_json::to_value(Vec::<Log>::new()).unwrap()).await;
        let proxy = ProxyZoneRpc::new(upstream);

        let err = proxy
            .zone_get_my_orders(
                darkpool::HistoryQuery {
                    account: Some(foreign),
                    ..Default::default()
                },
                AuthContext {
                    caller,
                    expires_at: u64::MAX,
                },
            )
            .await
            .expect_err("foreign account must be rejected");
        assert_eq!(err.code, -32004);
    }

    #[tokio::test]
    async fn zone_get_order_returns_null_for_other_owners_order() {
        // When upstream returns no logs (because the topic filter was scoped
        // to the caller), zone_getOrder should return JSON null, NOT leak
        // whether the order id exists for a different maker.
        let caller = address!("0x000000000000000000000000000000000000beef");
        let upstream = spawn_upstream_logs(serde_json::to_value(Vec::<Log>::new()).unwrap()).await;
        let proxy = ProxyZoneRpc::new(upstream);

        let raw = proxy
            .zone_get_order(
                42,
                AuthContext {
                    caller,
                    expires_at: u64::MAX,
                },
            )
            .await
            .expect("zone_getOrder should succeed");
        assert_eq!(raw.get(), "null");
    }

    fn order_filled_data(amount_filled: u128, price: u128) -> PrimitiveBytes {
        use alloy_sol_types::SolValue;
        (amount_filled, price).abi_encode().into()
    }

    fn make_order_filled_log(
        resting_order_id: u128,
        resting_maker: Address,
        taker: Address,
        amount_filled: u128,
        price: u128,
        block: u64,
        log_index: u64,
        tx_hash: B256,
    ) -> Log {
        make_log_at(
            darkpool::DARKPOOL_ADDRESS,
            vec![
                darkpool::OrderFilled::SIGNATURE_HASH,
                darkpool::order_id_topic(resting_order_id),
                darkpool::topic_for_address(&resting_maker),
                darkpool::topic_for_address(&taker),
            ],
            order_filled_data(amount_filled, price),
            block,
            log_index,
            tx_hash,
        )
    }

    /// End-to-end: `zone_getMyFills` must populate `baseToken` / `quoteToken`
    /// from the caller's own `OrderSubmitted` events (the precompile does not
    /// carry pair metadata on `OrderFilled` itself). This test fails on the
    /// old behaviour, which left base/quote at `Address::ZERO` and therefore
    /// dropped every row when a pair filter was applied.
    #[tokio::test]
    async fn zone_get_my_fills_populates_pair_metadata_for_both_roles() {
        let caller = address!("0x000000000000000000000000000000000000beef");
        let other = address!("0x000000000000000000000000000000000000c0de");
        let base = address!("0x0000000000000000000000000000000000ba51e1");
        let quote = address!("0x0000000000000000000000000000000000600073");

        // Caller's resting bid (orderId=1) and an unrelated taker fill of it.
        let submitted_bid = make_order_submitted_log(
            caller,
            1,
            base,
            quote,
            1_000_000,
            5,
            true,
            10,
            0,
            B256::with_last_byte(0xa1),
        );
        let maker_fill = make_order_filled_log(
            1,
            caller,
            other,
            500_000,
            5,
            12,
            0,
            B256::with_last_byte(0xa2),
        );

        // Caller later sends a taker ask (orderId=2) that fully fills against
        // someone else's resting bid (orderId=99). Both events share the
        // caller's tx hash, so the tx-hash join inside fill_entry_from_log
        // resolves the caller's own incoming order id and pair.
        let taker_tx = B256::with_last_byte(0xb3);
        let submitted_ask =
            make_order_submitted_log(caller, 2, base, quote, 200_000, 5, false, 13, 0, taker_tx);
        let taker_fill = make_order_filled_log(99, other, caller, 200_000, 5, 13, 1, taker_tx);

        let upstream = spawn_upstream_logs(
            serde_json::to_value(vec![submitted_bid, submitted_ask, maker_fill, taker_fill])
                .unwrap(),
        )
        .await;
        let proxy = ProxyZoneRpc::new(upstream);
        let auth = AuthContext {
            caller,
            expires_at: u64::MAX,
        };

        // Unfiltered query — should return both fills with correct pair.
        let raw = proxy
            .zone_get_my_fills(darkpool::HistoryQuery::default(), auth.clone())
            .await
            .expect("fills request should succeed");
        let page: darkpool::Page<darkpool::FillEntry> =
            serde_json::from_str(raw.get()).expect("deserialize fills page");
        assert_eq!(
            page.items.len(),
            2,
            "two fills expected (maker + taker leg)"
        );

        // Newest first: the taker fill is in block 13.
        let taker_row = &page.items[0];
        assert!(matches!(taker_row.role, darkpool::FillRole::Taker));
        assert_eq!(
            taker_row.base_token, base,
            "taker fill must carry pair base"
        );
        assert_eq!(
            taker_row.quote_token, quote,
            "taker fill must carry pair quote"
        );
        assert_eq!(
            taker_row.order_id,
            Some(U128::from(2u128)),
            "taker fill order_id must be the caller's own order, not the counterparty's"
        );
        assert_eq!(taker_row.amount_filled, U128::from(200_000u128));

        let maker_row = &page.items[1];
        assert!(matches!(maker_row.role, darkpool::FillRole::Maker));
        assert_eq!(maker_row.order_id, Some(U128::from(1u128)));
        assert_eq!(
            maker_row.base_token, base,
            "maker fill must carry pair base"
        );
        assert_eq!(maker_row.quote_token, quote);
        assert_eq!(maker_row.amount_filled, U128::from(500_000u128));

        // Privacy: serialized FillEntry must not include the counterparty's order id.
        assert!(
            !raw.get().contains("counterpartyOrderId"),
            "fills response must not expose counterparty order ids: {}",
            raw.get(),
        );

        // Pair filter on the matching pair — both rows survive.
        let matching_pair = format!("{base:#x}/{quote:#x}");
        let raw = proxy
            .zone_get_my_fills(
                darkpool::HistoryQuery {
                    pair: Some(matching_pair),
                    ..Default::default()
                },
                auth.clone(),
            )
            .await
            .expect("pair-filtered fills request should succeed");
        let page: darkpool::Page<darkpool::FillEntry> =
            serde_json::from_str(raw.get()).expect("deserialize pair-filtered fills page");
        assert_eq!(
            page.items.len(),
            2,
            "pair filter matching the actual pair should retain both fills (this assertion fails on the old base=ZERO behaviour)"
        );

        // Pair filter on a different pair — drops every row.
        let unrelated_base = address!("0x000000000000000000000000000000000000dead");
        let unrelated_pair = format!("{unrelated_base:#x}/{quote:#x}");
        let raw = proxy
            .zone_get_my_fills(
                darkpool::HistoryQuery {
                    pair: Some(unrelated_pair),
                    ..Default::default()
                },
                auth,
            )
            .await
            .expect("unrelated-pair fills request should succeed");
        let page: darkpool::Page<darkpool::FillEntry> =
            serde_json::from_str(raw.get()).expect("deserialize unrelated-pair fills page");
        assert!(
            page.items.is_empty(),
            "unrelated pair filter must drop all fills"
        );
    }

    #[tokio::test]
    async fn transaction_receipt_filters_logs() {
        let caller = address!("0x0000000000000000000000000000000000000001");
        let other = address!("0x0000000000000000000000000000000000000002");
        let third = address!("0x0000000000000000000000000000000000000003");

        let visible = make_log(
            Address::ZERO,
            vec![
                filter::TRANSFER_TOPIC,
                caller_word(&caller),
                caller_word(&other),
            ],
        );
        let hidden = make_log(
            Address::ZERO,
            vec![
                filter::TRANSFER_TOPIC,
                caller_word(&other),
                caller_word(&third),
            ],
        );
        let upstream = make_receipt(caller, vec![visible.clone(), hidden]);
        let proxy =
            ProxyZoneRpc::new(spawn_upstream(serde_json::to_value(&upstream).unwrap()).await);

        let raw = proxy
            .transaction_receipt(
                TxHash::with_last_byte(1),
                AuthContext {
                    caller,
                    expires_at: u64::MAX,
                },
            )
            .await
            .expect("proxy should return receipt");

        let receipt: TempoTransactionReceipt =
            serde_json::from_str(raw.get()).expect("deserialize filtered receipt");
        assert_eq!(receipt.inner.logs(), std::slice::from_ref(&visible));
        assert_eq!(
            receipt.inner.inner.logs_bloom,
            alloy_primitives::logs_bloom(receipt.inner.logs().iter().map(|log| log.as_ref())),
        );
        assert_ne!(
            receipt.inner.inner.logs_bloom,
            upstream.inner.inner.logs_bloom
        );
    }
}
