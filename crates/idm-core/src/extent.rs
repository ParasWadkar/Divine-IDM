//! Byte extents — the single source of truth for how byte ranges work.
//!
//! Two conventions here are load-bearing, and both are easy to get wrong in a
//! way that silently corrupts a download:
//!
//! * Every range in this crate is **half-open**: `[start, end)`. HTTP's
//!   `Range` and `Content-Range` headers are **inclusive** at both ends.
//!   [`ByteRange::to_range_header`] and [`parse_content_range`] are the only
//!   two functions that cross that boundary, so the `+1`/`-1` lives in exactly
//!   two places rather than being scattered through the scheduler.
//! * A [`RangeSet`] is always coalesced: it holds no empty, overlapping or
//!   merely adjacent extents. `[0,4)` and `[4,8)` are stored as `[0,8)`, which
//!   is what lets [`RangeSet::contiguous_prefix_end`] be one map lookup.

use std::collections::BTreeMap;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExtentError {
    #[error("range end {end} precedes start {start}")]
    Inverted { start: u64, end: u64 },
    #[error("Content-Range did not use the `bytes` unit: {0:?}")]
    NotBytesUnit(String),
    #[error("malformed Content-Range: {0:?}")]
    Malformed(String),
    #[error("Content-Range end {end} exceeds the stated total {total}")]
    ExceedsTotal { end: u64, total: u64 },
    #[error("Content-Range `bytes */...` requires a total length")]
    UnsatisfiedWithoutTotal,
    #[error("byte offset overflowed u64")]
    Overflow,
    #[error("cannot build a Range header from an empty extent")]
    EmptyRangeHeader,
}

/// A half-open byte range `[start, end)`.
///
/// Fields are private so that an inverted range cannot be built by struct
/// literal; use [`ByteRange::new`] from internal callers (it panics, because
/// such a range is a bug) or [`ByteRange::try_new`] for anything derived from
/// server input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct ByteRange {
    start: u64,
    end: u64,
}

impl ByteRange {
    /// Panics if `end < start`. Use [`try_new`](Self::try_new) for values that
    /// came off the wire.
    #[track_caller]
    pub fn new(start: u64, end: u64) -> Self {
        match Self::try_new(start, end) {
            Ok(range) => range,
            Err(error) => panic!("{error}"),
        }
    }

    pub fn try_new(start: u64, end: u64) -> Result<Self, ExtentError> {
        if end < start {
            return Err(ExtentError::Inverted { start, end });
        }
        Ok(Self { start, end })
    }

    pub fn from_start_len(start: u64, len: u64) -> Result<Self, ExtentError> {
        let end = start.checked_add(len).ok_or(ExtentError::Overflow)?;
        Ok(Self { start, end })
    }

    pub const fn empty_at(offset: u64) -> Self {
        Self { start: offset, end: offset }
    }

    pub const fn start(&self) -> u64 {
        self.start
    }

    pub const fn end(&self) -> u64 {
        self.end
    }

    pub const fn len(&self) -> u64 {
        self.end - self.start
    }

    pub const fn is_empty(&self) -> bool {
        self.start == self.end
    }

    pub const fn contains(&self, offset: u64) -> bool {
        offset >= self.start && offset < self.end
    }

    pub const fn contains_range(&self, other: Self) -> bool {
        other.is_empty() || (other.start >= self.start && other.end <= self.end)
    }

    /// Touching but disjoint ranges (`[0,4)` and `[4,8)`) do **not** overlap.
    pub const fn overlaps(&self, other: Self) -> bool {
        self.start < other.end && other.start < self.end
    }

    pub fn intersect(&self, other: Self) -> Option<Self> {
        let start = self.start.max(other.start);
        let end = self.end.min(other.end);
        (start < end).then_some(Self { start, end })
    }

    /// Splits into `([start, at), [at, end))`. Panics unless `at` lies within
    /// the range; either bound is allowed, so a side may come back empty.
    #[track_caller]
    pub fn split_at(&self, at: u64) -> (Self, Self) {
        assert!(at >= self.start && at <= self.end, "split point {at} outside {self}");
        (Self { start: self.start, end: at }, Self { start: at, end: self.end })
    }

    /// Narrows the end, as the coordinator does when a claim is stolen from.
    /// Panics if the new end would invert or extend the range.
    #[track_caller]
    pub fn with_end(&self, end: u64) -> Self {
        assert!(end >= self.start && end <= self.end, "new end {end} not within {self}");
        Self { start: self.start, end }
    }

    #[track_caller]
    pub fn with_start(&self, start: u64) -> Self {
        assert!(start >= self.start && start <= self.end, "new start {start} not within {self}");
        Self { start, end: self.end }
    }

    /// Renders an inclusive HTTP `Range` header value: the half-open
    /// `[0, 1048576)` becomes `bytes=0-1048575`.
    ///
    /// One of only two functions that cross between the half-open internal
    /// convention and HTTP's inclusive one.
    pub fn to_range_header(&self) -> Result<String, ExtentError> {
        if self.is_empty() {
            return Err(ExtentError::EmptyRangeHeader);
        }
        Ok(format!("bytes={}-{}", self.start, self.end - 1))
    }
}

impl fmt::Display for ByteRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}, {})", self.start, self.end)
    }
}

/// A parsed `Content-Range` response header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentRange {
    /// `bytes 0-1023/4096`, or `bytes 0-1023/*` when the total is withheld.
    Satisfied { range: ByteRange, total: Option<u64> },
    /// `bytes */4096`, the form a 416 response carries.
    Unsatisfied { total: u64 },
}

impl ContentRange {
    pub fn range(&self) -> Option<ByteRange> {
        match self {
            Self::Satisfied { range, .. } => Some(*range),
            Self::Unsatisfied { .. } => None,
        }
    }

    pub fn total(&self) -> Option<u64> {
        match self {
            Self::Satisfied { total, .. } => *total,
            Self::Unsatisfied { total } => Some(*total),
        }
    }
}

/// Parses a `Content-Range` header value into half-open form.
///
/// The other of the two inclusive/half-open conversion points. Lenient about
/// case and surrounding whitespace, strict about structure: an inverted range
/// or an end past the stated total is a server inconsistency the scheduler
/// needs to hear about rather than paper over.
pub fn parse_content_range(value: &str) -> Result<ContentRange, ExtentError> {
    let trimmed = value.trim();
    let unit_len = "bytes".len();
    let after_unit = match trimmed.get(..unit_len) {
        Some(unit) if unit.eq_ignore_ascii_case("bytes") => trimmed[unit_len..].trim_start(),
        _ => return Err(ExtentError::NotBytesUnit(value.to_owned())),
    };

    let (spec, total_text) =
        after_unit.rsplit_once('/').ok_or_else(|| ExtentError::Malformed(value.to_owned()))?;

    let total_text = total_text.trim();
    let total = if total_text == "*" {
        None
    } else {
        Some(total_text.parse::<u64>().map_err(|_| ExtentError::Malformed(value.to_owned()))?)
    };

    let spec = spec.trim();
    if spec == "*" {
        return total
            .map(|total| ContentRange::Unsatisfied { total })
            .ok_or(ExtentError::UnsatisfiedWithoutTotal);
    }

    let (first_text, last_text) =
        spec.split_once('-').ok_or_else(|| ExtentError::Malformed(value.to_owned()))?;
    let first =
        first_text.trim().parse::<u64>().map_err(|_| ExtentError::Malformed(value.to_owned()))?;
    let last =
        last_text.trim().parse::<u64>().map_err(|_| ExtentError::Malformed(value.to_owned()))?;

    // `last` is inclusive on the wire; this is the conversion.
    let end = last.checked_add(1).ok_or(ExtentError::Overflow)?;
    let range = ByteRange::try_new(first, end)?;
    if let Some(total) = total {
        if end > total {
            return Err(ExtentError::ExceedsTotal { end, total });
        }
    }
    Ok(ContentRange::Satisfied { range, total })
}

/// A coalesced set of half-open byte extents.
///
/// Invariants, upheld by every mutator and checked by
/// [`assert_invariants`](Self::assert_invariants): no empty extents, no two
/// extents overlap, and no two extents merely touch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RangeSet {
    /// `start -> end`, ordered by start.
    map: BTreeMap<u64, u64>,
}

impl RangeSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Number of disjoint extents, not bytes. For bytes see
    /// [`total_bytes`](Self::total_bytes).
    pub fn extent_count(&self) -> usize {
        self.map.len()
    }

    pub fn total_bytes(&self) -> u64 {
        self.map.iter().map(|(start, end)| end - start).sum()
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = ByteRange> + '_ {
        self.map.iter().map(|(&start, &end)| ByteRange { start, end })
    }

    pub fn clear(&mut self) {
        self.map.clear();
    }

    /// Start key from which a scan around `offset` must begin: the extent
    /// beginning at or before `offset` when it reaches that far, else `offset`.
    ///
    /// `touching` distinguishes the two callers. For `insert`, an extent ending
    /// exactly at `offset` is adjacent and must be merged. For `remove` and the
    /// gap scans, that same extent does not overlap and must be left alone.
    fn scan_start(&self, offset: u64, touching: bool) -> u64 {
        match self.map.range(..=offset).next_back() {
            Some((&start, &end)) if (touching && end >= offset) || end > offset => start,
            _ => offset,
        }
    }

    /// Adds `range`, merging with everything it overlaps or touches.
    pub fn insert(&mut self, range: ByteRange) {
        if range.is_empty() {
            return;
        }
        let mut start = range.start;
        let mut end = range.end;

        // Every extent in `scan_from..=end` either overlaps or is adjacent, so
        // they all fold into a single extent.
        let scan_from = self.scan_start(start, true);
        let doomed: Vec<u64> = self
            .map
            .range(scan_from..=end)
            .map(|(&existing_start, &existing_end)| {
                start = start.min(existing_start);
                end = end.max(existing_end);
                existing_start
            })
            .collect();
        for key in doomed {
            self.map.remove(&key);
        }
        self.map.insert(start, end);
    }

    /// Subtracts `range`, splitting any extent it cuts through.
    pub fn remove(&mut self, range: ByteRange) {
        if range.is_empty() {
            return;
        }
        let scan_from = self.scan_start(range.start, false);
        let mut doomed = Vec::new();
        let mut survivors = Vec::new();
        // `..range.end` is exclusive: an extent starting exactly at the removed
        // end does not overlap it.
        for (&start, &end) in self.map.range(scan_from..range.end) {
            if end <= range.start {
                continue;
            }
            doomed.push(start);
            if start < range.start {
                survivors.push((start, range.start));
            }
            if end > range.end {
                survivors.push((range.end, end));
            }
        }
        for key in doomed {
            self.map.remove(&key);
        }
        for (start, end) in survivors {
            self.map.insert(start, end);
        }
    }

    pub fn contains(&self, offset: u64) -> bool {
        self.map.range(..=offset).next_back().is_some_and(|(_, &end)| offset < end)
    }

    pub fn contains_range(&self, range: ByteRange) -> bool {
        if range.is_empty() {
            return true;
        }
        self.map.range(..=range.start).next_back().is_some_and(|(_, &end)| end >= range.end)
    }

    /// How many bytes of `bounds` this set covers.
    pub fn intersect_bytes(&self, bounds: ByteRange) -> u64 {
        self.iter().filter_map(|extent| extent.intersect(bounds)).map(|overlap| overlap.len()).sum()
    }

    /// End of the contiguous run starting at byte zero, or `0` when byte zero
    /// is missing. Because the set is coalesced this is one lookup — it is what
    /// `OrderedWindow` mode consults to bound how far ahead a claim may be
    /// handed out.
    pub fn contiguous_prefix_end(&self) -> u64 {
        self.map.get(&0).copied().unwrap_or(0)
    }

    pub fn is_complete(&self, total: u64) -> bool {
        self.contains_range(ByteRange { start: 0, end: total })
    }

    /// The lowest uncovered extent within `bounds`. Allocation-free, and the
    /// one used to hand out claims.
    pub fn first_gap_in(&self, bounds: ByteRange) -> Option<ByteRange> {
        if bounds.is_empty() {
            return None;
        }
        let mut cursor = bounds.start;
        for (&start, &end) in self.map.range(self.scan_start(bounds.start, false)..bounds.end) {
            if end <= cursor {
                continue;
            }
            if start > cursor {
                return Some(ByteRange { start: cursor, end: start.min(bounds.end) });
            }
            cursor = end;
            if cursor >= bounds.end {
                return None;
            }
        }
        (cursor < bounds.end).then_some(ByteRange { start: cursor, end: bounds.end })
    }

    /// Every uncovered extent within `bounds`, lowest first — the complement
    /// used to rebuild the work list after a resume.
    pub fn gaps_in(&self, bounds: ByteRange) -> Vec<ByteRange> {
        let mut gaps = Vec::new();
        if bounds.is_empty() {
            return gaps;
        }
        let mut cursor = bounds.start;
        for (&start, &end) in self.map.range(self.scan_start(bounds.start, false)..bounds.end) {
            if end <= cursor {
                continue;
            }
            if start > cursor {
                gaps.push(ByteRange { start: cursor, end: start.min(bounds.end) });
            }
            cursor = end;
            if cursor >= bounds.end {
                return gaps;
            }
        }
        if cursor < bounds.end {
            gaps.push(ByteRange { start: cursor, end: bounds.end });
        }
        gaps
    }

    /// Panics if any invariant is broken. Called by the property tests after
    /// every mutation, and by `debug_assert` in the coordinator.
    #[track_caller]
    pub fn assert_invariants(&self) {
        let mut previous_end: Option<u64> = None;
        for (&start, &end) in &self.map {
            assert!(start < end, "empty or inverted extent [{start}, {end})");
            if let Some(previous) = previous_end {
                assert!(
                    start > previous,
                    "extent [{start}, {end}) overlaps or touches one ending at {previous}"
                );
            }
            previous_end = Some(end);
        }
    }
}

impl FromIterator<ByteRange> for RangeSet {
    fn from_iter<I: IntoIterator<Item = ByteRange>>(iter: I) -> Self {
        let mut set = Self::new();
        for range in iter {
            set.insert(range);
        }
        set
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(start: u64, end: u64) -> ByteRange {
        ByteRange::new(start, end)
    }

    #[test]
    fn range_header_converts_to_inclusive() {
        assert_eq!(r(0, 1_048_576).to_range_header().unwrap(), "bytes=0-1048575");
        assert_eq!(r(5, 6).to_range_header().unwrap(), "bytes=5-5");
        assert_eq!(r(9, 9).to_range_header(), Err(ExtentError::EmptyRangeHeader));
    }

    #[test]
    fn content_range_round_trips_through_half_open() {
        let parsed = parse_content_range("bytes 0-1048575/2000000").unwrap();
        assert_eq!(
            parsed,
            ContentRange::Satisfied { range: r(0, 1_048_576), total: Some(2_000_000) }
        );
        assert_eq!(parsed.range().unwrap().to_range_header().unwrap(), "bytes=0-1048575");
    }

    #[test]
    fn content_range_accepts_unknown_total_and_odd_spacing() {
        assert_eq!(
            parse_content_range("BYTES  10-19 / *").unwrap(),
            ContentRange::Satisfied { range: r(10, 20), total: None }
        );
    }

    #[test]
    fn content_range_parses_the_416_form() {
        assert_eq!(
            parse_content_range("bytes */4096").unwrap(),
            ContentRange::Unsatisfied { total: 4096 }
        );
        assert_eq!(parse_content_range("bytes */*"), Err(ExtentError::UnsatisfiedWithoutTotal));
    }

    #[test]
    fn content_range_rejects_server_inconsistency() {
        // Inverted: last precedes first.
        assert!(matches!(
            parse_content_range("bytes 20-10/100"),
            Err(ExtentError::Inverted { .. })
        ));
        // End past the stated total.
        assert_eq!(
            parse_content_range("bytes 0-199/100"),
            Err(ExtentError::ExceedsTotal { end: 200, total: 100 })
        );
        assert!(matches!(parse_content_range("items 0-9/100"), Err(ExtentError::NotBytesUnit(_))));
        assert!(matches!(parse_content_range("bytes 0-9"), Err(ExtentError::Malformed(_))));
        assert!(matches!(
            parse_content_range(&format!("bytes 0-{}/100", u64::MAX)),
            Err(ExtentError::Overflow)
        ));
    }

    #[test]
    fn adjacent_inserts_coalesce() {
        let mut set = RangeSet::new();
        set.insert(r(0, 4));
        set.insert(r(4, 8));
        set.assert_invariants();
        assert_eq!(set.extent_count(), 1);
        assert_eq!(set.iter().next().unwrap(), r(0, 8));
    }

    #[test]
    fn insert_bridges_a_gap_between_two_extents() {
        let mut set: RangeSet = [r(0, 10), r(20, 30)].into_iter().collect();
        set.insert(r(10, 20));
        set.assert_invariants();
        assert_eq!(set.extent_count(), 1);
        assert_eq!(set.iter().next().unwrap(), r(0, 30));
    }

    #[test]
    fn insert_swallows_fully_covered_extents() {
        let mut set: RangeSet = [r(10, 20), r(30, 40), r(50, 60)].into_iter().collect();
        set.insert(r(0, 100));
        set.assert_invariants();
        assert_eq!(set.extent_count(), 1);
        assert_eq!(set.total_bytes(), 100);
    }

    #[test]
    fn remove_splits_an_extent_in_two() {
        let mut set: RangeSet = [r(0, 100)].into_iter().collect();
        set.remove(r(40, 60));
        set.assert_invariants();
        assert_eq!(set.iter().collect::<Vec<_>>(), vec![r(0, 40), r(60, 100)]);
    }

    #[test]
    fn remove_keeps_the_untouched_side_of_a_coalesced_extent() {
        let mut set: RangeSet = [r(0, 10), r(10, 20)].into_iter().collect();
        // The two coalesced into [0,20); removing [10,20) must leave [0,10).
        set.remove(r(10, 20));
        set.assert_invariants();
        assert_eq!(set.iter().collect::<Vec<_>>(), vec![r(0, 10)]);
    }

    #[test]
    fn gaps_are_the_complement_within_bounds() {
        let set: RangeSet = [r(10, 20), r(30, 40)].into_iter().collect();
        assert_eq!(set.gaps_in(r(0, 50)), vec![r(0, 10), r(20, 30), r(40, 50)]);
        // Bounds clip the first and last gap.
        assert_eq!(set.gaps_in(r(15, 35)), vec![r(20, 30)]);
        assert_eq!(set.first_gap_in(r(0, 50)), Some(r(0, 10)));
        assert_eq!(set.first_gap_in(r(10, 20)), None);
    }

    #[test]
    fn contiguous_prefix_ignores_a_detached_head() {
        let mut set: RangeSet = [r(4, 100)].into_iter().collect();
        assert_eq!(set.contiguous_prefix_end(), 0, "byte zero is missing");
        set.insert(r(0, 4));
        assert_eq!(set.contiguous_prefix_end(), 100);
    }

    #[test]
    fn completeness_and_coverage() {
        let mut set: RangeSet = [r(0, 50)].into_iter().collect();
        assert!(!set.is_complete(100));
        assert_eq!(set.intersect_bytes(r(25, 75)), 25);
        set.insert(r(50, 100));
        assert!(set.is_complete(100));
        assert!(set.contains_range(r(0, 100)));
        assert!(!set.contains(100));
    }
}
