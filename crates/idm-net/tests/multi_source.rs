//! Downloading one file from several addresses at once.
//!
//! Every assertion here is on content. A length check would pass on a file
//! assembled from the right number of wrong bytes, and the whole hazard of
//! multi-source downloading is a file of exactly the right length made of
//! two different builds — so the finished bytes are compared against a
//! sequence computed here, sharing no code with the server that served it.

mod support;

use idm_net::download::{DownloadConfig, download};
use support::MockServer;

/// Big enough to be split across connections rather than fetched whole: the
/// engine deliberately uses one connection for anything near the
/// bandwidth-delay product, and a single-connection download would exercise
/// one address however many were configured.
const SIZE: u64 = 32 << 20;

/// Matches `byteAt` in tests/mock-server/server.mjs, recomputed here so the
/// comparison shares no code with the thing it checks.
fn byte_at(offset: u64) -> u8 {
    let mut x = (offset as u32).wrapping_add(0x9e37_79b9);
    x = (x ^ (x >> 16)).wrapping_mul(0x85eb_ca6b);
    x = (x ^ (x >> 13)).wrapping_mul(0xc2b2_ae35);
    (x ^ (x >> 16)) as u8
}

fn expected(total: u64) -> Vec<u8> {
    (0..total).map(byte_at).collect()
}

fn config_for(server: &MockServer, dir: &std::path::Path) -> DownloadConfig {
    let mut config = DownloadConfig::new(server.url(&format!("/honest?size={SIZE}")), dir);
    config.filename = Some("payload.bin".to_owned());
    config
}

/// Asserts the finished file is the file that was asked for.
fn assert_content(path: &std::path::Path) {
    let written = std::fs::read(path).expect("the finished file should be readable");
    assert_eq!(written.len() as u64, SIZE, "wrong length");
    // Compared as slices rather than with assert_eq, which would try to
    // print thirty-two megabytes on failure.
    let wanted = expected(SIZE);
    match written.iter().zip(&wanted).position(|(a, b)| a != b) {
        None => {}
        Some(at) => panic!("the assembled file differs from the source at offset {at}"),
    }
}

#[tokio::test]
async fn a_file_fetched_from_two_agreeing_addresses_is_byte_for_byte_correct() {
    let server = MockServer::start();
    let dir = tempfile::tempdir().unwrap();

    let mut config = config_for(&server, dir.path());
    config.mirrors = vec![server.url(&format!("/mirror-agrees?size={SIZE}"))];

    let report = download(&config).await.expect("the download should finish");

    assert_eq!(report.mirrors_used, 1, "the agreeing mirror should have been used");
    assert_eq!(report.total, SIZE);
    assert_content(&report.path);
}

#[tokio::test]
async fn a_mirror_holding_a_different_build_is_dropped_and_costs_nothing() {
    // The case this whole mechanism exists for. The mirror is the same
    // length and shares its first megabyte, so nothing downstream could
    // catch it: it has to be refused before it contributes a byte. And
    // refusing it must cost the download nothing — not its success, and not
    // its contents.
    let server = MockServer::start();
    let dir = tempfile::tempdir().unwrap();

    let mut config = config_for(&server, dir.path());
    config.mirrors = vec![server.url(&format!("/mirror-tail-differs?size={SIZE}"))];

    let report = download(&config).await.expect("the download should still finish");

    assert_eq!(report.mirrors_used, 0, "a mirror serving different bytes must not be used");
    assert_content(&report.path);
}

#[tokio::test]
async fn a_dropped_mirror_is_said_out_loud() {
    // Dropping it is right. Dropping it without a word is not: somebody
    // typed that address expecting it to help, and silence is
    // indistinguishable from the feature not working.
    let server = MockServer::start();
    let dir = tempfile::tempdir().unwrap();
    let bad = server.url(&format!("/mirror-tail-differs?size={SIZE}"));

    let (notes, mut said) = tokio::sync::mpsc::unbounded_channel();
    let mut config = config_for(&server, dir.path());
    config.mirrors = vec![bad.clone()];
    config.notes = Some(notes);

    let report = download(&config).await.expect("the download should still finish");
    assert_eq!(report.mirrors_used, 0);
    drop(config);

    let note = said.recv().await.expect("the refusal should have been passed on");
    assert!(note.contains(&bad), "it should name the address that was left out: {note}");
    assert!(note.contains("not being used"), "and say plainly that it is not being used: {note}",);
}

#[tokio::test]
async fn a_download_with_nothing_to_say_says_nothing() {
    let server = MockServer::start();
    let dir = tempfile::tempdir().unwrap();

    let (notes, mut said) = tokio::sync::mpsc::unbounded_channel();
    let mut config = config_for(&server, dir.path());
    config.mirrors = vec![server.url(&format!("/mirror-agrees?size={SIZE}"))];
    config.notes = Some(notes);

    let report = download(&config).await.expect("the download should finish");
    assert_eq!(report.mirrors_used, 1, "the agreeing mirror should be used");
    drop(config);

    assert!(said.recv().await.is_none(), "a mirror that was used is not worth remarking on");
}

#[tokio::test]
async fn a_mirror_that_cannot_be_reached_is_dropped_and_costs_nothing() {
    let server = MockServer::start();
    let dir = tempfile::tempdir().unwrap();

    let mut config = config_for(&server, dir.path());
    config.mirrors = vec![server.url("/nothing-here"), server.url("/also-not-here")];

    let report = download(&config).await.expect("the download should still finish");

    assert_eq!(report.mirrors_used, 0);
    assert_content(&report.path);
}

#[tokio::test]
async fn one_good_mirror_among_bad_ones_is_still_used() {
    let server = MockServer::start();
    let dir = tempfile::tempdir().unwrap();

    let mut config = config_for(&server, dir.path());
    config.mirrors = vec![
        server.url(&format!("/mirror-shorter?size={SIZE}")),
        server.url(&format!("/mirror-agrees?size={SIZE}")),
        server.url(&format!("/mirror-tail-differs?size={SIZE}")),
    ];

    let report = download(&config).await.expect("the download should finish");

    assert_eq!(report.mirrors_used, 1, "exactly the one that agreed");
    assert_content(&report.path);
}

#[tokio::test]
async fn configuring_no_mirrors_changes_nothing() {
    // The path every existing download takes, asserted here so a regression
    // in the multi-source plumbing cannot pass as a mirror simply not being
    // used.
    let server = MockServer::start();
    let dir = tempfile::tempdir().unwrap();

    let report = download(&config_for(&server, dir.path())).await.expect("should finish");

    assert_eq!(report.mirrors_used, 0);
    assert!(report.lanes >= 1);
    assert_content(&report.path);
}
