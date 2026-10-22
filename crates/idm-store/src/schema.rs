//! Schema and forward-only migrations.
//!
//! Versioning rides on SQLite's own `user_version` pragma rather than a table
//! of applied migrations: there is exactly one writer, upgrades are strictly
//! forward, and the pragma is updated inside the same transaction as the
//! statements it describes, so a crash mid-upgrade leaves the version and the
//! schema agreeing with each other.
//!
//! Tables are `STRICT`. Without it SQLite will happily keep the string
//! `"lots"` in an INTEGER column, and the place that would surface is a
//! `get::<u64>` failing much later with nothing to say about how it got there.

use rusqlite::Connection;

use crate::{Result, StoreError};

/// Each entry is one version step, applied in order.
const MIGRATIONS: &[&str] = &[
    // v1 — host profiles, jobs, queues.
    r#"
    CREATE TABLE queues (
        id             INTEGER PRIMARY KEY,
        name           TEXT    NOT NULL UNIQUE,
        max_concurrent INTEGER NOT NULL DEFAULT 1,
        enabled        INTEGER NOT NULL DEFAULT 1,
        created_at_ms  INTEGER NOT NULL
    ) STRICT;

    CREATE TABLE jobs (
        id              INTEGER PRIMARY KEY,
        url             TEXT    NOT NULL,
        filename        TEXT    NOT NULL,
        output_dir      TEXT    NOT NULL,
        state           TEXT    NOT NULL,
        downloaded      INTEGER NOT NULL DEFAULT 0,
        total           INTEGER,
        error           TEXT,
        category        TEXT,
        referrer        TEXT,
        user_agent      TEXT,
        mime            TEXT,
        queue_id        INTEGER REFERENCES queues(id) ON DELETE SET NULL,
        position        INTEGER NOT NULL DEFAULT 0,
        created_at_ms   INTEGER NOT NULL,
        updated_at_ms   INTEGER NOT NULL,
        finished_at_ms  INTEGER
    ) STRICT;

    CREATE INDEX jobs_by_state ON jobs(state);
    CREATE INDEX jobs_in_queue ON jobs(queue_id, position);
    CREATE INDEX jobs_by_finished ON jobs(finished_at_ms DESC);

    -- The fingerprint is part of the key, not just a column to check: a
    -- profile learned on one network must not be overwritten by what a
    -- different network taught about the same host, or moving between the
    -- two would retrain from scratch every time.
    CREATE TABLE host_profiles (
        host                TEXT    NOT NULL,
        port                INTEGER NOT NULL,
        alpn                TEXT    NOT NULL,
        net_fingerprint     INTEGER NOT NULL,
        best_conn_count     INTEGER NOT NULL,
        conn_ceiling        INTEGER NOT NULL,
        multiplexing_helped INTEGER NOT NULL,
        h3_won_race         INTEGER,
        updated_at_ms       INTEGER NOT NULL,
        PRIMARY KEY (host, port, alpn, net_fingerprint)
    ) STRICT;
    "#,
];

pub const LATEST: u32 = MIGRATIONS.len() as u32;

pub fn version(connection: &Connection) -> Result<u32> {
    Ok(connection.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))? as u32)
}

pub fn migrate(connection: &Connection) -> Result<()> {
    let found = version(connection)?;
    if found > LATEST {
        // Running v1's CREATE TABLE over a v2 database would either fail
        // noisily or, worse, half-apply. Refusing keeps the user's history
        // intact and tells them what happened.
        return Err(StoreError::FromTheFuture { found, known: LATEST });
    }

    for (index, statements) in MIGRATIONS.iter().enumerate().skip(found as usize) {
        let target = index as u32 + 1;
        let transaction = connection.unchecked_transaction()?;
        transaction.execute_batch(statements)?;
        // Inside the transaction, so the version can never describe a schema
        // that was not applied.
        transaction.pragma_update(None, "user_version", target)?;
        transaction.commit()?;
        tracing::debug!(version = target, "applied store migration");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;

    #[test]
    fn a_fresh_database_lands_on_the_latest_version() {
        let store = Store::in_memory().unwrap();
        assert_eq!(version(&store.conn()).unwrap(), LATEST);
    }

    #[test]
    fn every_migration_is_applied_exactly_once() {
        let store = Store::in_memory().unwrap();
        // A second pass must be a no-op rather than re-running CREATE TABLE.
        migrate(&store.conn()).unwrap();
        migrate(&store.conn()).unwrap();
        assert_eq!(version(&store.conn()).unwrap(), LATEST);
    }

    #[test]
    fn tables_are_strict_so_a_wrong_type_fails_where_it_happens() {
        let store = Store::in_memory().unwrap();
        let conn = store.conn();
        conn.execute(
            "INSERT INTO queues (name, max_concurrent, created_at_ms) VALUES ('q', 1, 0)",
            [],
        )
        .unwrap();
        let wrong =
            conn.execute("UPDATE queues SET max_concurrent = 'plenty' WHERE name = 'q'", []);
        assert!(wrong.is_err(), "a STRICT table should refuse a string in an INTEGER column");
    }
}
