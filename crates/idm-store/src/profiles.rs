//! What this machine has learned about a server.
//!
//! The scheduler finds the right connection count by climbing to it, which
//! costs a measurable part of a short download. Remembering the answer means
//! paying that once per host instead of once per download.

use idm_core::sched::{HostKey, HostProfile, ProfileStore};
use rusqlite::{OptionalExtension, params};

use crate::{Result, Store, StoreError};

impl Store {
    /// The profile for this host on this network, if one was ever learned.
    ///
    /// Staleness is deliberately not filtered here. [`HostProfile::is_usable`]
    /// owns that decision, it needs the current clock to make it, and a
    /// caller that wants to display what is known should see it regardless.
    pub fn host_profile(&self, key: &HostKey, net_fingerprint: u64) -> Result<Option<HostProfile>> {
        let conn = self.conn();
        let mut statement = conn.prepare_cached(
            "SELECT best_conn_count, conn_ceiling, multiplexing_helped, h3_won_race, updated_at_ms
               FROM host_profiles
              WHERE host = ?1 AND port = ?2 AND alpn = ?3 AND net_fingerprint = ?4",
        )?;

        let profile = statement
            .query_row(params![key.host, key.port, key.alpn, net_fingerprint as i64], |row| {
                Ok(HostProfile {
                    best_conn_count: row.get::<_, i64>(0)? as u32,
                    conn_ceiling: row.get::<_, i64>(1)? as u32,
                    multiplexing_helped: row.get::<_, i64>(2)? != 0,
                    h3_won_race: row.get::<_, Option<i64>>(3)?.map(|v| v != 0),
                    net_fingerprint,
                    updated_at_ms: row.get::<_, i64>(4)? as u64,
                })
            })
            .optional()?;
        Ok(profile)
    }

    /// Records what a finished download learned.
    pub fn save_host_profile(&self, key: &HostKey, profile: &HostProfile) -> Result<()> {
        if profile.best_conn_count == 0 {
            // Nothing was learned; writing it would later be read back as a
            // tuned answer of zero connections.
            return Err(StoreError::Invalid(
                "a profile with no connection count is not worth keeping".into(),
            ));
        }

        let conn = self.conn();
        conn.prepare_cached(
            "INSERT INTO host_profiles
                 (host, port, alpn, net_fingerprint, best_conn_count, conn_ceiling,
                  multiplexing_helped, h3_won_race, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(host, port, alpn, net_fingerprint) DO UPDATE SET
                 best_conn_count     = excluded.best_conn_count,
                 conn_ceiling        = excluded.conn_ceiling,
                 multiplexing_helped = excluded.multiplexing_helped,
                 -- A run that never raced h3 knows nothing about it, and must
                 -- not erase the answer from a run that did.
                 h3_won_race         = COALESCE(excluded.h3_won_race, h3_won_race),
                 updated_at_ms       = excluded.updated_at_ms",
        )?
        .execute(params![
            key.host,
            key.port,
            key.alpn,
            profile.net_fingerprint as i64,
            profile.best_conn_count as i64,
            profile.conn_ceiling as i64,
            profile.multiplexing_helped as i64,
            profile.h3_won_race.map(|won| won as i64),
            profile.updated_at_ms as i64,
        ])?;
        Ok(())
    }

    /// Drops profiles not updated since `before_ms`.
    pub fn forget_host_profiles_older_than(&self, before_ms: u64) -> Result<usize> {
        let conn = self.conn();
        let removed = conn.execute(
            "DELETE FROM host_profiles WHERE updated_at_ms < ?1",
            params![before_ms as i64],
        )?;
        Ok(removed)
    }

    pub fn host_profile_count(&self) -> Result<u64> {
        let conn = self.conn();
        Ok(conn.query_row("SELECT COUNT(*) FROM host_profiles", [], |row| row.get::<_, i64>(0))?
            as u64)
    }
}

/// Persistence must never be why a download behaves differently.
///
/// A locked database, a full disk or a corrupt row costs at most a ramp that
/// has to be climbed again, so every failure here is logged and swallowed
/// rather than propagated into the scheduler.
impl ProfileStore for Store {
    fn load(&self, key: &HostKey, net_fingerprint: u64) -> Option<HostProfile> {
        if net_fingerprint == 0 {
            // Zero means the network could not be identified. Matching on it
            // would pair any two unidentified networks with each other.
            return None;
        }
        match self.host_profile(key, net_fingerprint) {
            Ok(found) => found,
            Err(error) => {
                tracing::warn!(%error, host = %key.host, "could not read the host profile");
                None
            }
        }
    }

    fn save(&self, key: &HostKey, profile: &HostProfile) {
        if profile.net_fingerprint == 0 {
            return;
        }
        if let Err(error) = self.save_host_profile(key, profile) {
            tracing::warn!(%error, host = %key.host, "could not save the host profile");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(connections: u32, fingerprint: u64) -> HostProfile {
        HostProfile {
            best_conn_count: connections,
            conn_ceiling: connections + 2,
            multiplexing_helped: false,
            h3_won_race: None,
            net_fingerprint: fingerprint,
            updated_at_ms: 1_000,
        }
    }

    #[test]
    fn a_profile_survives_a_round_trip() {
        let store = Store::in_memory().unwrap();
        let key = HostKey::new("cdn.example", 443, "h2");
        store.save_host_profile(&key, &profile(12, 7)).unwrap();

        let loaded = store.host_profile(&key, 7).unwrap().expect("saved profile");
        assert_eq!(loaded.best_conn_count, 12);
        assert_eq!(loaded.conn_ceiling, 14);
        assert_eq!(loaded.net_fingerprint, 7);
    }

    #[test]
    fn the_host_is_matched_without_regard_to_case() {
        let store = Store::in_memory().unwrap();
        store.save_host_profile(&HostKey::new("CDN.Example", 443, "h2"), &profile(8, 1)).unwrap();
        assert!(store.host_profile(&HostKey::new("cdn.example", 443, "h2"), 1).unwrap().is_some());
    }

    #[test]
    fn a_different_network_gets_its_own_profile() {
        // Moving between a fast link and a tethered phone must not have the
        // second overwrite what the first learned, or every move retrains.
        let store = Store::in_memory().unwrap();
        let key = HostKey::new("cdn.example", 443, "h2");
        store.save_host_profile(&key, &profile(16, 1)).unwrap();
        store.save_host_profile(&key, &profile(2, 2)).unwrap();

        assert_eq!(store.host_profile(&key, 1).unwrap().unwrap().best_conn_count, 16);
        assert_eq!(store.host_profile(&key, 2).unwrap().unwrap().best_conn_count, 2);
    }

    #[test]
    fn a_different_protocol_gets_its_own_profile() {
        // Sixteen h1 connections and sixteen h2 streams are not the same
        // thing; one is sixteen congestion windows and the other is one.
        let store = Store::in_memory().unwrap();
        store.save_host_profile(&HostKey::new("cdn.example", 443, "h2"), &profile(4, 1)).unwrap();
        store
            .save_host_profile(&HostKey::new("cdn.example", 443, "http/1.1"), &profile(16, 1))
            .unwrap();

        assert_eq!(
            store
                .host_profile(&HostKey::new("cdn.example", 443, "h2"), 1)
                .unwrap()
                .unwrap()
                .best_conn_count,
            4
        );
        assert_eq!(
            store
                .host_profile(&HostKey::new("cdn.example", 443, "http/1.1"), 1)
                .unwrap()
                .unwrap()
                .best_conn_count,
            16
        );
    }

    #[test]
    fn saving_again_updates_rather_than_duplicating() {
        let store = Store::in_memory().unwrap();
        let key = HostKey::new("cdn.example", 443, "h2");
        store.save_host_profile(&key, &profile(4, 1)).unwrap();
        store.save_host_profile(&key, &profile(9, 1)).unwrap();

        assert_eq!(store.host_profile_count().unwrap(), 1);
        assert_eq!(store.host_profile(&key, 1).unwrap().unwrap().best_conn_count, 9);
    }

    #[test]
    fn a_run_that_did_not_race_h3_does_not_erase_the_answer() {
        let store = Store::in_memory().unwrap();
        let key = HostKey::new("cdn.example", 443, "h2");

        let mut raced = profile(8, 1);
        raced.h3_won_race = Some(true);
        store.save_host_profile(&key, &raced).unwrap();

        // A later download with h3 disabled knows nothing about it.
        store.save_host_profile(&key, &profile(10, 1)).unwrap();

        let loaded = store.host_profile(&key, 1).unwrap().unwrap();
        assert_eq!(loaded.best_conn_count, 10, "the fresh measurement should win");
        assert_eq!(loaded.h3_won_race, Some(true), "the older answer should survive");
    }

    #[test]
    fn an_unknown_host_is_simply_absent() {
        let store = Store::in_memory().unwrap();
        assert!(
            store.host_profile(&HostKey::new("nowhere.invalid", 443, "h2"), 1).unwrap().is_none()
        );
    }

    #[test]
    fn a_profile_that_learned_nothing_is_refused() {
        let store = Store::in_memory().unwrap();
        let key = HostKey::new("cdn.example", 443, "h2");
        assert!(store.save_host_profile(&key, &profile(0, 1)).is_err());
        assert_eq!(store.host_profile_count().unwrap(), 0);
    }

    #[test]
    fn stale_profiles_can_be_swept() {
        let store = Store::in_memory().unwrap();
        let old = HostKey::new("old.example", 443, "h2");
        let fresh = HostKey::new("fresh.example", 443, "h2");

        let mut ancient = profile(4, 1);
        ancient.updated_at_ms = 10;
        store.save_host_profile(&old, &ancient).unwrap();

        let mut recent = profile(4, 1);
        recent.updated_at_ms = 10_000;
        store.save_host_profile(&fresh, &recent).unwrap();

        assert_eq!(store.forget_host_profiles_older_than(1_000).unwrap(), 1);
        assert!(store.host_profile(&old, 1).unwrap().is_none());
        assert!(store.host_profile(&fresh, 1).unwrap().is_some());
    }

    #[test]
    fn an_unidentifiable_network_is_never_keyed_to() {
        // Zero means "could not tell which network this is". Treating it
        // as a value would make every unidentified network match every
        // other one, which is exactly the mix-up the fingerprint exists
        // to prevent.
        let store = Store::in_memory().unwrap();
        let key = HostKey::new("cdn.example", 443, "h2");

        let mut unknown_network = profile(8, 0);
        unknown_network.net_fingerprint = 0;
        ProfileStore::save(&store, &key, &unknown_network);

        assert_eq!(store.host_profile_count().unwrap(), 0);
        assert!(ProfileStore::load(&store, &key, 0).is_none());
    }

    #[test]
    fn a_refused_profile_costs_a_ramp_not_a_download() {
        // The trait swallows errors by design: a profile the store will
        // not keep must read back as 'nothing learned' rather than
        // becoming a failure the scheduler has to handle.
        let store = Store::in_memory().unwrap();
        let key = HostKey::new("cdn.example", 443, "h2");
        ProfileStore::save(&store, &key, &profile(0, 5));
        assert!(ProfileStore::load(&store, &key, 5).is_none());
    }
}
