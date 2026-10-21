//! Turning a live response into the facts the scheduler reasons about.
//!
//! The decision table in `idm_core::sched::probe` is pure and borrows `&str`,
//! so this module owns the extracted strings and lends them out. Keeping the
//! two apart means the table can be exhaustively tested without HTTP, while
//! this module's job shrinks to reading headers correctly.
//!
//! Three details are worth stating, because getting any of them wrong feeds
//! the table a plausible lie:
//!
//! * `Content-Length` is read from the raw header, not from
//!   `Response::content_length()`. On a `206` the header describes the
//!   *range*, and a helper that normalises or synthesises it would blur the
//!   distinction the table depends on.
//! * The `Last-Modified` age is measured against the response's own `Date`
//!   where one exists, not against local time, because the client's clock may
//!   be wrong by far more than the sixty seconds the test turns on.
//! * `Retry-After` comes in two syntaxes — delta-seconds and an HTTP date —
//!   and both appear in the wild.

use std::time::{Duration, SystemTime};

use idm_core::sched::probe::ResponseFacts;
use reqwest::Response;
use reqwest::header::{
    ACCEPT_RANGES, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, DATE, ETAG, HeaderMap,
    LAST_MODIFIED, RETRY_AFTER, TRANSFER_ENCODING,
};

/// Owned copies of everything the probe table needs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OwnedFacts {
    pub status: u16,
    pub content_range: Option<String>,
    pub content_length: Option<u64>,
    pub accept_ranges: Option<String>,
    pub content_encoding: Option<String>,
    pub chunked: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// Seconds by which `Last-Modified` precedes the response `Date`.
    /// Negative when a server reports a future modification time.
    pub last_modified_age_secs: Option<i64>,
    /// Parsed from either syntax, for honouring a throttle exactly.
    pub retry_after: Option<Duration>,
    /// The URL actually served, after any redirects the resolver followed.
    pub final_url: String,
    /// Negotiated protocol, e.g. "HTTP/1.1" or "HTTP/2.0". Worth surfacing:
    /// the HTTP/2 flow-control window only matters when h2 was actually
    /// negotiated, so a measurement claiming to test it must prove it did.
    pub http_version: String,
}

fn header_string(headers: &HeaderMap, name: reqwest::header::HeaderName) -> Option<String> {
    headers.get(name)?.to_str().ok().map(str::to_owned)
}

/// Seconds `last_modified` precedes `reference`.
fn age_secs(last_modified: SystemTime, reference: SystemTime) -> i64 {
    match reference.duration_since(last_modified) {
        Ok(elapsed) => elapsed.as_secs() as i64,
        // A modification time in the future: report it as negative rather
        // than clamping, so it can never pass the freshness test.
        Err(error) => -(error.duration().as_secs() as i64),
    }
}

/// Parses `Retry-After`, which may be delta-seconds or an HTTP date.
pub fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let trimmed = value.trim();
    if let Ok(seconds) = trimmed.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let deadline = httpdate::parse_http_date(trimmed).ok()?;
    // A date already in the past means retry immediately, not never.
    Some(deadline.duration_since(now).unwrap_or(Duration::ZERO))
}

impl OwnedFacts {
    /// Extracts the facts from a response, without consuming its body.
    pub fn from_response(response: &Response) -> Self {
        let headers = response.headers();

        let content_length = headers
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok());

        let chunked = headers
            .get(TRANSFER_ENCODING)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"));

        let last_modified = header_string(headers, LAST_MODIFIED);
        // Against the server's own Date where available: the local clock can
        // be out by far more than the sixty-second threshold this feeds.
        let reference = header_string(headers, DATE)
            .and_then(|value| httpdate::parse_http_date(&value).ok())
            .unwrap_or_else(SystemTime::now);
        let last_modified_age_secs = last_modified
            .as_deref()
            .and_then(|value| httpdate::parse_http_date(value).ok())
            .map(|modified| age_secs(modified, reference));

        let retry_after = header_string(headers, RETRY_AFTER)
            .and_then(|value| parse_retry_after(&value, reference));

        Self {
            status: response.status().as_u16(),
            content_range: header_string(headers, CONTENT_RANGE),
            content_length,
            accept_ranges: header_string(headers, ACCEPT_RANGES),
            content_encoding: header_string(headers, CONTENT_ENCODING),
            chunked,
            etag: header_string(headers, ETAG),
            last_modified,
            last_modified_age_secs,
            retry_after,
            final_url: response.url().as_str().to_owned(),
            http_version: format!("{:?}", response.version()),
        }
    }

    /// Borrows these facts in the shape the decision table expects.
    pub fn as_facts(&self) -> ResponseFacts<'_> {
        ResponseFacts {
            status: self.status,
            content_range: self.content_range.as_deref(),
            content_length: self.content_length,
            accept_ranges: self.accept_ranges.as_deref(),
            content_encoding: self.content_encoding.as_deref(),
            chunked: self.chunked,
            etag: self.etag.as_deref(),
            last_modified_age_secs: self.last_modified_age_secs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: Duration = Duration::from_secs(3600);

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn retry_after_accepts_delta_seconds() {
        assert_eq!(parse_retry_after("120", at(0)), Some(Duration::from_secs(120)));
        assert_eq!(parse_retry_after("  7 ", at(0)), Some(Duration::from_secs(7)));
        assert_eq!(parse_retry_after("0", at(0)), Some(Duration::ZERO));
    }

    #[test]
    fn retry_after_accepts_an_http_date() {
        // Both syntaxes appear in the wild, so both must work.
        let now = at(1_000_000_000);
        let later = now + HOUR;
        let formatted = httpdate::fmt_http_date(later);
        let parsed = parse_retry_after(&formatted, now).expect("should parse a date");
        // Allow a second of rounding through the text form.
        assert!(
            parsed.as_secs().abs_diff(HOUR.as_secs()) <= 1,
            "expected about an hour, got {parsed:?}"
        );
    }

    #[test]
    fn a_retry_after_in_the_past_means_retry_now() {
        let now = at(1_000_000_000);
        let formatted = httpdate::fmt_http_date(now - HOUR);
        assert_eq!(parse_retry_after(&formatted, now), Some(Duration::ZERO));
    }

    #[test]
    fn nonsense_retry_after_is_ignored_rather_than_guessed() {
        assert_eq!(parse_retry_after("soon please", at(0)), None);
        assert_eq!(parse_retry_after("", at(0)), None);
    }

    #[test]
    fn last_modified_age_is_measured_forwards() {
        let date = at(1_000_000_000);
        assert_eq!(age_secs(date - HOUR, date), 3600);
        assert_eq!(age_secs(date, date), 0);
    }

    #[test]
    fn a_future_modification_time_reports_negative_not_zero() {
        // Clamping to zero would let it slip past the sixty-second freshness
        // test as if it were merely recent.
        let date = at(1_000_000_000);
        let age = age_secs(date + HOUR, date);
        assert_eq!(age, -3600);
        assert!(age < idm_core::sched::probe::MIN_LAST_MODIFIED_AGE_SECS);
    }
}
