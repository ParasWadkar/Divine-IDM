//! Why a download stopped, said twice: once for the person and once for whoever
//! has to debug it.
//!
//! Every error in this program already knows exactly what went wrong — the
//! preallocation failure carries the volume and the byte counts, the probe
//! carries the HTTP status, the fetch carries how far it got. All of it used to
//! be destroyed in one line, `error.to_string()`, before anything was written
//! down. What reached the history row was a sentence, and a sentence can only
//! be re-read, never re-examined: to find out whether a download died of a 403
//! or a 404 you had to match English prose.
//!
//! So classification happens here, while the typed error is still typed, and
//! produces three things that are kept apart on purpose:
//!
//! * a [`Kind`], which is a code the interface can switch on,
//! * a `summary`, one short line that answers "what went wrong?" and stops,
//! * a `detail`, the full technical text, kept for the one person in a hundred
//!   who opens it.
//!
//! The summary is allowed to do arithmetic the reader should not have to. A
//! download needing 190.95 GB on a disk with 180.74 GB free is reported as
//! "10.21 GB more space required", because the subtraction is the answer and
//! the two operands are merely how it was arrived at.

use idm_core::sched::probe::{ProbeOutcome, RestartReason};
use idm_core::win::prealloc::PreallocError;
use idm_net::download::DownloadError;
use idm_net::fetch::FetchError;

/// What went wrong, as a thing the interface can reason about.
///
/// The groupings are the ones a person would use when deciding what to do
/// next: a storage problem is theirs to fix, a server problem is worth
/// retrying later, a URL problem needs a new address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    StorageInsufficientSpace,
    StoragePermissionDenied,
    StorageDestinationUnavailable,

    ServerRangesUnsupported,
    ServerUnavailable,
    ServerConnectionRefused,

    HttpBadRequest,
    HttpForbidden,
    HttpNotFound,
    HttpServerError,

    UrlExpired,
    UrlInvalid,
    UrlRedirectFailure,

    DownloadInterrupted,
    DownloadIntegrity,
    DownloadUnknownSize,

    /// Nothing matched. The raw text is still in the detail, which is the
    /// whole reason this variant is allowed to exist rather than forcing a
    /// wrong guess into one of the others.
    Unclassified,
}

impl Kind {
    /// The stable string written to the database and read by the interface.
    ///
    /// Dotted rather than nested, so one column holds it and a prefix match
    /// still finds a whole category.
    pub fn code(self) -> &'static str {
        match self {
            Self::StorageInsufficientSpace => "storage.insufficient_space",
            Self::StoragePermissionDenied => "storage.permission_denied",
            Self::StorageDestinationUnavailable => "storage.destination_unavailable",
            Self::ServerRangesUnsupported => "server.ranges_unsupported",
            Self::ServerUnavailable => "server.unavailable",
            Self::ServerConnectionRefused => "server.connection_refused",
            Self::HttpBadRequest => "http.bad_request",
            Self::HttpForbidden => "http.forbidden",
            Self::HttpNotFound => "http.not_found",
            Self::HttpServerError => "http.server_error",
            Self::UrlExpired => "url.expired",
            Self::UrlInvalid => "url.invalid",
            Self::UrlRedirectFailure => "url.redirect_failure",
            Self::DownloadInterrupted => "download.interrupted",
            Self::DownloadIntegrity => "download.integrity",
            Self::DownloadUnknownSize => "download.unknown_size",
            Self::Unclassified => "unknown.unclassified",
        }
    }

    /// Whether a new address is what would fix this.
    ///
    /// The interface offers "Replace URL" on exactly these, rather than on
    /// everything that failed, because offering it on a full disk is an
    /// invitation to waste time.
    pub fn wants_new_url(self) -> bool {
        matches!(self, Self::UrlExpired | Self::HttpNotFound | Self::HttpForbidden)
    }
}

/// A stopped download, explained.
#[derive(Debug, Clone)]
pub struct Failure {
    pub kind: Kind,
    /// One short line. No status codes, no byte arithmetic left undone.
    pub summary: String,
    /// Everything the error actually said, for Technical details.
    pub detail: String,
    /// True when the network went away rather than the download being
    /// impossible. See [`DownloadError::is_transient`].
    pub transient: bool,
}

impl Failure {
    /// For the transports whose errors are not [`DownloadError`].
    ///
    /// FTP, SFTP and the stream fetcher each have their own error type, and
    /// all three run over a network that can go away. Treating their failures
    /// as worth another try is the same bet as for HTTP; a genuinely
    /// impossible transfer fails again straight away and the attempt costs a
    /// few seconds. They are unclassified because their error types are
    /// `{0}` passthroughs with nothing inside to classify.
    pub fn from_transport(error: impl std::fmt::Display) -> Self {
        let detail = error.to_string();
        Self {
            kind: Kind::Unclassified,
            summary: first_sentence(&detail),
            detail,
            transient: true,
        }
    }

    /// A download that stopped sending without anyone saying why.
    ///
    /// The stall watchdog notices an absence rather than catching an error,
    /// so there is no typed error to classify: all it knows is that no byte
    /// has reached the disk for a while. That is still a kind.
    pub fn stalled() -> Self {
        Self {
            kind: Kind::DownloadInterrupted,
            summary: "The connection went away".into(),
            detail: "no bytes reached the disk before the stall timeout".into(),
            transient: true,
        }
    }
}

impl From<DownloadError> for Failure {
    fn from(error: DownloadError) -> Self {
        let transient = error.is_transient();
        let detail = error.to_string();
        let (kind, summary) = classify(&error);
        Self { kind, summary, detail, transient }
    }
}

/// The whole taxonomy, as one match over the errors this program can raise.
///
/// Anything genuinely unmatched lands on [`Kind::Unclassified`] with the
/// error's own first sentence, which is worse than a written summary and
/// better than a guess.
fn classify(error: &DownloadError) -> (Kind, String) {
    match error {
        // The one error that already knows the number the reader wants. The
        // shortfall is what they have to act on; `needed` and `available` are
        // only how it was worked out, and are left for Technical details.
        DownloadError::Prealloc(PreallocError::InsufficientSpace {
            needed, available, ..
        }) => (
            Kind::StorageInsufficientSpace,
            format!("{} more space required", human(needed.saturating_sub(*available))),
        ),
        DownloadError::Prealloc(PreallocError::Io(io)) => from_io(io),

        DownloadError::Io(io) => from_io(io),
        DownloadError::Fetch(FetchError::Write(io)) => from_io(io),

        DownloadError::NoRangeSupport => (
            Kind::ServerRangesUnsupported,
            "Server doesn't support parallel downloads".into(),
        ),
        DownloadError::UnknownLength | DownloadError::Unusable(ProbeOutcome::SingleStreamUnknownLength) => {
            (Kind::DownloadUnknownSize, "Unable to determine file size".into())
        }

        DownloadError::Unusable(outcome) => from_probe(outcome),

        DownloadError::RepresentationChanged => (
            Kind::DownloadIntegrity,
            "The file changed on the server mid-download".into(),
        ),
        DownloadError::Journal(_) => {
            (Kind::DownloadIntegrity, "The resume record did not match the file".into())
        }

        DownloadError::Stalled { attempts } => (
            Kind::DownloadInterrupted,
            format!("Gave up after {attempts} attempts"),
        ),

        // `is_connect` is the one distinction worth drawing here: nothing
        // answered at all, versus a conversation that started and broke.
        DownloadError::Request(source) | DownloadError::Fetch(FetchError::Request(source)) => {
            if source.is_connect() {
                (Kind::ServerConnectionRefused, "Could not reach the server".into())
            } else if source.is_timeout() {
                (Kind::ServerUnavailable, "The server stopped responding".into())
            } else {
                (Kind::DownloadInterrupted, "Connection interrupted".into())
            }
        }
        DownloadError::Fetch(FetchError::Body { .. }) => {
            (Kind::DownloadInterrupted, "Connection interrupted".into())
        }

        DownloadError::Client(_) => (Kind::UrlInvalid, "That address cannot be fetched".into()),
        DownloadError::Fetch(FetchError::Range(_)) | DownloadError::Extent(_) => {
            (Kind::DownloadIntegrity, "The server's answer did not add up".into())
        }
    }
}

/// Windows tells us which of these it was; the taxonomy only has to agree.
fn from_io(error: &std::io::Error) -> (Kind, String) {
    use std::io::ErrorKind;
    match error.kind() {
        ErrorKind::StorageFull => {
            (Kind::StorageInsufficientSpace, "Ran out of disk space".into())
        }
        ErrorKind::PermissionDenied => {
            (Kind::StoragePermissionDenied, "Windows would not allow writing there".into())
        }
        ErrorKind::NotFound | ErrorKind::NotADirectory => (
            Kind::StorageDestinationUnavailable,
            "The download folder is not there any more".into(),
        ),
        ErrorKind::ReadOnlyFilesystem => {
            (Kind::StoragePermissionDenied, "That drive is read-only".into())
        }
        _ => (Kind::DownloadInterrupted, "Writing to disk failed".into()),
    }
}

/// What the probe worked out, turned back into a cause.
///
/// [`ProbeOutcome::explain`] deliberately erases the status code on the way to
/// prose; this reads the outcome before that happens, so the code survives in
/// the summary where it is genuinely the shortest true answer.
fn from_probe(outcome: &ProbeOutcome) -> (Kind, String) {
    match outcome {
        ProbeOutcome::ReResolve => (Kind::UrlExpired, "Download link expired".into()),
        ProbeOutcome::NeedsNewUrl => {
            (Kind::HttpNotFound, "The file is no longer at that address".into())
        }
        ProbeOutcome::Throttled => {
            (Kind::ServerUnavailable, "The server asked us to slow down".into())
        }
        ProbeOutcome::Restart(RestartReason::UnexpectedStatus(code)) => from_status(*code),
        ProbeOutcome::Restart(RestartReason::RangeStartMismatch { .. }) => (
            Kind::ServerRangesUnsupported,
            "The server sent the wrong part of the file".into(),
        ),
        ProbeOutcome::Restart(_) => {
            (Kind::DownloadIntegrity, "The server's answer did not make sense".into())
        }
        // The outcomes that mean the download can proceed are not failures,
        // and reaching here with one is a bug rather than a user's problem.
        _ => (Kind::Unclassified, "The download could not be started".into()),
    }
}

fn from_status(code: u16) -> (Kind, String) {
    match code {
        400 => (Kind::HttpBadRequest, "The server rejected the request".into()),
        401 | 403 => (Kind::HttpForbidden, "The server refused access".into()),
        404 | 410 => (Kind::HttpNotFound, "The file is no longer at that address".into()),
        500..=599 => (Kind::HttpServerError, format!("Server error (HTTP {code})")),
        _ => (Kind::Unclassified, format!("The server refused the request (HTTP {code})")),
    }
}

/// As much of a message as belongs on one line of a list.
///
/// Used only where there is nothing better: an unclassified error still has to
/// say something, and the first sentence of it is the part most likely to be
/// the answer.
fn first_sentence(text: &str) -> String {
    let trimmed = text.trim();
    let cut = trimmed.find(". ").map(|at| at + 1).unwrap_or(trimmed.len());
    let mut line = trimmed[..cut].trim_end_matches('.').to_owned();
    if line.chars().count() > 80 {
        line = line.chars().take(79).collect::<String>() + "\u{2026}";
    }
    line
}

/// Bytes as a person writes them.
///
/// Two decimals, unlike the rest of the program's one, because this number is
/// the whole summary: "10.21 GB more space required" is a figure to act on,
/// where "10.2 GB" in a list of sizes is a figure to glance at.
fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut power = 0;
    while value >= 1024.0 && power < UNITS.len() - 1 {
        value /= 1024.0;
        power += 1;
    }
    if power == 0 {
        format!("{bytes} {}", UNITS[0])
    } else {
        format!("{value:.2} {}", UNITS[power])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_disk_reports_the_shortfall_and_not_the_two_numbers_it_came_from() {
        // The point of the whole module: 190.95 GB needed against 180.74 GB
        // free is one fact -- 10.21 GB -- and the reader should not have to
        // subtract to find it.
        let needed = 205_031_179_059;
        let available = 194_069_596_569;
        let error = DownloadError::Prealloc(PreallocError::InsufficientSpace {
            volume: "D:\\".into(),
            needed,
            available,
            headroom: 64 << 20,
        });
        let failure = Failure::from(error);

        assert_eq!(failure.kind, Kind::StorageInsufficientSpace);
        assert_eq!(failure.summary, "10.21 GB more space required");
        assert!(!failure.transient, "a full disk is not fixed by waiting");
        // The arithmetic's operands are not lost, only moved out of the way.
        assert!(failure.detail.contains("not enough space"));
    }

    #[test]
    fn the_summary_never_carries_the_detail_it_replaced() {
        let error = DownloadError::Prealloc(PreallocError::InsufficientSpace {
            volume: "D:\\".into(),
            needed: 1 << 40,
            available: 0,
            headroom: 0,
        });
        let failure = Failure::from(error);
        // A summary that is merely the first line of the long message is the
        // failure mode this module exists to prevent.
        assert!(failure.summary.len() < 40, "a summary is one short line: {}", failure.summary);
        assert!(failure.detail.len() > failure.summary.len());
    }

    #[test]
    fn a_status_code_survives_long_enough_to_be_classified() {
        // `ProbeOutcome::explain` erases the code on purpose. Classifying from
        // the outcome rather than from its prose is what keeps 403 and 404
        // apart, which decides whether a new address would help.
        let forbidden = Failure::from(DownloadError::Unusable(ProbeOutcome::Restart(
            RestartReason::UnexpectedStatus(403),
        )));
        let missing = Failure::from(DownloadError::Unusable(ProbeOutcome::NeedsNewUrl));
        let server = Failure::from(DownloadError::Unusable(ProbeOutcome::Restart(
            RestartReason::UnexpectedStatus(503),
        )));

        assert_eq!(forbidden.kind, Kind::HttpForbidden);
        assert_eq!(missing.kind, Kind::HttpNotFound);
        assert_eq!(server.kind, Kind::HttpServerError);
        assert!(forbidden.kind.wants_new_url());
        assert!(missing.kind.wants_new_url());
        assert!(!server.kind.wants_new_url(), "a 503 is fixed by waiting, not by a new link");
    }

    #[test]
    fn an_expired_link_is_told_apart_from_a_deleted_file() {
        // Both are "the server said no", and the right next step differs:
        // one is re-resolved automatically, the other needs the user.
        let expired = Failure::from(DownloadError::Unusable(ProbeOutcome::ReResolve));
        assert_eq!(expired.kind, Kind::UrlExpired);
        assert_eq!(expired.summary, "Download link expired");
    }

    #[test]
    fn a_denied_folder_is_a_storage_problem_and_not_a_mystery() {
        // Reached history as "I/O failed: Access is denied. (os error 5)",
        // which names the operation rather than the cause.
        let error = DownloadError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Access is denied. (os error 5)",
        ));
        let failure = Failure::from(error);
        assert_eq!(failure.kind, Kind::StoragePermissionDenied);
        assert_eq!(failure.summary, "Windows would not allow writing there");
    }

    #[test]
    fn every_kind_has_a_code_of_its_own() {
        // The code is written to the database and matched by the interface, so
        // two kinds sharing one would silently merge two rows' meanings.
        let kinds = [
            Kind::StorageInsufficientSpace,
            Kind::StoragePermissionDenied,
            Kind::StorageDestinationUnavailable,
            Kind::ServerRangesUnsupported,
            Kind::ServerUnavailable,
            Kind::ServerConnectionRefused,
            Kind::HttpBadRequest,
            Kind::HttpForbidden,
            Kind::HttpNotFound,
            Kind::HttpServerError,
            Kind::UrlExpired,
            Kind::UrlInvalid,
            Kind::UrlRedirectFailure,
            Kind::DownloadInterrupted,
            Kind::DownloadIntegrity,
            Kind::DownloadUnknownSize,
            Kind::Unclassified,
        ];
        let mut codes: Vec<&str> = kinds.iter().map(|kind| kind.code()).collect();
        codes.sort_unstable();
        let count = codes.len();
        codes.dedup();
        assert_eq!(codes.len(), count, "two kinds share a code");
        assert!(codes.iter().all(|code| code.contains('.')), "a code names its category");
    }

    #[test]
    fn an_unclassified_error_still_says_something_short() {
        let long = "the writer refused the job because the generation had moved on. \
                    This is a long technical explanation that nobody reading a list wants.";
        let failure = Failure::from_transport(long);
        assert_eq!(failure.kind, Kind::Unclassified);
        assert!(failure.summary.chars().count() <= 80);
        assert!(!failure.summary.ends_with('.'));
        assert_eq!(failure.detail, long, "the whole thing is still kept");
    }
}
