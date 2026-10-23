//! The coordinator driving whole downloads against the mock server.

mod support;

use std::time::Duration;

use idm_net::download::{DownloadConfig, DownloadError, download, probe};
use support::MockServer;

fn byte_at(offset: u64) -> u8 {
    let mut x = (offset as u32).wrapping_add(0x9e37_79b9);
    x = (x ^ (x >> 16)).wrapping_mul(0x85eb_ca6b);
    x = (x ^ (x >> 13)).wrapping_mul(0xc2b2_ae35);
    (x ^ (x >> 16)) as u8
}

fn assert_file_matches(path: &std::path::Path, total: u64) {
    let got = std::fs::read(path).expect("read finished file");
    assert_eq!(got.len() as u64, total, "wrong size");
    let expected: Vec<u8> = (0..total).map(byte_at).collect();
    if got != expected {
        let at = got.iter().zip(&expected).position(|(a, b)| a != b).unwrap();
        panic!("content differs at offset {at}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn it_downloads_a_whole_file_and_finalises_it() {
    let total = 12 << 20;
    let server = MockServer::start();
    let dir = tempfile::tempdir().unwrap();

    let mut config = DownloadConfig::new(server.url(&format!("/honest?size={total}")), dir.path());
    // The mock server is on localhost, so there are no edges to pin to.
    config.pin_edges = false;
    config.filename = Some("payload.bin".into());

    let report = download(&config).await.expect("download should succeed");

    assert_eq!(report.total, total);
    assert_eq!(report.path, dir.path().join("payload.bin"));
    assert!(report.path.exists(), "the finished file should be at its final name");
    assert!(!dir.path().join("payload.bin.idmpart").exists(), ".part should be gone");
    assert!(!dir.path().join("payload.bin.idmjrn").exists(), "journal should be gone");
    assert_file_matches(&report.path, total);

    // Mark-of-the-Web: without it Windows treats the file as locally created.
    let mut stream = report.path.as_os_str().to_os_string();
    stream.push(":Zone.Identifier");
    let zone = std::fs::read_to_string(stream).expect("Zone.Identifier should exist");
    assert!(zone.contains("ZoneId=3"), "expected the internet zone, got {zone}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn it_resumes_a_partially_downloaded_file() {
    let total = 40 << 20;
    let server = MockServer::start();
    let dir = tempfile::tempdir().unwrap();
    let url = server.url(&format!("/honest?size={total}"));

    // Interrupt a trickled download partway through, which is what a crash or
    // a cancelled transfer actually looks like: a .part file plus a journal
    // recording whatever the last checkpoint committed.
    //
    // The parallelism is pinned so the interruption does not depend on how
    // fast the scheduler happens to be. /slow trickles at `bps` *per
    // connection*, so this once read 6 MB/s over the 4 connections a cold
    // start used to be stuck at -- 1.75s for the file, interrupted at 1.5s
    // by a margin of a quarter second. Fixing the ramp so it reaches 16
    // connections made the same download finish in under half a second, and
    // the test failed with "a partial file should remain" because there was
    // nothing partial left: it had been measuring the engine's slowness.
    // Four connections at 3 MB/s is 3.5s of work stopped at 1.5s, which
    // leaves a comfortable margin and still fragments the extent set across
    // several connections, which is the interesting thing to resume from.
    let mut interrupted =
        DownloadConfig::new(server.url(&format!("/slow?size={total}&bps=3000000")), dir.path());
    interrupted.pin_edges = false;
    interrupted.fixed_connections = Some(4);
    interrupted.filename = Some("payload.bin".into());
    interrupted.checkpoint_interval = Duration::from_millis(100);
    let _ = tokio::time::timeout(Duration::from_millis(1500), download(&interrupted)).await;

    let part = dir.path().join("payload.bin.idmpart");
    let journal = dir.path().join("payload.bin.idmjrn");
    assert!(part.exists(), "a partial file should remain");
    assert!(journal.exists(), "a journal should remain to resume from");

    // Now finish it from the honest route; the journal must stop it starting over.
    let mut good = DownloadConfig::new(url, dir.path());
    good.pin_edges = false;
    good.filename = Some("payload.bin".into());
    let report = download(&good).await.expect("resume should succeed");

    assert!(report.resumed_bytes > 0, "should have resumed, not restarted");
    assert_file_matches(&report.path, total);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_server_without_range_support_is_reported_clearly() {
    let server = MockServer::start();
    let dir = tempfile::tempdir().unwrap();
    let mut config = DownloadConfig::new(server.url("/ignores-range?size=1048576"), dir.path());
    config.pin_edges = false;

    match download(&config).await {
        Err(DownloadError::NoRangeSupport) => {}
        other => panic!("expected a clear range-support error, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_probe_learns_the_filename_and_validator() {
    let total = 2 << 20;
    let server = MockServer::start();
    let config = DownloadConfig::new(server.url(&format!("/honest?size={total}")), ".");
    let target = probe(&config).await.expect("probe should succeed");

    assert_eq!(target.total, total);
    assert_eq!(target.validator, idm_core::journal::ValidatorKind::StrongETag);
    assert!(target.etag.is_some());
    assert_eq!(target.filename, "honest", "derived from the URL path");
}
