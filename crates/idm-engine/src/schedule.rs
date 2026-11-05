//! Slowing downloads down, or stopping them, at certain times of day.
//!
//! The rules live here, apart from the engine, because they are the part
//! worth testing on their own: no store, no async, and above all no clock.
//! Every decision is a function of a time that was handed in, so a week's
//! worth of boundaries can be checked in a millisecond without waiting for
//! any of them to arrive.
//!
//! # What a rule beats
//!
//! A rule in force wins over the engine-wide speed limit rather than being
//! combined with it. That is the more useful reading of the two: it makes
//! "one megabyte a second normally, full speed overnight" something a
//! person can express, where taking the lower of the pair would silently
//! make it impossible. Outside every window the engine-wide limit applies
//! unchanged.
//!
//! # What beats a rule
//!
//! Another rule, if it is more restrictive. Somebody who writes "slow down
//! during work" and "slow down while the backup runs" means both of them,
//! so where two windows overlap the tighter one is the answer — stopping
//! beats a low limit beats no limit. Honouring the looser one would throw
//! away a rule they had taken the trouble to write.

/// Minutes in a day, and in a week.
const DAY: u32 = 24 * 60;
const WEEK: u32 = 7 * DAY;

/// A local date and time, reduced to the two things a rule cares about.
///
/// Handed in rather than read, which is what makes every function below
/// testable. `weekday` is 0 for Monday, matching the bit order in
/// [`Rule::days`] — Monday rather than Sunday because a rule people write
/// is nearly always about a working week.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    pub weekday: u8,
    pub minute_of_day: u16,
}

impl LocalTime {
    pub fn new(weekday: u8, minute_of_day: u16) -> Self {
        Self { weekday: weekday % 7, minute_of_day }
    }

    /// Where this falls in the week, in minutes from Monday midnight.
    fn week_minute(self) -> u32 {
        u32::from(self.weekday) * DAY + u32::from(self.minute_of_day)
    }
}

/// What a rule does while it is in force.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Nothing downloads. Running downloads go back into the queue and wait.
    Stop,
    /// Bytes per second, where zero means no limit at all.
    ///
    /// Zero is not a missing value: a rule that lifts the engine-wide limit
    /// for a few hours is the main reason this feature is worth having, and
    /// it is spelled `Limit(0)`.
    Limit(u64),
}

impl Action {
    /// How restrictive this is, for picking between overlapping rules.
    ///
    /// Ordered rather than compared ad hoc, so "most restrictive wins" is
    /// one sentence of code rather than a nest of branches that could
    /// disagree with itself.
    fn severity(self) -> (u8, u64) {
        match self {
            Self::Stop => (2, 0),
            // No limit is the least restrictive thing there is, and must
            // not be read as "a limit of zero bytes a second".
            Self::Limit(0) => (0, 0),
            // Among real limits, lower is tighter. Inverted so a plain
            // comparison puts the tightest first.
            Self::Limit(bytes) => (1, u64::MAX - bytes),
        }
    }
}

/// One rule: when it applies, and what it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rule {
    pub id: i64,
    /// Which days, as a bitmask with Monday in bit 0.
    pub days: u8,
    /// Minutes past midnight, inclusive.
    pub start_minute: u16,
    /// Minutes past midnight, exclusive; 1440 means the end of the day.
    ///
    /// When this is *less* than `start_minute` the window runs past
    /// midnight, and [`Rule::days`] is read as the days it **starts** on.
    /// So "every Friday, 23:00 to 02:00" is one rule rather than two, and
    /// the Saturday small hours it covers are not a Saturday rule.
    pub end_minute: u16,
    pub action: Action,
}

/// The largest value `end_minute` may take: midnight at the end of the day.
pub const END_OF_DAY: u16 = DAY as u16;

impl Rule {
    /// Whether this window runs past midnight.
    fn wraps(&self) -> bool {
        self.end_minute < self.start_minute
    }

    fn on(&self, weekday: u8) -> bool {
        self.days & (1 << (weekday % 7)) != 0
    }

    /// Whether this rule covers the given moment.
    fn covers(&self, now: LocalTime) -> bool {
        let minute = now.minute_of_day;
        if self.wraps() {
            // Two halves: the evening of a day the rule names, and the
            // small hours of the day after it.
            let yesterday = (now.weekday + 6) % 7;
            (self.on(now.weekday) && minute >= self.start_minute)
                || (self.on(yesterday) && minute < self.end_minute)
        } else {
            self.on(now.weekday) && minute >= self.start_minute && minute < self.end_minute
        }
    }

    /// Every moment in the week at which this rule starts or stops applying.
    ///
    /// Both edges, because the engine has to wake for either: one puts a
    /// limit on and the other takes it off.
    fn boundaries(&self) -> Vec<u32> {
        let mut out = Vec::new();
        for day in 0..7u8 {
            if !self.on(day) {
                continue;
            }
            let start = u32::from(day) * DAY + u32::from(self.start_minute);
            // A window that wraps ends on the following day, and one that
            // ends at 1440 ends at midnight, which is the same arithmetic.
            let end = u32::from(day) * DAY
                + u32::from(self.end_minute)
                + if self.wraps() { DAY } else { 0 };
            out.push(start % WEEK);
            out.push(end % WEEK);
        }
        out
    }
}

/// The action in force at `now`, or `None` when no rule covers it.
///
/// `None` means the engine-wide speed limit applies unchanged, which is
/// what happens for anyone who has written no rules at all.
pub fn in_force(now: LocalTime, rules: &[Rule]) -> Option<Action> {
    rules
        .iter()
        .filter(|rule| rule.covers(now))
        .map(|rule| rule.action)
        .max_by_key(|action| action.severity())
}

/// How many minutes until the answer could change.
///
/// Minutes from now rather than a moment to wake at, deliberately. Local
/// time is not a steady line: it jumps when the clocks go forward, when a
/// laptop wakes from sleep with a stale clock, and when somebody corrects
/// it by hand. A duration derived from a fresh reading survives all three,
/// where a stored instant quietly describes a moment that no longer means
/// what it did.
///
/// Never zero: a boundary exactly now has just been applied, so the next
/// one worth waking for is the one after it.
pub fn next_change(now: LocalTime, rules: &[Rule]) -> Option<u16> {
    let here = now.week_minute();
    rules
        .iter()
        .flat_map(Rule::boundaries)
        .map(|at| {
            let ahead = (at + WEEK - here) % WEEK;
            if ahead == 0 { WEEK } else { ahead }
        })
        .min()
        .map(|minutes| minutes.min(u32::from(u16::MAX)) as u16)
}

/// The most days a rule may name, and the widest window it may cover.
///
/// Checked here as well as in the store, worded the same way, so whichever
/// layer catches it says the same thing.
pub fn vet(days: u8, start_minute: u16, end_minute: u16) -> Result<(), String> {
    if days == 0 {
        return Err("a rule needs at least one day to apply on".into());
    }
    if days & !0b0111_1111 != 0 {
        return Err("a rule can only name the seven days of the week".into());
    }
    if start_minute >= END_OF_DAY {
        return Err("a rule has to start before midnight".into());
    }
    if end_minute > END_OF_DAY {
        return Err("a rule cannot end after midnight".into());
    }
    if start_minute == end_minute {
        // Otherwise the window is either empty or the whole week, and which
        // of those somebody meant is not something to guess at.
        return Err("a rule needs a start and an end that differ".into());
    }
    Ok(())
}

/// Writes a minute-of-day the way somebody would read it back.
pub fn clock(minute: u16) -> String {
    format!("{:02}:{:02}", minute / 60, minute % 60)
}

/// The machine's own local time, as the rules want it.
///
/// Windows' answer rather than one computed here, so it agrees with the
/// clock in the corner of the screen — including when the clocks go
/// forward, which is a question nobody should have to answer twice. That
/// also keeps a date library out of the tree for one call, which is the
/// same choice `pac.rs` makes about proxy scripts.
///
/// Nothing above this reads it directly: every decision takes a
/// [`LocalTime`] as an argument, so the rules are testable without a clock
/// and without waiting for a boundary to come round.
pub fn now() -> LocalTime {
    platform::now()
}

#[cfg(windows)]
mod platform {
    use super::LocalTime;

    pub(super) fn now() -> LocalTime {
        // SAFETY: no arguments and no failure mode; it fills and returns a
        // SYSTEMTIME by value.
        let time = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
        LocalTime::new(
            // Windows counts from Sunday and the rules count from Monday,
            // because a rule somebody writes is nearly always about a
            // working week. Getting this backwards would move every rule
            // by a day, which is the kind of mistake that looks like the
            // feature simply not working.
            ((time.wDayOfWeek + 6) % 7) as u8,
            time.wHour * 60 + time.wMinute,
        )
    }
}

#[cfg(not(windows))]
mod platform {
    use super::LocalTime;

    pub(super) fn now() -> LocalTime {
        // Monday midnight: no rule written for a working week will fire,
        // which is the quiet direction for a platform this does not ship on.
        LocalTime::new(0, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Monday is 0, so this reads like the bitmask it builds.
    const MON: u8 = 1 << 0;
    const TUE: u8 = 1 << 1;
    const FRI: u8 = 1 << 4;
    const SAT: u8 = 1 << 5;
    const SUN: u8 = 1 << 6;

    fn at(hour: u16, minute: u16) -> u16 {
        hour * 60 + minute
    }

    fn rule(days: u8, start: u16, end: u16, action: Action) -> Rule {
        Rule { id: 1, days, start_minute: start, end_minute: end, action }
    }

    #[test]
    fn a_plain_window_applies_inside_itself_and_nowhere_else() {
        let work = rule(MON, at(9, 0), at(17, 0), Action::Limit(500_000));
        let rules = [work];

        assert_eq!(in_force(LocalTime::new(0, at(8, 59)), &rules), None, "before");
        assert_eq!(
            in_force(LocalTime::new(0, at(9, 0)), &rules),
            Some(Action::Limit(500_000)),
            "the start is inside",
        );
        assert_eq!(in_force(LocalTime::new(0, at(16, 59)), &rules), Some(Action::Limit(500_000)));
        // The end is exclusive, or two windows that meet would both apply
        // for one minute.
        assert_eq!(in_force(LocalTime::new(0, at(17, 0)), &rules), None, "the end is outside");
        assert_eq!(in_force(LocalTime::new(1, at(12, 0)), &rules), None, "a day it does not name");
    }

    #[test]
    fn a_window_that_runs_past_midnight_is_one_rule_not_two() {
        // Friday 23:00 to 02:00. The small hours it covers are Saturday's,
        // but it is a Friday rule -- which is what somebody means when they
        // write it, and why the mask is read as the days it starts on.
        let overnight = rule(FRI, at(23, 0), at(2, 0), Action::Limit(0));
        let rules = [overnight];

        assert_eq!(in_force(LocalTime::new(4, at(22, 59)), &rules), None, "before it starts");
        assert_eq!(in_force(LocalTime::new(4, at(23, 30)), &rules), Some(Action::Limit(0)));
        assert_eq!(
            in_force(LocalTime::new(5, at(1, 30)), &rules),
            Some(Action::Limit(0)),
            "the small hours of the day after",
        );
        assert_eq!(in_force(LocalTime::new(5, at(2, 0)), &rules), None, "and it ends there");
        // Saturday evening is not covered: the rule names Friday.
        assert_eq!(in_force(LocalTime::new(5, at(23, 30)), &rules), None);
    }

    #[test]
    fn a_whole_day_is_midnight_to_midnight() {
        let all_day = rule(SAT | SUN, 0, END_OF_DAY, Action::Limit(0));
        let rules = [all_day];
        for minute in [0, at(12, 0), END_OF_DAY - 1] {
            assert_eq!(in_force(LocalTime::new(5, minute), &rules), Some(Action::Limit(0)));
        }
        assert_eq!(in_force(LocalTime::new(4, at(12, 0)), &rules), None, "Friday is not named");
    }

    #[test]
    fn where_two_rules_overlap_the_tighter_one_wins() {
        // Both were written on purpose, so honouring the looser one would
        // throw one of them away.
        let work = rule(MON, at(9, 0), at(17, 0), Action::Limit(1_000_000));
        let backup = rule(MON, at(12, 0), at(13, 0), Action::Limit(100_000));
        let rules = [work, backup];

        assert_eq!(in_force(LocalTime::new(0, at(10, 0)), &rules), Some(Action::Limit(1_000_000)));
        assert_eq!(
            in_force(LocalTime::new(0, at(12, 30)), &rules),
            Some(Action::Limit(100_000)),
            "the tighter limit during the overlap",
        );
        // And the order they are written in must not decide it.
        let swapped = [backup, work];
        assert_eq!(in_force(LocalTime::new(0, at(12, 30)), &swapped), Some(Action::Limit(100_000)));
    }

    #[test]
    fn stopping_beats_any_limit_and_no_limit_loses_to_everything() {
        let open = rule(MON, 0, END_OF_DAY, Action::Limit(0));
        let slow = rule(MON, at(9, 0), at(17, 0), Action::Limit(500_000));
        let shut = rule(MON, at(12, 0), at(13, 0), Action::Stop);

        assert_eq!(
            in_force(LocalTime::new(0, at(3, 0)), &[open, slow, shut]),
            Some(Action::Limit(0))
        );
        assert_eq!(
            in_force(LocalTime::new(0, at(10, 0)), &[open, slow, shut]),
            Some(Action::Limit(500_000)),
            "a real limit beats no limit",
        );
        assert_eq!(
            in_force(LocalTime::new(0, at(12, 30)), &[open, slow, shut]),
            Some(Action::Stop),
            "and stopping beats both",
        );
    }

    #[test]
    fn no_rules_means_no_answer_rather_than_no_limit() {
        // The difference matters: `None` leaves the engine-wide limit
        // alone, where `Limit(0)` would lift it.
        assert_eq!(in_force(LocalTime::new(0, at(12, 0)), &[]), None);
        assert_eq!(next_change(LocalTime::new(0, at(12, 0)), &[]), None);
    }

    #[test]
    fn the_next_change_is_the_nearer_edge() {
        let work = rule(MON, at(9, 0), at(17, 0), Action::Limit(500_000));
        let rules = [work];

        // Before it starts: the start.
        assert_eq!(next_change(LocalTime::new(0, at(8, 0)), &rules), Some(60));
        // Inside it: the end.
        assert_eq!(next_change(LocalTime::new(0, at(16, 30)), &rules), Some(30));
    }

    #[test]
    fn a_boundary_exactly_now_is_not_the_next_one() {
        // It has just been applied. Returning zero would spin the waking
        // task as fast as the scheduler would let it.
        let work = rule(MON, at(9, 0), at(17, 0), Action::Limit(500_000));
        let ahead = next_change(LocalTime::new(0, at(9, 0)), &[work]).unwrap();
        assert_eq!(ahead, 8 * 60, "the end of the window, not this instant");
        assert!(ahead > 0);
    }

    #[test]
    fn the_next_change_can_be_in_a_later_day() {
        let friday = rule(FRI, at(23, 0), at(2, 0), Action::Limit(0));
        // Monday noon: the next edge is Friday at 23:00.
        let from_monday = next_change(LocalTime::new(0, at(12, 0)), &[friday]).unwrap();
        assert_eq!(u32::from(from_monday), 4 * DAY + 11 * 60);
    }

    #[test]
    fn the_next_change_wraps_around_the_end_of_the_week() {
        // Sunday evening looking forward to a Monday morning rule: the
        // answer has to come back round rather than being a week away.
        let monday = rule(MON, at(9, 0), at(17, 0), Action::Limit(1));
        let ahead = next_change(LocalTime::new(6, at(22, 0)), &[monday]).unwrap();
        assert_eq!(u32::from(ahead), 11 * 60, "two hours to midnight, then nine");
    }

    #[test]
    fn an_overnight_rules_end_is_counted_on_the_following_day() {
        let overnight = rule(FRI, at(23, 0), at(2, 0), Action::Limit(0));
        // Saturday 01:00, inside the wrapped half: one hour to the end.
        assert_eq!(next_change(LocalTime::new(5, at(1, 0)), &[overnight]), Some(60));
    }

    #[test]
    fn every_boundary_of_a_many_day_rule_is_found() {
        let weekdays = rule(MON | TUE, at(9, 0), at(17, 0), Action::Limit(1));
        // Monday 17:00 is an end; the next is Tuesday 09:00.
        assert_eq!(next_change(LocalTime::new(0, at(17, 0)), &[weekdays]), Some(16 * 60));
    }

    #[test]
    fn a_rule_with_no_days_or_no_length_is_refused_in_words() {
        assert!(vet(0, at(9, 0), at(17, 0)).unwrap_err().contains("at least one day"));
        assert!(vet(0b1000_0000, at(9, 0), at(17, 0)).unwrap_err().contains("seven days"));
        assert!(vet(MON, END_OF_DAY, at(17, 0)).unwrap_err().contains("before midnight"));
        assert!(vet(MON, at(9, 0), END_OF_DAY + 1).unwrap_err().contains("after midnight"));
        // Equal start and end is either an empty window or a whole week,
        // and guessing which was meant is worse than asking.
        assert!(vet(MON, at(9, 0), at(9, 0)).unwrap_err().contains("differ"));
    }

    #[test]
    fn the_rules_that_people_actually_write_are_accepted() {
        assert!(vet(MON | TUE | FRI, at(9, 0), at(17, 0)).is_ok(), "a working week");
        assert!(vet(FRI, at(23, 0), at(2, 0)).is_ok(), "overnight");
        assert!(vet(SAT | SUN, 0, END_OF_DAY).is_ok(), "all weekend");
    }

    #[test]
    fn a_time_is_written_the_way_it_is_read() {
        assert_eq!(clock(0), "00:00");
        assert_eq!(clock(at(9, 5)), "09:05");
        assert_eq!(clock(at(23, 59)), "23:59");
        assert_eq!(clock(END_OF_DAY), "24:00");
    }

    #[cfg(windows)]
    #[test]
    fn the_clock_reads_back_something_a_rule_could_match() {
        // Not what the time is -- that changes between runs -- but that it
        // is in range, which is what a rule needs to be comparable against.
        let now = now();
        assert!(now.weekday < 7, "{now:?}");
        assert!(now.minute_of_day < END_OF_DAY, "{now:?}");
    }

    #[cfg(windows)]
    #[test]
    fn the_clock_agrees_with_windows_about_which_day_it_is() {
        // The conversion from Sunday-first to Monday-first, checked against
        // the reading it came from rather than against itself. An error
        // here moves every rule by a day.
        let raw = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
        let now = now();
        let expected = (raw.wDayOfWeek + 6) % 7;
        // Re-read, so a tick between the two readings is the only thing
        // that could differ -- and across midnight even that is allowed.
        assert!(
            now.weekday == expected as u8 || now.minute_of_day <= 1,
            "{now:?} against wDayOfWeek {}",
            raw.wDayOfWeek,
        );
    }

    #[test]
    fn a_weekday_past_sunday_wraps_rather_than_panicking() {
        // The clock hands this in, and a reading that is somehow out of
        // range should not take the engine down.
        assert_eq!(LocalTime::new(9, 0).weekday, 2);
    }
}
