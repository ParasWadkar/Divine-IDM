//! The rules that slow downloads down, or stop them, at certain times.
//!
//! Storage only. What a rule *means* — which of two overlapping ones wins,
//! what happens at midnight — lives in `idm-engine`'s `schedule` module,
//! where it can be tested without a database. This half just keeps them.

use rusqlite::params;

use crate::{Result, Store, StoreError, now_ms};

/// One stored rule.
///
/// Deliberately not the engine's `Rule`: this one carries a creation time
/// nothing scheduling cares about, and keeps "stop" and "bytes per second"
/// as the two columns they are rather than the one enum the engine wants.
/// Mapping between them costs a few lines once and keeps each layer honest
/// about what it actually holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BandwidthRule {
    pub id: i64,
    /// Which days, as a bitmask with Monday in bit 0.
    pub days: u8,
    /// Minutes past midnight, inclusive.
    pub start_minute: u16,
    /// Minutes past midnight, exclusive; 1440 is midnight at the end.
    pub end_minute: u16,
    /// Bytes per second, where zero means no limit.
    pub bytes_per_sec: u64,
    /// When true nothing downloads, whatever `bytes_per_sec` says.
    pub stop: bool,
    pub created_at_ms: u64,
}

/// Midnight at the end of the day, the largest `end_minute` may be.
const END_OF_DAY: u16 = 24 * 60;

impl Store {
    /// Every rule, oldest first.
    ///
    /// A stable order rather than a meaningful one: rules do not take
    /// precedence by position — the engine decides that from how
    /// restrictive each is — so the only thing an order has to do here is
    /// not reshuffle itself between two readings of the same list.
    pub fn bandwidth_rules(&self) -> Result<Vec<BandwidthRule>> {
        let conn = self.conn();
        let mut statement = conn.prepare_cached(
            "SELECT id, days, start_minute, end_minute, bytes_per_sec, stop, created_at_ms
             FROM bandwidth_rules ORDER BY created_at_ms, id",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok(BandwidthRule {
                    id: row.get(0)?,
                    days: row.get::<_, i64>(1)? as u8,
                    start_minute: row.get::<_, i64>(2)? as u16,
                    end_minute: row.get::<_, i64>(3)? as u16,
                    bytes_per_sec: row.get::<_, i64>(4)? as u64,
                    stop: row.get::<_, i64>(5)? != 0,
                    created_at_ms: row.get::<_, i64>(6)? as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Adds a rule, or replaces the one with this id.
    ///
    /// `id` of `None` adds. Editing by id rather than by a natural key
    /// because a rule has none: two rules may legitimately name the same
    /// days and the same hours and differ only in what they do, and
    /// treating that as a conflict would make the second one unsaveable.
    pub fn upsert_bandwidth_rule(
        &self,
        id: Option<i64>,
        days: u8,
        start_minute: u16,
        end_minute: u16,
        bytes_per_sec: u64,
        stop: bool,
    ) -> Result<i64> {
        // Checked here as well as in the engine, worded the same way, so
        // whichever layer catches it says the same thing.
        if days == 0 || days & !0b0111_1111 != 0 {
            return Err(StoreError::Invalid("a rule needs at least one day to apply on".into()));
        }
        if start_minute >= END_OF_DAY || end_minute > END_OF_DAY {
            return Err(StoreError::Invalid("a rule has to start and end within a day".into()));
        }
        if start_minute == end_minute {
            return Err(StoreError::Invalid("a rule needs a start and an end that differ".into()));
        }

        let conn = self.conn();
        match id {
            Some(id) => {
                let changed = conn.execute(
                    "UPDATE bandwidth_rules
                     SET days = ?2, start_minute = ?3, end_minute = ?4,
                         bytes_per_sec = ?5, stop = ?6
                     WHERE id = ?1",
                    params![
                        id,
                        i64::from(days),
                        i64::from(start_minute),
                        i64::from(end_minute),
                        bytes_per_sec as i64,
                        stop,
                    ],
                )?;
                if changed == 0 {
                    // Editing a rule somebody else removed. Refused rather
                    // than silently added back, which would resurrect a
                    // rule the user had deliberately deleted.
                    return Err(StoreError::Invalid("that rule is no longer there".into()));
                }
                Ok(id)
            }
            None => {
                conn.execute(
                    "INSERT INTO bandwidth_rules
                       (days, start_minute, end_minute, bytes_per_sec, stop, created_at_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        i64::from(days),
                        i64::from(start_minute),
                        i64::from(end_minute),
                        bytes_per_sec as i64,
                        stop,
                        now_ms() as i64,
                    ],
                )?;
                Ok(conn.last_insert_rowid())
            }
        }
    }

    /// Removes one. `false` means there was nothing to remove.
    pub fn remove_bandwidth_rule(&self, id: i64) -> Result<bool> {
        let conn = self.conn();
        Ok(conn.execute("DELETE FROM bandwidth_rules WHERE id = ?1", params![id])? > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state.sqlite3")).unwrap();
        (dir, store)
    }

    #[test]
    fn a_rule_survives_being_written_and_read_back() {
        let (_dir, store) = store();
        let id = store
            .upsert_bandwidth_rule(None, 0b0001_1111, 9 * 60, 17 * 60, 500_000, false)
            .unwrap();

        let rules = store.bandwidth_rules().unwrap();
        assert_eq!(rules.len(), 1);
        let rule = &rules[0];
        assert_eq!(rule.id, id);
        assert_eq!(rule.days, 0b0001_1111);
        assert_eq!(rule.start_minute, 9 * 60);
        assert_eq!(rule.end_minute, 17 * 60);
        assert_eq!(rule.bytes_per_sec, 500_000);
        assert!(!rule.stop);
    }

    #[test]
    fn an_overnight_rule_keeps_its_ends_the_way_round_they_were_given() {
        // The engine reads an end before a start as "runs past midnight",
        // so the store must not tidy them into order.
        let (_dir, store) = store();
        store.upsert_bandwidth_rule(None, 1 << 4, 23 * 60, 2 * 60, 0, false).unwrap();
        let rule = store.bandwidth_rules().unwrap().remove(0);
        assert_eq!(rule.start_minute, 23 * 60);
        assert_eq!(rule.end_minute, 2 * 60);
    }

    #[test]
    fn editing_a_rule_changes_it_rather_than_adding_another() {
        let (_dir, store) = store();
        let id = store.upsert_bandwidth_rule(None, 1, 9 * 60, 17 * 60, 500_000, false).unwrap();
        let same = store.upsert_bandwidth_rule(Some(id), 1, 10 * 60, 18 * 60, 0, true).unwrap();

        assert_eq!(same, id);
        let rules = store.bandwidth_rules().unwrap();
        assert_eq!(rules.len(), 1, "{rules:?}");
        assert_eq!(rules[0].start_minute, 10 * 60);
        assert!(rules[0].stop);
    }

    #[test]
    fn two_rules_that_look_alike_are_both_kept() {
        // They differ in what they do, which is a legitimate pair. Treating
        // the days and hours as a natural key would make the second one
        // overwrite the first.
        let (_dir, store) = store();
        store.upsert_bandwidth_rule(None, 1, 9 * 60, 17 * 60, 500_000, false).unwrap();
        store.upsert_bandwidth_rule(None, 1, 9 * 60, 17 * 60, 0, true).unwrap();
        assert_eq!(store.bandwidth_rules().unwrap().len(), 2);
    }

    #[test]
    fn editing_a_rule_that_is_gone_is_refused_rather_than_added_back() {
        let (_dir, store) = store();
        let error =
            store.upsert_bandwidth_rule(Some(404), 1, 9 * 60, 17 * 60, 0, false).unwrap_err();
        assert!(error.to_string().contains("no longer there"), "{error}");
        assert!(store.bandwidth_rules().unwrap().is_empty(), "nothing should have been added");
    }

    #[test]
    fn removing_says_whether_there_was_anything_to_remove() {
        let (_dir, store) = store();
        let id = store.upsert_bandwidth_rule(None, 1, 9 * 60, 17 * 60, 0, false).unwrap();
        assert!(store.remove_bandwidth_rule(id).unwrap());
        assert!(!store.remove_bandwidth_rule(id).unwrap(), "the second time there is nothing");
        assert!(store.bandwidth_rules().unwrap().is_empty());
    }

    #[test]
    fn a_rule_that_could_never_apply_is_refused() {
        let (_dir, store) = store();
        for (days, start, end) in [
            (0, 9 * 60, 17 * 60),           // no days
            (0b1000_0000, 9 * 60, 17 * 60), // an eighth day
            (1, 24 * 60, 17 * 60),          // starting at midnight's far side
            (1, 9 * 60, 24 * 60 + 1),       // ending after midnight
            (1, 9 * 60, 9 * 60),            // no length
        ] {
            assert!(
                store.upsert_bandwidth_rule(None, days, start, end, 0, false).is_err(),
                "days {days:#b} {start}..{end} should be refused",
            );
        }
        assert!(store.bandwidth_rules().unwrap().is_empty());
    }

    #[test]
    fn the_order_does_not_change_between_two_readings() {
        // Rules do not take precedence by position, so the only thing the
        // order has to do is stay still.
        let (_dir, store) = store();
        for hour in [9, 13, 7, 22] {
            store.upsert_bandwidth_rule(None, 1, hour * 60, (hour + 1) * 60, 0, false).unwrap();
        }
        let first: Vec<i64> = store.bandwidth_rules().unwrap().iter().map(|r| r.id).collect();
        let again: Vec<i64> = store.bandwidth_rules().unwrap().iter().map(|r| r.id).collect();
        assert_eq!(first, again);
    }
}
