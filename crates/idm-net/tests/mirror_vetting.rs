//! Whether a second address is allowed to contribute bytes to one file.
//!
//! These run against the mock server's mirror routes, all three of which are
//! honest servers. The question here is never whether a mirror behaves, it
//! is whether it is serving the same file — which is a different question
//! and the only one that stops multi-source downloading from producing
//! silent corruption.

mod support;

use idm_net::download::{DownloadConfig, Target, probe};
use idm_net::mirror::{self, Fingerprint, Refusal};
use support::MockServer;

/// Big enough that head and tail are sampled separately rather than the
/// whole file being read.
const SIZE: u64 = 8 << 20;

async fn primary(server: &MockServer) -> (DownloadConfig, Target, Fingerprint) {
    let config =
        DownloadConfig::new(server.url(&format!("/honest?size={SIZE}")), std::env::temp_dir());
    let target = probe(&config).await.expect("the primary should probe");
    let print = mirror::fingerprint(&config, &target).await.expect("the primary should sample");
    (config, target, print)
}

#[tokio::test]
async fn a_mirror_serving_the_same_bytes_is_accepted() {
    let server = MockServer::start();
    let (config, primary_target, print) = primary(&server).await;

    let url = server.url(&format!("/mirror-agrees?size={SIZE}"));
    let accepted = mirror::vet(&config, &print, &url).await.expect("should be accepted");

    assert_eq!(accepted.total, SIZE);
    // The whole point of the route: its ETag is its own, as a real mirror's
    // is. If these matched, this test would not be proving that an ETag
    // disagreement is tolerated.
    assert_ne!(
        accepted.etag, primary_target.etag,
        "the mirror should have had its own ETag, or this test proves nothing"
    );
}

#[tokio::test]
async fn a_mirror_differing_only_in_its_tail_is_refused() {
    let server = MockServer::start();
    let (config, _target, print) = primary(&server).await;

    let url = server.url(&format!("/mirror-tail-differs?size={SIZE}"));
    match mirror::vet(&config, &print, &url).await {
        Err(Refusal::DifferentBytes { at }) => {
            // The offset is itself the proof that sampling the head alone
            // would have accepted this mirror: windows are compared in
            // order and the first difference is the one reported, so a
            // difference found this late means the head agreed.
            assert!(
                at >= SIZE - (1 << 20),
                "the difference should have been found in the tail, got offset {at}"
            );
        }
        other => panic!("a different build must be refused, got {other:?}"),
    }
}

#[tokio::test]
async fn a_mirror_of_a_different_length_is_refused() {
    let server = MockServer::start();
    let (config, _target, print) = primary(&server).await;

    let url = server.url(&format!("/mirror-shorter?size={SIZE}"));
    match mirror::vet(&config, &print, &url).await {
        Err(Refusal::DifferentLength { primary, mirror }) => {
            assert_eq!(primary, SIZE);
            assert_eq!(mirror, SIZE - 1);
        }
        other => panic!("a different length must be refused, got {other:?}"),
    }
}

#[tokio::test]
async fn an_address_that_cannot_be_split_is_unusable_rather_than_accused() {
    // A server that ignores Range is no use as a mirror, but it is not
    // serving the wrong file either, and saying so would be a lie in a log.
    let server = MockServer::start();
    let (config, _target, print) = primary(&server).await;

    let url = server.url(&format!("/ignores-range?size={SIZE}"));
    match mirror::vet(&config, &print, &url).await {
        Err(Refusal::Unusable(_)) => {}
        other => panic!("expected an Unusable refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn an_address_that_is_not_there_is_unusable() {
    let server = MockServer::start();
    let (config, _target, print) = primary(&server).await;

    let url = server.url("/nothing-here");
    match mirror::vet(&config, &print, &url).await {
        Err(Refusal::Unusable(_)) => {}
        other => panic!("expected an Unusable refusal, got {other:?}"),
    }
}
