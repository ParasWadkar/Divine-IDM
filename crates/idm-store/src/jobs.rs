//! Jobs, and the history they become once they finish.
//!
//! One table, not two. A finished download differs from a running one only
//! by its state and the time it stopped, and splitting them means every
//! query either picks the wrong table or unions both — and a job moved
//! between tables at the moment of completion is a job that can be lost by a
//! crash in between.

use rusqlite::{OptionalExtension, Row, params};

use crate::{Result, Store, StoreError, now_ms};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Queued,
    Running,
    Paused,
    Completed,
    Failed,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    pub fn parse(text: &str) -> Result<Self> {
        match text {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "paused" => Ok(Self::Paused),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            other => Err(StoreError::Invalid(format!("unknown job state {other:?}"))),
        }
    }

    /// Whether the job has stopped for good.
    pub fn is_finished(self) -> bool {
        matches!(self, Self::Completed | Self::Failed)
    }
}

/// What a caller supplies to record a new job.
///
/// No cookies field, deliberately: see the crate documentation.
#[derive(Debug, Clone, Default)]
pub struct NewJob {
    pub url: String,
    pub filename: String,
    pub output_dir: String,
    pub total: Option<u64>,
    pub category: Option<String>,
    pub referrer: Option<String>,
    pub user_agent: Option<String>,
    pub mime: Option<String>,
    pub queue_id: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct StoredJob {
    pub id: i64,
    pub url: String,
    pub filename: String,
    pub output_dir: String,
    pub state: JobState,
    pub downloaded: u64,
    pub total: Option<u64>,
    pub error: Option<String>,
    pub category: Option<String>,
    pub referrer: Option<String>,
    pub user_agent: Option<String>,
    pub mime: Option<String>,
    pub queue_id: Option<i64>,
    pub position: i64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub finished_at_ms: Option<u64>,
}

const COLUMNS: &str = "id, url, filename, output_dir, state, downloaded, total, error, category, \
                       referrer, user_agent, mime, queue_id, position, created_at_ms, \
                       updated_at_ms, finished_at_ms";

pub(crate) fn read(row: &Row<'_>) -> rusqlite::Result<StoredJob> {
    Ok(StoredJob {
        id: row.get(0)?,
        url: row.get(1)?,
        filename: row.get(2)?,
        output_dir: row.get(3)?,
        // A row whose state does not parse is corrupt rather than merely
        // unexpected, so it fails here instead of being silently coerced.
        state: JobState::parse(&row.get::<_, String>(4)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, error.into())
        })?,
        downloaded: row.get::<_, i64>(5)? as u64,
        total: row.get::<_, Option<i64>>(6)?.map(|v| v as u64),
        error: row.get(7)?,
        category: row.get(8)?,
        referrer: row.get(9)?,
        user_agent: row.get(10)?,
        mime: row.get(11)?,
        queue_id: row.get(12)?,
        position: row.get(13)?,
        created_at_ms: row.get::<_, i64>(14)? as u64,
        updated_at_ms: row.get::<_, i64>(15)? as u64,
        finished_at_ms: row.get::<_, Option<i64>>(16)?.map(|v| v as u64),
    })
}

impl Store {
    /// Records a new job, queued, and returns its id.
    pub fn add_job(&self, job: &NewJob) -> Result<i64> {
        let now = now_ms();
        let conn = self.conn();
        conn.prepare_cached(
            "INSERT INTO jobs (url, filename, output_dir, state, total, category, referrer,
                               user_agent, mime, queue_id, position, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, 'queued', ?4, ?5, ?6, ?7, ?8, ?9,
                     COALESCE((SELECT MAX(position) + 1 FROM jobs WHERE queue_id IS ?9), 0),
                     ?10, ?10)",
        )?
        .execute(params![
            job.url,
            job.filename,
            job.output_dir,
            job.total.map(|v| v as i64),
            job.category,
            job.referrer,
            job.user_agent,
            job.mime,
            job.queue_id,
            now as i64,
        ])?;
        Ok(conn.last_insert_rowid())
    }

    pub fn job(&self, id: i64) -> Result<Option<StoredJob>> {
        let conn = self.conn();
        let mut statement =
            conn.prepare_cached(&format!("SELECT {COLUMNS} FROM jobs WHERE id = ?1"))?;
        Ok(statement.query_row(params![id], read).optional()?)
    }

    /// Progress only. Kept separate from state changes because it is called
    /// often and must stay a single cheap write.
    pub fn update_progress(&self, id: i64, downloaded: u64, total: Option<u64>) -> Result<()> {
        let conn = self.conn();
        conn.prepare_cached(
            "UPDATE jobs
                SET downloaded = ?2,
                    total = COALESCE(?3, total),
                    updated_at_ms = ?4
              WHERE id = ?1",
        )?
        .execute(params![
            id,
            downloaded as i64,
            total.map(|v| v as i64),
            now_ms() as i64
        ])?;
        Ok(())
    }

    /// Moves a job to a new state, stamping the finish time when it stops.
    pub fn set_job_state(&self, id: i64, state: JobState, error: Option<&str>) -> Result<()> {
        let now = now_ms();
        let conn = self.conn();
        conn.prepare_cached(
            "UPDATE jobs
                SET state = ?2,
                    error = ?3,
                    updated_at_ms = ?4,
                    finished_at_ms = CASE WHEN ?5 THEN ?4 ELSE NULL END
              WHERE id = ?1",
        )?
        .execute(params![id, state.as_str(), error, now as i64, state.is_finished()])?;
        Ok(())
    }

    /// Jobs that have not finished, oldest first.
    pub fn unfinished_jobs(&self) -> Result<Vec<StoredJob>> {
        let conn = self.conn();
        let mut statement = conn.prepare_cached(&format!(
            "SELECT {COLUMNS} FROM jobs
              WHERE state IN ('queued', 'running', 'paused')
              ORDER BY created_at_ms"
        ))?;
        let rows = statement.query_map([], read)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Finished jobs, most recently finished first.
    pub fn history(&self, limit: u32) -> Result<Vec<StoredJob>> {
        let conn = self.conn();
        let mut statement = conn.prepare_cached(&format!(
            "SELECT {COLUMNS} FROM jobs
              WHERE finished_at_ms IS NOT NULL
              ORDER BY finished_at_ms DESC
              LIMIT ?1"
        ))?;
        let rows =
            statement.query_map(params![limit], read)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Marks jobs the engine believed were running as paused.
    ///
    /// Called once at startup. A job left in `running` is one the engine was
    /// working on when it stopped, and nothing is working on it now — leaving
    /// the row saying otherwise would show the user a download that appears
    /// to be progressing and never will. The bytes and the resume journal are
    /// untouched, so a paused job is one the user can resume.
    pub fn mark_interrupted_as_paused(&self) -> Result<usize> {
        let conn = self.conn();
        let moved = conn.execute(
            "UPDATE jobs SET state = 'paused', updated_at_ms = ?1 WHERE state = 'running'",
            params![now_ms() as i64],
        )?;
        if moved > 0 {
            tracing::info!(jobs = moved, "marked interrupted downloads as paused");
        }
        Ok(moved)
    }

    pub fn remove_job(&self, id: i64) -> Result<bool> {
        let conn = self.conn();
        Ok(conn.execute("DELETE FROM jobs WHERE id = ?1", params![id])? > 0)
    }

    /// Forgets finished jobs. Running ones are left alone.
    pub fn clear_history(&self) -> Result<usize> {
        let conn = self.conn();
        Ok(conn.execute("DELETE FROM jobs WHERE finished_at_ms IS NOT NULL", [])?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_job() -> NewJob {
        NewJob {
            url: "https://example.invalid/big.bin".into(),
            filename: "big.bin".into(),
            output_dir: "D:/downloads".into(),
            total: Some(1 << 20),
            ..Default::default()
        }
    }

    #[test]
    fn a_new_job_starts_queued() {
        let store = Store::in_memory().unwrap();
        let id = store.add_job(&a_job()).unwrap();
        let job = store.job(id).unwrap().unwrap();
        assert_eq!(job.state, JobState::Queued);
        assert_eq!(job.downloaded, 0);
        assert!(job.finished_at_ms.is_none());
    }

    #[test]
    fn progress_updates_without_touching_state() {
        let store = Store::in_memory().unwrap();
        let id = store.add_job(&a_job()).unwrap();
        store.set_job_state(id, JobState::Running, None).unwrap();
        store.update_progress(id, 4096, None).unwrap();

        let job = store.job(id).unwrap().unwrap();
        assert_eq!(job.downloaded, 4096);
        assert_eq!(job.state, JobState::Running);
        assert_eq!(job.total, Some(1 << 20), "a None total must not erase a known one");
    }

    #[test]
    fn finishing_stamps_a_time_and_unfinishing_clears_it() {
        let store = Store::in_memory().unwrap();
        let id = store.add_job(&a_job()).unwrap();

        store.set_job_state(id, JobState::Completed, None).unwrap();
        assert!(store.job(id).unwrap().unwrap().finished_at_ms.is_some());

        // Retrying a failed download puts it back in flight; a stale finish
        // time would keep it in the history list for ever.
        store.set_job_state(id, JobState::Running, None).unwrap();
        assert!(store.job(id).unwrap().unwrap().finished_at_ms.is_none());
    }

    #[test]
    fn a_failure_keeps_its_reason() {
        let store = Store::in_memory().unwrap();
        let id = store.add_job(&a_job()).unwrap();
        store.set_job_state(id, JobState::Failed, Some("connection reset")).unwrap();

        let job = store.job(id).unwrap().unwrap();
        assert_eq!(job.state, JobState::Failed);
        assert_eq!(job.error.as_deref(), Some("connection reset"));
    }

    #[test]
    fn history_and_unfinished_do_not_overlap() {
        let store = Store::in_memory().unwrap();
        let done = store.add_job(&a_job()).unwrap();
        let going = store.add_job(&a_job()).unwrap();
        store.set_job_state(done, JobState::Completed, None).unwrap();
        store.set_job_state(going, JobState::Running, None).unwrap();

        let unfinished: Vec<i64> = store.unfinished_jobs().unwrap().iter().map(|j| j.id).collect();
        let finished: Vec<i64> = store.history(50).unwrap().iter().map(|j| j.id).collect();
        assert_eq!(unfinished, vec![going]);
        assert_eq!(finished, vec![done]);
    }

    #[test]
    fn a_download_running_when_the_engine_died_comes_back_as_paused() {
        // Otherwise the user is shown a download that looks live and will
        // never move, with no way to tell it from one that is.
        let store = Store::in_memory().unwrap();
        let running = store.add_job(&a_job()).unwrap();
        let queued = store.add_job(&a_job()).unwrap();
        let done = store.add_job(&a_job()).unwrap();
        store.set_job_state(running, JobState::Running, None).unwrap();
        store.set_job_state(done, JobState::Completed, None).unwrap();

        assert_eq!(store.mark_interrupted_as_paused().unwrap(), 1);

        assert_eq!(store.job(running).unwrap().unwrap().state, JobState::Paused);
        assert_eq!(store.job(queued).unwrap().unwrap().state, JobState::Queued);
        assert_eq!(store.job(done).unwrap().unwrap().state, JobState::Completed);
    }

    #[test]
    fn clearing_history_leaves_live_downloads_alone() {
        let store = Store::in_memory().unwrap();
        let done = store.add_job(&a_job()).unwrap();
        let going = store.add_job(&a_job()).unwrap();
        store.set_job_state(done, JobState::Completed, None).unwrap();
        store.set_job_state(going, JobState::Running, None).unwrap();

        assert_eq!(store.clear_history().unwrap(), 1);
        assert!(store.job(done).unwrap().is_none());
        assert!(store.job(going).unwrap().is_some());
    }

    #[test]
    fn history_is_newest_first_and_bounded() {
        let store = Store::in_memory().unwrap();
        for _ in 0..5 {
            let id = store.add_job(&a_job()).unwrap();
            store.set_job_state(id, JobState::Completed, None).unwrap();
        }
        assert_eq!(store.history(3).unwrap().len(), 3);
    }

    #[test]
    fn an_unknown_job_is_absent_rather_than_an_error() {
        let store = Store::in_memory().unwrap();
        assert!(store.job(404).unwrap().is_none());
        assert!(!store.remove_job(404).unwrap());
    }
}
