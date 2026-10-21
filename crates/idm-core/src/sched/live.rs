//! The live claim: the shared object a fetcher and the coordinator both touch.
//!
//! [`ClaimSnapshot`](super::ClaimSnapshot) is the immutable view the policy
//! functions reason about. This is the mutable reality behind it, and the
//! channel through which a steal is communicated.
//!
//! # Single writer per field
//!
//! There is no mutex here, and none is needed, because each atomic has exactly
//! one writer:
//!
//! * `cursor` — written only by the fetcher that owns the claim.
//! * `end_soft` — written only by the coordinator, to steal.
//! * `splits` / `last_split_ms` — written only by the coordinator.
//!
//! Everything else reads. That discipline is what keeps the hot path free of
//! locks, and it is worth preserving: a second writer to any of these turns a
//! lock-free structure into a racy one with no compiler complaint.
//!
//! # Why duplicate writes are safe
//!
//! A steal narrows `end_soft` while the fetcher may already have requested
//! bytes beyond it. Those bytes are still written, and the thief fetches the
//! same range — so the same offsets get written twice. That is harmless, and
//! deliberately so: both copies come from the same URL in the same generation,
//! guarded by `If-Match`, so they are byte-identical. The same property is what
//! makes tail hedging safe.

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use super::{ClaimId, ClaimSnapshot, Protocol};
use crate::extent::ByteRange;

/// A claim as it actually exists while being fetched.
#[derive(Debug)]
pub struct LiveClaim {
    id: ClaimId,
    start: u64,
    /// Set once, from the response's `Content-Range`. Never grows.
    end_hard: u64,
    end_soft: AtomicU64,
    cursor: AtomicU64,
    splits: AtomicU8,
    last_split_ms: AtomicU64,
    protocol: Protocol,
}

impl LiveClaim {
    pub fn new(id: ClaimId, range: ByteRange, protocol: Protocol, now_ms: u64) -> Self {
        Self {
            id,
            start: range.start(),
            end_hard: range.end(),
            end_soft: AtomicU64::new(range.end()),
            cursor: AtomicU64::new(range.start()),
            splits: AtomicU8::new(0),
            last_split_ms: AtomicU64::new(now_ms),
            protocol,
        }
    }

    pub fn id(&self) -> ClaimId {
        self.id
    }

    pub fn start(&self) -> u64 {
        self.start
    }

    pub fn end_hard(&self) -> u64 {
        self.end_hard
    }

    pub fn protocol(&self) -> Protocol {
        self.protocol
    }

    pub fn end_soft(&self) -> u64 {
        self.end_soft.load(Ordering::Acquire)
    }

    pub fn cursor(&self) -> u64 {
        self.cursor.load(Ordering::Acquire)
    }

    /// What the claim currently owns.
    pub fn range(&self) -> ByteRange {
        ByteRange::new(self.start, self.end_soft())
    }

    pub fn remaining(&self) -> u64 {
        self.end_soft().saturating_sub(self.cursor())
    }

    /// Whether the fetcher should stop reading.
    ///
    /// Checked after every chunk. This is the whole steal mechanism on the
    /// worker side: no cancellation signal, no channel, one atomic load.
    pub fn is_done(&self) -> bool {
        self.cursor() >= self.end_soft()
    }

    /// Advances the cursor by `bytes`. Fetcher only.
    pub fn advance(&self, bytes: u64) -> u64 {
        self.cursor.fetch_add(bytes, Ordering::AcqRel) + bytes
    }

    pub fn splits_done(&self) -> u8 {
        self.splits.load(Ordering::Relaxed)
    }

    /// Narrows the soft end so a thief can take `[at, end_soft)`.
    ///
    /// Refuses when `at` would not reduce the range, or when the fetcher has
    /// already requested past it — in which case the coordinator re-evaluates
    /// rather than handing out bytes that are already on their way.
    ///
    /// Coordinator only.
    pub fn try_steal_at(&self, at: u64, now_ms: u64) -> Option<ByteRange> {
        let mut current = self.end_soft.load(Ordering::Acquire);
        loop {
            if at >= current || at < self.start {
                return None;
            }
            if at < self.cursor() {
                // The fetcher is already past the proposed cut.
                return None;
            }
            match self.end_soft.compare_exchange_weak(
                current,
                at,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.splits.fetch_add(1, Ordering::Relaxed);
                    self.last_split_ms.store(now_ms, Ordering::Relaxed);
                    return Some(ByteRange::new(at, current));
                }
                // Another steal landed first; re-read and reconsider against
                // the new end rather than clobbering it.
                Err(actual) => current = actual,
            }
        }
    }

    /// A snapshot for the policy functions.
    pub fn snapshot(&self, rate_bps: f64, now_ms: u64) -> ClaimSnapshot {
        ClaimSnapshot {
            id: self.id,
            range: self.range(),
            cursor: self.cursor(),
            rate_bps,
            splits_done: self.splits_done(),
            millis_since_split: now_ms.saturating_sub(self.last_split_ms.load(Ordering::Relaxed)),
            protocol: self.protocol,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const MIB: u64 = 1 << 20;

    fn claim() -> LiveClaim {
        LiveClaim::new(ClaimId(1), ByteRange::new(0, 64 * MIB), Protocol::Http2, 1000)
    }

    #[test]
    fn a_fresh_claim_owns_its_whole_range() {
        let claim = claim();
        assert_eq!(claim.range(), ByteRange::new(0, 64 * MIB));
        assert_eq!(claim.cursor(), 0);
        assert_eq!(claim.remaining(), 64 * MIB);
        assert!(!claim.is_done());
    }

    #[test]
    fn advancing_to_the_soft_end_finishes_the_claim() {
        let claim = claim();
        claim.advance(64 * MIB);
        assert!(claim.is_done());
        assert_eq!(claim.remaining(), 0);
    }

    #[test]
    fn a_steal_narrows_the_range_and_hands_back_the_tail() {
        let claim = claim();
        claim.advance(4 * MIB);

        let taken = claim.try_steal_at(32 * MIB, 5000).expect("steal should succeed");
        assert_eq!(taken, ByteRange::new(32 * MIB, 64 * MIB));
        assert_eq!(claim.range(), ByteRange::new(0, 32 * MIB), "donor keeps the near side");
        assert_eq!(claim.splits_done(), 1);
        assert!(!claim.is_done(), "the donor still has work");
    }

    #[test]
    fn the_donor_retires_once_it_reaches_the_new_soft_end() {
        // The entire worker-side steal mechanism: one atomic load per chunk.
        let claim = claim();
        claim.advance(8 * MIB);
        claim.try_steal_at(16 * MIB, 5000).unwrap();

        assert!(!claim.is_done());
        claim.advance(8 * MIB); // now at 16 MiB
        assert!(claim.is_done(), "donor should stop cleanly at the lowered end");
    }

    #[test]
    fn a_steal_behind_the_cursor_is_refused() {
        // Those bytes are already requested, so handing them to a thief would
        // duplicate work the coordinator can avoid.
        let claim = claim();
        claim.advance(20 * MIB);
        assert_eq!(claim.try_steal_at(10 * MIB, 5000), None);
        assert_eq!(claim.range().end(), 64 * MIB, "range must be untouched");
        assert_eq!(claim.splits_done(), 0);
    }

    #[test]
    fn a_steal_that_would_not_reduce_the_range_is_refused() {
        let claim = claim();
        assert_eq!(claim.try_steal_at(64 * MIB, 5000), None, "equal to the end is no reduction");
        assert_eq!(claim.try_steal_at(80 * MIB, 5000), None, "past the end is not a reduction");
        assert_eq!(claim.splits_done(), 0);
    }

    #[test]
    fn repeated_steals_tile_the_original_range_exactly() {
        let claim = claim();
        let original = claim.range();
        let mut taken = Vec::new();

        for at in [48 * MIB, 32 * MIB, 16 * MIB, 8 * MIB] {
            taken.push(claim.try_steal_at(at, 5000).expect("should steal"));
        }
        assert_eq!(claim.splits_done(), 4);

        // The donor's remainder plus every stolen piece must be exactly the
        // original range, with no gap and no overlap.
        let mut all = taken.clone();
        all.push(claim.range());
        let tiled: crate::RangeSet = all.iter().copied().collect();
        tiled.assert_invariants();
        assert_eq!(
            tiled.total_bytes(),
            all.iter().map(ByteRange::len).sum::<u64>(),
            "stolen pieces overlap"
        );
        assert_eq!(tiled.iter().next(), Some(original));
    }

    #[test]
    fn concurrent_steals_never_double_assign_a_byte() {
        // Four coordinator-side threads racing to steal from one claim. The CAS
        // must make every handed-out range disjoint; a plain store would not.
        let claim = Arc::new(claim());
        let mut handles = Vec::new();
        for index in 0..4u64 {
            let claim = Arc::clone(&claim);
            handles.push(std::thread::spawn(move || {
                let mut got = Vec::new();
                for step in 0..8 {
                    let at = (4 + index * 8 + step) * MIB;
                    if let Some(range) = claim.try_steal_at(at, 5000) {
                        got.push(range);
                    }
                }
                got
            }));
        }

        let mut all: Vec<ByteRange> =
            handles.into_iter().flat_map(|handle| handle.join().unwrap()).collect();
        all.push(claim.range());

        let total: u64 = all.iter().map(ByteRange::len).sum();
        let tiled: crate::RangeSet = all.iter().copied().collect();
        tiled.assert_invariants();
        assert_eq!(total, tiled.total_bytes(), "a byte was assigned to two owners");
        assert_eq!(tiled.iter().next(), Some(ByteRange::new(0, 64 * MIB)));
    }

    #[test]
    fn a_snapshot_reflects_live_state() {
        let claim = claim();
        claim.advance(12 * MIB);
        claim.try_steal_at(48 * MIB, 4000).unwrap();

        let snapshot = claim.snapshot(5_000_000.0, 9000);
        assert_eq!(snapshot.id, ClaimId(1));
        assert_eq!(snapshot.range, ByteRange::new(0, 48 * MIB));
        assert_eq!(snapshot.cursor, 12 * MIB);
        assert_eq!(snapshot.remaining(), 36 * MIB);
        assert_eq!(snapshot.splits_done, 1);
        assert_eq!(snapshot.millis_since_split, 5000, "9000 - 4000");
        assert_eq!(snapshot.protocol, Protocol::Http2);
    }

    #[test]
    fn end_hard_never_moves_even_as_the_soft_end_shrinks() {
        // end_hard records what the server actually promised, which the
        // coordinator needs even after the claim has been carved up.
        let claim = claim();
        claim.try_steal_at(16 * MIB, 5000).unwrap();
        assert_eq!(claim.end_soft(), 16 * MIB);
        assert_eq!(claim.end_hard(), 64 * MIB);
    }
}
