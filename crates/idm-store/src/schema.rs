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
    // v2 — whether a job was authorised by cookies.
    //
    // Cookies are never stored, so a job that needed them cannot simply be
    // restarted after a reboot: it would fail with a 403 the user has no way
    // to account for. Recording that it needed them lets the engine leave it
    // alone and say why, instead of resuming it into a failure.
    r#"
    ALTER TABLE jobs ADD COLUMN needs_browser_auth INTEGER NOT NULL DEFAULT 0;
    "#,
    // v3 — what a failure was, beside what it said.
    //
    // `error` held a sentence, and a sentence can only be re-read. Deciding
    // whether a row was a full disk or a dead link meant matching English
    // prose, which is why the interface ended up asking `error == "cancelled"`
    // to find out whether the user had cancelled something.
    //
    // Both columns are nullable with no default: a row that predates them has
    // no classification and never will, and a default would claim otherwise.
    r#"
    ALTER TABLE jobs ADD COLUMN error_kind TEXT;
    ALTER TABLE jobs ADD COLUMN error_summary TEXT;
    "#,
    // v4 — the other addresses a job may be fetched from.
    //
    // Newline-separated rather than JSON, because a URL cannot contain a
    // raw newline and this crate has no JSON of its own to pull in for one
    // column. Nullable with no default: a row written before this existed
    // has no mirrors, which is exactly what absent means.
    //
    // Kept at all so that pausing a multi-source download and resuming it
    // does not quietly turn it into a single-source one -- a degradation
    // nothing would report and only a throughput graph would show.
    r#"
    ALTER TABLE jobs ADD COLUMN mirrors TEXT;
    "#,
    // v5 -- slowing downloads down, or stopping them, at certain times.
    //
    // A table rather than a column, and so the first migration since v1 to
    // add one: a schedule is a list of its own length, and nothing about a
    // job points at it.
    //
    // `days` is a bitmask with Monday in bit 0, which is one small integer
    // where seven booleans or a comma-separated string would be seven
    // columns or a parser. `start_minute` and `end_minute` are minutes past
    // midnight; an end *before* a start is how a window that runs past
    // midnight is spelled, so they are stored exactly as given and never
    // tidied into order.
    //
    // `stop` is its own column rather than a reserved value of
    // `bytes_per_sec`, because zero already means something: no limit at
    // all, which is how a rule lifts the engine-wide limit for a few hours.
    r#"
    CREATE TABLE bandwidth_rules (
        id            INTEGER PRIMARY KEY,
        days          INTEGER NOT NULL,
        start_minute  INTEGER NOT NULL,
        end_minute    INTEGER NOT NULL,
        bytes_per_sec INTEGER NOT NULL DEFAULT 0,
        stop          INTEGER NOT NULL DEFAULT 0,
        created_at_ms INTEGER NOT NULL
    ) STRICT;
    "#,
    // v6 -- which transport a host answered on, so HTTP/3 is not re-raced
    // against every host on every download.
    //
    // Its own table rather than a column on `host_profiles`, and the reason
    // is the shape of the fact rather than tidiness. A host profile is keyed
    // by `(host, port, alpn)` because the right connection count differs
    // sharply between protocols -- sixteen HTTP/1.1 connections are sixteen
    // congestion windows, sixteen h2 streams are one. "Does this host speak
    // HTTP/3" is not a fact about a protocol, it is the fact that *chooses*
    // the protocol, so keying it by the answer would be circular: the
    // lookup has to happen before anything has been negotiated.
    //
    // It also must not be gated on the same conditions. A host profile is
    // only written once the connection ramp has settled, because an
    // unsettled count was never measured and would mislead the next
    // download. A race result is valid the moment it finishes, settled or
    // not, and tying the two together meant every download short enough not
    // to settle -- which is most of them -- threw the answer away.
    //
    // `h3_won_race` is dropped from `host_profiles` in the same step rather
    // than left behind, so there is one home for this and not two.
    r#"
    CREATE TABLE host_transport (
        host            TEXT NOT NULL,
        port            INTEGER NOT NULL,
        net_fingerprint INTEGER NOT NULL,
        h3_won          INTEGER NOT NULL,
        updated_at_ms   INTEGER NOT NULL,
        PRIMARY KEY (host, port, net_fingerprint)
    ) STRICT;
    ALTER TABLE host_profiles DROP COLUMN h3_won_race;
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
    fn an_older_database_is_upgraded_without_losing_its_rows() {
        // The point of migrations, and the first time this one is exercised
        // against data rather than an empty file.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.sqlite3");

        let id = {
            let store = Store::open(&path).unwrap();
            let conn = store.conn();
            // Wind the database back to v1, then write a row the way v1
            // would have: the point is that an upgrade preserves data that
            // predates the columns it adds. Every column added since has to
            // come off, or replaying that migration hits a duplicate.
            for column in ["needs_browser_auth", "error_kind", "error_summary", "mirrors"] {
                conn.execute(&format!("ALTER TABLE jobs DROP COLUMN {column}"), []).unwrap();
            }
            // Tables added since v1 have to go as well, for the same
            // reason: replaying the migration that creates one would find
            // it already there.
            conn.execute("DROP TABLE bandwidth_rules", []).unwrap();
            conn.execute("DROP TABLE host_transport", []).unwrap();
            // And a column a later migration *removes* has to come back,
            // or replaying that migration drops something absent.
            conn.execute("ALTER TABLE host_profiles ADD COLUMN h3_won_race INTEGER", []).unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
            conn.execute(
                "INSERT INTO jobs (url, filename, output_dir, state, created_at_ms, updated_at_ms)
                 VALUES ('https://example.invalid/a.bin', 'a.bin', 'D:/downloads', 'queued', 1, 1)",
                [],
            )
            .unwrap();
            conn.last_insert_rowid()
        };

        let upgraded = Store::open(&path).unwrap();
        assert_eq!(version(&upgraded.conn()).unwrap(), LATEST);

        let job = upgraded.job(id).unwrap().expect("the row should survive the upgrade");
        assert_eq!(job.filename, "a.bin");
        assert!(!job.needs_browser_auth, "the new column should take its default");
        // Nullable with no default, on purpose: a row written before the
        // taxonomy existed has no classification, and inventing one would
        // claim the old failure had been understood.
        assert!(job.error_kind.is_none(), "an old row must not claim a classification");
        assert!(job.error_summary.is_none());
        // Same reasoning: absent and "no mirrors" are the same thing here,
        // and neither is one blank address.
        assert!(job.mirrors.is_empty(), "an old row has no mirrors");
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
