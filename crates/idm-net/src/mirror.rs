//! Deciding whether a second address is serving the same file as the first.
//!
//! This is the gate in front of multi-source downloading, and the reason
//! that feature is safe to have at all. Fetching one file from several
//! mirrors means interleaving their bytes, and two mirrors holding different
//! builds of the same release produce a file that is exactly the right
//! length, satisfies every range check, and is quietly wrong. Nothing
//! afterwards can detect it — not the journal's block checksums, which only
//! prove the bytes written are the bytes that arrived, and not the length.
//! So the decision is made before any mirror contributes a byte.
//!
//! # Why the ETag is no use here
//!
//! Everywhere else in this engine a strong ETag is the authority on whether
//! a representation changed under us. Across origins it says nothing at all.
//! An ETag is opaque and chosen by whichever server issued it, so two
//! mirrors serving byte-identical content normally disagree about theirs —
//! one `"a1b2"`, one `W/"1699-xyz"` — and rejecting on that would reject
//! essentially every real mirror. Equal ETags across hosts are mild positive
//! evidence; unequal ones are no evidence whatsoever.
//!
//! That leaves the bytes as the only cross-origin evidence there is.
//!
//! # Why the tail, and not just the head
//!
//! Two builds of one release share a beginning far more often than an end:
//! the same bootloader, the same archive magic, the same header written by
//! the same tool from the same template. A check that samples only the first
//! block accepts them both and then blends them. The tail is where they
//! actually diverge, so it is the window that does the deciding — the head
//! is there to catch the cheaper mistake of a mirror serving something
//! entirely different at the same size.

use std::sync::atomic::{AtomicUsize, Ordering};

use idm_core::ByteRange;
use idm_core::journal::ValidatorKind;
use reqwest::Client;
use reqwest::header::{COOKIE, RANGE, REFERER};

use crate::client::LanePool;
use crate::download::{DownloadConfig, Target, probe};

/// How much is read from each end to compare two addresses.
///
/// Small enough to be free beside the download it guards, large enough that
/// two different files agreeing across all of it is not a thing that
/// happens.
const WINDOW: u64 = 64 << 10;

/// Why a mirror cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The lengths disagree, so whatever these are, they are not one file.
    DifferentLength { primary: u64, mirror: u64 },
    /// A sampled window differed. Definitive, and the case this exists for.
    DifferentBytes { at: u64 },
    /// It could not be checked — unreachable, no range support, a refusal.
    ///
    /// Not an accusation. A mirror that cannot be vetted is simply one that
    /// cannot be used, which is the same outcome by a duller route.
    Unusable(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DifferentLength { primary, mirror } => {
                write!(f, "it has {mirror} bytes where the first address has {primary}")
            }
            Self::DifferentBytes { at } => {
                write!(f, "its bytes differ from the first address at offset {at}")
            }
            Self::Unusable(why) => write!(f, "it could not be checked: {why}"),
        }
    }
}

/// The windows worth comparing for a file of this size.
///
/// Head and tail, except for a file small enough that reading all of it is
/// cheaper than deciding which part to trust.
fn windows(total: u64) -> Vec<ByteRange> {
    if total == 0 {
        return Vec::new();
    }
    if total <= 2 * WINDOW {
        return ByteRange::try_new(0, total).into_iter().collect();
    }
    [ByteRange::try_new(0, WINDOW), ByteRange::try_new(total - WINDOW, total)]
        .into_iter()
        .flatten()
        .collect()
}

/// Reads exactly one window from an address.
///
/// A short read is an error rather than something to work with: this
/// compares bytes for a living, and comparing a prefix of what was asked
/// for would quietly weaken the check it exists to perform.
async fn read_window(
    client: &Client,
    url: &str,
    range: ByteRange,
    config: &DownloadConfig,
) -> Result<Vec<u8>, String> {
    let header = range.to_range_header().map_err(|error| error.to_string())?;
    let mut request = client.get(url).header(RANGE, header);
    if let Some(referrer) = &config.referrer {
        request = request.header(REFERER, referrer);
    }
    if let Some(cookies) = &config.cookies {
        request = request.header(COOKIE, cookies);
    }

    let response = request.send().await.map_err(|error| error.to_string())?;
    if !response.status().is_success() {
        return Err(format!("it answered {}", response.status()));
    }
    let body = response.bytes().await.map_err(|error| error.to_string())?;

    let wanted = range.len();
    if body.len() as u64 != wanted {
        return Err(format!("{wanted} bytes were asked for and {} came back", body.len()));
    }
    Ok(body.to_vec())
}

/// The primary's sample windows, read once and compared against every mirror.
///
/// Held rather than re-read per mirror: with four mirrors the naive version
/// reads the primary's windows four times, and the primary is the one
/// address already known to be busy serving this download.
#[derive(Debug, Clone)]
pub struct Fingerprint {
    total: u64,
    windows: Vec<(ByteRange, Vec<u8>)>,
}

impl Fingerprint {
    /// How long the file is, according to the address this came from.
    pub fn total(&self) -> u64 {
        self.total
    }
}

/// Samples the primary, so mirrors have something to be compared against.
pub async fn fingerprint(config: &DownloadConfig, target: &Target) -> Result<Fingerprint, String> {
    let client = crate::client::redirect_resolver(&config.http).map_err(|e| e.to_string())?;
    let mut windows_read = Vec::new();
    for range in windows(target.total) {
        let bytes = read_window(&client, &target.pinned_url, range, config).await?;
        windows_read.push((range, bytes));
    }
    Ok(Fingerprint { total: target.total, windows: windows_read })
}

/// Whether `url` may contribute bytes to the download `fingerprint` describes.
///
/// On success the mirror's own probed target comes back, because what the
/// scheduler needs afterwards is that address's post-redirect URL and
/// negotiated protocol, not the one that was typed.
pub async fn vet(
    config: &DownloadConfig,
    fingerprint: &Fingerprint,
    url: &str,
) -> Result<Target, Refusal> {
    // Probed through the ordinary path, so a mirror has to clear every bar
    // the primary did: range support, a truthful `Content-Range`, no content
    // coding. A mirror that cannot be split is of no use as a mirror.
    let mut probe_config = config.clone();
    probe_config.url = url.to_owned();
    let target = match probe(&probe_config).await {
        Ok(target) => target,
        Err(error) => return Err(Refusal::Unusable(error.to_string())),
    };

    if target.total != fingerprint.total {
        return Err(Refusal::DifferentLength { primary: fingerprint.total, mirror: target.total });
    }

    let client = match crate::client::redirect_resolver(&config.http) {
        Ok(client) => client,
        Err(error) => return Err(Refusal::Unusable(error.to_string())),
    };

    for (range, expected) in &fingerprint.windows {
        let found = match read_window(&client, &target.pinned_url, *range, config).await {
            Ok(bytes) => bytes,
            Err(why) => return Err(Refusal::Unusable(why)),
        };
        if let Some(offset) = first_difference(expected, &found) {
            return Err(Refusal::DifferentBytes { at: range.start() + offset });
        }
    }

    Ok(target)
}

/// Where two windows first disagree, if they do.
///
/// The offset is reported rather than just the fact, because it is the
/// difference between "this mirror is a different build" and "this mirror is
/// serving something else entirely", and whoever reads the log will want to
/// know which.
fn first_difference(left: &[u8], right: &[u8]) -> Option<u64> {
    left.iter()
        .zip(right)
        .position(|(a, b)| a != b)
        .map(|at| at as u64)
        .or_else(|| (left.len() != right.len()).then_some(left.len().min(right.len()) as u64))
}

/// One address a download may take bytes from.
#[derive(Debug)]
pub struct Source {
    pub url: String,
    /// This source's own strong ETag, if it published one.
    ///
    /// Its own, and that is the point. An ETag is per-origin, so sending the
    /// primary's `If-Match` to a mirror earns a `412` from a mirror serving
    /// precisely the right bytes — the download would then look like it was
    /// being refused by half its sources. Each address validates against
    /// itself, or not at all.
    pub etag: Option<String>,
    pub lanes: LanePool,
}

impl Source {
    /// Builds a source from a probed address and the lanes it was given.
    pub fn new(target: &Target, lanes: LanePool) -> Self {
        Self {
            url: target.pinned_url.clone(),
            // Only a strong one. A weak ETag permits byte-different but
            // equivalent representations, which is the one thing `If-Match`
            // is being used here to rule out.
            etag: target.etag.clone().filter(|_| target.validator == ValidatorKind::StrongETag),
            lanes,
        }
    }
}

/// Every address in play, handed out one claim at a time.
#[derive(Debug)]
pub struct Sources {
    sources: Vec<Source>,
    cursor: AtomicUsize,
}

impl Sources {
    pub fn new(sources: Vec<Source>) -> Self {
        Self { sources, cursor: AtomicUsize::new(0) }
    }

    pub fn len(&self) -> usize {
        self.sources.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    /// Lanes across every source, which is what the report means by lanes.
    pub fn total_lanes(&self) -> usize {
        self.sources.iter().map(|source| source.lanes.len()).sum()
    }

    /// The next address to ask.
    ///
    /// Round-robin, and deliberately not weighted by measured rate. The
    /// steal heuristic already takes work away from whichever claim is
    /// going to finish last, and a slow mirror's claims are exactly those —
    /// so weighting here would be a second controller pulling in the same
    /// direction as the first, with no way afterwards to tell which of them
    /// caused a given shift. One mechanism, in one place.
    pub fn next(&self) -> &Source {
        let index = self.cursor.fetch_add(1, Ordering::Relaxed) % self.sources.len();
        &self.sources[index]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_large_file_is_sampled_at_both_ends() {
        let total = 100 << 20;
        let sampled = windows(total);
        assert_eq!(sampled.len(), 2, "{sampled:?}");
        assert_eq!(sampled[0], ByteRange::new(0, WINDOW));
        assert_eq!(
            sampled[1],
            ByteRange::new(total - WINDOW, total),
            "the tail is the window that decides"
        );
    }

    #[test]
    fn the_windows_do_not_overlap_for_anything_worth_sampling_twice() {
        // Overlapping would make the tail partly a re-check of the head,
        // which is the one thing it is there not to be.
        let sampled = windows(2 * WINDOW + 1);
        assert_eq!(sampled.len(), 2);
        assert!(sampled[0].end() <= sampled[1].start(), "{sampled:?}");
    }

    #[test]
    fn a_small_file_is_read_whole_rather_than_sampled() {
        for total in [1u64, 1024, WINDOW, 2 * WINDOW] {
            let sampled = windows(total);
            assert_eq!(sampled.len(), 1, "{total}: {sampled:?}");
            assert_eq!(sampled[0], ByteRange::new(0, total));
        }
    }

    #[test]
    fn nothing_is_sampled_from_an_empty_file() {
        assert!(windows(0).is_empty());
    }

    #[test]
    fn identical_windows_agree() {
        assert_eq!(first_difference(b"abcdef", b"abcdef"), None);
        assert_eq!(first_difference(b"", b""), None);
    }

    #[test]
    fn the_first_differing_offset_is_reported() {
        assert_eq!(first_difference(b"abcdef", b"abcXef"), Some(3));
        assert_eq!(first_difference(b"Xbcdef", b"abcdef"), Some(0));
    }

    #[test]
    fn a_window_that_came_back_short_counts_as_a_difference_at_its_end() {
        // Guarded against separately in `read_window`, but `zip` stops at the
        // shorter side, so a prefix match would otherwise read as agreement.
        assert_eq!(first_difference(b"abcdef", b"abc"), Some(3));
        assert_eq!(first_difference(b"abc", b"abcdef"), Some(3));
    }

    #[test]
    fn a_refusal_says_which_offset_disagreed() {
        let said = Refusal::DifferentBytes { at: 4096 }.to_string();
        assert!(said.contains("4096"), "{said}");
    }

    #[test]
    fn a_length_refusal_names_both_lengths() {
        let said = Refusal::DifferentLength { primary: 100, mirror: 99 }.to_string();
        assert!(said.contains("100") && said.contains("99"), "{said}");
    }
}
