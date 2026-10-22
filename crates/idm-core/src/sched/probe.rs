//! Working out what a server actually did, as opposed to what it claimed.
//!
//! # Why the probe is a ranged GET and never a HEAD
//!
//! HEAD looks like the obvious way to learn a file's size. It is wrong often
//! enough to matter: it gets answered from a different cache tier, returns 405
//! on some origins, is rejected outright by some signed URLs, and can report a
//! `Content-Length` that disagrees with the GET that follows. So the probe *is*
//! the first ranged GET, and doubles as the first segment — no wasted round
//! trip either.
//!
//! # Two header rules that are easy to get backwards
//!
//! * `Accept-Ranges: bytes` is **advisory only**. Only an actual `206` with a
//!   correct `Content-Range` proves range support.
//! * The **absence** of `Accept-Ranges` proves nothing. Plenty of servers omit
//!   it and honour ranges perfectly, so the ranged probe is always worth
//!   sending.
//!
//! Everything here is a pure function over observed facts, so each row of the
//! table can be tested directly. The rows correspond one-to-one with the routes
//! in `tests/mock-server`.

use crate::extent::{ByteRange, ContentRange, parse_content_range};
use crate::journal::ValidatorKind;

/// A `Last-Modified` must be at least this much older than the response `Date`
/// to be trusted. HTTP dates have one-second granularity, so a modification
/// within the same second as the response is undetectable.
pub const MIN_LAST_MODIFIED_AGE_SECS: i64 = 60;

/// What the response told us, with dates already resolved by the transport.
#[derive(Debug, Clone, Default)]
pub struct ResponseFacts<'a> {
    pub status: u16,
    pub content_range: Option<&'a str>,
    pub content_length: Option<u64>,
    /// Advisory only; see the module docs.
    pub accept_ranges: Option<&'a str>,
    /// Anything other than `identity` makes byte offsets meaningless.
    pub content_encoding: Option<&'a str>,
    pub chunked: bool,
    pub etag: Option<&'a str>,
    /// How much older `Last-Modified` is than the response `Date`, in seconds.
    pub last_modified_age_secs: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartReason {
    /// A `206` whose `Content-Range` could not be parsed at all.
    ContentRangeUnparseable,
    /// A `206` for a different offset than was asked for: the signature of a
    /// broken range-caching proxy. Trusting it writes one offset's bytes at
    /// another offset, which no length check would catch.
    RangeStartMismatch {
        expected: u64,
        got: u64,
    },
    /// `Content-Range` claims an end past the total it also states.
    InconsistentTotal,
    UnexpectedStatus(u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// Ranges genuinely work. `got` is what this response actually covers,
    /// which may be less than requested; the caller requeues the residue.
    Ranged {
        got: ByteRange,
        total: u64,
        validator: ValidatorKind,
    },
    /// Ranges work but the total is withheld (`bytes 0-N/*`). The caller
    /// should issue a `bytes=-1` suffix request to learn it.
    NeedsSuffixProbe {
        got: ByteRange,
        validator: ValidatorKind,
    },
    /// The server ignored `Range` and is sending the whole body with `200`.
    ///
    /// The body must be **kept and streamed from offset zero**, not discarded:
    /// throwing away a response that is already arriving can cost a gigabyte
    /// of transfer for nothing.
    SingleStreamFromZero {
        total: Option<u64>,
        resumable: bool,
    },
    /// No length available, so no preallocation and no resume.
    SingleStreamUnknownLength,
    /// Our idea of the total was wrong; re-probe from scratch.
    Reprobe {
        total: Option<u64>,
    },
    /// A signed URL expired. Re-resolve the original URL for a fresh
    /// signature and carry on from where we were.
    ReResolve,
    /// Gone for good; the user must supply a refreshed address. The partial
    /// file and journal are kept so they can.
    NeedsNewUrl,
    /// Back off, honour `Retry-After`, and do **not** retry on a fresh
    /// connection — that is what turns a 429 into a temporary IP ban.
    Throttled,
    Restart(RestartReason),
}

/// Picks the strongest usable validator.
///
/// A weak ETag permits byte-different-but-equivalent representations, so it is
/// invalid for `If-Range` and is recorded as weak rather than silently treated
/// as strong.
pub fn pick_validator(facts: &ResponseFacts<'_>) -> ValidatorKind {
    if let Some(etag) = facts.etag {
        let trimmed = etag.trim();
        if trimmed.starts_with("W/") || trimmed.starts_with("w/") {
            return ValidatorKind::WeakETag;
        }
        if !trimmed.is_empty() {
            return ValidatorKind::StrongETag;
        }
    }
    if let Some(age) = facts.last_modified_age_secs
        && age >= MIN_LAST_MODIFIED_AGE_SECS
    {
        return ValidatorKind::LastModified;
    }
    if facts.content_length.is_some() {
        return ValidatorKind::LengthOnly;
    }
    ValidatorKind::None
}

/// Whether a content coding makes byte offsets meaningless.
///
/// A ranged response that is also encoded addresses the *encoded* entity, so
/// our offsets no longer mean what we think. The HTTP client is configured with
/// decompression disabled for exactly this reason; if a coding still appears,
/// the only safe move is a single stream.
fn encoding_breaks_offsets(encoding: Option<&str>) -> bool {
    match encoding {
        None => false,
        Some(value) => {
            let value = value.trim();
            !(value.is_empty() || value.eq_ignore_ascii_case("identity"))
        }
    }
}

/// Classifies a probe response against what was requested.
pub fn classify(requested: ByteRange, facts: &ResponseFacts<'_>) -> ProbeOutcome {
    match facts.status {
        429 | 503 | 509 => return ProbeOutcome::Throttled,
        500 | 502 | 504 => return ProbeOutcome::Throttled,
        403 => return ProbeOutcome::ReResolve,
        404 | 410 => return ProbeOutcome::NeedsNewUrl,
        416 => {
            // Carries `bytes */total`, which tells us the real size.
            let total = facts
                .content_range
                .and_then(|value| parse_content_range(value).ok())
                .and_then(|parsed| parsed.total());
            return ProbeOutcome::Reprobe { total };
        }
        _ => {}
    }

    // Checked before anything offset-based, because an encoded body makes
    // every offset in the response refer to the wrong thing.
    if encoding_breaks_offsets(facts.content_encoding) {
        return ProbeOutcome::SingleStreamUnknownLength;
    }

    let validator = pick_validator(facts);

    match facts.status {
        206 => {
            let Some(header) = facts.content_range else {
                // A 206 with no Content-Range is unusable: there is no way to
                // know which bytes arrived.
                return ProbeOutcome::Restart(RestartReason::ContentRangeUnparseable);
            };
            let parsed = match parse_content_range(header) {
                Ok(parsed) => parsed,
                Err(crate::extent::ExtentError::ExceedsTotal { .. }) => {
                    return ProbeOutcome::Restart(RestartReason::InconsistentTotal);
                }
                Err(_) => {
                    return ProbeOutcome::Restart(RestartReason::ContentRangeUnparseable);
                }
            };

            match parsed {
                ContentRange::Unsatisfied { total } => ProbeOutcome::Reprobe { total: Some(total) },
                ContentRange::Satisfied { range, total } => {
                    if range.start() != requested.start() {
                        // A broken range-caching proxy. Writing these bytes at
                        // the offset we asked for would corrupt the file in a
                        // way no length check could detect.
                        return ProbeOutcome::Restart(RestartReason::RangeStartMismatch {
                            expected: requested.start(),
                            got: range.start(),
                        });
                    }
                    match total {
                        Some(total) => ProbeOutcome::Ranged { got: range, total, validator },
                        None => ProbeOutcome::NeedsSuffixProbe { got: range, validator },
                    }
                }
            }
        }

        200 => {
            if facts.chunked || facts.content_length.is_none() {
                return ProbeOutcome::SingleStreamUnknownLength;
            }
            // We asked for a range and got the whole thing, so ranges are not
            // honoured however the headers are decorated. Resume is impossible
            // without them.
            ProbeOutcome::SingleStreamFromZero { total: facts.content_length, resumable: false }
        }

        other => ProbeOutcome::Restart(RestartReason::UnexpectedStatus(other)),
    }
}

/// Whether a response's validators still match what the download started with.
///
/// Checked on **every** response, not just on resume. A representation that
/// changes mid-download and is spliced into the same file passes every length
/// check and is silently corrupt, so a mismatch has to stop the download
/// rather than be smoothed over.
pub fn validators_agree(
    expected_etag: Option<&str>,
    expected_total: Option<u64>,
    facts: &ResponseFacts<'_>,
    observed_total: Option<u64>,
) -> bool {
    if let (Some(expected), Some(seen)) = (expected_etag, facts.etag)
        && expected.trim() != seen.trim()
    {
        return false;
    }
    if let (Some(expected), Some(seen)) = (expected_total, observed_total)
        && expected != seen
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROBE: ByteRange = ByteRange::new_const(0, 1 << 20);

    fn facts<'a>(status: u16) -> ResponseFacts<'a> {
        ResponseFacts { status, ..Default::default() }
    }

    // Each test below mirrors a route in tests/mock-server.

    #[test]
    fn honest_ranged_response_enables_parallel_fetching() {
        let mut f = facts(206);
        f.content_range = Some("bytes 0-1048575/2000000");
        f.accept_ranges = Some("bytes");
        f.etag = Some("\"abc\"");

        assert_eq!(
            classify(PROBE, &f),
            ProbeOutcome::Ranged {
                got: ByteRange::new(0, 1 << 20),
                total: 2_000_000,
                validator: ValidatorKind::StrongETag,
            }
        );
    }

    #[test]
    fn a_server_that_ignores_range_keeps_its_body() {
        // /ignores-range. Discarding a gigabyte already in flight would be
        // worse than the lie itself.
        let mut f = facts(200);
        f.content_length = Some(8 << 20);
        f.accept_ranges = Some("bytes"); // the lie
        assert_eq!(
            classify(PROBE, &f),
            ProbeOutcome::SingleStreamFromZero { total: Some(8 << 20), resumable: false }
        );
    }

    #[test]
    fn missing_accept_ranges_does_not_deny_range_support() {
        // /omits-accept-ranges. The header's absence proves nothing, so a 206
        // still confirms ranges.
        let mut f = facts(206);
        f.content_range = Some("bytes 0-1048575/4000000");
        f.accept_ranges = None;
        assert!(matches!(classify(PROBE, &f), ProbeOutcome::Ranged { .. }));
    }

    #[test]
    fn accept_ranges_alone_never_confirms_range_support() {
        // The converse: the header says yes, the behaviour says no, and
        // behaviour wins.
        let mut f = facts(200);
        f.accept_ranges = Some("bytes");
        f.content_length = Some(1 << 20);
        assert!(matches!(classify(PROBE, &f), ProbeOutcome::SingleStreamFromZero { .. }));
    }

    #[test]
    fn a_skewed_content_range_is_refused_outright() {
        // /bad-content-range: a broken caching proxy answering for the wrong
        // offset. This is the one that would silently corrupt the file.
        let mut f = facts(206);
        f.content_range = Some("bytes 4096-1052671/65536000");
        let requested = ByteRange::new(8192, 8192 + (1 << 20));
        assert_eq!(
            classify(requested, &f),
            ProbeOutcome::Restart(RestartReason::RangeStartMismatch { expected: 8192, got: 4096 })
        );
    }

    #[test]
    fn a_short_but_legal_range_reports_what_actually_arrived() {
        // /short-range. The end comes from Content-Range, never from what we
        // asked for, and the caller requeues the residue.
        let mut f = facts(206);
        f.content_range = Some("bytes 0-524287/65536000");
        let ProbeOutcome::Ranged { got, total, .. } = classify(PROBE, &f) else {
            panic!("expected a ranged response");
        };
        assert_eq!(got, ByteRange::new(0, 512 << 10));
        assert!(got.len() < PROBE.len(), "should report the shorter actual range");
        assert_eq!(total, 65_536_000);
    }

    #[test]
    fn a_withheld_total_asks_for_a_suffix_probe() {
        let mut f = facts(206);
        f.content_range = Some("bytes 0-1048575/*");
        f.etag = Some("\"x\"");
        assert_eq!(
            classify(PROBE, &f),
            ProbeOutcome::NeedsSuffixProbe {
                got: ByteRange::new(0, 1 << 20),
                validator: ValidatorKind::StrongETag,
            }
        );
    }

    #[test]
    fn a_content_encoded_range_falls_back_to_a_single_stream() {
        // /content-encoding. Offsets would address the encoded entity, so
        // every byte position in the response means something else.
        let mut f = facts(206);
        f.content_range = Some("bytes 0-1048575/65536000");
        f.content_encoding = Some("gzip");
        assert_eq!(classify(PROBE, &f), ProbeOutcome::SingleStreamUnknownLength);

        // `identity` is not an encoding and must not trigger the fallback.
        f.content_encoding = Some("identity");
        assert!(matches!(classify(PROBE, &f), ProbeOutcome::Ranged { .. }));
    }

    #[test]
    fn chunked_with_no_length_is_unknown_length() {
        // /chunked: no total, so no preallocation and no resume.
        let mut f = facts(200);
        f.chunked = true;
        assert_eq!(classify(PROBE, &f), ProbeOutcome::SingleStreamUnknownLength);
    }

    #[test]
    fn a_416_carries_the_real_total() {
        // /always-416, and the case where our stored total was simply wrong.
        let mut f = facts(416);
        f.content_range = Some("bytes */4096");
        assert_eq!(classify(PROBE, &f), ProbeOutcome::Reprobe { total: Some(4096) });
    }

    #[test]
    fn an_expired_signed_url_is_re_resolved_not_failed() {
        // /expiring. The commonest real resume failure: the link aged out.
        assert_eq!(classify(PROBE, &facts(403)), ProbeOutcome::ReResolve);
    }

    #[test]
    fn a_gone_url_asks_the_user_for_a_new_one() {
        assert_eq!(classify(PROBE, &facts(404)), ProbeOutcome::NeedsNewUrl);
        assert_eq!(classify(PROBE, &facts(410)), ProbeOutcome::NeedsNewUrl);
    }

    #[test]
    fn throttling_statuses_are_recognised() {
        // /throttle-429, plus the transient server errors worth retrying.
        for status in [429, 503, 509, 500, 502, 504] {
            assert_eq!(
                classify(PROBE, &facts(status)),
                ProbeOutcome::Throttled,
                "status {status} should back off"
            );
        }
    }

    #[test]
    fn an_unparseable_content_range_restarts_rather_than_guesses() {
        let mut f = facts(206);
        f.content_range = Some("bytes garbage");
        assert_eq!(
            classify(PROBE, &f),
            ProbeOutcome::Restart(RestartReason::ContentRangeUnparseable)
        );

        // A 206 with no Content-Range at all is equally unusable.
        let f = facts(206);
        assert_eq!(
            classify(PROBE, &f),
            ProbeOutcome::Restart(RestartReason::ContentRangeUnparseable)
        );
    }

    #[test]
    fn a_content_range_past_its_own_total_is_rejected() {
        let mut f = facts(206);
        f.content_range = Some("bytes 0-199/100");
        assert_eq!(classify(PROBE, &f), ProbeOutcome::Restart(RestartReason::InconsistentTotal));
    }

    #[test]
    fn a_weak_etag_is_recorded_as_weak() {
        // /weak-etag. Treating it as strong would allow a resume against a
        // byte-different representation.
        let mut f = facts(206);
        f.content_range = Some("bytes 0-1048575/4000000");
        f.etag = Some("W/\"weak-1\"");
        let ProbeOutcome::Ranged { validator, .. } = classify(PROBE, &f) else {
            panic!("expected a ranged response");
        };
        assert_eq!(validator, ValidatorKind::WeakETag);
        assert!(!validator.usable_for_if_range(), "a weak ETag must not be used with If-Range");
    }

    #[test]
    fn a_same_second_last_modified_is_not_trusted() {
        // /same-second-mtime. One-second granularity makes a modification
        // within that second invisible.
        let mut f = facts(206);
        f.content_range = Some("bytes 0-1048575/4000000");
        f.content_length = Some(1 << 20);
        f.last_modified_age_secs = Some(0);
        let ProbeOutcome::Ranged { validator, .. } = classify(PROBE, &f) else {
            panic!("expected a ranged response");
        };
        assert_eq!(validator, ValidatorKind::LengthOnly, "must fall back, not trust the date");

        // Comfortably older is fine.
        f.last_modified_age_secs = Some(3600);
        let ProbeOutcome::Ranged { validator, .. } = classify(PROBE, &f) else {
            panic!("expected a ranged response");
        };
        assert_eq!(validator, ValidatorKind::LastModified);
        assert!(validator.usable_for_if_range());
    }

    #[test]
    fn validator_preference_runs_strongest_first() {
        let strong = ResponseFacts { etag: Some("\"s\""), ..Default::default() };
        assert_eq!(pick_validator(&strong), ValidatorKind::StrongETag);

        let nothing = ResponseFacts::default();
        assert_eq!(pick_validator(&nothing), ValidatorKind::None);
        assert!(!ValidatorKind::None.usable_for_if_range());
    }

    #[test]
    fn a_changed_etag_is_detected() {
        // /etag-flips. Blending two representations passes every length check
        // and is silently corrupt, so this must be caught on every response.
        let mut f = facts(206);
        f.etag = Some("\"gen-2\"");
        assert!(!validators_agree(Some("\"gen-1\""), None, &f, None));
        assert!(validators_agree(Some("\"gen-2\""), None, &f, None));
    }

    #[test]
    fn a_changed_total_is_detected() {
        // /lies-about-length, and any load balancer serving two sizes.
        let f = facts(206);
        assert!(!validators_agree(None, Some(1000), &f, Some(2000)));
        assert!(validators_agree(None, Some(1000), &f, Some(1000)));
    }

    #[test]
    fn an_unexpected_status_restarts_rather_than_proceeding() {
        assert_eq!(
            classify(PROBE, &facts(204)),
            ProbeOutcome::Restart(RestartReason::UnexpectedStatus(204))
        );
    }
}
