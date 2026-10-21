//! Property tests for the extent algebra.
//!
//! `RangeSet` is checked against a naive byte-by-byte model over a small
//! universe. If the two ever disagree the set's coalescing or splitting logic
//! is wrong, which is the class of bug that silently corrupts a download.

use idm_core::RangeSet;
use idm_core::extent::{ByteRange, ContentRange, parse_content_range};
use proptest::prelude::*;

/// Small enough that the naive model stays cheap, large enough to exercise
/// multi-extent coalescing and splitting.
const UNIVERSE: u64 = 256;

#[derive(Debug, Clone, Copy)]
enum Op {
    Insert(ByteRange),
    Remove(ByteRange),
}

/// A half-open range inside the universe, allowing empty ranges so that the
/// no-op paths get exercised too.
fn range_strategy() -> impl Strategy<Value = ByteRange> {
    (0..UNIVERSE, 0..UNIVERSE).prop_map(|(a, b)| ByteRange::new(a.min(b), a.max(b)))
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![range_strategy().prop_map(Op::Insert), range_strategy().prop_map(Op::Remove),]
}

proptest! {
    /// The set must agree with a bitmap model after every single operation,
    /// and must never break its own invariants along the way.
    #[test]
    fn range_set_matches_a_naive_bitmap(ops in prop::collection::vec(op_strategy(), 1..80)) {
        let mut set = RangeSet::new();
        let mut model = vec![false; UNIVERSE as usize];

        for op in ops {
            match op {
                Op::Insert(range) => {
                    set.insert(range);
                    for byte in range.start()..range.end() {
                        model[byte as usize] = true;
                    }
                }
                Op::Remove(range) => {
                    set.remove(range);
                    for byte in range.start()..range.end() {
                        model[byte as usize] = false;
                    }
                }
            }
            // Coalesced, non-overlapping, non-empty — checked every step so a
            // failure points at the operation that broke it.
            set.assert_invariants();
        }

        for byte in 0..UNIVERSE {
            prop_assert_eq!(
                set.contains(byte),
                model[byte as usize],
                "disagreement at byte {}",
                byte
            );
        }

        let covered = model.iter().filter(|covered| **covered).count() as u64;
        prop_assert_eq!(set.total_bytes(), covered);
    }

    /// `gaps_in` must be the exact complement: every gap byte uncovered, and
    /// gaps plus coverage accounting for the whole universe.
    #[test]
    fn gaps_are_the_exact_complement(ranges in prop::collection::vec(range_strategy(), 0..24)) {
        let set: RangeSet = ranges.into_iter().collect();
        set.assert_invariants();

        let whole = ByteRange::new(0, UNIVERSE);
        let gaps = set.gaps_in(whole);

        for gap in &gaps {
            prop_assert!(!gap.is_empty(), "a reported gap was empty");
            for byte in gap.start()..gap.end() {
                prop_assert!(!set.contains(byte), "byte {} reported as a gap but is covered", byte);
            }
        }

        // Gaps are ordered and disjoint.
        for pair in gaps.windows(2) {
            prop_assert!(pair[0].end() < pair[1].start(), "gaps touch or overlap");
        }

        let gap_bytes: u64 = gaps.iter().map(ByteRange::len).sum();
        prop_assert_eq!(gap_bytes + set.total_bytes(), UNIVERSE);

        // first_gap_in must agree with the full list.
        prop_assert_eq!(set.first_gap_in(whole), gaps.first().copied());
    }

    /// A non-empty range survives the trip out to an inclusive HTTP header and
    /// back through the Content-Range parser. This is the `+1`/`-1` boundary,
    /// so an off-by-one here would be an off-by-one everywhere.
    #[test]
    fn range_header_round_trips(start in 0u64..1_000_000, len in 1u64..1_000_000) {
        let range = ByteRange::from_start_len(start, len).unwrap();
        let header = range.to_range_header().unwrap();

        // Re-present it the way a well-behaved server would echo it back.
        let last = range.end() - 1;
        let echoed = format!("bytes {}-{}/{}", range.start(), last, range.end() + 7);
        let parsed = parse_content_range(&echoed).unwrap();

        prop_assert_eq!(parsed.range(), Some(range));
        prop_assert_eq!(header, format!("bytes={}-{}", start, last));
        // An explicit message is required here: prop_assert! would otherwise
        // stringify the expression into a format string, where the `{ .. }` of
        // the pattern reads as a malformed placeholder.
        prop_assert!(
            matches!(parsed, ContentRange::Satisfied { .. }),
            "a well-formed Content-Range parsed as unsatisfied"
        );
    }

    /// The scheduler invariant from the plan: however a download's work is
    /// carved up by repeated mid-flight splits, the pieces must still tile
    /// `[0, total)` exactly — no gaps, no overlaps, nothing lost.
    #[test]
    fn simulated_work_stealing_still_tiles_the_file(
        total in 1u64..1_000_000,
        split_points in prop::collection::vec(0u64..1_000_000, 0..40),
    ) {
        // One worker starts owning the whole file.
        let mut claims = vec![ByteRange::new(0, total)];

        for raw_point in split_points {
            // Steal from whichever claim is currently largest, as the
            // coordinator does when picking a donor.
            let (index, donor) = claims
                .iter()
                .enumerate()
                .max_by_key(|(_, claim)| claim.len())
                .map(|(index, claim)| (index, *claim))
                .unwrap();

            if donor.len() < 2 {
                continue;
            }
            // Keep the split strictly inside the donor so neither side is empty.
            let at = donor.start() + 1 + (raw_point % (donor.len() - 1));
            let (kept, stolen) = donor.split_at(at);

            claims[index] = kept;
            claims.push(stolen);
        }

        // Every claim non-empty.
        for claim in &claims {
            prop_assert!(!claim.is_empty(), "a split produced an empty claim");
        }

        // Pairwise disjoint, and together exactly the whole file.
        let tiled: RangeSet = claims.iter().copied().collect();
        tiled.assert_invariants();
        prop_assert_eq!(
            tiled.total_bytes(),
            claims.iter().map(ByteRange::len).sum::<u64>(),
            "claims overlap: coalescing lost bytes"
        );
        prop_assert_eq!(tiled.extent_count(), 1, "claims do not form one contiguous run");
        prop_assert_eq!(tiled.iter().next(), Some(ByteRange::new(0, total)));
        prop_assert!(tiled.is_complete(total));
    }

    /// Removing exactly what was inserted must leave nothing behind, whatever
    /// order the pieces arrive in.
    #[test]
    fn insert_then_remove_is_empty(ranges in prop::collection::vec(range_strategy(), 1..24)) {
        let mut set = RangeSet::new();
        for range in &ranges {
            set.insert(*range);
        }
        for range in &ranges {
            set.remove(*range);
        }
        set.assert_invariants();
        prop_assert!(set.is_empty(), "leftover extents: {:?}", set.iter().collect::<Vec<_>>());
        prop_assert_eq!(set.total_bytes(), 0);
    }
}
