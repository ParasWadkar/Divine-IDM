//! Who owns which bytes, and how big a piece of work to hand out.
//!
//! The ledger tracks three disjoint-by-construction sets over the file:
//! bytes that are durable, bytes acknowledged by the writer but not yet
//! journalled, and bytes currently claimed by a worker. Their union is
//! *assigned*; anything else is free work.
//!
//! # Why claims are pulled, not divided up front
//!
//! The obvious approach — split the file into N equal pieces, one per
//! connection — guarantees unequal *finish* times, because per-connection
//! rates routinely differ by three to five times (different edge nodes,
//! different shaping). The slowest piece then dictates the whole download.
//!
//! So workers pull modest claims instead and come back for more. Because the
//! size is derived from what remains, granularity shrinks on its own towards
//! the tail with no mode switch. This is the highest-value decision in the
//! scheduler: good claim sizing makes work stealing a tail-only correction
//! rather than the primary load balancer.

use crate::extent::{ByteRange, RangeSet};

/// Below this, a gap is not worth a request: an extra round trip costs more
/// than the parallelism returns.
pub const MIN_NEW_CLAIM: u64 = 2 << 20;

/// Largest claim worth handing to one worker. Beyond this a straggler holds
/// too much hostage, whatever the arithmetic says.
pub const MAX_CLAIM: u64 = 64 << 20;

/// Absolute smallest claim, below which a request costs more than it moves.
///
/// This is the only floor. An 8 MiB *preferred* floor used to sit above it
/// and was the thing that quietly cancelled the divisor below: on a 128 MiB
/// file at sixteen connections the divisor asks for 2 MiB and the floor
/// raised it back to 8 MiB, which is exactly sixteen claims for sixteen
/// connections. One each, nothing spare, and three consequences that all
/// read as something else when measured:
///
/// * a worker that finishes early finds no free work, which is what
///   [`CLAIMS_PER_CONNECTION`] exists to prevent;
/// * the tail is as coarse as the claim, so the last connections finish at
///   wildly different times and a shared bucket goes under-drawn at the end
///   while the stragglers run;
/// * the in-flight count drains on its own as claims run out, so the
///   connection controller's target stops describing reality.
///
/// The steal counter reads zero throughout, because nothing is ever left to
/// steal — which is how this hid behind "claim sizing is not the problem".
pub const HARD_MIN_CLAIM: u64 = 256 << 10;

/// Files below this never get more than one connection, whatever the
/// connection count says. Handshakes and slow-start dominate here, so
/// splitting a 400 KiB file sixteen ways is sixteen handshakes to move 25 KiB
/// each.
pub const SINGLE_CONNECTION_BELOW: u64 = 1 << 20;

/// How many claims ahead of the current connection count to aim for, so a
/// worker that finishes early finds free work rather than having to steal.
const CLAIMS_PER_CONNECTION: u64 = 4;

/// Size of the next claim to hand out.
///
/// Two sizes, and the smaller wins.
///
/// The **working** size is `total / (4 * connections)`: enough claims that
/// every connection has several to choose from, and fixed for the download
/// rather than recomputed from what is left. Sizing off `remaining` instead
/// looks tidier and is not: each claim is a quarter-of-a-connection's share
/// of a shrinking number, so the pieces halve and halve again and the
/// request count stops being a function of the file. Measured on 128 MiB
/// over sixteen connections, that was 177 requests where a fixed 2 MiB is
/// 64.
///
/// The **tail** size is `remaining / connections`, which only bites once
/// less than one full round is left. It is what stops fifteen connections
/// waiting on a sixteenth that still holds a whole working claim.
pub fn claim_size(total: u64, remaining: u64, connections: u32) -> u64 {
    let connections = u64::from(connections.max(1));
    let working = total / (CLAIMS_PER_CONNECTION * connections);
    let tail = remaining / connections;
    // Clamped by the hard floor, below which a request costs more than it
    // moves, and the cap, above which one straggler holds too much.
    working.min(tail.max(1)).clamp(HARD_MIN_CLAIM, MAX_CLAIM).min(remaining.max(1))
}

/// Whether a download should use a single connection regardless of tuning.
///
/// `bdp` is the bandwidth-delay product; a file smaller than a few of those
/// cannot keep extra connections busy long enough to pay for them.
pub fn force_single_connection(total: u64, bdp: u64) -> bool {
    total < SINGLE_CONNECTION_BELOW.max(bdp.saturating_mul(4))
}

/// The authoritative record of which bytes are spoken for.
#[derive(Debug, Clone)]
pub struct WorkLedger {
    total: u64,
    durable: RangeSet,
    acked: RangeSet,
    claimed: RangeSet,
}

impl WorkLedger {
    /// A download of known length. Unknown-length transfers are a separate,
    /// single-stream append-only path and never reach this type, because there
    /// is no end to divide.
    pub fn new(total: u64) -> Self {
        Self { total, durable: RangeSet::new(), acked: RangeSet::new(), claimed: RangeSet::new() }
    }

    /// Restores from a journal's durable set on resume.
    pub fn resumed(total: u64, durable: RangeSet) -> Self {
        Self { total, durable, acked: RangeSet::new(), claimed: RangeSet::new() }
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    pub fn whole(&self) -> ByteRange {
        ByteRange::new(0, self.total)
    }

    pub fn durable(&self) -> &RangeSet {
        &self.durable
    }

    /// Everything spoken for: durable, acknowledged, or claimed.
    fn assigned(&self) -> RangeSet {
        let mut assigned = self.durable.clone();
        for range in self.acked.iter() {
            assigned.insert(range);
        }
        for range in self.claimed.iter() {
            assigned.insert(range);
        }
        assigned
    }

    /// Bytes neither done nor being worked on.
    pub fn unassigned_bytes(&self) -> u64 {
        self.total - self.assigned().total_bytes()
    }

    /// Bytes still to fetch, whether claimed or not.
    pub fn remaining_bytes(&self) -> u64 {
        self.total - self.durable.total_bytes() - self.acked.total_bytes()
    }

    pub fn is_complete(&self) -> bool {
        self.durable.is_complete(self.total)
    }

    /// Upper bound on how far ahead work may be handed out.
    ///
    /// With no claim window the whole file is fair game. With one — the
    /// `OrderedWindow` write mode, where NTFS would otherwise zero-fill on
    /// every out-of-order write — claims are confined to a window ahead of the
    /// contiguous durable prefix, which bounds the zero-fill to once per window
    /// advance instead of once per write.
    pub fn claim_bound(&self, claim_window: Option<u64>) -> ByteRange {
        match claim_window {
            None => self.whole(),
            Some(window) => {
                let end = self.durable.contiguous_prefix_end().saturating_add(window);
                ByteRange::new(0, end.min(self.total))
            }
        }
    }

    /// The next piece of free work, sized for the current connection count.
    ///
    /// Returns `None` when there is nothing free worth taking — which is the
    /// condition under which the caller considers stealing instead.
    pub fn next_claim(&mut self, connections: u32, claim_window: Option<u64>) -> Option<ByteRange> {
        let bound = self.claim_bound(claim_window);
        let gap = self.assigned().first_gap_in(bound)?;

        // A gap smaller than MIN_NEW_CLAIM is still worth taking when it is all
        // that is left -- otherwise the tail of a file would never be fetched.
        let size = claim_size(self.total, self.remaining_bytes().max(1), connections);
        let take = gap.len().min(size);
        if take == 0 {
            return None;
        }
        let claim = ByteRange::new(gap.start(), gap.start() + take);
        self.claimed.insert(claim);
        Some(claim)
    }

    /// Records a claim the caller constructed itself, such as the thief's half
    /// of a split.
    pub fn reserve(&mut self, range: ByteRange) {
        self.claimed.insert(range);
    }

    /// Returns unfetched bytes to the free pool when a claim is abandoned.
    pub fn release(&mut self, range: ByteRange) {
        self.claimed.remove(range);
    }

    /// The writer has issued these bytes; not yet journalled.
    pub fn mark_acked(&mut self, range: ByteRange) {
        self.claimed.remove(range);
        self.acked.insert(range);
    }

    /// The journal has committed these bytes.
    pub fn mark_durable(&mut self, range: ByteRange) {
        self.claimed.remove(range);
        self.acked.remove(range);
        self.durable.insert(range);
    }

    /// Panics if the three sets ever overlap or exceed the file.
    ///
    /// They are disjoint by construction, so a violation means a claim was
    /// handed out twice or a steal lost track of a range — the class of bug
    /// that silently corrupts output.
    #[track_caller]
    pub fn assert_invariants(&self) {
        self.durable.assert_invariants();
        self.acked.assert_invariants();
        self.claimed.assert_invariants();

        let separate =
            self.durable.total_bytes() + self.acked.total_bytes() + self.claimed.total_bytes();
        let merged = self.assigned().total_bytes();
        assert_eq!(
            separate, merged,
            "durable/acked/claimed overlap: {separate} bytes across sets but {merged} merged"
        );
        assert!(merged <= self.total, "assigned {merged} bytes exceeds the file's {}", self.total);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    #[test]
    fn claim_size_shrinks_towards_the_tail() {
        let total = 1024 * MIB;
        // Early on, claims are capped so no single worker owns too much.
        assert_eq!(claim_size(100 * 1024 * MIB, 100 * 1024 * MIB, 8), MAX_CLAIM);
        // Through the body of the download the working size holds steady,
        // which is what keeps the request count a function of the file.
        assert_eq!(claim_size(total, total, 8), 32 * MIB);
        assert_eq!(claim_size(total, total / 2, 8), 32 * MIB);
        // Inside the last round it gives way to remaining / connections, so
        // the connections finish together.
        assert_eq!(claim_size(total, 64 * MIB, 8), 8 * MIB);
        // And bottoms out at the hard floor rather than issuing pointlessly
        // tiny requests.
        assert_eq!(claim_size(total, MIB, 16), HARD_MIN_CLAIM);
    }

    #[test]
    fn the_request_count_stays_a_function_of_the_file() {
        // Sizing off `remaining` made the pieces halve as the file drained:
        // 128 MiB over sixteen connections came to 177 requests where the
        // working size alone is 64. Walked here rather than reasoned about,
        // because the tail rule is what decides where it stops.
        let total = 128 * MIB;
        let mut remaining = total;
        let mut claims = 0u64;
        while remaining > 0 {
            remaining -= claim_size(total, remaining, 16).min(remaining);
            claims += 1;
            assert!(claims < 1000, "claim sizing did not terminate");
        }
        // 97 as this stands: 48 working claims of 2 MiB, then the tail rule
        // taking sixteenths of what is left down to the hard floor. The bar
        // is set above that rather than at it, so an adjustment to the tail
        // does not fail this test -- what it is guarding against is the
        // runaway, which was approaching 200.
        assert!(claims <= 120, "{claims} requests to fetch 128 MiB");
        assert!(claims >= 32, "only {claims} claims: too coarse to steer by");
    }

    #[test]
    fn every_connection_gets_several_claims_to_choose_from() {
        // The case an 8 MiB preferred floor used to cancel: 128 MiB over
        // sixteen connections asked for 2 MiB and was handed 8 MiB, which is
        // one claim each and nothing spare. The guarantee is that the count
        // of claims scales with the connection count, not that any particular
        // size comes out.
        for connections in [4u32, 8, 16] {
            let total = 128 * MIB;
            let claims = total / claim_size(total, total, connections);
            assert!(
                claims >= u64::from(connections) * 2,
                "{connections} connections got only {claims} claims to share"
            );
        }
    }

    #[test]
    fn more_connections_mean_finer_claims() {
        // The point of the divisor: at higher concurrency each claim is
        // smaller, so finishing workers find free work instead of stealing.
        // Sized so neither result is pinned at MAX_CLAIM, which would make the
        // comparison vacuous.
        let total = 512 * MIB;
        assert_eq!(claim_size(total, total, 4), 32 * MIB);
        assert_eq!(claim_size(total, total, 16), 8 * MIB);
        assert!(claim_size(total, total, 16) < claim_size(total, total, 4));
    }

    #[test]
    fn a_small_file_is_still_divided_across_its_connections() {
        // The regression: an 8 MiB preferred floor yielded two claims on a
        // 16 MiB file however many connections were open, so most sat idle.
        let total = 16 * MIB;
        for connections in [4u32, 8, 16] {
            let size = claim_size(total, total, connections);
            let claims = total / size;
            assert!(
                claims >= u64::from(connections).min(total / HARD_MIN_CLAIM),
                "{connections} connections got only {claims} claims of {size} bytes"
            );
        }
    }

    #[test]
    fn claims_never_shrink_below_the_hard_floor() {
        // Below this a request costs more than it moves.
        for connections in [1u32, 8, 64, 1024] {
            assert!(claim_size(4 * MIB, 4 * MIB, connections) >= HARD_MIN_CLAIM);
        }
    }

    #[test]
    fn small_files_get_one_connection() {
        let bdp = 2 * MIB;
        assert!(force_single_connection(512 * 1024, bdp));
        assert!(force_single_connection(4 * MIB, bdp), "below 4x BDP");
        assert!(!force_single_connection(64 * MIB, bdp));
        // With a tiny BDP the 1 MiB floor still applies.
        assert!(force_single_connection(900 * 1024, 0));
        assert!(!force_single_connection(2 * MIB, 0));
    }

    #[test]
    fn claims_tile_the_file_without_overlap() {
        let mut ledger = WorkLedger::new(512 * MIB);
        let mut taken = Vec::new();
        while let Some(claim) = ledger.next_claim(8, None) {
            taken.push(claim);
            ledger.assert_invariants();
        }

        // Every byte claimed exactly once.
        let tiled: RangeSet = taken.iter().copied().collect();
        assert_eq!(
            tiled.total_bytes(),
            taken.iter().map(ByteRange::len).sum::<u64>(),
            "claims overlap"
        );
        assert_eq!(tiled.extent_count(), 1);
        assert_eq!(tiled.iter().next(), Some(ledger.whole()));
        assert_eq!(ledger.unassigned_bytes(), 0);
    }

    #[test]
    fn a_released_claim_becomes_free_work_again() {
        let mut ledger = WorkLedger::new(256 * MIB);
        let first = ledger.next_claim(4, None).unwrap();
        let second = ledger.next_claim(4, None).unwrap();
        assert_eq!(second.start(), first.end());

        ledger.release(first);
        ledger.assert_invariants();

        // The abandoned range must be handed out again, not lost.
        let reissued = ledger.next_claim(4, None).unwrap();
        assert_eq!(reissued.start(), first.start());
    }

    #[test]
    fn durable_bytes_are_never_reissued() {
        let mut ledger = WorkLedger::new(256 * MIB);
        let claim = ledger.next_claim(4, None).unwrap();
        ledger.mark_acked(claim);
        ledger.mark_durable(claim);
        ledger.assert_invariants();

        assert_eq!(ledger.durable().total_bytes(), claim.len());
        let next = ledger.next_claim(4, None).unwrap();
        assert!(next.start() >= claim.end(), "re-handed out a durable range");
    }

    #[test]
    fn resume_only_hands_out_what_the_journal_lacks() {
        let total = 256 * MIB;
        let durable: RangeSet = [ByteRange::new(0, 64 * MIB), ByteRange::new(128 * MIB, 192 * MIB)]
            .into_iter()
            .collect();
        let mut ledger = WorkLedger::resumed(total, durable);

        assert_eq!(ledger.remaining_bytes(), 128 * MIB);
        let first = ledger.next_claim(4, None).unwrap();
        assert_eq!(first.start(), 64 * MIB, "should resume at the first hole");
        assert!(first.end() <= 128 * MIB, "a claim must not span a durable region");
    }

    #[test]
    fn an_ordered_window_bounds_how_far_ahead_claims_go() {
        // The NTFS fallback mode: without this the scheduler would hand out an
        // offset far past the written prefix and pay a huge zero-fill.
        let window = 16 * MIB;
        let mut ledger = WorkLedger::new(512 * MIB);

        let bound = ledger.claim_bound(Some(window));
        assert_eq!(bound, ByteRange::new(0, window), "no durable prefix yet");

        // Claims may not be issued beyond the window.
        let mut issued = Vec::new();
        while let Some(claim) = ledger.next_claim(8, Some(window)) {
            assert!(claim.end() <= window, "{claim} escaped the window");
            issued.push(claim);
        }
        assert_eq!(issued.iter().map(ByteRange::len).sum::<u64>(), window);

        // Committing the prefix slides the window forward.
        for claim in &issued {
            ledger.mark_durable(*claim);
        }
        assert_eq!(ledger.claim_bound(Some(window)), ByteRange::new(0, 2 * window));
        let next = ledger.next_claim(8, Some(window)).unwrap();
        assert!(next.start() >= window && next.end() <= 2 * window);
    }

    #[test]
    fn a_detached_durable_region_does_not_advance_the_window() {
        // contiguous_prefix_end is deliberately about byte zero: a region
        // committed further in does not make it safe to write past it.
        let window = 16 * MIB;
        let durable: RangeSet = [ByteRange::new(64 * MIB, 128 * MIB)].into_iter().collect();
        let ledger = WorkLedger::resumed(512 * MIB, durable);
        assert_eq!(ledger.claim_bound(Some(window)), ByteRange::new(0, window));
    }

    #[test]
    fn the_last_partial_gap_is_still_claimed() {
        // A gap under MIN_NEW_CLAIM must not be abandoned, or files would
        // never finish.
        let total = 9 * MIB;
        let durable: RangeSet = [ByteRange::new(0, 8 * MIB)].into_iter().collect();
        let mut ledger = WorkLedger::resumed(total, durable);

        // The tail is claimable -- now in several pieces, since the floor
        // adapts so the open connections all get work -- and together those
        // pieces must cover exactly the remaining megabyte.
        let mut claimed = Vec::new();
        while let Some(claim) = ledger.next_claim(8, None) {
            claimed.push(claim);
            ledger.mark_durable(claim);
        }
        assert!(!claimed.is_empty(), "the tail must still be claimable");
        let tiled: RangeSet = claimed.iter().copied().collect();
        assert_eq!(tiled.iter().next(), Some(ByteRange::new(8 * MIB, 9 * MIB)));
        assert!(ledger.is_complete());
    }

    #[test]
    fn exhausted_work_reports_nothing_free() {
        let mut ledger = WorkLedger::new(32 * MIB);
        while ledger.next_claim(4, None).is_some() {}
        assert_eq!(ledger.next_claim(4, None), None, "claimed everything, so nothing is free");
        assert_eq!(ledger.unassigned_bytes(), 0);
        assert!(!ledger.is_complete(), "claimed is not the same as durable");
    }
}
