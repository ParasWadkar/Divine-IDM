//! Work stealing and tail hedging: the policy that kills the straggler.
//!
//! Near the end of a parallel download, fifteen connections are finished and
//! one slow one still owns a few megabytes. Wall-clock time is then set
//! entirely by that straggler. This module decides what to do about it.
//!
//! Every function here is pure, over snapshots of claim state, so the policy
//! can be tested exhaustively without a network, a clock, or a disk.
//!
//! # The part that is easy to get wrong
//!
//! A steal lowers the donor's soft end; the donor notices after its next chunk
//! and retires cleanly. The question is *where* to cut. Bytes already in flight
//! occupy roughly `[cursor, cursor + receive_buffer + BDP + server_write_buffer)`,
//! so cutting at the naive midpoint of the remaining range throws away data
//! that has already been paid for — on every single steal. Cutting beyond an
//! in-flight guard instead makes a steal cost *zero* bytes: just a stream
//! cancellation.
//!
//! # The protocol asymmetry
//!
//! On HTTP/2 and HTTP/3, dropping a response body sends `RST_STREAM` or
//! `STOP_SENDING` and the connection stays pooled, so a steal is nearly free.
//! On HTTP/1.1 an un-drained response makes the connection unpoolable and it
//! gets closed, costing a fresh handshake. So the minimum worthwhile steal is
//! higher on HTTP/1.1, and a remainder small enough to simply drain is never
//! worth splitting at all.

use super::ClaimId;
use crate::extent::ByteRange;

/// Which transport a claim is running over, which sets what a steal costs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Http1,
    Http2,
    Http3,
}

impl Protocol {
    /// Whether cancelling a response body keeps the connection usable.
    pub fn cancel_is_cheap(self) -> bool {
        matches!(self, Self::Http2 | Self::Http3)
    }
}

/// A point-in-time view of one in-flight claim.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClaimSnapshot {
    pub id: ClaimId,
    /// What the claim currently owns: `[start, end_soft)`.
    pub range: ByteRange,
    /// Next byte not yet requested.
    pub cursor: u64,
    /// Recent throughput for this claim. Zero means stalled.
    pub rate_bps: f64,
    pub splits_done: u8,
    pub millis_since_split: u64,
    pub protocol: Protocol,
}

impl ClaimSnapshot {
    pub fn remaining(&self) -> u64 {
        self.range.end().saturating_sub(self.cursor)
    }

    /// Projected seconds to finish. A stalled claim is infinitely far from
    /// done, which makes it the most attractive donor — exactly right.
    pub fn eta_secs(&self) -> f64 {
        let remaining = self.remaining();
        if remaining == 0 {
            return 0.0;
        }
        if self.rate_bps <= 0.0 {
            return f64::INFINITY;
        }
        remaining as f64 / self.rate_bps
    }

    /// How far past the cursor a cut must be to avoid discarding bytes already
    /// on the wire.
    fn inflight_guard(&self, policy: &StealPolicy) -> u64 {
        let by_time = (self.rate_bps * policy.inflight_seconds) as u64;
        policy.inflight_floor.max(by_time)
    }

    fn min_steal(&self, policy: &StealPolicy) -> u64 {
        if self.protocol.cancel_is_cheap() {
            policy.min_steal_cheap_cancel
        } else {
            policy.min_steal_costly_cancel
        }
    }
}

#[derive(Debug, Clone)]
pub struct StealPolicy {
    /// Smallest remainder worth splitting when cancelling is cheap (H2/H3).
    pub min_steal_cheap_cancel: u64,
    /// Smallest remainder worth splitting when cancelling costs a handshake (H1).
    pub min_steal_costly_cancel: u64,
    /// Each side of a split must be at least this big.
    pub min_split_half: u64,
    /// Floor for the in-flight guard, for when the rate estimate is low.
    pub inflight_floor: u64,
    /// Seconds of the donor's throughput to treat as in flight.
    pub inflight_seconds: f64,
    /// A split must save at least this long...
    pub min_gain_secs: f64,
    /// ...and at least this fraction of the donor's remaining time.
    pub min_gain_frac: f64,
    /// Never split one claim more than this many times.
    pub max_splits: u8,
    /// Let the rate estimate catch up before splitting the same claim again.
    pub split_cooldown_ms: u64,
    /// A remainder this small is drained rather than split: the bytes are
    /// already coming and are usable.
    pub drain_threshold: u64,
    /// Hedging only applies once the whole download is this close to done.
    pub hedge_window: u64,
    /// A hedge needs the straggler to be at least this slow in absolute terms.
    pub hedge_min_eta_secs: f64,
    /// ...and this many times worse than the median claim.
    pub hedge_eta_multiple: f64,
    pub max_hedges: u8,
}

impl Default for StealPolicy {
    fn default() -> Self {
        Self {
            min_steal_cheap_cancel: 8 << 20,
            min_steal_costly_cancel: 16 << 20,
            min_split_half: 4 << 20,
            inflight_floor: 4 << 20,
            inflight_seconds: 1.5,
            min_gain_secs: 2.0,
            min_gain_frac: 0.15,
            max_splits: 4,
            split_cooldown_ms: 1500,
            drain_threshold: 256 << 10,
            hedge_window: 32 << 20,
            hedge_min_eta_secs: 1.5,
            hedge_eta_multiple: 2.0,
            max_hedges: 2,
        }
    }
}

/// Why nothing was done, so callers and tests can see which guard fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleReason {
    /// No claims at all, nothing to steal from.
    NothingInFlight,
    /// The best donor's remainder is below the minimum worth a request.
    RemainderTooSmall,
    /// Small enough that draining it beats cancelling it.
    DrainInstead,
    /// The in-flight guard leaves too little on the far side to be worth taking.
    NoRoomAfterGuard,
    /// The split would not save enough time to pay for the request.
    GainTooSmall,
    /// Anti-thrash: too many splits, too recent, or the budget is spent.
    SplitBudgetSpent,
    /// Not yet in the endgame, so hedging would waste bandwidth.
    NotEndgame,
    /// No straggler stands out from the pack.
    NoStraggler,
    /// Already hedged as much as allowed.
    HedgeExhausted,
}

/// What a worker that just became free should do next.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    /// Free work was available; no stealing needed.
    TakeGap(ByteRange),
    /// Lower `donor`'s soft end to `at` and take the far side.
    Split {
        donor: ClaimId,
        at: u64,
        taken: ByteRange,
    },
    /// Request `range` a second time, racing the straggler that owns it.
    Hedge {
        donor: ClaimId,
        range: ByteRange,
    },
    Idle(IdleReason),
}

/// Decides what a newly free worker should do.
///
/// `thief_rate_bps` should be the cohort's median rate for a brand-new worker
/// rather than zero, or the gain test below could never pass on a first steal.
#[allow(clippy::too_many_arguments)]
pub fn next_action(
    free_gap: Option<ByteRange>,
    claims: &[ClaimSnapshot],
    thief_rate_bps: f64,
    total_remaining: u64,
    hedges_used: u8,
    steal_tokens: u32,
    policy: &StealPolicy,
) -> Action {
    // Free work always wins: it costs no cancellation and wastes nothing.
    if let Some(gap) = free_gap {
        if !gap.is_empty() {
            return Action::TakeGap(gap);
        }
    }

    match plan_split(claims, thief_rate_bps, steal_tokens, policy) {
        Ok(action) => action,
        Err(split_reason) => {
            // Splitting cannot help, so consider racing the straggler instead.
            match plan_hedge(claims, total_remaining, hedges_used, policy) {
                Ok(action) => action,
                // Hedging is the last resort, so its refusal is the reason
                // nothing at all happened -- except when there was never
                // anything in flight to begin with.
                Err(hedge_reason) => Action::Idle(match hedge_reason {
                    IdleReason::NothingInFlight => split_reason,
                    other => other,
                }),
            }
        }
    }
}

/// Picks the donor whose projected finish is furthest away and works out where
/// to cut it.
///
/// Public so the individual guards can be tested directly: through
/// [`next_action`] a refusal here competes with the hedge planner's, and only
/// one reason can be reported.
pub fn plan_split(
    claims: &[ClaimSnapshot],
    thief_rate_bps: f64,
    steal_tokens: u32,
    policy: &StealPolicy,
) -> Result<Action, IdleReason> {
    // Slowest-to-finish, not largest-remaining: a big claim moving fast is not
    // the problem, a small one that is stalled is.
    let donor = claims
        .iter()
        .filter(|claim| claim.remaining() > 0)
        .max_by(|a, b| a.eta_secs().total_cmp(&b.eta_secs()))
        .ok_or(IdleReason::NothingInFlight)?;

    let remaining = donor.remaining();
    if remaining < policy.drain_threshold {
        return Err(IdleReason::DrainInstead);
    }
    if remaining < donor.min_steal(policy) {
        return Err(IdleReason::RemainderTooSmall);
    }

    if steal_tokens == 0
        || donor.splits_done >= policy.max_splits
        || donor.millis_since_split < policy.split_cooldown_ms
    {
        return Err(IdleReason::SplitBudgetSpent);
    }

    // The fair-share point equalises both halves' projected finish times; the
    // in-flight guard is the floor that keeps the cut clear of bytes already
    // on the wire.
    let thief_rate = thief_rate_bps.max(0.0);
    let fair_offset = if donor.rate_bps + thief_rate > 0.0 {
        (remaining as f64 * (donor.rate_bps / (donor.rate_bps + thief_rate))) as u64
    } else {
        remaining / 2
    };
    let offset = donor.inflight_guard(policy).max(fair_offset);
    let at = donor.cursor.saturating_add(offset);

    if at >= donor.range.end() {
        return Err(IdleReason::NoRoomAfterGuard);
    }
    let taken = ByteRange::new(at, donor.range.end());
    if taken.len() < policy.min_split_half {
        return Err(IdleReason::NoRoomAfterGuard);
    }

    // Would this actually help? Compare the donor finishing alone against the
    // two halves running in parallel.
    let eta_before = donor.eta_secs();
    let eta_after = if donor.rate_bps + thief_rate > 0.0 {
        remaining as f64 / (donor.rate_bps + thief_rate)
    } else {
        f64::INFINITY
    };
    let gain = eta_before - eta_after;
    let enough = gain.is_infinite()
        || (gain >= policy.min_gain_secs && gain >= policy.min_gain_frac * eta_before);
    if !enough {
        return Err(IdleReason::GainTooSmall);
    }

    Ok(Action::Split { donor: donor.id, at, taken })
}

/// Once splitting can no longer help, race the straggler for its tail.
///
/// A worker holding 6 MiB at 2 MB/s adds three seconds to a download that is
/// otherwise finished, and by then there is nothing left to split. Issuing a
/// duplicate request for its tail and taking whichever arrives first is the
/// only remaining lever.
///
/// Public for the same reason as [`plan_split`].
pub fn plan_hedge(
    claims: &[ClaimSnapshot],
    total_remaining: u64,
    hedges_used: u8,
    policy: &StealPolicy,
) -> Result<Action, IdleReason> {
    if hedges_used >= policy.max_hedges {
        return Err(IdleReason::HedgeExhausted);
    }
    // Duplicating work costs bandwidth, so only in the endgame.
    if total_remaining >= policy.hedge_window {
        return Err(IdleReason::NotEndgame);
    }

    let active: Vec<&ClaimSnapshot> = claims.iter().filter(|claim| claim.remaining() > 0).collect();
    if active.is_empty() {
        return Err(IdleReason::NothingInFlight);
    }

    let slowest = active
        .iter()
        .copied()
        .max_by(|a, b| a.eta_secs().total_cmp(&b.eta_secs()))
        .ok_or(IdleReason::NothingInFlight)?;

    // The median must exclude the straggler itself. Comparing it against a
    // median it is part of makes `slowest > 2 * median` arithmetically
    // impossible once it is the only claim left -- which is exactly the
    // single-straggler endgame hedging exists for.
    let slowest_eta = slowest.eta_secs();
    let mut others: Vec<f64> = active
        .iter()
        .filter(|claim| claim.id != slowest.id)
        .map(|claim| claim.eta_secs())
        .collect();

    let stands_out = if others.is_empty() {
        // No pack to stand out from, so the absolute threshold is the only
        // meaningful test.
        slowest_eta.is_infinite() || slowest_eta > policy.hedge_min_eta_secs
    } else {
        others.sort_by(f64::total_cmp);
        let median = others[others.len() / 2];
        slowest_eta.is_infinite()
            || (slowest_eta > policy.hedge_min_eta_secs
                && slowest_eta > policy.hedge_eta_multiple * median)
    };
    if !stands_out {
        return Err(IdleReason::NoStraggler);
    }

    // The tail half, so the straggler keeps making progress on the front half
    // and the duplicated work is bounded.
    let remaining = slowest.remaining();
    let at = slowest.cursor.saturating_add(remaining / 2);
    if at >= slowest.range.end() {
        return Err(IdleReason::NoStraggler);
    }
    Ok(Action::Hedge { donor: slowest.id, range: ByteRange::new(at, slowest.range.end()) })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;
    /// 10 MB/s, a plausible single-connection rate.
    const FAST: f64 = 10.0 * 1_000_000.0;

    fn claim(id: u32, start: u64, end: u64, cursor: u64, rate: f64) -> ClaimSnapshot {
        ClaimSnapshot {
            id: ClaimId(id),
            range: ByteRange::new(start, end),
            cursor,
            rate_bps: rate,
            splits_done: 0,
            // Well past the cooldown, so tests opt in to thrash guards rather
            // than tripping over them.
            millis_since_split: 60_000,
            protocol: Protocol::Http2,
        }
    }

    #[test]
    fn free_work_is_always_preferred_to_stealing() {
        let gap = ByteRange::new(0, 8 * MIB);
        let claims = [claim(1, 100 * MIB, 200 * MIB, 100 * MIB, 1000.0)];
        let action =
            next_action(Some(gap), &claims, FAST, 200 * MIB, 0, 8, &StealPolicy::default());
        assert_eq!(action, Action::TakeGap(gap), "stole when free work existed");
    }

    #[test]
    fn the_split_point_clears_the_in_flight_window() {
        // The bug this guards: cutting at the midpoint of the remaining range
        // discards bytes already on the wire, on every steal.
        let policy = StealPolicy::default();
        let donor_rate = 20.0 * 1_000_000.0; // 20 MB/s
        let claims = [claim(1, 0, 256 * MIB, 0, donor_rate)];

        let Action::Split { at, taken, .. } =
            next_action(None, &claims, FAST, 256 * MIB, 0, 8, &policy)
        else {
            panic!("expected a split");
        };

        // 1.5s at 20 MB/s is 30 MB, well above the 4 MiB floor.
        let guard = (donor_rate * policy.inflight_seconds) as u64;
        assert!(
            at >= guard,
            "cut at {at} is inside the ~{guard}-byte in-flight window; those bytes would be thrown away"
        );
        assert_eq!(taken.end(), 256 * MIB);
        assert_eq!(taken.start(), at);
    }

    #[test]
    fn the_floor_applies_when_the_donor_is_slow() {
        // A slow donor has little in flight, but a cut right at the cursor
        // would still discard whatever is buffered.
        let policy = StealPolicy::default();
        let claims = [claim(1, 0, 256 * MIB, 0, 1000.0)];
        let Action::Split { at, .. } = next_action(None, &claims, FAST, 256 * MIB, 0, 8, &policy)
        else {
            panic!("expected a split");
        };
        assert!(at >= policy.inflight_floor, "cut at {at} ignored the in-flight floor");
    }

    #[test]
    fn the_slowest_to_finish_is_chosen_not_the_largest() {
        // A big claim moving fast is not the problem. The small stalled one is.
        let claims = [
            claim(1, 0, 400 * MIB, 0, FAST * 10.0), // huge but very fast
            claim(2, 400 * MIB, 440 * MIB, 400 * MIB, 1000.0), // small and crawling
        ];
        let Action::Split { donor, .. } =
            next_action(None, &claims, FAST, 440 * MIB, 0, 8, &StealPolicy::default())
        else {
            panic!("expected a split");
        };
        assert_eq!(donor, ClaimId(2), "stole from the largest rather than the slowest");
    }

    #[test]
    fn a_stalled_claim_is_the_most_attractive_donor() {
        let claims = [
            claim(1, 0, 100 * MIB, 0, FAST),
            claim(2, 100 * MIB, 200 * MIB, 100 * MIB, 0.0), // stalled
        ];
        let Action::Split { donor, .. } =
            next_action(None, &claims, FAST, 200 * MIB, 0, 8, &StealPolicy::default())
        else {
            panic!("expected a split");
        };
        assert_eq!(donor, ClaimId(2));
    }

    // The guard tests below call `plan_split` directly. Through `next_action` a
    // split refusal competes with the hedge planner's, and only one reason can
    // be reported, so testing the unit keeps each assertion about one guard.

    #[test]
    fn a_remainder_below_the_minimum_is_left_alone() {
        let policy = StealPolicy::default();
        // 4 MiB left on an H2 claim, under the 8 MiB minimum.
        let claims = [claim(1, 0, 100 * MIB, 96 * MIB, FAST)];
        assert_eq!(
            plan_split(&claims, FAST, 8, &policy).unwrap_err(),
            IdleReason::RemainderTooSmall
        );
    }

    #[test]
    fn http1_demands_a_bigger_prize_than_http2() {
        let policy = StealPolicy::default();
        // 10 MiB remaining: worth it on H2, not on H1 where cancelling the
        // body makes the connection unpoolable and costs a handshake.
        let mut h2 = claim(1, 0, 100 * MIB, 90 * MIB, 1000.0);
        h2.protocol = Protocol::Http2;
        assert!(
            plan_split(&[h2], FAST, 8, &policy).is_ok(),
            "10 MiB should be worth splitting on H2"
        );

        let mut h1 = h2;
        h1.protocol = Protocol::Http1;
        assert_eq!(
            plan_split(&[h1], FAST, 8, &policy).unwrap_err(),
            IdleReason::RemainderTooSmall,
            "on H1 a steal must pay for the handshake it costs"
        );
    }

    #[test]
    fn a_tiny_remainder_is_drained_rather_than_cancelled() {
        let policy = StealPolicy::default();
        // 64 KiB left: the bytes are already coming and are usable, so
        // draining beats cancelling.
        let claims = [claim(1, 0, 100 * MIB, 100 * MIB - 64 * 1024, FAST)];
        assert_eq!(plan_split(&claims, FAST, 8, &policy).unwrap_err(), IdleReason::DrainInstead);
    }

    #[test]
    fn a_split_that_saves_too_little_is_refused() {
        let policy = StealPolicy::default();
        // 500 MiB at 50 MB/s finishes in ~10.5s; splitting with a 10 MB/s
        // thief brings that to ~8.7s. A 1.7s saving does not clear the
        // 2-second floor, so the request is not worth issuing.
        let claims = [claim(1, 0, 500 * MIB, 0, 50.0 * 1_000_000.0)];
        assert_eq!(plan_split(&claims, FAST, 8, &policy).unwrap_err(), IdleReason::GainTooSmall);
    }

    #[test]
    fn anti_thrash_guards_each_stop_a_split() {
        let policy = StealPolicy::default();
        let base = claim(1, 0, 256 * MIB, 0, 1000.0);

        // Already split the maximum number of times.
        let mut exhausted = base;
        exhausted.splits_done = policy.max_splits;
        assert_eq!(
            plan_split(&[exhausted], FAST, 8, &policy).unwrap_err(),
            IdleReason::SplitBudgetSpent
        );

        // Split too recently for the rate estimate to mean anything yet.
        let mut recent = base;
        recent.millis_since_split = policy.split_cooldown_ms - 1;
        assert_eq!(
            plan_split(&[recent], FAST, 8, &policy).unwrap_err(),
            IdleReason::SplitBudgetSpent
        );

        // The download-wide token bucket is empty.
        assert_eq!(
            plan_split(&[base], FAST, 0, &policy).unwrap_err(),
            IdleReason::SplitBudgetSpent
        );
    }

    #[test]
    fn no_split_leaves_a_sliver_on_the_far_side() {
        let policy = StealPolicy::default();
        // At 40 MB/s the in-flight guard is ~57 MiB, which on a 60 MiB
        // remainder leaves the thief under min_split_half. Better to leave the
        // donor alone than issue a request for 3 MiB.
        let claims = [claim(1, 0, 60 * MIB, 0, 40.0 * 1_000_000.0)];
        assert_eq!(
            plan_split(&claims, FAST, 8, &policy).unwrap_err(),
            IdleReason::NoRoomAfterGuard
        );
    }

    #[test]
    fn a_hedge_races_the_straggler_in_the_endgame() {
        let policy = StealPolicy::default();
        // Nothing left to split: one claim, 6 MiB remaining at 2 MB/s, which is
        // three seconds of nobody else doing anything.
        let claims = [claim(1, 0, 100 * MIB, 94 * MIB, 2.0 * 1_000_000.0)];
        let action = next_action(None, &claims, FAST, 6 * MIB, 0, 8, &policy);

        let Action::Hedge { donor, range } = action else {
            panic!("expected a hedge, got {action:?}");
        };
        assert_eq!(donor, ClaimId(1));
        // The tail half, so the donor keeps working on the front half.
        assert_eq!(range, ByteRange::new(94 * MIB + 3 * MIB, 100 * MIB));
    }

    #[test]
    fn a_lone_straggler_can_still_be_hedged() {
        // Regression guard. The median used to include the straggler itself,
        // which makes `slowest > 2 * median` arithmetically impossible once it
        // is the only claim left -- precisely the case hedging exists for.
        let policy = StealPolicy::default();
        let only = [claim(1, 0, 100 * MIB, 94 * MIB, 2.0 * 1_000_000.0)];
        assert!(
            plan_hedge(&only, 6 * MIB, 0, &policy).is_ok(),
            "a single slow claim must be hedgeable"
        );
    }

    #[test]
    fn hedging_waits_for_the_endgame() {
        let policy = StealPolicy::default();
        // Same slow claim, but plenty of the file still outstanding: hedging
        // here would just duplicate work that parallelism is already covering.
        let claims = [claim(1, 0, 400 * MIB, 396 * MIB, 2.0 * 1_000_000.0)];
        assert_eq!(plan_hedge(&claims, 300 * MIB, 0, &policy).unwrap_err(), IdleReason::NotEndgame);
    }

    #[test]
    fn hedging_needs_an_actual_outlier() {
        let policy = StealPolicy::default();
        // Three claims all equally slow: none is the straggler, so duplicating
        // any of them is pure waste.
        let rate = 2.0 * 1_000_000.0;
        let claims = [
            claim(1, 0, 10 * MIB, 8 * MIB, rate),
            claim(2, 10 * MIB, 20 * MIB, 18 * MIB, rate),
            claim(3, 20 * MIB, 30 * MIB, 28 * MIB, rate),
        ];
        assert_eq!(plan_hedge(&claims, 6 * MIB, 0, &policy).unwrap_err(), IdleReason::NoStraggler);
    }

    #[test]
    fn one_claim_far_worse_than_its_peers_is_hedged() {
        let policy = StealPolicy::default();
        // Two healthy claims and one crawling: the outlier should be raced.
        let claims = [
            claim(1, 0, 10 * MIB, 9 * MIB, 20.0 * 1_000_000.0),
            claim(2, 10 * MIB, 20 * MIB, 19 * MIB, 20.0 * 1_000_000.0),
            claim(3, 20 * MIB, 30 * MIB, 22 * MIB, 500_000.0),
        ];
        let Action::Hedge { donor, .. } = plan_hedge(&claims, 10 * MIB, 0, &policy).unwrap() else {
            panic!("expected a hedge");
        };
        assert_eq!(donor, ClaimId(3));
    }

    #[test]
    fn hedges_are_capped() {
        let policy = StealPolicy::default();
        let claims = [claim(1, 0, 100 * MIB, 94 * MIB, 2.0 * 1_000_000.0)];
        assert_eq!(
            plan_hedge(&claims, 6 * MIB, policy.max_hedges, &policy).unwrap_err(),
            IdleReason::HedgeExhausted
        );
    }

    #[test]
    fn nothing_in_flight_is_reported_as_such() {
        assert_eq!(
            next_action(None, &[], FAST, 0, 0, 8, &StealPolicy::default()),
            Action::Idle(IdleReason::NothingInFlight)
        );
    }

    #[test]
    fn a_finished_claim_is_never_chosen_as_a_donor() {
        let policy = StealPolicy::default();
        let done = claim(1, 0, 50 * MIB, 50 * MIB, FAST); // cursor at the end
        let live = claim(2, 50 * MIB, 300 * MIB, 50 * MIB, 1000.0);
        let Action::Split { donor, .. } =
            next_action(None, &[done, live], FAST, 250 * MIB, 0, 8, &policy)
        else {
            panic!("expected a split");
        };
        assert_eq!(donor, ClaimId(2));
    }

    #[test]
    fn repeated_splits_tile_the_donor_without_gaps_or_overlap() {
        // The invariant that matters: however often a claim is split, the
        // pieces must still exactly cover what the original owned.
        let policy = StealPolicy::default();
        let original = ByteRange::new(0, 512 * MIB);
        let mut claims = vec![claim(1, original.start(), original.end(), 0, 1000.0)];
        for next_id in 2u32..8 {
            let action = next_action(None, &claims, FAST, original.len(), 0, 8, &policy);
            let Action::Split { donor, at, taken } = action else { break };

            let donor_index =
                claims.iter().position(|claim| claim.id == donor).expect("donor must exist");
            // The donor keeps the near side; the thief gets the far side.
            claims[donor_index].range = claims[donor_index].range.with_end(at);
            claims[donor_index].splits_done += 1;

            let mut thief = claim(next_id, taken.start(), taken.end(), taken.start(), 1000.0);
            thief.protocol = Protocol::Http2;
            claims.push(thief);
        }

        assert!(claims.len() > 1, "no split ever happened, so nothing is proven");
        let tiled: crate::RangeSet = claims.iter().map(|claim| claim.range).collect();
        assert_eq!(
            tiled.total_bytes(),
            claims.iter().map(|claim| claim.range.len()).sum::<u64>(),
            "splits overlap"
        );
        assert_eq!(tiled.extent_count(), 1, "splits left a gap");
        assert_eq!(tiled.iter().next(), Some(original));
    }
}
