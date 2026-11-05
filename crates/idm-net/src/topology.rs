//! Finding out which connection shape an origin actually likes.
//!
//! [`crate::client::lane_topology`] picks how many lanes and how many
//! streams per lane a download gets, from a table. The comment beside its
//! use in `download` has always said what that table is: *"this is
//! reasoning, not a measured win"*. The reasoning is good — against an h2
//! origin `1 x 16` is one congestion window and `16 x 1` throws away cheap
//! cancellation — but which middle shape wins depends on the origin, the
//! path and the time of day, and none of those are in the table.
//!
//! So this measures instead. Two h2 shapes, against the same origin, and
//! the faster one wins. Everything below is about not fooling ourselves
//! while doing it.
//!
//! # Why this is so cautious
//!
//! A download is not a controlled experiment. Two shapes are tried minutes
//! apart, over a link shared with whatever else the house is doing, against
//! a CDN edge that may not even be the same machine the second time. The
//! difference between 4.1 MB/s and 4.4 MB/s is not evidence of anything.
//!
//! Three rules follow, and they are the whole design:
//!
//! - **A short download is not a measurement.** Under [`ENOUGH_BYTES`] or
//!   [`ENOUGH_SECONDS`] what was measured is the server's think time and
//!   the slow start, not the shape. Those trials are discarded, not
//!   averaged in.
//! - **The sitting shape keeps its seat unless beaten clearly.** A
//!   challenger has to be [`CLEAR_WIN`] times faster. A ratio rather than
//!   a margin, because noise on a shared link scales with the rate.
//! - **Nothing is remembered past this run of the program.** A shape that
//!   lost on hotel wifi at a conference is not evidence about the same
//!   site from home a week later, and a download manager that acted as if
//!   it were would be slower than one that simply guessed. The memory
//!   lives in the process and dies with it.
//!
//! # Switched off
//!
//! Measuring is free — the byte count and the elapsed time are already
//! known at the end of every download — so it always happens. *Acting* on
//! what was measured does not, unless [`crate::client::HttpConfig`] says
//! so. Until somebody switches it on, the table's answer is the answer,
//! and this module is a record of what would have been chosen.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use crate::client::lane_topology;

/// One connection shape: how many independent lanes, how many streams on
/// each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Shape {
    pub lanes: u16,
    pub streams: usize,
}

impl Shape {
    /// How many requests may be in flight across the whole shape.
    pub fn connections(&self) -> usize {
        self.lanes as usize * self.streams
    }
}

/// How much has to arrive before a trial counts as a measurement.
///
/// Below this the number is mostly slow start: TCP and h2 both spend the
/// first few megabytes finding out how fast they are allowed to go, and
/// comparing two shapes on that stretch compares two warm-ups.
pub const ENOUGH_BYTES: u64 = 8 << 20;

/// And how long.
///
/// A large file off a local mirror can pass the byte threshold in half a
/// second, which measures the disk and the LAN rather than the shape.
pub const ENOUGH_SECONDS: f64 = 3.0;

/// How much faster a challenger has to be before it takes the seat.
///
/// A ratio, not a difference: noise on a shared link is proportional to
/// the rate, so "200 KB/s better" means something very different at
/// 500 KB/s and at 50 MB/s.
pub const CLEAR_WIN: f64 = 1.15;

/// How many origins are remembered at once.
///
/// Small on purpose. This is a scratch note about what is happening now,
/// not a profile of somebody's browsing, and an unbounded map keyed on
/// host is both a leak and a thing nobody asked to be kept.
pub const MOST_ORIGINS: usize = 64;

/// What one download achieved with one shape.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Trial {
    pub shape: Shape,
    pub bytes: u64,
    pub took: Duration,
}

impl Trial {
    /// The rate, or `None` when the trial is too small to mean anything.
    pub fn rate(&self) -> Option<f64> {
        let seconds = self.took.as_secs_f64();
        (self.bytes >= ENOUGH_BYTES && seconds >= ENOUGH_SECONDS)
            .then(|| self.bytes as f64 / seconds)
    }
}

/// The two shapes worth comparing for a given connection count.
///
/// The table's answer, and one deliberate step away from it: the same
/// number of connections spread over more lanes. That is the axis the
/// table is actually guessing at — independent congestion windows against
/// cheap stream cancellation — and the one an origin can disagree about.
///
/// Two, not every shape that divides the number. A download manager that
/// spent six downloads exploring a search space would be slower than one
/// that guessed, and would still be exploring when the user had finished.
pub fn candidates(target: u32) -> [Shape; 2] {
    let (lanes, streams) = lane_topology(target);
    let table = Shape { lanes, streams };

    // More lanes, fewer streams each, same total. Capped so a large
    // connection count cannot turn into a lane per connection, which is
    // the shape the module docs say throws multiplexing away.
    let wider_lanes = lanes.saturating_mul(2).clamp(1, 8);
    let wider = Shape {
        lanes: wider_lanes,
        streams: (table.connections()).div_ceil(wider_lanes as usize).max(1),
    };

    [table, wider]
}

/// What is known about one origin.
#[derive(Debug, Default, Clone)]
pub struct Learned {
    rates: HashMap<Shape, f64>,
}

impl Learned {
    /// The shape to use next.
    ///
    /// An untried candidate first, so the comparison gets made at all;
    /// then whichever won. `table` is the shape the table would have
    /// picked, and it is what comes back when nothing has been measured —
    /// a guess is a better starting point than an arbitrary choice.
    pub fn next(&self, target: u32) -> Shape {
        let [table, wider] = candidates(target);
        if table == wider {
            return table;
        }
        match (self.rates.get(&table), self.rates.get(&wider)) {
            // Nothing tried, or only the table tried: try the other one.
            (None, _) => table,
            (Some(_), None) => wider,
            (Some(sitting), Some(challenger)) => {
                // The table keeps its seat unless clearly beaten. See the
                // module docs: a small difference is noise, and acting on
                // noise is how a measured system ends up worse than a
                // guess.
                if challenger / sitting >= CLEAR_WIN { wider } else { table }
            }
        }
    }

    /// Records a trial, if it is one.
    ///
    /// A later trial replaces an earlier one for the same shape rather
    /// than being averaged with it, because conditions move: what this
    /// link did ten minutes ago is weaker evidence than what it did just
    /// now, and an average lets a stale figure hold a seat for ever.
    pub fn record(&mut self, trial: Trial) {
        if let Some(rate) = trial.rate() {
            self.rates.insert(trial.shape, rate);
        }
    }
}

/// What the process knows, keyed by origin.
///
/// A process-wide note rather than something threaded through the
/// download, settings and engine layers. It is genuinely per-process state
/// with no owner — every download against one host informs the next one,
/// whichever job it belongs to — and passing it down five layers for a
/// feature that is off by default would be the larger of the two sins.
#[derive(Debug, Default)]
pub struct Memory {
    origins: Mutex<HashMap<String, Learned>>,
}

impl Memory {
    /// The shape to use for this origin, when acting on what was learned
    /// is switched on.
    pub fn next(&self, origin: &str, target: u32) -> Shape {
        let origins = self.origins.lock().unwrap_or_else(|poison| poison.into_inner());
        origins.get(origin).map(|learned| learned.next(target)).unwrap_or_else(|| {
            let (lanes, streams) = lane_topology(target);
            Shape { lanes, streams }
        })
    }

    /// Notes what a shape achieved against an origin.
    pub fn record(&self, origin: &str, trial: Trial) {
        if trial.rate().is_none() {
            return;
        }
        let mut origins = self.origins.lock().unwrap_or_else(|poison| poison.into_inner());
        if origins.len() >= MOST_ORIGINS && !origins.contains_key(origin) {
            // Full, and this is a host nothing is known about. Cleared
            // rather than evicting one at random: the whole map is a
            // scratch note, and starting it again costs one download's
            // worth of guessing.
            origins.clear();
        }
        origins.entry(origin.to_owned()).or_default().record(trial);
    }

    /// What is known about one origin, for a log line or a test.
    pub fn learned(&self, origin: &str) -> Option<Learned> {
        let origins = self.origins.lock().unwrap_or_else(|poison| poison.into_inner());
        origins.get(origin).cloned()
    }
}

/// The process's memory.
pub fn memory() -> &'static Memory {
    static MEMORY: OnceLock<Memory> = OnceLock::new();
    MEMORY.get_or_init(Memory::default)
}

/// The origin a URL belongs to, as this module keys on it.
///
/// Scheme, host and port. Not the path: two files from one CDN travel the
/// same way, and keying on the path would mean never measuring the same
/// thing twice.
/// Parsed rather than split on `://`, which this did first and which got
/// the keying wrong in the one way that matters: `https://CDN.invalid` and
/// `https://cdn.invalid`, or `https://h/x` and `https://h:443/x`, are one
/// origin and would have become two keys — splitting the very memory this
/// module exists to accumulate.
pub fn origin_of(url: &str) -> Option<String> {
    let parsed = crate::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    let port = parsed.port_or_known_default()?;
    Some(format!("{}://{}:{}", parsed.scheme(), host.to_ascii_lowercase(), port))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trial(shape: Shape, bytes_per_sec: f64) -> Trial {
        Trial { shape, bytes: (bytes_per_sec * 10.0) as u64, took: Duration::from_secs_f64(10.0) }
    }

    #[test]
    fn the_two_candidates_hold_the_same_number_of_connections() {
        // The comparison is about shape, not size. Two shapes with
        // different connection counts would be measuring the count.
        for target in [1u32, 2, 4, 8, 16, 32] {
            let [table, wider] = candidates(target);
            assert!(wider.connections() >= table.connections(), "{target}: {table:?} vs {wider:?}",);
            assert!(
                wider.connections() <= table.connections() + wider.lanes as usize,
                "{target}: the rounding should not change the size: {table:?} vs {wider:?}",
            );
        }
    }

    #[test]
    fn the_wider_shape_never_becomes_a_lane_per_connection() {
        // Which the module docs call out as the shape that throws
        // multiplexing's cheap cancellation away.
        for target in [8u32, 16, 32, 64] {
            let [_, wider] = candidates(target);
            assert!(wider.lanes <= 8, "{target}: {wider:?}");
        }
    }

    #[test]
    fn a_short_download_is_not_a_measurement() {
        // Most of a small download is slow start, which is a property of
        // the protocol rather than of the shape.
        let shape = Shape { lanes: 2, streams: 4 };
        let brief = Trial { shape, bytes: 64 << 20, took: Duration::from_millis(400) };
        assert!(brief.rate().is_none(), "half a second measures the LAN");
        let small = Trial { shape, bytes: 1 << 20, took: Duration::from_secs(20) };
        assert!(small.rate().is_none(), "a megabyte is mostly warm-up");
        let real = Trial { shape, bytes: 64 << 20, took: Duration::from_secs(10) };
        assert!(real.rate().is_some());
    }

    #[test]
    fn the_untried_shape_is_tried() {
        let mut learned = Learned::default();
        let [table, wider] = candidates(8);
        assert_eq!(learned.next(8), table, "the guess goes first");
        learned.record(trial(table, 1.0e6));
        assert_eq!(learned.next(8), wider, "the other one has not been tried");
    }

    #[test]
    fn a_small_difference_does_not_move_the_seat() {
        // The rule the whole module exists for. Two downloads minutes
        // apart over a shared link differ by this much for reasons that
        // have nothing to do with the shape.
        let mut learned = Learned::default();
        let [table, wider] = candidates(8);
        learned.record(trial(table, 4.0e6));
        learned.record(trial(wider, 4.4e6));
        assert_eq!(learned.next(8), table, "10% is not evidence");
    }

    #[test]
    fn a_clear_win_does() {
        let mut learned = Learned::default();
        let [table, wider] = candidates(8);
        learned.record(trial(table, 4.0e6));
        learned.record(trial(wider, 9.0e6));
        assert_eq!(learned.next(8), wider);
    }

    #[test]
    fn a_shape_that_lost_can_win_later() {
        // Conditions move. A later trial replaces an earlier one rather
        // than being averaged with it, so a figure from a bad ten minutes
        // cannot hold a seat for ever.
        let mut learned = Learned::default();
        let [table, wider] = candidates(8);
        learned.record(trial(table, 4.0e6));
        learned.record(trial(wider, 9.0e6));
        assert_eq!(learned.next(8), wider);

        learned.record(trial(wider, 1.0e6));
        assert_eq!(learned.next(8), table, "the newer measurement is the one that counts");
    }

    #[test]
    fn nothing_measured_is_the_table_s_answer() {
        let learned = Learned::default();
        for target in [1u32, 3, 8, 24] {
            let (lanes, streams) = lane_topology(target);
            assert_eq!(learned.next(target), Shape { lanes, streams });
        }
    }

    #[test]
    fn one_origin_says_nothing_about_another() {
        let memory = Memory::default();
        let [table, wider] = candidates(8);
        memory.record("https://a.invalid", trial(table, 1.0e6));
        memory.record("https://a.invalid", trial(wider, 9.0e6));

        assert_eq!(memory.next("https://a.invalid", 8), wider);
        assert_eq!(memory.next("https://b.invalid", 8), table, "a different host, afresh");
    }

    #[test]
    fn a_trial_too_small_to_count_is_not_recorded() {
        let memory = Memory::default();
        let [table, _] = candidates(8);
        memory.record(
            "https://a.invalid",
            Trial { shape: table, bytes: 1 << 10, took: Duration::from_millis(50) },
        );
        assert!(memory.learned("https://a.invalid").is_none());
    }

    #[test]
    fn the_memory_does_not_grow_without_bound() {
        let memory = Memory::default();
        let [table, _] = candidates(8);
        for n in 0..MOST_ORIGINS + 10 {
            memory.record(&format!("https://host{n}.invalid"), trial(table, 1.0e6));
        }
        let held = memory.origins.lock().unwrap().len();
        assert!(held <= MOST_ORIGINS, "{held} origins remembered");
    }

    #[test]
    fn an_origin_is_scheme_host_and_port() {
        assert_eq!(
            origin_of("https://cdn.invalid:8443/a/b.bin?x=1").as_deref(),
            Some("https://cdn.invalid:8443"),
        );
        // Two files from one CDN travel the same way, so they are one
        // origin and one measurement.
        assert_eq!(origin_of("https://cdn.invalid/a.bin"), origin_of("https://cdn.invalid/b.bin"));
        // And the scheme matters: h2 is on the https side of this.
        assert_ne!(origin_of("https://cdn.invalid/a"), origin_of("http://cdn.invalid/a"));
        assert_eq!(origin_of("not a url"), None);
        // No host to key on, whatever the rest of it says.
        assert_eq!(origin_of("file:///C:/x"), None);
        // The two that a split on `://` got wrong, and that would have
        // quietly halved the evidence for every site written with a
        // capital or a spelled-out default port.
        assert_eq!(origin_of("https://CDN.invalid/a"), origin_of("https://cdn.invalid/a"));
        assert_eq!(origin_of("https://cdn.invalid:443/a"), origin_of("https://cdn.invalid/a"));
    }
}
