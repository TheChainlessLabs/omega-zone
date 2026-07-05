//! In-memory aggregate midpoint history for one darkpool market.
//!
//! The history stores only wall-clock timestamps and aggregate midpoint
//! prices. It never sees account, order, maker, taker, or fill-level data —
//! so responses derived from it cannot leak owner-linked information.
//!
//! ## Interval support
//!
//! Three interval labels are accepted at query time. Each maps to a
//! fixed-width bucket; within a bucket the **last** observed midpoint wins.
//! Unknown labels are rejected at the RPC layer.
//!
//! | Label  | Bucket size |
//! |--------|-------------|
//! | `"1m"` | 60 seconds  |
//! | `"5m"` | 300 seconds |
//! | `"1h"` | 3600 seconds |
//!
//! ## Retention
//!
//! The store retains at most [`MIDPOINT_RETENTION`] raw samples. Older
//! samples are evicted from the head of the buffer in FIFO order. At the
//! default sampler cadence of [`MIDPOINT_SAMPLE_INTERVAL`], the buffer
//! holds roughly 12 hours of aggregate midpoint history.
//!
//! ## Pagination
//!
//! [`MidpointHistory::query`] returns samples in oldest → newest order. The
//! `cursor` argument selects samples with `bucket_end` strictly less than
//! the cursor (i.e., the next, older page). The returned `next_cursor` is
//! the oldest `bucket_end` in the current page when older samples remain,
//! otherwise `None`.

use std::{
    collections::{BTreeMap, VecDeque},
    time::Duration,
};

use parking_lot::RwLock;

/// Default raw-sample retention. At [`MIDPOINT_SAMPLE_INTERVAL`] this covers
/// roughly 12 hours of history.
pub const MIDPOINT_RETENTION: usize = 2_880;

/// Default polling interval for the on-chain sampler.
pub const MIDPOINT_SAMPLE_INTERVAL: Duration = Duration::from_secs(15);

/// Interval labels accepted by `zone_getMidpointHistory`.
pub const SUPPORTED_INTERVALS: &[&str] = &["1m", "5m", "1h"];

/// Translate a request `interval` label to a bucket size in seconds.
pub fn interval_seconds(interval: &str) -> Option<u64> {
    match interval {
        "1m" => Some(60),
        "5m" => Some(300),
        "1h" => Some(3_600),
        _ => None,
    }
}

/// One raw aggregate midpoint sample written by the sampler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawSample {
    /// Wall-clock seconds since UNIX epoch.
    pub timestamp: u64,
    /// Midpoint price in raw integer units.
    pub midpoint: u128,
}

/// One bucketed sample returned to the RPC layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BucketSample {
    /// Bucket end timestamp (wall-clock seconds since UNIX epoch).
    pub bucket_end: u64,
    /// Last midpoint observed inside the bucket.
    pub midpoint: u128,
}

/// Bounded in-memory store for aggregate midpoint samples.
#[derive(Debug)]
pub struct MidpointHistory {
    inner: RwLock<VecDeque<RawSample>>,
    capacity: usize,
}

impl MidpointHistory {
    /// Create an empty history with the given retention capacity.
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            inner: RwLock::new(VecDeque::with_capacity(capacity)),
            capacity,
        }
    }

    /// Append a sample, evicting the oldest entry if at capacity.
    ///
    /// The sampler must append samples in chronological order; the query
    /// implementation relies on the buffer being time-ordered so that
    /// "last write wins" in a bucket corresponds to "newest midpoint in the
    /// bucket".
    pub fn record(&self, sample: RawSample) {
        let mut buf = self.inner.write();
        if buf.len() == self.capacity {
            buf.pop_front();
        }
        buf.push_back(sample);
    }

    /// Number of raw samples currently retained.
    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    /// Returns `true` when no samples have been recorded yet.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bucketize samples to `interval_secs` and return up to `limit` buckets
    /// strictly older than `cursor` (or the newest `limit` buckets when
    /// `cursor` is `None`). Samples are returned in oldest → newest order.
    ///
    /// `next_cursor` is the oldest `bucket_end` in the returned page when
    /// older buckets remain, otherwise `None`.
    pub fn query(
        &self,
        interval_secs: u64,
        limit: u32,
        cursor: Option<u64>,
    ) -> (Vec<BucketSample>, Option<u64>) {
        if interval_secs == 0 || limit == 0 {
            return (Vec::new(), None);
        }

        let buf = self.inner.read();
        if buf.is_empty() {
            return (Vec::new(), None);
        }

        // BTreeMap keyed by bucket_end gives sorted, deduped buckets. Because
        // raw samples are stored in chronological order, later inserts for
        // the same bucket overwrite earlier ones — producing "last midpoint
        // per bucket".
        let mut buckets: BTreeMap<u64, u128> = BTreeMap::new();
        for sample in buf.iter() {
            let bucket_start = (sample.timestamp / interval_secs) * interval_secs;
            let bucket_end = bucket_start.saturating_add(interval_secs);
            buckets.insert(bucket_end, sample.midpoint);
        }

        let cursor_cap = cursor.unwrap_or(u64::MAX);
        let filtered: Vec<(u64, u128)> =
            buckets.range(..cursor_cap).map(|(k, v)| (*k, *v)).collect();

        let total = filtered.len();
        let take = (limit as usize).min(total);
        let drop_count = total - take;

        let page: Vec<BucketSample> = filtered
            .into_iter()
            .skip(drop_count)
            .map(|(bucket_end, midpoint)| BucketSample {
                bucket_end,
                midpoint,
            })
            .collect();

        let next_cursor = if drop_count > 0 {
            page.first().map(|s| s.bucket_end)
        } else {
            None
        };

        (page, next_cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(timestamp: u64, midpoint: u128) -> RawSample {
        RawSample {
            timestamp,
            midpoint,
        }
    }

    #[test]
    fn midpoint_interval_seconds_recognises_supported_labels() {
        assert_eq!(interval_seconds("1m"), Some(60));
        assert_eq!(interval_seconds("5m"), Some(300));
        assert_eq!(interval_seconds("1h"), Some(3_600));
    }

    #[test]
    fn midpoint_interval_seconds_rejects_unknown_labels() {
        assert_eq!(interval_seconds("2m"), None);
        assert_eq!(interval_seconds("30s"), None);
        assert_eq!(interval_seconds(""), None);
    }

    #[test]
    fn midpoint_history_empty_query_returns_no_samples_or_cursor() {
        let history = MidpointHistory::new(8);
        let (samples, cursor) = history.query(60, 100, None);
        assert!(samples.is_empty());
        assert!(cursor.is_none());
    }

    #[test]
    fn midpoint_history_query_returns_last_midpoint_per_bucket() {
        let history = MidpointHistory::new(16);
        // Two samples in the [60, 120) bucket — second one wins.
        history.record(sample(70, 100));
        history.record(sample(80, 110));
        // One sample in the [120, 180) bucket.
        history.record(sample(125, 200));

        let (samples, _) = history.query(60, 10, None);
        let pairs: Vec<(u64, u128)> = samples.iter().map(|s| (s.bucket_end, s.midpoint)).collect();
        assert_eq!(pairs, vec![(120, 110), (180, 200)]);
    }

    #[test]
    fn midpoint_history_query_caps_to_limit_and_emits_next_cursor() {
        let history = MidpointHistory::new(16);
        for i in 0..5u64 {
            history.record(sample(60 * i + 1, 100 + i as u128));
        }
        // Five buckets total, ending at 60, 120, 180, 240, 300.
        let (samples, next_cursor) = history.query(60, 2, None);
        assert_eq!(samples.len(), 2);
        // Newest two, returned oldest → newest.
        assert_eq!(samples[0].bucket_end, 240);
        assert_eq!(samples[1].bucket_end, 300);
        // next_cursor is the oldest bucket in the current page when older
        // buckets remain.
        assert_eq!(next_cursor, Some(240));
    }

    #[test]
    fn midpoint_history_cursor_returns_strictly_older_buckets() {
        let history = MidpointHistory::new(16);
        for i in 0..5u64 {
            history.record(sample(60 * i + 1, 100 + i as u128));
        }
        // First page covers buckets [180, 240, 300].
        let (page1, next1) = history.query(60, 3, None);
        let ends_1: Vec<u64> = page1.iter().map(|s| s.bucket_end).collect();
        assert_eq!(ends_1, vec![180, 240, 300]);
        assert_eq!(next1, Some(180));

        // Following the cursor must return buckets strictly older than 180.
        let (page2, next2) = history.query(60, 3, Some(180));
        let ends_2: Vec<u64> = page2.iter().map(|s| s.bucket_end).collect();
        assert_eq!(ends_2, vec![60, 120]);
        assert!(next2.is_none(), "no older buckets remain");
    }

    #[test]
    fn midpoint_history_query_returns_no_cursor_when_all_buckets_fit() {
        let history = MidpointHistory::new(16);
        history.record(sample(70, 100));
        history.record(sample(130, 200));

        let (samples, next_cursor) = history.query(60, 10, None);
        assert_eq!(samples.len(), 2);
        assert!(next_cursor.is_none());
    }

    #[test]
    fn midpoint_history_record_evicts_oldest_when_at_capacity() {
        let history = MidpointHistory::new(2);
        history.record(sample(60, 1));
        history.record(sample(120, 2));
        history.record(sample(180, 3));
        assert_eq!(history.len(), 2);

        let (samples, _) = history.query(60, 10, None);
        let pairs: Vec<(u64, u128)> = samples.iter().map(|s| (s.bucket_end, s.midpoint)).collect();
        assert_eq!(pairs, vec![(180, 2), (240, 3)]);
    }

    #[test]
    fn midpoint_history_query_with_zero_limit_returns_empty() {
        let history = MidpointHistory::new(8);
        history.record(sample(70, 100));
        let (samples, cursor) = history.query(60, 0, None);
        assert!(samples.is_empty());
        assert!(cursor.is_none());
    }

    #[test]
    fn midpoint_history_query_buckets_by_requested_interval() {
        let history = MidpointHistory::new(64);
        // Spread samples across a 30-minute window at 1-minute spacing,
        // alternating midpoints so 1m and 5m bucketing diverge.
        for i in 0..30u64 {
            history.record(sample(60 * i + 1, 100 + i as u128));
        }

        let (one_min, _) = history.query(60, 100, None);
        assert_eq!(one_min.len(), 30);
        // 1-minute buckets keep every observation distinct.
        assert_eq!(one_min[0].midpoint, 100);
        assert_eq!(one_min[29].midpoint, 129);

        let (five_min, _) = history.query(300, 100, None);
        // 30 minutes at 5-minute bucketing → 6 buckets.
        assert_eq!(five_min.len(), 6);
        // Last sample in each 5-minute window wins.
        let midpoints: Vec<u128> = five_min.iter().map(|s| s.midpoint).collect();
        assert_eq!(midpoints, vec![104, 109, 114, 119, 124, 129]);
    }
}
