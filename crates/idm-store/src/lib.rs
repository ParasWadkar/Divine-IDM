//! Durable state for the engine: host profiles, jobs, history and queues.
//!
//! Everything here is state the engine is allowed to forget without losing a
//! download. The bytes on disk and the resume journal beside them are the
//! real record of progress; this database records what the *user* would
//! otherwise have to remember — what was queued, what finished, and what this
//! machine has learned about a server.
//!
//! # What is deliberately not stored
//!
//! **Cookies.** A download handed over by a browser carries the session
//! cookies that authorised it, and [`DownloadRequest`] documents them as a
//! secret that is never logged and never echoed back. A column for them here
//! would quietly turn a request-scoped secret into one that outlives the
//! session on disk, in a file with none of the protection a credential store
//! gives. There is no such column, and a test enforces it. The cost is honest
//! and small: a job resumed after a restart has no cookies, so a
//! cookie-gated URL has to be handed over by the browser again.
//!
//! # Threading
//!
//! Methods are synchronous and serialised behind one connection. Writes are
//! rare — a profile when a download settles, a job row when its state changes
//! — and sub-millisecond under WAL, so callers on an async runtime can call
//! them directly. Anything that might loop over many rows belongs in
//! `spawn_blocking`.
//!
//! [`DownloadRequest`]: https://example.invalid

use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

mod jobs;
mod profiles;
mod queues;
mod schema;

pub use idm_core::sched::HostKey;
pub use jobs::{JobState, NewJob, StoredJob};
pub use queues::Queue;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error(
        "the database is at schema version {found}, newer than this build understands ({known})"
    )]
    FromTheFuture { found: u32, known: u32 },
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// Wall-clock milliseconds. Used for staleness, never for ordering within a
/// download, where the scheduler's own monotonic clock is what matters.
pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// `Debug` prints nothing about the contents: a store holds a user's
/// download history, and the obvious derive would put it in any log line
/// that formatted a struct containing one.
pub struct Store {
    connection: Mutex<Connection>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Store")
    }
}

impl Store {
    /// Opens the database at `path`, creating and migrating it as needed.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        Self::from_connection(Connection::open(path)?)
    }

    /// An in-memory database, for tests.
    pub fn in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(connection: Connection) -> Result<Self> {
        // WAL so a reader never blocks the writer; NORMAL is the matching
        // durability setting, since WAL only risks the last transaction on
        // power loss and nothing here is worth an fsync per write. The
        // download's own journal is where durability is actually earned.
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.pragma_update(None, "foreign_keys", true)?;
        // SQLite's own default is 1000 pages, and it folds the log back into
        // the database when the last connection closes. Neither happens here:
        // this database is written to a few times per download, so the log
        // takes weeks to reach 1000 pages, and every way out of the app is
        // `process::exit`, which closes nothing. So the log grows and
        // `state.sqlite3` keeps whatever it held when it was last folded back
        // -- which is what anything reading the file alone sees, a backup or
        // a copy included. A quarter of a megabyte is the most it may drift.
        connection.pragma_update(None, "wal_autocheckpoint", 64)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;

        let store = Self { connection: Mutex::new(connection) };
        schema::migrate(&store.conn())?;
        Ok(store)
    }

    /// A poisoned lock means a previous caller panicked mid-query. SQLite has
    /// already rolled back any open transaction, so the connection is still
    /// usable and recovering it is better than making every later call fail.
    pub(crate) fn conn(&self) -> MutexGuard<'_, Connection> {
        self.connection.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_twice_keeps_the_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite3");

        {
            let store = Store::open(&path).unwrap();
            store.upsert_queue("overnight", 2).unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.queues().unwrap().len(), 1);
    }

    #[test]
    fn the_database_file_keeps_up_with_what_has_been_written_to_it() {
        // The write-ahead log is folded back into the database on a schedule
        // and when the last connection closes. This app never closes one --
        // every way out is `process::exit` -- so the schedule is the only
        // thing keeping `state.sqlite3` current. Left at SQLite's default of
        // 1000 pages it is reached about once a month here, and until then
        // the file on its own holds whatever it held weeks ago. That file on
        // its own is what a backup copies and what a reader that does not
        // attach the log sees.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite3");

        let store = Store::open(&path).unwrap();
        for index in 0..100 {
            store
                .add_job(&NewJob {
                    url: format!("https://example.invalid/{index}.bin"),
                    filename: format!("{index}.bin"),
                    output_dir: "D:/downloads".into(),
                    ..Default::default()
                })
                .unwrap();
        }

        // Copied the way a backup would take it: the database, without the
        // log beside it. The store is still open, so nothing has been folded
        // back on the way out.
        let copy = dir.path().join("backup.sqlite3");
        std::fs::copy(&path, &copy).unwrap();

        let backed_up = Store::open(&copy).unwrap().unfinished_jobs().unwrap().len();
        assert!(
            backed_up >= 50,
            "the database alone should hold most of the 100 jobs written, not {backed_up}",
        );
    }

    #[test]
    fn migrating_an_already_current_database_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite3");
        for _ in 0..3 {
            Store::open(&path).unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(schema::version(&store.conn()).unwrap(), schema::LATEST);
    }

    #[test]
    fn a_database_from_a_newer_build_is_refused_rather_than_damaged() {
        // Downgrading and silently running old migrations over a newer schema
        // is how a user loses their history.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite3");
        {
            let store = Store::open(&path).unwrap();
            store.conn().pragma_update(None, "user_version", schema::LATEST + 5).unwrap();
        }
        let error = Store::open(&path).unwrap_err();
        assert!(matches!(error, StoreError::FromTheFuture { .. }), "{error}");
    }

    #[test]
    fn no_table_has_anywhere_to_put_a_cookie() {
        // Structural, not a matter of remembering: a browser hands over the
        // session cookies that authorised a download, and a column for them
        // would make a request-scoped secret durable on disk.
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        let mut statement =
            conn.prepare("SELECT name FROM sqlite_master WHERE type = 'table'").unwrap();
        let tables: Vec<String> =
            statement.query_map([], |row| row.get(0)).unwrap().map(|r| r.unwrap()).collect();
        assert!(!tables.is_empty());

        for table in tables {
            let mut columns = conn.prepare(&format!("PRAGMA table_info({table})")).unwrap();
            let names: Vec<String> =
                columns.query_map([], |row| row.get(1)).unwrap().map(|r| r.unwrap()).collect();
            for name in names {
                let lowered = name.to_ascii_lowercase();
                for forbidden in ["cookie", "password", "secret", "token", "credential"] {
                    assert!(
                        !lowered.contains(forbidden),
                        "{table}.{name} looks like somewhere a secret could be kept"
                    );
                }
            }
        }
    }
}
