//! The probe decision table against real HTTP.
//!
//! `idm_core::sched::probe` is unit-tested against hand-written headers, which
//! proves the logic but not that the logic is fed the truth. These tests drive
//! real responses from the adversarial mock server through the real header
//! extraction and into the same table, so a mistake anywhere in that chain
//! shows up here.
//!
//! One test per pathology, matching the routes in `tests/mock-server`.

mod support;

use idm_core::ByteRange;
use idm_core::journal::ValidatorKind;
use idm_core::sched::probe::{ProbeOutcome, RestartReason, classify, validators_agree};
use idm_net::client::{HttpConfig, LanePool};
use idm_net::facts::OwnedFacts;
use reqwest::header::RANGE;
use support::MockServer;

const PROBE_LEN: u64 = 1 << 20;

/// Issues the engine's real probe — a ranged GET, never a HEAD — and runs the
/// response through extraction and classification.
async fn probe_at(url: &str, start: u64) -> (OwnedFacts, ProbeOutcome, ByteRange) {
    let pool = LanePool::new(&HttpConfig::default(), 1).expect("lane pool");
    let requested = ByteRange::new(start, start + PROBE_LEN);
    let response = pool
        .next_lane()
        .client()
        .get(url)
        .header(RANGE, requested.to_range_header().expect("non-empty probe"))
        .send()
        .await
        .expect("request should complete");

    let facts = OwnedFacts::from_response(&response);
    let outcome = classify(requested, &facts.as_facts());
    (facts, outcome, requested)
}

async fn probe(url: &str) -> (OwnedFacts, ProbeOutcome) {
    let (facts, outcome, _) = probe_at(url, 0).await;
    (facts, outcome)
}

#[tokio::test]
async fn an_honest_server_enables_parallel_fetching() {
    let server = MockServer::start();
    let (facts, outcome) = probe(&server.url("/honest?size=2000000")).await;

    assert_eq!(facts.status, 206);
    let ProbeOutcome::Ranged { got, total, validator } = outcome else {
        panic!("expected a ranged response, got {outcome:?}");
    };
    assert_eq!(got, ByteRange::new(0, PROBE_LEN));
    assert_eq!(total, 2_000_000);
    assert_eq!(validator, ValidatorKind::StrongETag);
    assert!(validator.usable_for_if_range());
}

#[tokio::test]
async fn a_server_that_ignores_range_keeps_its_body() {
    // Advertises Accept-Ranges and then ignores Range. Discarding a body
    // already arriving would be worse than the lie.
    let server = MockServer::start();
    let (facts, outcome) = probe(&server.url("/ignores-range?size=1048576")).await;

    assert_eq!(facts.status, 200);
    assert_eq!(facts.accept_ranges.as_deref(), Some("bytes"), "the server does claim ranges");
    assert_eq!(
        outcome,
        ProbeOutcome::SingleStreamFromZero { total: Some(1 << 20), resumable: false }
    );
}

#[tokio::test]
async fn declared_no_ranges_is_a_single_stream() {
    let server = MockServer::start();
    let (facts, outcome) = probe(&server.url("/no-accept-ranges?size=524288")).await;
    assert_eq!(facts.accept_ranges.as_deref(), Some("none"));
    assert!(matches!(outcome, ProbeOutcome::SingleStreamFromZero { .. }));
}

#[tokio::test]
async fn a_missing_accept_ranges_header_does_not_deny_range_support() {
    // The header's absence proves nothing: plenty of servers omit it and
    // honour ranges perfectly, which is why the probe is always sent.
    let server = MockServer::start();
    let (facts, outcome) = probe(&server.url("/omits-accept-ranges?size=4000000")).await;

    assert_eq!(facts.accept_ranges, None, "route sends no Accept-Ranges");
    assert!(
        matches!(outcome, ProbeOutcome::Ranged { .. }),
        "behaviour must win over the missing header, got {outcome:?}"
    );
}

#[tokio::test]
async fn a_range_caching_proxy_answering_the_wrong_offset_is_refused() {
    // The dangerous one: a 206 whose Content-Range names a different offset.
    // Writing those bytes where we asked would corrupt the file in a way no
    // length check could detect.
    let server = MockServer::start();
    let (facts, outcome, _) = probe_at(&server.url("/bad-content-range?size=65536000"), 8192).await;

    assert_eq!(facts.status, 206);
    assert_eq!(
        outcome,
        ProbeOutcome::Restart(RestartReason::RangeStartMismatch { expected: 8192, got: 4096 })
    );
}

#[tokio::test]
async fn a_short_range_reports_what_actually_arrived() {
    // The end must come from Content-Range, never from what was asked for.
    let server = MockServer::start();
    let (_, outcome, requested) = probe_at(&server.url("/short-range?size=65536000"), 0).await;

    let ProbeOutcome::Ranged { got, total, .. } = outcome else {
        panic!("expected a ranged response, got {outcome:?}");
    };
    assert_eq!(got.start(), requested.start());
    assert_eq!(got.len(), PROBE_LEN / 2, "route serves half of what is asked");
    assert!(got.len() < requested.len());
    assert_eq!(total, 65_536_000);
}

#[tokio::test]
async fn gzip_on_a_ranged_response_forces_a_single_stream() {
    // Offsets would address the encoded entity, so every byte position in the
    // response means something other than what the scheduler assumes.
    let server = MockServer::start();
    let (facts, outcome) = probe(&server.url("/content-encoding?size=65536")).await;

    assert_eq!(facts.content_encoding.as_deref(), Some("gzip"));
    assert_eq!(outcome, ProbeOutcome::SingleStreamUnknownLength);
}

#[tokio::test]
async fn the_client_never_advertises_a_content_coding() {
    // The other half of the defence: decompression features are not enabled
    // and Accept-Encoding is pinned to identity, so a well-behaved server has
    // no reason to encode in the first place.
    let server = MockServer::start();
    let (facts, outcome) = probe(&server.url("/honest?size=1048576")).await;

    assert_eq!(facts.content_encoding, None, "an honest server should not encode");
    assert!(matches!(outcome, ProbeOutcome::Ranged { .. }));
}

#[tokio::test]
async fn chunked_with_no_length_is_unknown_length() {
    let server = MockServer::start();
    let (facts, outcome) = probe(&server.url("/chunked?size=4096")).await;
    assert!(facts.chunked, "transfer-encoding should be chunked");
    assert_eq!(facts.content_length, None);
    assert_eq!(outcome, ProbeOutcome::SingleStreamUnknownLength);
}

#[tokio::test]
async fn a_416_yields_the_real_total() {
    let server = MockServer::start();
    let (facts, outcome) = probe(&server.url("/always-416?size=4096")).await;
    assert_eq!(facts.status, 416);
    assert_eq!(outcome, ProbeOutcome::Reprobe { total: Some(4096) });
}

#[tokio::test]
async fn an_expired_signed_url_is_re_resolved_then_works() {
    let server = MockServer::start();

    let (_, denied) = probe(&server.url("/expiring?size=1048576")).await;
    assert_eq!(denied, ProbeOutcome::ReResolve, "no signature should ask for a re-resolve");

    let (_, allowed) = probe(&server.url("/expiring?size=1048576&sig=fresh")).await;
    assert!(
        matches!(allowed, ProbeOutcome::Ranged { .. }),
        "a fresh signature should work, got {allowed:?}"
    );
}

#[tokio::test]
async fn a_throttling_server_is_detected_with_its_retry_after() {
    let server = MockServer::start();
    // The route 429s every second request, so one of the first two must.
    let mut saw_throttle = false;
    for _ in 0..4 {
        let (facts, outcome) = probe(&server.url("/throttle-429?size=524288")).await;
        if outcome == ProbeOutcome::Throttled {
            saw_throttle = true;
            assert_eq!(facts.status, 429);
            assert_eq!(
                facts.retry_after,
                Some(std::time::Duration::from_secs(2)),
                "Retry-After must be honoured exactly, not guessed"
            );
            break;
        }
    }
    assert!(saw_throttle, "the route should have throttled at least once");
}

#[tokio::test]
async fn a_weak_etag_is_not_usable_for_resume() {
    let server = MockServer::start();
    let (facts, outcome) = probe(&server.url("/weak-etag?size=4000000")).await;

    assert!(facts.etag.as_deref().unwrap_or_default().starts_with("W/"));
    let ProbeOutcome::Ranged { validator, .. } = outcome else {
        panic!("expected a ranged response, got {outcome:?}");
    };
    assert_eq!(validator, ValidatorKind::WeakETag);
    assert!(!validator.usable_for_if_range(), "a weak ETag permits byte-different representations");
}

#[tokio::test]
async fn a_same_second_last_modified_is_not_trusted() {
    // HTTP dates have one-second granularity, so a modification inside the
    // same second as the response is undetectable.
    let server = MockServer::start();
    let (facts, outcome) = probe(&server.url("/same-second-mtime?size=1048576")).await;

    assert_eq!(facts.etag, None, "route deliberately omits any ETag");
    assert!(facts.last_modified.is_some());
    assert!(
        facts.last_modified_age_secs.unwrap_or(i64::MAX) < 60,
        "age was {:?}",
        facts.last_modified_age_secs
    );

    let ProbeOutcome::Ranged { validator, .. } = outcome else {
        panic!("expected a ranged response, got {outcome:?}");
    };
    assert_ne!(validator, ValidatorKind::LastModified, "must not trust a same-second date");
    assert!(!validator.usable_for_if_range());
}

#[tokio::test]
async fn an_honest_servers_last_modified_is_trusted_when_old_enough() {
    let server = MockServer::start();
    let (facts, _) = probe(&server.url("/honest?size=1048576")).await;
    // The route dates the file to 2020, so the age is years, not seconds.
    assert!(
        facts.last_modified_age_secs.unwrap_or(0) > 60,
        "age was {:?}",
        facts.last_modified_age_secs
    );
}

#[tokio::test]
async fn a_representation_changing_mid_download_is_detected() {
    // The silent-corruption case: splicing two server versions into one file
    // passes every length check. The route flips its ETag on the third hit.
    let server = MockServer::start();
    let url = server.url("/etag-flips?size=1048576");

    let (first, _) = probe(&url).await;
    let original = first.etag.clone().expect("an ETag");

    let mut detected = false;
    for _ in 0..4 {
        let (later, _) = probe(&url).await;
        if !validators_agree(Some(&original), None, &later.as_facts(), None) {
            detected = true;
            assert_ne!(later.etag, first.etag, "detection should follow a real change");
            break;
        }
    }
    assert!(detected, "a changed ETag must be noticed");
}

#[tokio::test]
async fn head_is_not_trusted_because_it_disagrees_with_get() {
    // Why the probe is a ranged GET: this route reports double the length to
    // HEAD. A HEAD-based probe would preallocate twice the file and compute
    // every segment boundary from a number that was never true.
    let server = MockServer::start();
    let url = server.url("/head-disagrees?size=1000");

    let pool = LanePool::new(&HttpConfig::default(), 1).unwrap();
    let head = pool.next_lane().client().head(&url).send().await.expect("HEAD should complete");
    let head_length = OwnedFacts::from_response(&head).content_length;
    assert_eq!(head_length, Some(2000), "route lies to HEAD");

    // The ranged GET is authoritative.
    let (_, outcome) = probe(&url).await;
    let ProbeOutcome::Ranged { total, .. } = outcome else {
        panic!("expected a ranged response, got {outcome:?}");
    };
    assert_eq!(total, 1000, "the GET's total is the real one");
    assert_ne!(Some(total), head_length);
}

#[tokio::test]
async fn a_404_asks_for_a_fresh_address() {
    let server = MockServer::start();
    let (_, outcome) = probe(&server.url("/does-not-exist")).await;
    assert_eq!(outcome, ProbeOutcome::NeedsNewUrl);
}
