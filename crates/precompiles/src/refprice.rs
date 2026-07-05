//! Reference-price guardrail helper for the darkpool orderbook.
//!
//! This is **alpha infrastructure** for configured darkpool markets.
//! It is not a production oracle: the reference price is whatever the
//! sequencer-side configuration provides (typically a static value), and the
//! guard only enforces a configurable max-deviation and staleness bound.
//!
//! Units match the orderbook precompile: prices are raw integer values and
//! `quote = baseAmount * price`. The guard treats `max_staleness_secs == 0`
//! as "never stale" so static providers can opt out of the freshness check.
//!
//! The helper is `no_std`-friendly so it can be reused inside the precompile
//! crate, the RPC layer, and any future enforcement point.

use alloc::string::String;

/// A single snapshot of a public market reference price.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferencePrice {
    /// Raw integer price; same units as the orderbook precompile.
    pub price: u128,
    /// Origin tag (e.g. `"static:alpha"`).
    pub source: String,
    /// Zone L2 block at which this snapshot was minted.
    pub as_of_block: u64,
    /// Unix timestamp (seconds) at which this snapshot was minted.
    pub as_of_timestamp: u64,
}

/// Guardrail bounds applied when an alpha reference price is available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReferencePriceGuard {
    /// Maximum allowed deviation between an order's limit price and the
    /// reference price, in basis points. `0` rejects any non-equal price.
    pub max_deviation_bps: u32,
    /// Maximum staleness window in seconds; `0` disables the staleness check.
    pub max_staleness_secs: u64,
}

/// Reasons the guard can reject an order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardrailRejection {
    /// No reference price was supplied (provider disabled or missing).
    ProviderDisabled,
    /// The snapshot is older than `max_staleness_secs`.
    StaleReference {
        observed_age_secs: u64,
        max_age_secs: u64,
    },
    /// The order's limit price deviates from the reference by more than the
    /// configured bound.
    OutOfRange {
        reference_price: u128,
        order_price: u128,
        max_deviation_bps: u32,
        deviation_bps: u128,
    },
    /// The provider returned a zero reference price, which cannot bound
    /// anything. Treated as an explicit rejection so callers do not silently
    /// pass every order through.
    ZeroReferencePrice,
}

impl ReferencePriceGuard {
    /// Compute the snapshot's age in seconds at `now_secs`. Clamps to zero if
    /// `now_secs` precedes the snapshot's timestamp.
    pub fn age_secs(snapshot: &ReferencePrice, now_secs: u64) -> u64 {
        now_secs.saturating_sub(snapshot.as_of_timestamp)
    }

    /// `true` when `snapshot` is within the configured staleness window.
    ///
    /// A `max_staleness_secs` of `0` disables the freshness check, so the
    /// snapshot is always considered fresh. This is the natural setting for
    /// the alpha static provider, which has no real freshness signal.
    pub fn is_fresh(&self, snapshot: &ReferencePrice, now_secs: u64) -> bool {
        if self.max_staleness_secs == 0 {
            return true;
        }
        Self::age_secs(snapshot, now_secs) <= self.max_staleness_secs
    }

    /// Apply the guardrails to a candidate `order_price`.
    ///
    /// Returns `Ok(())` when the order is within bounds, or an explanatory
    /// [`GuardrailRejection`] otherwise. Callers are expected to surface the
    /// rejection variant to the frontend so users understand why an order
    /// was refused.
    pub fn check_order_price(
        &self,
        reference: Option<&ReferencePrice>,
        now_secs: u64,
        order_price: u128,
    ) -> Result<(), GuardrailRejection> {
        let Some(snapshot) = reference else {
            return Err(GuardrailRejection::ProviderDisabled);
        };

        if self.max_staleness_secs > 0 {
            let age = Self::age_secs(snapshot, now_secs);
            if age > self.max_staleness_secs {
                return Err(GuardrailRejection::StaleReference {
                    observed_age_secs: age,
                    max_age_secs: self.max_staleness_secs,
                });
            }
        }

        if snapshot.price == 0 {
            return Err(GuardrailRejection::ZeroReferencePrice);
        }

        let diff = order_price.abs_diff(snapshot.price);
        let deviation_bps = diff.saturating_mul(10_000) / snapshot.price;

        if deviation_bps > self.max_deviation_bps as u128 {
            return Err(GuardrailRejection::OutOfRange {
                reference_price: snapshot.price,
                order_price,
                max_deviation_bps: self.max_deviation_bps,
                deviation_bps,
            });
        }

        Ok(())
    }
}
