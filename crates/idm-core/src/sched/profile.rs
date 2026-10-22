//! Where a learned [`HostProfile`] comes from, and what it is keyed to.
//!
//! The trait lives here, beside the thing it carries, so the scheduler can be
//! handed somewhere to remember without the engine's core growing a
//! dependency on a database. `idm-store` implements it; `idm-net` only ever
//! sees this.

use super::HostProfile;

/// Identifies the thing a profile describes.
///
/// Host rather than registrable domain. The design called for eTLD+1, so that
/// `a.cdn.example` and `b.cdn.example` pool what they learn, but computing
/// that correctly needs a public suffix list, and pooling cuts both ways: one
/// registrable domain often serves a fast object store and a slow API under
/// different names, and a shared profile teaches the wrong number for both.
/// Keying on the host learns more slowly and is never wrong about whose
/// behaviour it saw. Widening it later is a change to this type alone.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HostKey {
    pub host: String,
    pub port: u16,
    /// Negotiated protocol: `h2`, `http/1.1`, `h3`.
    ///
    /// Part of the key because the right connection count differs sharply
    /// between them. Sixteen h1 connections are sixteen congestion windows;
    /// sixteen h2 streams on one connection are one, and a number learned
    /// for the first is actively harmful applied to the second.
    pub alpn: String,
}

impl HostKey {
    pub fn new(host: impl Into<String>, port: u16, alpn: impl Into<String>) -> Self {
        Self { host: host.into().to_ascii_lowercase(), port, alpn: alpn.into() }
    }
}

/// Somewhere to keep what was learned about a host.
///
/// Neither method returns an error, and that is deliberate. A download must
/// never fail, stall or change behaviour because a database was locked or a
/// disk was full; the worst a broken store may cost is a ramp that has to be
/// climbed again. Implementations log their own failures and return `None`
/// or do nothing.
pub trait ProfileStore: Send + Sync {
    fn load(&self, key: &HostKey, net_fingerprint: u64) -> Option<HostProfile>;
    fn save(&self, key: &HostKey, profile: &HostProfile);
}

/// A store that remembers nothing, for tests and for the `--no-store` path.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoProfileStore;

impl ProfileStore for NoProfileStore {
    fn load(&self, _key: &HostKey, _net_fingerprint: u64) -> Option<HostProfile> {
        None
    }
    fn save(&self, _key: &HostKey, _profile: &HostProfile) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_is_compared_without_regard_to_case() {
        assert_eq!(HostKey::new("CDN.Example", 443, "h2"), HostKey::new("cdn.example", 443, "h2"));
    }

    #[test]
    fn protocol_and_port_are_part_of_the_identity() {
        let h2 = HostKey::new("cdn.example", 443, "h2");
        assert_ne!(h2, HostKey::new("cdn.example", 443, "http/1.1"));
        assert_ne!(h2, HostKey::new("cdn.example", 8443, "h2"));
    }
}
