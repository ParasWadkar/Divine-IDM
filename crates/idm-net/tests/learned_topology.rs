//! The learned connection shape, as far as a localhost test can prove it.
//!
//! `topology`'s own tests cover the decision rule, which is where all the
//! care is. What they cannot cover is whether the decision reaches the
//! transport at all — a chooser nothing consults passes every test it has.
//!
//! # What this cannot prove
//!
//! That the shape makes any difference. The mock server is HTTP/1.1 over
//! loopback, where reqwest opens sockets per client on demand and the
//! round trip is microseconds; the whole argument for shapes is about
//! congestion windows on a real path to a real h2 origin. So the rate
//! here says nothing, and no assertion is made about it. What is asserted
//! is which shape was used, which is exactly the part a bug would break.

mod support;

use std::time::Duration;

use idm_net::download::{DownloadConfig, download};
use idm_net::topology::{self, Shape, Trial};
use support::MockServer;

/// Primes the process memory for an origin with a clear winner.
fn teach(origin: &str, winner: Shape, loser: Shape) {
    let big = |rate: f64| Trial {
        shape: winner,
        bytes: (rate * 10.0) as u64,
        took: Duration::from_secs(10),
    };
    topology::memory().record(origin, Trial { shape: loser, ..big(1.0e6) });
    topology::memory().record(origin, big(50.0e6));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_table_decides_while_learning_is_switched_off() {
    // The gate. Even with a shape measured and clearly ahead, nothing
    // changes until somebody asks for it.
    let total = 4 << 20;
    let server = MockServer::start();
    let dir = tempfile::tempdir().unwrap();
    let url = server.url(&format!("/honest?size={total}"));
    let origin = topology::origin_of(&url).expect("the mock server has an origin");

    let [table, wider] = topology::candidates(8);
    assert_ne!(table, wider, "the test needs two shapes to tell apart");
    teach(&origin, wider, table);

    let mut config = DownloadConfig::new(&url, dir.path());
    config.pin_edges = false;
    config.filename = Some("off.bin".into());
    config.fixed_connections = Some(8);
    assert!(!config.learn_topology, "it has to be off by default");

    let report = download(&config).await.expect("download should succeed");
    assert_eq!(report.lanes, table.lanes as usize, "the table's shape, not the measured one");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn what_was_measured_is_used_once_it_is_switched_on() {
    let total = 4 << 20;
    let server = MockServer::start();
    let dir = tempfile::tempdir().unwrap();
    let url = server.url(&format!("/honest?size={total}"));
    let origin = topology::origin_of(&url).expect("the mock server has an origin");

    let [table, wider] = topology::candidates(8);
    teach(&origin, wider, table);

    let mut config = DownloadConfig::new(&url, dir.path());
    config.pin_edges = false;
    config.filename = Some("on.bin".into());
    config.fixed_connections = Some(8);
    config.learn_topology = true;

    let report = download(&config).await.expect("download should succeed");
    assert_eq!(report.lanes, wider.lanes as usize, "the measured shape should have been used");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_http1_download_teaches_nothing() {
    // Shapes are about streams over one connection, which HTTP/1.1 does
    // not have. Recording one here would put a number against a shape
    // that never applied, and that number would then decide an h2
    // download later.
    let total = 32 << 20;
    let server = MockServer::start();
    let dir = tempfile::tempdir().unwrap();
    let url = server.url(&format!("/honest?size={total}"));
    let origin = topology::origin_of(&url).expect("the mock server has an origin");

    let mut config = DownloadConfig::new(&url, dir.path());
    config.pin_edges = false;
    config.filename = Some("plain.bin".into());

    let report = download(&config).await.expect("download should succeed");
    assert!(report.http_version.starts_with("HTTP/1"), "{}", report.http_version);
    assert!(
        topology::memory().learned(&origin).is_none(),
        "an h1 download must not teach the shape chooser anything",
    );
}
