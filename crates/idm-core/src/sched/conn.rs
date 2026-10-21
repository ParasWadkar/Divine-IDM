//! How many connections to use, decided by measurement rather than by config.
//!
//! # Why a fixed number is wrong
//!
//! Commercial download managers make you pick 8, 16 or 32 in a settings
//! dialog. No such number can be right, because the optimum depends on five
//! things the dialog cannot know:
//!
//! * the bandwidth-delay product (1 Gbps x 20 ms is 2.5 MB; x 200 ms is 25 MB);
//! * whether the origin shapes **per connection** (so N connections give
//!   roughly N times the rate) or **per IP** (so N connections give the *same*
//!   rate split N ways, plus N times the abuse-heuristic risk);
//! * path loss, where independent flows are a throughput lever but also an
//!   incast problem at a consumer router;
//! * the disk's write ceiling, beyond which another connection is pure memory
//!   cost;
//! * the file size, since 32 connections on a 4 MB file is 32 handshakes to
//!   move 128 KB each.
//!
//! Fixed 8 leaves most of a gigabit unused against an origin that caps a single
//! stream at 25 MB/s. Fixed 32 against a per-IP-shaped CDN earns a 429 for
//! exactly the same throughput. So the count is ramped and the *marginal*
//! benefit of each addition is measured.
//!
//! Times are plain monotonic milliseconds rather than `Instant`, so the whole
//! controller is testable without sleeping or faking a clock.

use std::time::Duration;

/// A settled count, plus what was learned about a host, so the next download
/// from the same CDN starts tuned instead of climbing from four again. On a
/// 200 MB file the ramp is a measurable fraction of total time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostProfile {
    pub best_conn_count: u32,
    pub conn_ceiling: u32,
    /// Did extra streams on one connection actually add throughput? If not,
    /// this origin needs separate connections rather than more streams.
    pub multiplexing_helped: bool,
    pub h3_won_race: Option<bool>,
    /// Gateway MAC plus interface, so a profile learned on one network is not
    /// trusted on another.
    pub net_fingerprint: u64,
    /// Monotonic-ish wall clock millis, for staleness.
    pub updated_at_ms: u64,
}

/// Profiles older than this are ignored: CDNs, routes and links all change.
pub const PROFILE_MAX_AGE_MS: u64 = 7 * 24 * 60 * 60 * 1000;

impl HostProfile {
    /// Whether this profile should be trusted as a starting point.
    pub fn is_usable(&self, now_ms: u64, fingerprint: u64) -> bool {
        self.net_fingerprint == fingerprint
            && now_ms.saturating_sub(self.updated_at_ms) < PROFILE_MAX_AGE_MS
    }
}

#[derive(Debug, Clone)]
pub struct ConnPolicy {
    pub start: u32,
    pub step: u32,
    /// Never grow by more than this fraction in one decision.
    pub max_step_frac: f64,
    /// Per-host cap, and the absolute ceiling across all downloads.
    pub per_host_max: u32,
    pub global_max: u32,
    /// An addition must improve aggregate throughput by at least this much.
    pub min_gain_frac: f64,
    /// A mean per-connection rate falling this far means the connections are
    /// actively fighting each other, not merely sharing.
    ///
    /// Only consulted once a gain has already proved weak. A flat aggregate
    /// makes per-connection rate fall *arithmetically* as the count grows
    /// (`per_conn = aggregate / n`), so treating any drop as a terminal signal
    /// would fire on every weak window and hide the ordinary
    /// diminishing-returns case entirely. Hence a deliberately severe
    /// threshold.
    pub per_conn_collapse_frac: f64,
    /// Aggregate change below this counts as flat.
    pub flat_gain_frac: f64,
    /// Low spread across connections plus a flat aggregate is the signature of
    /// a single per-IP token bucket.
    pub low_variance_frac: f64,
    /// Chunk interarrival p95 blowing out by this multiple means queueing.
    pub queueing_p95_multiple: f64,
    pub probe_interval: Duration,
    /// On a throttle, drop to this fraction of the current count.
    pub throttle_backoff_frac: f64,
    /// And do not probe upwards again for this long.
    pub throttle_quiet: Duration,
    /// Settle after this many consecutive weak gains.
    pub weak_gains_before_settling: u32,
}

impl Default for ConnPolicy {
    fn default() -> Self {
        Self {
            start: 4,
            step: 2,
            max_step_frac: 0.5,
            per_host_max: 16,
            global_max: 64,
            min_gain_frac: 0.15,
            per_conn_collapse_frac: 0.40,
            flat_gain_frac: 0.05,
            low_variance_frac: 0.10,
            queueing_p95_multiple: 3.0,
            probe_interval: Duration::from_millis(1500),
            throttle_backoff_frac: 0.7,
            throttle_quiet: Duration::from_secs(30),
            weak_gains_before_settling: 2,
        }
    }
}

impl ConnPolicy {
    /// Probing faster than a few round trips measures noise, so the interval
    /// is the larger of the configured one and four RTTs.
    pub fn probe_interval_ms(&self, rtt: Duration) -> u64 {
        let four_rtts = rtt.as_millis().saturating_mul(4) as u64;
        (self.probe_interval.as_millis() as u64).max(four_rtts)
    }
}

/// What a measurement window looked like.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Observation {
    pub aggregate_bps: f64,
    pub mean_per_conn_bps: f64,
    /// Spread of per-connection rates, as a fraction of the mean.
    pub per_conn_spread_frac: f64,
    /// 429/503/509, a `Retry-After`, or an RST surge.
    pub throttled: bool,
    /// The writer has been back-pressuring: the disk is the bottleneck.
    pub writer_backpressure: bool,
    /// Chunk interarrival p95 relative to its earlier baseline.
    pub interarrival_p95_ratio: f64,
    pub rtt: Duration,
}

impl Observation {
    /// A healthy window at a given rate, for tests and for the first sample.
    pub fn healthy(aggregate_bps: f64, connections: u32) -> Self {
        Self {
            aggregate_bps,
            mean_per_conn_bps: aggregate_bps / f64::from(connections.max(1)),
            per_conn_spread_frac: 0.3,
            throttled: false,
            writer_backpressure: false,
            interarrival_p95_ratio: 1.0,
            rtt: Duration::from_millis(20),
        }
    }
}

/// Why the ramp stopped, which is worth surfacing: "disk-limited" is a very
/// different message to the user than "the server is throttling you".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettleReason {
    /// Each addition bought too little, twice running.
    DiminishingReturns,
    /// Aggregate flat while the count grew, with little spread between
    /// connections: one shared token bucket.
    PerIpShaping,
    /// The disk cannot keep up, so more connections are pure memory cost.
    DiskBound,
    /// Interarrival times blew out: queueing or loss on the path.
    Queueing,
    /// Per-connection rate collapsed: the connections compete with each other.
    PerConnCollapse,
    /// Reached the configured cap.
    AtCap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnDecision {
    /// Keep the current count; not enough time has passed, or we are settled.
    Hold,
    Add {
        to: u32,
    },
    Settle {
        at: u32,
        reason: SettleReason,
    },
    /// Throttled: back off and stop probing for a while.
    Reduce {
        to: u32,
        ceiling: u32,
    },
}

/// The AIMD controller: additive increase while it pays, multiplicative
/// decrease the moment the server objects.
#[derive(Debug, Clone)]
pub struct ConnController {
    policy: ConnPolicy,
    current: u32,
    ceiling: u32,
    weak_gains: u32,
    settled: Option<SettleReason>,
    /// Aggregate throughput before the most recent addition, to measure the
    /// marginal benefit of that addition rather than absolute speed.
    baseline_bps: f64,
    baseline_per_conn_bps: f64,
    quiet_until_ms: u64,
    next_decision_ms: u64,
}

impl ConnController {
    pub fn new(policy: ConnPolicy, now_ms: u64) -> Self {
        let start = policy.start.min(policy.per_host_max);
        let ceiling = policy.per_host_max;
        Self {
            policy,
            current: start,
            ceiling,
            weak_gains: 0,
            settled: None,
            baseline_bps: 0.0,
            baseline_per_conn_bps: 0.0,
            quiet_until_ms: 0,
            next_decision_ms: now_ms,
        }
    }

    /// Starts from a previously learned profile, skipping the ramp.
    pub fn from_profile(
        policy: ConnPolicy,
        profile: &HostProfile,
        now_ms: u64,
        fingerprint: u64,
    ) -> Self {
        let mut controller = Self::new(policy, now_ms);
        if profile.is_usable(now_ms, fingerprint) {
            controller.current = profile.best_conn_count.clamp(1, controller.policy.per_host_max);
            controller.ceiling = profile.conn_ceiling.clamp(1, controller.policy.per_host_max);
            // Already tuned: hold here rather than re-climbing, but stay ready
            // to back off if this network behaves differently.
            controller.settled = Some(SettleReason::DiminishingReturns);
        }
        controller
    }

    pub fn connections(&self) -> u32 {
        self.current
    }

    pub fn ceiling(&self) -> u32 {
        self.ceiling
    }

    pub fn settled(&self) -> Option<SettleReason> {
        self.settled
    }

    /// Forces a single upward probe, used once a throttle's quiet period ends.
    pub fn allow_one_probe(&mut self, now_ms: u64) {
        self.settled = None;
        self.weak_gains = 0;
        self.next_decision_ms = now_ms;
    }

    /// Feeds in a measurement window and returns what to do.
    pub fn observe(
        &mut self,
        now_ms: u64,
        observation: &Observation,
        global_in_use: u32,
    ) -> ConnDecision {
        // A throttle is acted on immediately, whatever else is going on.
        //
        // Retrying the throttled request on a *fresh* connection is precisely
        // what escalates a 429 into a temporary IP ban, so the count comes down
        // and stays down for a while.
        if observation.throttled {
            // The count we were at is now known to be too many, so that
            // becomes the ceiling. The backoff is then applied to the *new*
            // ceiling rather than the old count, which is what leaves headroom
            // for the single upward probe after the quiet period. Backing off
            // from the old count instead can land exactly on the ceiling, and
            // the probe then has nowhere to go and can never fire.
            self.ceiling = self.current.saturating_sub(1).max(1);
            let reduced = ((f64::from(self.ceiling) * self.policy.throttle_backoff_frac).ceil()
                as u32)
                .max(1);
            self.current = reduced.min(self.ceiling);
            self.weak_gains = 0;
            self.settled = None;
            self.quiet_until_ms = now_ms + self.policy.throttle_quiet.as_millis() as u64;
            self.next_decision_ms = self.quiet_until_ms;
            self.baseline_bps = 0.0;
            return ConnDecision::Reduce { to: self.current, ceiling: self.ceiling };
        }

        if now_ms < self.quiet_until_ms || now_ms < self.next_decision_ms {
            return ConnDecision::Hold;
        }
        if let Some(reason) = self.settled {
            // Settled means settled; only `allow_one_probe` or a throttle
            // moves the count again.
            let _ = reason;
            return ConnDecision::Hold;
        }

        self.next_decision_ms = now_ms + self.policy.probe_interval_ms(observation.rtt);

        // Signals that mean another connection cannot possibly help.
        if observation.writer_backpressure {
            return self.settle(SettleReason::DiskBound);
        }
        if observation.interarrival_p95_ratio > self.policy.queueing_p95_multiple {
            return self.settle(SettleReason::Queueing);
        }

        // The first window is only a baseline; there is nothing to compare to.
        if self.baseline_bps <= 0.0 {
            self.baseline_bps = observation.aggregate_bps;
            self.baseline_per_conn_bps = observation.mean_per_conn_bps;
            return self.try_add(global_in_use);
        }

        let gain = (observation.aggregate_bps - self.baseline_bps) / self.baseline_bps;

        // A strong aggregate gain always justifies the addition that produced
        // it, so only a weak gain gets diagnosed. Per-connection rate falling
        // while aggregate climbs is expected, not a problem.
        if gain < self.policy.min_gain_frac {
            // Flat aggregate while the count grew, with all connections moving
            // at much the same rate, is the signature of one shared token
            // bucket. More connections buy nothing there and raise the risk of
            // the 429 escalating to a ban.
            if gain < self.policy.flat_gain_frac
                && observation.per_conn_spread_frac < self.policy.low_variance_frac
            {
                return self.settle(SettleReason::PerIpShaping);
            }

            // A severe collapse means the connections are fighting rather than
            // sharing. See `per_conn_collapse_frac` for why the bar is high.
            if self.baseline_per_conn_bps > 0.0 {
                let drop = (self.baseline_per_conn_bps - observation.mean_per_conn_bps)
                    / self.baseline_per_conn_bps;
                if drop > self.policy.per_conn_collapse_frac {
                    return self.settle(SettleReason::PerConnCollapse);
                }
            }

            self.weak_gains += 1;
            if self.weak_gains >= self.policy.weak_gains_before_settling {
                return self.settle(SettleReason::DiminishingReturns);
            }
            return ConnDecision::Hold;
        }

        self.weak_gains = 0;
        self.baseline_bps = observation.aggregate_bps;
        self.baseline_per_conn_bps = observation.mean_per_conn_bps;
        self.try_add(global_in_use)
    }

    fn try_add(&mut self, global_in_use: u32) -> ConnDecision {
        let headroom_frac = ((self.current as f64) * self.policy.max_step_frac).floor() as u32;
        let step = self.policy.step.min(headroom_frac.max(1));
        let target = (self.current + step).min(self.ceiling).min(self.policy.per_host_max);

        let global_room = self.policy.global_max.saturating_sub(global_in_use);
        let target = target.min(self.current + global_room);

        if target <= self.current {
            return self.settle(SettleReason::AtCap);
        }
        self.current = target;
        ConnDecision::Add { to: target }
    }

    fn settle(&mut self, reason: SettleReason) -> ConnDecision {
        self.settled = Some(reason);
        ConnDecision::Settle { at: self.current, reason }
    }

    /// What to remember about this host for next time.
    pub fn to_profile(
        &self,
        now_ms: u64,
        fingerprint: u64,
        multiplexing_helped: bool,
    ) -> HostProfile {
        HostProfile {
            best_conn_count: self.current,
            conn_ceiling: self.ceiling,
            multiplexing_helped,
            h3_won_race: None,
            net_fingerprint: fingerprint,
            updated_at_ms: now_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MBPS: f64 = 1_000_000.0;
    const FINGERPRINT: u64 = 0xABCD;

    /// Drives the controller forward, always past the probe interval.
    fn step(
        controller: &mut ConnController,
        clock_ms: &mut u64,
        observation: &Observation,
    ) -> ConnDecision {
        *clock_ms += 2000;
        controller.observe(*clock_ms, observation, 0)
    }

    #[test]
    fn it_starts_low_and_ramps_while_additions_pay() {
        let policy = ConnPolicy::default();
        let mut clock = 10_000;
        let mut controller = ConnController::new(policy, clock);
        assert_eq!(controller.connections(), 4, "should start conservatively");

        // Each addition roughly doubles throughput: a per-connection-shaped
        // origin, where more connections genuinely help.
        let mut rate = 10.0 * MBPS;
        let mut counts = vec![controller.connections()];
        for _ in 0..6 {
            let observation = Observation::healthy(rate, controller.connections());
            match step(&mut controller, &mut clock, &observation) {
                ConnDecision::Add { to } => counts.push(to),
                ConnDecision::Hold => {}
                other => panic!("unexpected {other:?}"),
            }
            rate *= 1.8;
        }
        assert!(controller.connections() > 4, "should have ramped up, stayed at {counts:?}");
        assert!(controller.connections() <= 16, "must respect the per-host cap");
    }

    #[test]
    fn it_settles_when_additions_stop_paying() {
        let policy = ConnPolicy::default();
        let mut clock = 0;
        let mut controller = ConnController::new(policy, clock);

        // Establish a baseline, then stop improving.
        step(&mut controller, &mut clock, &Observation::healthy(100.0 * MBPS, 4));
        let settled = loop {
            let mut observation = Observation::healthy(100.0 * MBPS, controller.connections());
            // Keep the spread high so this reads as diminishing returns rather
            // than the per-IP signature.
            observation.per_conn_spread_frac = 0.5;
            match step(&mut controller, &mut clock, &observation) {
                ConnDecision::Settle { reason, .. } => break reason,
                ConnDecision::Hold => continue,
                ConnDecision::Add { .. } => panic!("added despite no gain"),
                other => panic!("unexpected {other:?}"),
            }
        };
        assert_eq!(settled, SettleReason::DiminishingReturns);
        assert!(controller.settled().is_some());
    }

    #[test]
    fn a_flat_aggregate_with_uniform_rates_is_read_as_per_ip_shaping() {
        // The CDN signature the plan calls out: adding connections divides the
        // same bucket instead of enlarging it.
        let policy = ConnPolicy::default();
        let mut clock = 0;
        let mut controller = ConnController::new(policy, clock);

        step(&mut controller, &mut clock, &Observation::healthy(50.0 * MBPS, 4));
        let mut observation = Observation::healthy(50.0 * MBPS, controller.connections());
        observation.per_conn_spread_frac = 0.02; // all connections equal

        let decision = step(&mut controller, &mut clock, &observation);
        assert_eq!(
            decision,
            ConnDecision::Settle {
                at: controller.connections(),
                reason: SettleReason::PerIpShaping
            }
        );
    }

    #[test]
    fn writer_backpressure_settles_as_disk_bound() {
        // Worth distinguishing: the UI should say "disk-limited" rather than
        // implying the network is at fault.
        let policy = ConnPolicy::default();
        let mut clock = 0;
        let mut controller = ConnController::new(policy, clock);

        let mut observation = Observation::healthy(900.0 * MBPS, 4);
        observation.writer_backpressure = true;
        let decision = step(&mut controller, &mut clock, &observation);
        assert!(
            matches!(decision, ConnDecision::Settle { reason: SettleReason::DiskBound, .. }),
            "got {decision:?}"
        );
    }

    #[test]
    fn queueing_settles_the_ramp() {
        let policy = ConnPolicy::default();
        let mut clock = 0;
        let mut controller = ConnController::new(policy, clock);

        let mut observation = Observation::healthy(100.0 * MBPS, 4);
        observation.interarrival_p95_ratio = 5.0;
        assert!(matches!(
            step(&mut controller, &mut clock, &observation),
            ConnDecision::Settle { reason: SettleReason::Queueing, .. }
        ));
    }

    #[test]
    fn per_connection_collapse_stops_the_ramp() {
        let policy = ConnPolicy::default();
        let mut clock = 0;
        let mut controller = ConnController::new(policy, clock);

        // Baseline: 4 connections at 25 MB/s each.
        step(&mut controller, &mut clock, &Observation::healthy(100.0 * MBPS, 4));

        // Aggregate barely moved, but each connection is now far slower than
        // the 25 MB/s baseline: they are fighting, not sharing.
        let observation = Observation {
            aggregate_bps: 105.0 * MBPS,
            mean_per_conn_bps: 8.0 * MBPS,
            per_conn_spread_frac: 0.4,
            throttled: false,
            writer_backpressure: false,
            interarrival_p95_ratio: 1.0,
            rtt: Duration::from_millis(20),
        };
        assert!(matches!(
            step(&mut controller, &mut clock, &observation),
            ConnDecision::Settle { reason: SettleReason::PerConnCollapse, .. }
        ));
    }

    #[test]
    fn a_throttle_halves_the_count_and_lowers_the_ceiling() {
        let policy = ConnPolicy::default();
        let mut clock = 0;
        let mut controller = ConnController::new(policy.clone(), clock);

        // Climb a little first.
        let mut rate = 10.0 * MBPS;
        for _ in 0..3 {
            let connections = controller.connections();
            step(&mut controller, &mut clock, &Observation::healthy(rate, connections));
            rate *= 2.0;
        }
        let before = controller.connections();
        assert!(before > 4);

        let mut observation = Observation::healthy(rate, before);
        observation.throttled = true;
        let decision = controller.observe(clock + 100, &observation, 0);

        let ConnDecision::Reduce { to, ceiling } = decision else {
            panic!("expected a reduction, got {decision:?}");
        };
        assert!(to < before, "count should fall from {before}, got {to}");
        assert_eq!(ceiling, before - 1, "ceiling should sit below where it broke");
        assert!(controller.connections() <= ceiling);
    }

    #[test]
    fn after_a_throttle_it_stays_quiet_then_probes_once() {
        let policy = ConnPolicy::default();
        let mut clock = 1_000_000;
        let mut controller = ConnController::new(policy.clone(), clock);
        step(&mut controller, &mut clock, &Observation::healthy(50.0 * MBPS, 4));

        let mut observation = Observation::healthy(50.0 * MBPS, controller.connections());
        observation.throttled = true;
        controller.observe(clock, &observation, 0);

        // Within the quiet period nothing moves, however good it looks.
        let good = Observation::healthy(500.0 * MBPS, controller.connections());
        let quiet_ms = policy.throttle_quiet.as_millis() as u64;
        assert_eq!(controller.observe(clock + quiet_ms / 2, &good, 0), ConnDecision::Hold);

        // Afterwards, one deliberate probe is allowed.
        let later = clock + quiet_ms + 1;
        controller.allow_one_probe(later);
        let probe = controller.observe(later, &good, 0);
        assert!(
            matches!(probe, ConnDecision::Add { .. }),
            "the post-quiet probe must have somewhere to go, got {probe:?}"
        );
        assert!(
            controller.connections() <= controller.ceiling(),
            "a probe must not exceed the learned ceiling"
        );
    }

    #[test]
    fn the_global_cap_is_respected() {
        let policy = ConnPolicy { global_max: 5, ..ConnPolicy::default() };
        let mut clock = 0;
        let mut controller = ConnController::new(policy, clock);

        // Four already in use elsewhere plus this download's four leaves only
        // one slot, so the ramp must stop almost immediately.
        let observation = Observation::healthy(10.0 * MBPS, 4);
        clock += 2000;
        let decision = controller.observe(clock, &observation, 5);
        assert!(
            matches!(decision, ConnDecision::Settle { reason: SettleReason::AtCap, .. }),
            "got {decision:?}"
        );
    }

    #[test]
    fn a_fresh_profile_skips_the_ramp_entirely() {
        let policy = ConnPolicy::default();
        let now = 5_000_000;
        let profile = HostProfile {
            best_conn_count: 12,
            conn_ceiling: 14,
            multiplexing_helped: false,
            h3_won_race: None,
            net_fingerprint: FINGERPRINT,
            updated_at_ms: now - 1000,
        };
        let controller = ConnController::from_profile(policy, &profile, now, FINGERPRINT);
        assert_eq!(controller.connections(), 12, "should start where it left off");
        assert_eq!(controller.ceiling(), 14);
    }

    #[test]
    fn a_profile_from_another_network_is_not_trusted() {
        let policy = ConnPolicy::default();
        let now = 5_000_000;
        let profile = HostProfile {
            best_conn_count: 12,
            conn_ceiling: 14,
            multiplexing_helped: false,
            h3_won_race: None,
            net_fingerprint: FINGERPRINT,
            updated_at_ms: now - 1000,
        };
        // Same host, different network: what was learned on a fibre link says
        // nothing about a tethered phone.
        let controller = ConnController::from_profile(policy, &profile, now, 0x9999);
        assert_eq!(controller.connections(), 4, "should ignore a foreign profile");
    }

    #[test]
    fn a_stale_profile_is_not_trusted() {
        let policy = ConnPolicy::default();
        let now = PROFILE_MAX_AGE_MS * 3;
        let profile = HostProfile {
            best_conn_count: 12,
            conn_ceiling: 14,
            multiplexing_helped: false,
            h3_won_race: None,
            net_fingerprint: FINGERPRINT,
            updated_at_ms: now - PROFILE_MAX_AGE_MS - 1,
        };
        assert!(!profile.is_usable(now, FINGERPRINT));
        let controller = ConnController::from_profile(policy, &profile, now, FINGERPRINT);
        assert_eq!(controller.connections(), 4);
    }

    #[test]
    fn the_probe_interval_never_undercuts_four_round_trips() {
        let policy = ConnPolicy::default();
        // On a fast LAN the configured floor dominates.
        assert_eq!(policy.probe_interval_ms(Duration::from_millis(5)), 1500);
        // On a satellite link, four RTTs does.
        assert_eq!(policy.probe_interval_ms(Duration::from_millis(600)), 2400);
    }

    #[test]
    fn decisions_are_not_taken_faster_than_the_probe_interval() {
        let policy = ConnPolicy::default();
        let mut controller = ConnController::new(policy, 0);
        let observation = Observation::healthy(10.0 * MBPS, 4);

        // First window sets the baseline and may add.
        controller.observe(0, &observation, 0);
        // Immediately afterwards there is nothing new to learn.
        assert_eq!(controller.observe(100, &observation, 0), ConnDecision::Hold);
        assert_eq!(controller.observe(500, &observation, 0), ConnDecision::Hold);
    }

    #[test]
    fn a_learned_profile_round_trips() {
        let policy = ConnPolicy::default();
        let mut clock = 0;
        let mut controller = ConnController::new(policy.clone(), clock);
        let mut rate = 10.0 * MBPS;
        for _ in 0..3 {
            let connections = controller.connections();
            step(&mut controller, &mut clock, &Observation::healthy(rate, connections));
            rate *= 2.0;
        }

        let profile = controller.to_profile(clock, FINGERPRINT, true);
        assert_eq!(profile.best_conn_count, controller.connections());
        assert!(profile.is_usable(clock, FINGERPRINT));

        let restored = ConnController::from_profile(policy, &profile, clock, FINGERPRINT);
        assert_eq!(restored.connections(), controller.connections());
    }
}
