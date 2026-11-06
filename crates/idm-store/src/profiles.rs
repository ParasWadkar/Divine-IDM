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
            "SELECT best_conn_count, conn_ceiling, multiplexing_helped, updated_at_ms
               FROM host_profiles
              WHERE host = ?1 AND port = ?2 AND alpn = ?3 AND net_fingerprint = ?4",
        )?;

        let profile = statement
            .query_row(params![key.host, key.port, key.alpn, net_fingerprint as i64], |row| {
                Ok(HostProfile {
                    best_conn_count: row.get::<_, i64>(0)? as u32,
                    conn_ceiling: row.get::<_, i64>(1)? as u32,
                    multiplexing_helped: row.get::<_, i64>(2)? != 0,
                    net_fingerprint,
                    updated_at_ms: row.get::<_, i64>(3)? as u64,
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
                  multiplexing_helped, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(host, port, alpn, net_fingerprint) DO UPDATE SET
                 best_conn_count     = excluded.best_conn_count,
                 conn_ceiling        = excluded.conn_ceiling,
                 multiplexing_helped = excluded.multiplexing_helped,
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
            profile.updated_at_ms as i64,
        ])?;
        Ok(())
    }

    /// Whether HTTP/3 beat TCP to this host on this network, if it has been
    /// tried.
    ///
    /// Keyed by host and port with no protocol in the key, because this is
    /// the fact that decides the protocol: keying it by the answer would
    /// mean needing the answer to look it up. Stored apart from
    /// [`Store::host_profile`] for the same reason it is not gated like one
    /// -- a race result is valid as soon as it finishes, while a connection
    /// count is only valid once the ramp has settled.
    pub fn host_prefers_h3(
        &self,
        host: &str,
        port: u16,
        net_fingerprint: u64,
    ) -> Result<Option<bool>> {
        let conn = self.conn();
        let found = conn
            .prepare_cached(
                "SELECT h3_won FROM host_transport
                  WHERE host = ?1 AND port = ?2 AND net_fingerprint = ?3",
            )?
            .query_row(params![host.to_lowercase(), port, net_fingerprint as i64], |row| {
                Ok(row.get::<_, i64>(0)? != 0)
            })
            .optional()?;
        Ok(found)
    }

    /// Records which transport won, replacing any earlier answer.
    ///
    /// Replacing rather than preserving, unlike a connection count: a host
    /// that has stopped offering HTTP/3, or started, should be believed the
    /// next time it is raced rather than held to what it did last month.
    pub fn save_host_transport(
        &self,
        host: &str,
        port: u16,
        net_fingerprint: u64,
        h3_won: bool,
        now_ms: u64,
    ) -> Result<()> {
        if net_fingerprint == 0 {
            // Zero means "could not tell which network this is", and a row
            // keyed to nothing would be matched by every later unknown.
            return Err(StoreError::Invalid(
                "a transport answer keyed to an unknown network is not worth keeping".into(),
            ));
        }
        let conn = self.conn();
        conn.prepare_cached(
            "INSERT INTO host_transport (host, port, net_fingerprint, h3_won, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(host, port, net_fingerprint) DO UPDATE SET
                 h3_won        = excluded.h3_won,
                 updated_at_ms = excluded.updated_at_ms",
        )?
        .execute(params![
            host.to_lowercase(),
            port,
            net_fingerprint as i64,
            h3_won as i64,
            now_ms as i64
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
    fn h3_preference(&self, host: &str, port: u16, net_fingerprint: u64) -> Option<bool> {
        self.host_prefers_h3(host, port, net_fingerprint).ok().flatten()
    }

    fn save_h3_preference(&self, host: &str, port: u16, net_fingerprint: u64, h3_won: bool) {
        if let Err(error) =
            self.save_host_transport(host, port, net_fingerprint, h3_won, crate::now_ms())
        {
            tracing::debug!(%error, host, "could not record which transport won");
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
    fn the_transport_answer_outlives_a_download_that_learned_no_count() {
        // The bug this table exists to fix. The answer used to live on the
        // host profile, which is only written once the connection ramp has
        // settled -- so every download short enough not to settle raced
        // HTTP/3, found out, and threw the answer away.
        let store = Store::in_memory().unwrap();
        store.save_host_transport("cdn.example", 443, 1, true, 1_000).unwrap();
        assert_eq!(store.host_prefers_h3("cdn.example", 443, 1).unwrap(), Some(true));
        // And no host profile was written at all.
        assert_eq!(store.host_profile_count().unwrap(), 0);
    }

    #[test]
    fn a_later_race_replaces_an_earlier_answer() {
        // Unlike a connection count, which is preserved when a later run
        // learned nothing: a host that has stopped offering HTTP/3, or
        // started, should be believed rather than held to last month.
        let store = Store::in_memory().unwrap();
        store.save_host_transport("cdn.example", 443, 1, true, 1_000).unwrap();
        store.save_host_transport("cdn.example", 443, 1, false, 2_000).unwrap();
        assert_eq!(store.host_prefers_h3("cdn.example", 443, 1).unwrap(), Some(false));
    }

    #[test]
    fn the_transport_answer_is_per_network_and_per_port() {
        // A CDN reachable over HTTP/3 from home and not from a hotel is the
        // ordinary case, and the fingerprint is what keeps the two apart.
        let store = Store::in_memory().unwrap();
        store.save_host_transport("cdn.example", 443, 1, true, 1_000).unwrap();
        assert_eq!(store.host_prefers_h3("cdn.example", 443, 2).unwrap(), None);
        assert_eq!(store.host_prefers_h3("cdn.example", 8443, 1).unwrap(), None);
        assert_eq!(store.host_prefers_h3("other.example", 443, 1).unwrap(), None);
    }

    #[test]
    fn a_transport_answer_keyed_to_an_unknown_network_is_refused() {
        // Zero means "could not tell which network this is", and a row
        // keyed to nothing would be matched by every later unknown.
        let store = Store::in_memory().unwrap();
        assert!(store.save_host_transport("cdn.example", 443, 0, true, 1_000).is_err());
    }

    #[test]
    fn the_transport_host_is_matched_without_regard_to_case() {
        let store = Store::in_memory().unwrap();
        store.save_host_transport("CDN.Example", 443, 1, true, 1_000).unwrap();
        assert_eq!(store.host_prefers_h3("cdn.example", 443, 1).unwrap(), Some(true));
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
