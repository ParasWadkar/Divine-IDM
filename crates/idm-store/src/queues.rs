//! Named queues, so a pile of downloads can be run a few at a time.

use rusqlite::{OptionalExtension, params};

use crate::{Result, Store, StoreError, jobs::StoredJob, now_ms};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Queue {
    pub id: i64,
    pub name: String,
    pub max_concurrent: u32,
    pub enabled: bool,
    pub created_at_ms: u64,
}

impl Store {
    /// Creates the queue, or updates its concurrency if it already exists.
    pub fn upsert_queue(&self, name: &str, max_concurrent: u32) -> Result<i64> {
        let name = name.trim();
        if name.is_empty() {
            return Err(StoreError::Invalid("a queue needs a name".into()));
        }
        if max_concurrent == 0 {
            // Zero would be a queue that never runs anything, which is what
            // `enabled` is for and is far clearer about saying so.
            return Err(StoreError::Invalid(
                "a queue that runs nothing should be disabled, not set to zero".into(),
            ));
        }

        let conn = self.conn();
        conn.prepare_cached(
            "INSERT INTO queues (name, max_concurrent, created_at_ms)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(name) DO UPDATE SET max_concurrent = excluded.max_concurrent",
        )?
        .execute(params![name, max_concurrent, now_ms() as i64])?;

        Ok(conn
            .query_row("SELECT id FROM queues WHERE name = ?1", params![name], |row| row.get(0))?)
    }

    pub fn queues(&self) -> Result<Vec<Queue>> {
        let conn = self.conn();
        let mut statement = conn.prepare_cached(
            "SELECT id, name, max_concurrent, enabled, created_at_ms FROM queues ORDER BY name",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok(Queue {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    max_concurrent: row.get::<_, i64>(2)? as u32,
                    enabled: row.get::<_, i64>(3)? != 0,
                    created_at_ms: row.get::<_, i64>(4)? as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn set_queue_enabled(&self, id: i64, enabled: bool) -> Result<()> {
        let conn = self.conn();
        conn.execute("UPDATE queues SET enabled = ?2 WHERE id = ?1", params![id, enabled])?;
        Ok(())
    }

    /// Removes a queue. Its jobs are kept and become unqueued rather than
    /// being deleted along with it — losing a download because a queue was
    /// tidied away would be indefensible. The foreign key does this, and the
    /// test below is what stops a later schema change from making it a
    /// cascade.
    pub fn remove_queue(&self, id: i64) -> Result<bool> {
        let conn = self.conn();
        Ok(conn.execute("DELETE FROM queues WHERE id = ?1", params![id])? > 0)
    }

    /// The next jobs a queue should start, respecting its concurrency limit.
    ///
    /// Returns nothing for a disabled queue, and counts what is already
    /// running against the limit, so a caller can poll this and act on
    /// whatever comes back.
    pub fn next_in_queue(&self, id: i64) -> Result<Vec<StoredJob>> {
        let conn = self.conn();

        let found = conn
            .query_row(
                "SELECT max_concurrent, enabled FROM queues WHERE id = ?1",
                params![id],
                |row| Ok((row.get::<_, i64>(0)? as u32, row.get::<_, i64>(1)? != 0)),
            )
            .optional()?;
        let Some((max_concurrent, enabled)) = found else {
            return Ok(Vec::new());
        };
        if !enabled {
            return Ok(Vec::new());
        }

        let running: u32 = conn.query_row(
            "SELECT COUNT(*) FROM jobs WHERE queue_id = ?1 AND state = 'running'",
            params![id],
            |row| row.get::<_, i64>(0),
        )? as u32;
        let room = max_concurrent.saturating_sub(running);
        if room == 0 {
            return Ok(Vec::new());
        }
        drop(conn);

        let conn = self.conn();
        // The same column list the row reader expects, not a copy of it: a
        // column added to one and not the other reads the wrong field, or
        // fails far from the edit that caused it.
        let mut statement = conn.prepare_cached(&format!(
            "SELECT {}
               FROM jobs
              WHERE queue_id = ?1 AND state = 'queued'
              ORDER BY position, id
              LIMIT ?2",
            crate::jobs::COLUMNS
        ))?;
        let rows = statement
            .query_map(params![id, room], crate::jobs::read)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Puts a job in a queue, at the end, or removes it from one with `None`.
    pub fn assign_to_queue(&self, job_id: i64, queue_id: Option<i64>) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE jobs
                SET queue_id = ?2,
                    position = COALESCE((SELECT MAX(position) + 1 FROM jobs WHERE queue_id IS ?2), 0),
                    updated_at_ms = ?3
              WHERE id = ?1",
            params![job_id, queue_id, now_ms() as i64],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::{JobState, NewJob};

    fn a_job() -> NewJob {
        NewJob {
            url: "https://example.invalid/a.bin".into(),
            filename: "a.bin".into(),
            output_dir: "D:/downloads".into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_queue_round_trips() {
        let store = Store::in_memory().unwrap();
        let id = store.upsert_queue("overnight", 2).unwrap();
        let queues = store.queues().unwrap();
        assert_eq!(queues.len(), 1);
        assert_eq!(queues[0].id, id);
        assert_eq!(queues[0].max_concurrent, 2);
        assert!(queues[0].enabled);
    }

    #[test]
    fn naming_the_same_queue_again_updates_it() {
        let store = Store::in_memory().unwrap();
        let first = store.upsert_queue("overnight", 2).unwrap();
        let second = store.upsert_queue("overnight", 5).unwrap();
        assert_eq!(first, second);
        assert_eq!(store.queues().unwrap()[0].max_concurrent, 5);
    }

    #[test]
    fn a_nameless_or_zero_width_queue_is_refused() {
        let store = Store::in_memory().unwrap();
        assert!(store.upsert_queue("   ", 1).is_err());
        assert!(store.upsert_queue("overnight", 0).is_err());
    }

    #[test]
    fn a_queue_hands_out_no_more_than_its_limit() {
        let store = Store::in_memory().unwrap();
        let queue = store.upsert_queue("overnight", 2).unwrap();
        let ids: Vec<i64> = (0..4)
            .map(|_| {
                let id = store.add_job(&a_job()).unwrap();
                store.assign_to_queue(id, Some(queue)).unwrap();
                id
            })
            .collect();

        let first = store.next_in_queue(queue).unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].id, ids[0], "the queue should be in order");

        store.set_job_state(ids[0], JobState::Running, None).unwrap();
        assert_eq!(store.next_in_queue(queue).unwrap().len(), 1, "a running job takes a slot");

        store.set_job_state(ids[1], JobState::Running, None).unwrap();
        assert!(store.next_in_queue(queue).unwrap().is_empty(), "full");

        store.set_job_state(ids[0], JobState::Completed, None).unwrap();
        assert_eq!(store.next_in_queue(queue).unwrap().len(), 1, "finishing frees a slot");
    }

    #[test]
    fn a_disabled_queue_starts_nothing() {
        let store = Store::in_memory().unwrap();
        let queue = store.upsert_queue("overnight", 2).unwrap();
        let job = store.add_job(&a_job()).unwrap();
        store.assign_to_queue(job, Some(queue)).unwrap();

        store.set_queue_enabled(queue, false).unwrap();
        assert!(store.next_in_queue(queue).unwrap().is_empty());

        store.set_queue_enabled(queue, true).unwrap();
        assert_eq!(store.next_in_queue(queue).unwrap().len(), 1);
    }

    #[test]
    fn deleting_a_queue_keeps_its_downloads() {
        // Tidying a queue away must never take someone's downloads with it.
        let store = Store::in_memory().unwrap();
        let queue = store.upsert_queue("overnight", 2).unwrap();
        let job = store.add_job(&a_job()).unwrap();
        store.assign_to_queue(job, Some(queue)).unwrap();

        assert!(store.remove_queue(queue).unwrap());

        let survivor = store.job(job).unwrap().expect("the job should outlive its queue");
        assert_eq!(survivor.queue_id, None);
    }

    #[test]
    fn an_unknown_queue_simply_has_nothing_to_start() {
        let store = Store::in_memory().unwrap();
        assert!(store.next_in_queue(404).unwrap().is_empty());
    }
}
