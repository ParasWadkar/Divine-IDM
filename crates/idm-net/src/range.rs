//! Reading part of a file into memory, without downloading the rest.
//!
//! Everything else in this crate writes what it fetches to disk: the
//! scheduler hands a byte range to a writer and never sees the bytes. That
//! is right for a download and useless for the handful of things that need
//! to *look* at a few kilobytes and decide something — what is inside a zip
//! before committing to it, what a published checksum file says.
//!
//! So this is the one place that returns bytes.
//!
//! # Why a suffix read is trustworthy here
//!
//! A zip's index is at its end, which means `Range: bytes=-N` — the last N
//! bytes, without knowing how many there are. Two things make the answer
//! usable rather than a guess. [`crate::client`] pins
//! `Accept-Encoding: identity` and leaves reqwest's decompression features
//! off, so a byte offset means a byte offset in the file rather than in
//! some encoded form of it. And a server that ignores the range answers
//! `200` with the whole body, which is detected here and refused rather
//! than mistaken for the tail.
//!
//! # Everything is capped
//!
//! Every read takes a ceiling and stops at it. A server is free to answer
//! a request for sixty kilobytes with a gigabyte, and the only thing
//! standing between that and this process's memory is a limit that is
//! checked while the body arrives rather than after.

use crate::client::HttpConfig;

#[derive(Debug, thiserror::Error)]
pub enum RangeError {
    #[error("{0}")]
    Refused(String),
    #[error("http: {0}")]
    Http(String),
    #[error("the server sent more than the {cap} bytes asked for")]
    TooLarge { cap: usize },
}

type Result<T> = std::result::Result<T, RangeError>;

/// What came back, and whether the server honoured the range.
#[derive(Debug, Clone)]
pub struct Window {
    pub bytes: Vec<u8>,
    /// Where these bytes begin in the file, when the server said.
    ///
    /// `None` means it answered `200` and these are the opening bytes of
    /// the whole file — which a caller reading a tail must not mistake for
    /// the tail.
    pub offset: Option<u64>,
    /// The file's total length, when `Content-Range` stated it.
    pub total: Option<u64>,
}

impl Window {
    /// Whether the server actually gave the part that was asked for.
    pub fn is_ranged(&self) -> bool {
        self.offset.is_some()
    }
}

/// Reads `length` bytes from `at`.
pub async fn read_at(
    config: &HttpConfig,
    url: &str,
    at: u64,
    length: usize,
    cap: usize,
) -> Result<Window> {
    if length == 0 {
        return Err(RangeError::Refused("a read of nothing is not a read".into()));
    }
    let last = at + length as u64 - 1;
    fetch(config, url, &format!("bytes={at}-{last}"), cap).await
}

/// Reads the last `length` bytes, without needing to know the total.
///
/// `bytes=-N` is the only way to ask for a file's tail in one round trip,
/// and the tail is where a zip keeps its index. A server that does not
/// support it answers `200` with the whole file, which comes back with
/// `offset: None` so the caller can tell.
pub async fn read_suffix(
    config: &HttpConfig,
    url: &str,
    length: usize,
    cap: usize,
) -> Result<Window> {
    if length == 0 {
        return Err(RangeError::Refused("a read of nothing is not a read".into()));
    }
    fetch(config, url, &format!("bytes=-{length}"), cap).await
}

async fn fetch(config: &HttpConfig, url: &str, range: &str, cap: usize) -> Result<Window> {
    let client = crate::client::redirect_resolver(config)
        .map_err(|error| RangeError::Http(error.to_string()))?;

    let request = client.get(url).header(reqwest::header::RANGE, range);
    let (response, _) = crate::auth::send(request, config.auth.as_ref(), url)
        .await
        .map_err(|error| RangeError::Http(error.to_string()))?;

    let status = response.status();
    if !status.is_success() {
        return Err(RangeError::Http(format!("the server answered {status}")));
    }

    // Read before the body, because the body is what it describes.
    let content_range = response
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    let (offset, total) = match (status.as_u16(), content_range.as_deref()) {
        (206, Some(header)) => parse_content_range(header),
        // A `206` with no `Content-Range` is unusable: there is no way to
        // know which bytes arrived, and guessing is how the wrong part of
        // a file gets read as the right one.
        (206, None) => {
            return Err(RangeError::Refused(
                "the server said it sent part of the file but not which part".into(),
            ));
        }
        // `200` means the range was ignored and this is the whole file from
        // the beginning. Said plainly rather than passed off as the window
        // that was asked for.
        _ => (None, response.content_length()),
    };

    Ok(Window { bytes: drain(response, cap).await?, offset, total })
}

/// A whole document, fetched outright and capped.
///
/// For the small text files this program reads rather than downloads: a
/// page whose links are wanted, a `robots.txt` that says whether they may
/// be. No range is asked for, because these have no interesting middle --
/// either the whole thing fits under the cap or it is not the kind of file
/// this was for.
#[derive(Debug, Clone)]
pub struct Page {
    pub bytes: Vec<u8>,
    /// Where the body actually came from, after any redirects.
    ///
    /// Not the address that was asked for. Relative links in a page resolve
    /// against where the page arrived from, and a same-origin rule applied
    /// to the requested address would be trivially escaped by a redirect.
    pub url: String,
    /// The `Content-Type` header, verbatim and including any parameters.
    pub content_type: Option<String>,
    /// The status, so a caller can tell "there is no such file" from "that
    /// went wrong" -- which is the whole of how a missing `robots.txt` is
    /// read as permission.
    pub status: u16,
}

/// Fetches a whole document, giving up if it runs past `cap`.
pub async fn read_page(config: &HttpConfig, url: &str, cap: usize) -> Result<Page> {
    let client = crate::client::redirect_resolver(config)
        .map_err(|error| RangeError::Http(error.to_string()))?;

    let (response, _) = crate::auth::send(client.get(url), config.auth.as_ref(), url)
        .await
        .map_err(|error| RangeError::Http(error.to_string()))?;

    let status = response.status().as_u16();
    let landed = response.url().to_string();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    // The body is read whatever the status, because an error page's body is
    // the only thing that explains it -- but it is capped like any other.
    let bytes = drain(response, cap).await?;
    Ok(Page { bytes, url: landed, content_type, status })
}

/// Collects a body, refusing it the moment it passes `cap`.
async fn drain(response: reqwest::Response, cap: usize) -> Result<Vec<u8>> {
    use tokio_stream::StreamExt;

    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| RangeError::Http(error.to_string()))?;
        if body.len() + chunk.len() > cap {
            // Checked as it arrives, not after. A server is free to answer
            // a request for sixty kilobytes with a gigabyte, and waiting
            // for the end to find out means holding the gigabyte.
            return Err(RangeError::TooLarge { cap });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Pulls the start offset and the total out of `bytes 100-199/1234`.
///
/// Returns what it could read rather than failing: a `*` total is legal and
/// common, and a header this cannot parse is better treated as "the server
/// did not say" than as an error, because the bytes are still there.
fn parse_content_range(header: &str) -> (Option<u64>, Option<u64>) {
    let rest = match header.trim().strip_prefix("bytes") {
        Some(rest) => rest.trim(),
        None => return (None, None),
    };
    let Some((span, total)) = rest.split_once('/') else {
        return (None, None);
    };
    let start = span.trim().split('-').next().and_then(|at| at.trim().parse::<u64>().ok());
    let total = total.trim().parse::<u64>().ok();
    (start, total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_content_range_gives_up_its_offset_and_total() {
        assert_eq!(parse_content_range("bytes 100-199/1234"), (Some(100), Some(1234)));
        // The shape a suffix read comes back as.
        assert_eq!(parse_content_range("bytes 1200-1233/1234"), (Some(1200), Some(1234)));
    }

    #[test]
    fn a_withheld_total_still_gives_the_offset() {
        // Legal, and common behind a proxy that is streaming.
        assert_eq!(parse_content_range("bytes 100-199/*"), (Some(100), None));
    }

    #[test]
    fn a_header_that_makes_no_sense_is_not_an_error() {
        // The bytes arrived either way; treating this as a failure would
        // throw away a usable read over a header nobody needed.
        assert_eq!(parse_content_range("pages 1-2/3"), (None, None));
        assert_eq!(parse_content_range("bytes nonsense"), (None, None));
        assert_eq!(parse_content_range(""), (None, None));
    }

    #[test]
    fn whitespace_around_the_parts_is_tolerated() {
        assert_eq!(parse_content_range("  bytes  100-199 / 1234 "), (Some(100), Some(1234)));
    }

    #[tokio::test]
    async fn a_read_of_nothing_is_refused_rather_than_sent() {
        let config = HttpConfig::default();
        assert!(read_at(&config, "https://example.invalid/a", 0, 0, 1024).await.is_err());
        assert!(read_suffix(&config, "https://example.invalid/a", 0, 1024).await.is_err());
    }

    #[test]
    fn a_window_knows_whether_it_is_the_part_that_was_asked_for() {
        // The distinction a caller reading a tail depends on: without it,
        // the first bytes of a file get read as its last.
        let ranged = Window { bytes: vec![1], offset: Some(10), total: Some(100) };
        let whole = Window { bytes: vec![1], offset: None, total: Some(100) };
        assert!(ranged.is_ranged());
        assert!(!whole.is_ranged());
    }
}
