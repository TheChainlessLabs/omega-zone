//! Privacy policy enforcement helpers.
//!
//! Shared between the in-process ([`TempoZoneRpc`]) and proxy
//! ([`ProxyZoneRpc`]) implementations of [`ZoneRpcApi`].

use alloy_consensus::transaction::SignerRecoverable;
use alloy_eips::eip2718::Decodable2718;
use alloy_network::TransactionBuilder;
use tempo_alloy::rpc::TempoTransactionRequest;
use tempo_primitives::TempoTxEnvelope;

use crate::{auth::AuthContext, types::JsonRpcError};

const CONTRACT_CREATION_NOT_SUPPORTED: &str = "contract creation not supported on zones";

/// Enforce that `from` matches the authenticated caller.
///
/// - If `from` is omitted, sets it to `auth.caller`.
/// - If present and mismatched, returns `-32004 Account mismatch`.
pub fn enforce_from(
    request: &mut TempoTransactionRequest,
    auth: &AuthContext,
) -> Result<(), JsonRpcError> {
    match TransactionBuilder::from(request as &TempoTransactionRequest) {
        Some(from) if from != auth.caller => Err(JsonRpcError::account_mismatch()),
        None => {
            request.set_from(auth.caller);
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Reject create-style transaction requests.
///
/// Zones do not support contract creation, so plain Ethereum-style create
/// requests (`to = null`) and Tempo AA calls targeting `TxKind::Create` are
/// rejected with `-32602 Invalid params`.
pub fn enforce_no_contract_creation(request: &TempoTransactionRequest) -> Result<(), JsonRpcError> {
    let outer_create = request.inner.to.is_some_and(|to| to.is_create());
    let implicit_plain_create = request.calls.is_empty() && request.inner.to.is_none();
    let tempo_create = request.calls.iter().any(|call| call.to.is_create());

    if outer_create || implicit_plain_create || tempo_create {
        return Err(JsonRpcError::invalid_params(
            CONTRACT_CREATION_NOT_SUPPORTED,
        ));
    }

    Ok(())
}

/// Decode a raw transaction and verify the recovered sender matches the
/// authenticated caller. Returns `-32003 Transaction rejected` on mismatch.
pub fn verify_raw_tx_sender(data: &[u8], auth: &AuthContext) -> Result<(), JsonRpcError> {
    let tx = TempoTxEnvelope::decode_2718_exact(data)
        .map_err(|_| JsonRpcError::invalid_params("failed to decode transaction"))?;

    let sender = tx
        .recover_signer()
        .map_err(|_| JsonRpcError::invalid_params("invalid transaction signature"))?;

    if sender != auth.caller {
        return Err(JsonRpcError::transaction_rejected());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{SignableTransaction, TxEip1559};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Address, Bytes, TxKind, U256};
    use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use tempo_alloy::rpc::TempoTransactionRequest;
    use tempo_primitives::{TempoTxEnvelope, transaction::Call};

    use super::{enforce_no_contract_creation, verify_raw_tx_sender};
    use crate::auth::AuthContext;

    fn call_target(byte: u8) -> TxKind {
        TxKind::Call(Address::repeat_byte(byte))
    }

    fn call_request(to: Option<TxKind>) -> TempoTransactionRequest {
        TempoTransactionRequest {
            inner: TransactionRequest {
                to,
                input: TransactionInput::new(Bytes::default()),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn no_create_allows_standard_call_request() {
        let request = call_request(Some(call_target(0x11)));
        assert!(enforce_no_contract_creation(&request).is_ok());
    }

    #[test]
    fn no_create_rejects_plain_create_request() {
        let request = call_request(None);
        let err = enforce_no_contract_creation(&request).unwrap_err();
        assert_eq!(err.code, -32602);
        assert_eq!(err.message, "contract creation not supported on zones");
    }

    #[test]
    fn no_create_rejects_explicit_outer_create_request() {
        let request = call_request(Some(TxKind::Create));
        let err = enforce_no_contract_creation(&request).unwrap_err();
        assert_eq!(err.code, -32602);
        assert_eq!(err.message, "contract creation not supported on zones");
    }

    #[test]
    fn no_create_allows_tempo_calls_without_outer_to() {
        let mut request = call_request(None);
        request.calls = vec![Call {
            to: call_target(0x22),
            value: U256::ZERO,
            input: Bytes::default(),
        }];

        assert!(enforce_no_contract_creation(&request).is_ok());
    }

    fn sign_eip1559_tx(signer: &PrivateKeySigner) -> Vec<u8> {
        let tx = TxEip1559 {
            chain_id: 99,
            nonce: 0,
            gas_limit: 21_000,
            max_fee_per_gas: 1,
            max_priority_fee_per_gas: 1,
            to: TxKind::Call(Address::repeat_byte(0x11)),
            value: U256::ZERO,
            access_list: Default::default(),
            input: Bytes::default(),
        };
        let sig = signer
            .sign_hash_sync(&tx.signature_hash())
            .expect("eip-1559 signing");
        let signed = tx.into_signed(sig);
        TempoTxEnvelope::Eip1559(signed).encoded_2718()
    }

    #[test]
    fn verify_raw_tx_sender_accepts_matching_caller() {
        let signer = PrivateKeySigner::random();
        let encoded = sign_eip1559_tx(&signer);
        let auth = AuthContext {
            caller: signer.address(),
            expires_at: u64::MAX,
        };

        verify_raw_tx_sender(&encoded, &auth).expect("matching sender accepted");
    }

    #[test]
    fn verify_raw_tx_sender_rejects_sender_mismatch() {
        let signer = PrivateKeySigner::random();
        let encoded = sign_eip1559_tx(&signer);
        let auth = AuthContext {
            caller: Address::repeat_byte(0xff),
            expires_at: u64::MAX,
        };

        let err = verify_raw_tx_sender(&encoded, &auth).unwrap_err();
        assert_eq!(err.code, -32003);
        assert_eq!(err.message, "Transaction rejected");
    }

    #[test]
    fn verify_raw_tx_sender_rejects_malformed_payload() {
        let auth = AuthContext {
            caller: Address::repeat_byte(0xaa),
            expires_at: u64::MAX,
        };

        let err = verify_raw_tx_sender(b"not-a-tx", &auth).unwrap_err();
        assert_eq!(err.code, -32602);
        assert!(err.message.contains("decode"));
    }

    #[test]
    fn no_create_rejects_tempo_create_call() {
        let mut request = call_request(None);
        request.calls = vec![Call {
            to: TxKind::Create,
            value: U256::ZERO,
            input: Bytes::default(),
        }];

        let err = enforce_no_contract_creation(&request).unwrap_err();
        assert_eq!(err.code, -32602);
        assert_eq!(err.message, "contract creation not supported on zones");
    }
}
