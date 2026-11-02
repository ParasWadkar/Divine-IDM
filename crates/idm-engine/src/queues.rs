//! Named queues: running a pile of downloads a few at a time.
//!
//! The settings already hold one global limit on how many downloads run at
//! once. A queue is the same idea made plural and nameable, which is what
//! makes it useful: "overnight" can hold forty large files and release two at
//! a time while "now" runs four, and switching "overnight" off in the evening
//! is one click rather than forty.
//!
//! The scheduling rules live here, apart from the engine, because they are
//! the part worth testing on their own. `serve.rs` owns the job registry and
//! the locks; this owns the question of which waiting download may begin.

use std::collections::HashMap;

/// Longer than this is a paragraph, not a name.
pub const MAX_NAME: usize = 64;

/// The most a single queue may run at once.
///
/// The same ceiling the global limit uses, for the same reason: a queue set
/// to five hundred is not a faster queue, it is five hundred fans of
/// connections and a rate limit.
pub const MOST_AT_ONCE: u32 = 16;

/// What the scheduler needs to know about one queue.
///
/// Deliberately not the store's `Queue` and not the protocol's `QueueView`.
/// The store's carries a creation time nothing here cares about; the
/// protocol's carries live counts the store cannot know. Mapping between
/// three small types costs a few lines once and keeps each layer honest
/// about what it actually holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Queue {
    pub id: i64,
    pub name: String,
    pub max_concurrent: u32,
    pub enabled: bool,
}

/// A queue name fit to store, or the reason it is not.
pub fn vet_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("a queue needs a name".into());
    }
    if name.chars().count() > MAX_NAME {
        return Err(format!("a queue name is at most {MAX_NAME} characters"));
    }
    // A newline or a tab in a name would be invisible in the window and
    // would make two queues look identical while being different rows.
    if name.chars().any(char::is_control) {
        return Err("a queue name cannot contain control characters".into());
    }
    Ok(name.to_owned())
}

/// A concurrency limit fit to store, or the reason it is not.
pub fn vet_limit(max_concurrent: u32) -> Result<u32, String> {
    if max_concurrent == 0 {
        // The store refuses this too. Said in the same words here so the
        // window shows one message whichever layer catches it.
        return Err("a queue that runs nothing should be switched off, not set to zero".into());
    }
    if max_concurrent > MOST_AT_ONCE {
        return Err(format!("{MOST_AT_ONCE} at once is the most a queue may run"));
    }
    Ok(max_concurrent)
}

/// Whether one more of a queue's downloads may start.
///
/// `None` is both "this download is in no queue" and "its queue has gone
/// away", and both start freely. That second case is the important one: a
/// download must never become unstartable because of bookkeeping, so an id
/// pointing at nothing is read as no queue rather than as a queue with no
/// room. The global limit still applies either way.
pub fn may_start(queue: Option<&Queue>, busy: u32) -> bool {
    match queue {
        None => true,
        Some(queue) => queue.enabled && busy < queue.max_concurrent,
    }
}

/// The queue with this id, if it is still there.
pub fn find(queues: &[Queue], id: i64) -> Option<&Queue> {
    queues.iter().find(|queue| queue.id == id)
}

/// The first waiting download that may start, skipping any whose queue is
/// full or switched off.
///
/// Skipping rather than stopping at the first blocked one is the whole
/// point. A paused queue must not hold up a download that is in no queue at
/// all, or in a queue with room — otherwise switching one queue off would
/// look like a frozen app. `waiting` is in the order the downloads were
/// asked for, and that order is preserved among those that can start.
pub fn next_to_start<T: Copy>(
    waiting: &[(T, Option<i64>)],
    queues: &[Queue],
    busy: &HashMap<i64, u32>,
) -> Option<T> {
    waiting
        .iter()
        .find(|(_, queue)| {
            let found = queue.and_then(|id| find(queues, id));
            let used = queue.and_then(|id| busy.get(&id).copied()).unwrap_or(0);
            may_start(found, used)
        })
        .map(|(job, _)| *job)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue(id: i64, max_concurrent: u32, enabled: bool) -> Queue {
        Queue { id, name: format!("q{id}"), max_concurrent, enabled }
    }

    #[test]
    fn a_name_is_trimmed_and_an_empty_one_refused() {
        assert_eq!(vet_name("  overnight  ").unwrap(), "overnight");
        assert!(vet_name("   ").is_err());
        assert!(vet_name("").is_err());
    }

    #[test]
    fn a_name_that_would_not_show_truthfully_is_refused() {
        assert!(vet_name("over\nnight").is_err(), "a newline would be invisible in the window");
        assert!(vet_name(&"x".repeat(MAX_NAME + 1)).is_err());
        assert!(vet_name(&"x".repeat(MAX_NAME)).is_ok(), "the longest allowed name is allowed");
    }

    #[test]
    fn a_limit_of_zero_is_refused_and_says_what_to_do_instead() {
        let error = vet_limit(0).unwrap_err();
        assert!(error.contains("switched off"), "{error}");
        assert!(vet_limit(1).is_ok());
    }

    #[test]
    fn a_limit_past_the_ceiling_is_refused() {
        assert!(vet_limit(MOST_AT_ONCE).is_ok());
        let error = vet_limit(MOST_AT_ONCE + 1).unwrap_err();
        assert!(error.contains(&MOST_AT_ONCE.to_string()), "the ceiling should be said: {error}");
    }

    #[test]
    fn a_download_in_no_queue_always_starts() {
        assert!(may_start(None, 0));
        assert!(may_start(None, 99), "the global limit is what governs this one");
    }

    #[test]
    fn a_queue_stops_at_its_limit() {
        let q = queue(1, 2, true);
        assert!(may_start(Some(&q), 0));
        assert!(may_start(Some(&q), 1));
        assert!(!may_start(Some(&q), 2));
    }

    #[test]
    fn a_switched_off_queue_starts_nothing() {
        assert!(!may_start(Some(&queue(1, 4, false)), 0));
    }

    #[test]
    fn a_full_queue_does_not_hold_up_the_rest() {
        // The head-of-line case. Without skipping, switching one queue off
        // would stop every other download in the app.
        let queues = vec![queue(1, 1, true), queue(2, 4, true)];
        let busy = HashMap::from([(1, 1)]);
        let waiting = [(10u64, Some(1)), (11, Some(2)), (12, None)];

        assert_eq!(next_to_start(&waiting, &queues, &busy), Some(11));
    }

    #[test]
    fn order_is_kept_among_those_that_can_start() {
        let queues = vec![queue(1, 4, true)];
        let waiting = [(10u64, Some(1)), (11, None)];
        assert_eq!(next_to_start(&waiting, &queues, &HashMap::new()), Some(10));
    }

    #[test]
    fn nothing_starts_when_every_waiting_download_is_held() {
        let queues = vec![queue(1, 1, true), queue(2, 2, false)];
        let busy = HashMap::from([(1, 1)]);
        let waiting = [(10u64, Some(1)), (11, Some(2))];
        assert_eq!(next_to_start(&waiting, &queues, &busy), None);
    }

    #[test]
    fn a_download_whose_queue_has_gone_away_still_starts() {
        // Deleting a queue keeps its downloads, and a download left pointing
        // at a row that no longer exists must not be stuck forever.
        let waiting = [(10u64, Some(404))];
        assert_eq!(next_to_start(&waiting, &[], &HashMap::new()), Some(10));
    }

    #[test]
    fn an_empty_wait_list_starts_nothing() {
        let empty: [(u64, Option<i64>); 0] = [];
        assert_eq!(next_to_start(&empty, &[queue(1, 4, true)], &HashMap::new()), None);
    }
}
