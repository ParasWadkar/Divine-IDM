//! Looking up the figure a download's publisher said it should hash to.
//!
//! The finding is done in `idm_net::checksum`; what lives here is the policy
//! around it. The window has no way to make an outbound request of its own,
//! deliberately, so every network decision for this feature — which client,
//! how long to wait — is made on this side of the line with the rest of them.

use std::time::Duration;

/// A published hash and the address it was read from.
///
/// Its own type rather than `idm_net`'s because this one crosses into the
/// window and so has to serialise, and that is not a reason to give the
/// transport crate an opinion about serde.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Published {
    pub hash: String,
    /// Shown beside the verdict. A hash is worth exactly as much as its
    /// provenance, so the provenance travels with it rather than being
    /// dropped on the way.
    pub source: String,
}

/// Everything a lookup may spend, start to finish.
///
/// One deadline for the whole walk rather than one per address. The list is
/// several addresses long and most of them do not exist, so per-request
/// timeouts multiply into a wait nobody would sit through; a single budget
/// means a slow mirror costs the lookup rather than the user.
const BUDGET: Duration = Duration::from_secs(20);

/// Asks the usual places for `filename`'s published hash.
///
/// `None` is the ordinary answer rather than a failure: most downloads have
/// no published hash anywhere, and the caller falls back to asking.
pub async fn published(
    url: &str,
    referrer: Option<&str>,
    filename: &str,
) -> Option<Published> {
    // Tighter than the engine's own timeouts, which are sized for a segment
    // of a large file over a slow link. This is a few lines of hex; waiting
    // an hour for one would be absurd.
    let config = idm_net::client::HttpConfig {
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(8),
        ..Default::default()
    };
    let client = idm_net::client::redirect_resolver(&config).ok()?;

    let found =
        tokio::time::timeout(BUDGET, idm_net::checksum::discover(&client, url, referrer, filename))
            .await
            .ok()??;
    Some(Published { hash: found.hash, source: found.source })
}
