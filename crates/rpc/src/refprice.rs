//! Reference-price provider configuration for the alpha private RPC.
//!
//! The provider abstraction lets `zone_getReferencePrice` answer truthfully
//! whether the alpha node is publishing a public reference price, what that
//! price is, where it came from, when it was minted, and how stale it has
//! become. The pure validation helper lives in
//! [`zone_precompiles::refprice`] so future enforcement points (RPC pre-check
//! or precompile-side guard) can share it.
//!
//! This is alpha infrastructure — not a production oracle.

use serde::{Deserialize, Serialize};

/// Configuration for the alpha reference-price provider.
///
/// Frontends interpret `None` (provider not configured) as "no public
/// reference price available". When `Some`, the RPC layer surfaces the
/// price, source, snapshot timestamp/block, and the configured guardrail
/// bounds via `zone_getReferencePrice`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ReferencePriceProviderConfig {
    /// Maximum allowed deviation between an order's limit price and the
    /// reference price, in basis points. `0` rejects any non-equal price.
    pub max_deviation_bps: u32,
    /// Maximum staleness window in seconds; `0` disables the staleness check
    /// (the natural setting for a static provider).
    pub max_staleness_secs: u64,
    /// Provider variant supplying the canonical reference price.
    pub kind: ReferencePriceProviderKind,
}

/// Reference-price provider variant.
///
/// Only `Static` is implemented today. The enum exists so a future external
/// oracle adapter can slot in without changing the RPC shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReferencePriceProviderKind {
    /// A statically configured price (alpha default).
    Static {
        /// Raw integer price; same units as the orderbook precompile.
        price: u128,
        /// Origin tag surfaced to clients (e.g. `"static:alpha"`).
        source: String,
    },
}

impl ReferencePriceProviderConfig {
    /// Convenience constructor for the alpha static provider.
    pub fn static_alpha(price: u128, max_deviation_bps: u32) -> Self {
        Self {
            max_deviation_bps,
            max_staleness_secs: 0,
            kind: ReferencePriceProviderKind::Static {
                price,
                source: "static:alpha".to_string(),
            },
        }
    }

    /// Borrow the static snapshot, if the provider is a static variant.
    pub fn static_snapshot(&self) -> Option<(u128, &str)> {
        let ReferencePriceProviderKind::Static { price, source } = &self.kind;
        Some((*price, source.as_str()))
    }
}
