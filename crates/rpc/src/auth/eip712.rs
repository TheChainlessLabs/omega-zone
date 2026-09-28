//! EIP-712 typed data for zone RPC authorization tokens (version 1).
//!
//! The typed data structure used by [`crate::auth::token`] for `version == 1`
//! tokens. This provides a wallet-native signing flow via `eth_signTypedData_v4`.

use alloy_primitives::{B256, U256, hex, keccak256};

/// EIP-712 domain name for zone RPC auth tokens.
pub const EIP712_DOMAIN_NAME: &[u8] = b"TempoZoneRPC";

/// EIP-712 domain version. Bumped together with the token version byte.
pub const EIP712_DOMAIN_VERSION: &[u8] = b"1";

/// `EIP712Domain(string name,string version,uint256 chainId)`
pub const EIP712_DOMAIN_TYPE_HASH: B256 = B256::new(hex!(
    "c2f8787176b8ac6bf7215b4adcc1e069bf4ab82d9ab1df05a57a91d425935b6e"
));

/// `ZoneRPCAuth(uint32 zoneId,uint64 issuedAt,uint64 expiresAt)`
pub const ZONE_RPC_AUTH_TYPE_HASH: B256 = B256::new(hex!(
    "233c6ebf0d655a3a06683eaf1a4002d4e6b8407efec9bee487dfea782cd238bf"
));

/// Compute the EIP-712 domain separator for the given chain.
pub fn domain_separator(chain_id: u64) -> B256 {
    let mut buf = Vec::with_capacity(32 + 32 + 32 + 32);
    buf.extend_from_slice(EIP712_DOMAIN_TYPE_HASH.as_slice());
    buf.extend_from_slice(keccak256(EIP712_DOMAIN_NAME).as_slice());
    buf.extend_from_slice(keccak256(EIP712_DOMAIN_VERSION).as_slice());
    buf.extend_from_slice(&U256::from(chain_id).to_be_bytes::<32>());
    keccak256(&buf)
}

/// Compute the EIP-712 struct hash for a `ZoneRPCAuth` message.
pub fn struct_hash(zone_id: u32, issued_at: u64, expires_at: u64) -> B256 {
    let mut buf = Vec::with_capacity(32 * 4);
    buf.extend_from_slice(ZONE_RPC_AUTH_TYPE_HASH.as_slice());
    buf.extend_from_slice(&U256::from(zone_id).to_be_bytes::<32>());
    buf.extend_from_slice(&U256::from(issued_at).to_be_bytes::<32>());
    buf.extend_from_slice(&U256::from(expires_at).to_be_bytes::<32>());
    keccak256(&buf)
}

/// Compute the EIP-712 digest for a zone RPC auth token.
pub fn digest(zone_id: u32, chain_id: u64, issued_at: u64, expires_at: u64) -> B256 {
    let mut buf = Vec::with_capacity(2 + 32 + 32);
    buf.push(0x19);
    buf.push(0x01);
    buf.extend_from_slice(domain_separator(chain_id).as_slice());
    buf.extend_from_slice(struct_hash(zone_id, issued_at, expires_at).as_slice());
    keccak256(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_hash_constants_match_keccak256() {
        assert_eq!(
            EIP712_DOMAIN_TYPE_HASH,
            keccak256(b"EIP712Domain(string name,string version,uint256 chainId)")
        );
        assert_eq!(
            ZONE_RPC_AUTH_TYPE_HASH,
            keccak256(b"ZoneRPCAuth(uint32 zoneId,uint64 issuedAt,uint64 expiresAt)")
        );
    }

    #[test]
    fn eip712_digest_is_deterministic() {
        assert_eq!(
            digest(10, 421700010, 1_000, 1_600),
            digest(10, 421700010, 1_000, 1_600)
        );
    }

    #[test]
    fn eip712_digest_matches_viem_reference_vector() {
        assert_eq!(
            digest(10, 421700010, 1_700_000_000, 1_700_000_600),
            B256::from_slice(
                &alloy_primitives::hex::decode(
                    "938c4a868d932fca1550d00c9afe8c6ef6aaa93b0057e27be7511f653035a7b9",
                )
                .unwrap(),
            ),
        );
    }

    #[test]
    fn eip712_digest_changes_with_zone_id() {
        assert_ne!(
            digest(10, 421700010, 1_000, 1_600),
            digest(11, 421700010, 1_000, 1_600)
        );
    }

    #[test]
    fn eip712_digest_changes_with_chain_id() {
        assert_ne!(
            digest(10, 421700010, 1_000, 1_600),
            digest(10, 421700011, 1_000, 1_600)
        );
    }

    #[test]
    fn eip712_digest_changes_with_issued_at() {
        assert_ne!(
            digest(10, 421700010, 1_000, 1_600),
            digest(10, 421700010, 1_001, 1_600)
        );
    }

    #[test]
    fn eip712_digest_changes_with_expires_at() {
        assert_ne!(
            digest(10, 421700010, 1_000, 1_600),
            digest(10, 421700010, 1_000, 1_601)
        );
    }
}
