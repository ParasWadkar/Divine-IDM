//! Going through a proxy that wants a password.
//!
//! One thing is being proven: that the password saved against a proxy
//! address reaches the wire as `Proxy-Authorization`, goes to the proxy and
//! not to the origin, and is not joined there by the site's own credential.
//!
//! None of that can be shown by a unit test, and all of it would fail
//! quietly — a download through a proxy that happens not to ask for a
//! password works perfectly whether or not the header is ever built.

mod support;

use std::sync::Arc;

use idm_net::auth::{AuthState, HttpCredential};
use idm_net::client::ProxyMode;
use idm_net::download::{DownloadConfig, download};
use support::MockServer;

/// Matches the proxy script's own credentials.
const PROXY_USER: &str = "proxyuser";
const PROXY_PASS: &str = "proxypass";

/// Small enough to keep the test quick, large enough to be split across
/// more than one request so the header is needed more than once.
const SIZE: u64 = 4 << 20;

/// Matches `byteAt` in tests/mock-server/proxy.mjs, recomputed here so the
/// comparison shares no code with the thing that produced it.
fn byte_at(offset: u64) -> u8 {
    let mut x = (offset as u32).wrapping_add(0x9e37_79b9);
    x = (x ^ (x >> 16)).wrapping_mul(0x85eb_ca6b);
    x = (x ^ (x >> 13)).wrapping_mul(0xc2b2_ae35);
    (x ^ (x >> 16)) as u8
}

fn assert_content(path: &std::path::Path) {
    let written = std::fs::read(path).expect("the finished file should be readable");
    assert_eq!(written.len() as u64, SIZE, "wrong length");
    let wanted: Vec<u8> = (0..SIZE).map(byte_at).collect();
    match written.iter().zip(&wanted).position(|(a, b)| a != b) {
        None => {}
        Some(at) => panic!("the proxied file differs from the source at offset {at}"),
    }
}

/// A download aimed at an origin that does not exist, routed through the
/// proxy — which answers in place. If the proxy were bypassed, the request
/// would go to `nowhere.invalid` and fail to resolve, so a success here is
/// itself evidence that the proxy was used.
fn config_through(proxy: &MockServer, dir: &std::path::Path) -> DownloadConfig {
    let mut config =
        DownloadConfig::new(format!("http://nowhere.invalid/file.bin?size={SIZE}"), dir);
    config.filename = Some("payload.bin".to_owned());
    config.http.proxy = ProxyMode::Manual(proxy.url(""));
    config
}

#[tokio::test]
async fn a_proxy_password_gets_the_download_through() {
    let proxy = MockServer::start_proxy();
    let dir = tempfile::tempdir().unwrap();

    let mut config = config_through(&proxy, dir.path());
    config.http.proxy_auth = Some(HttpCredential::new(PROXY_USER, PROXY_PASS));

    let report = download(&config).await.expect("the proxy should have let this through");
    assert_eq!(report.total, SIZE);
    assert_content(&report.path);
}

#[tokio::test]
async fn without_the_password_the_proxy_refuses() {
    // The other half, and what makes the test above mean something: without
    // the header the same download fails, so passing cannot be an accident
    // of the proxy letting everything through.
    let proxy = MockServer::start_proxy();
    let dir = tempfile::tempdir().unwrap();

    let config = config_through(&proxy, dir.path());
    let error = download(&config).await.expect_err("the proxy wants a password");
    let said = error.to_string();
    assert!(
        said.contains("407") || said.to_lowercase().contains("proxy"),
        "it should be about the proxy: {said}",
    );
}

#[tokio::test]
async fn the_wrong_proxy_password_is_refused_too() {
    let proxy = MockServer::start_proxy();
    let dir = tempfile::tempdir().unwrap();

    let mut config = config_through(&proxy, dir.path());
    config.http.proxy_auth = Some(HttpCredential::new(PROXY_USER, "not-the-password"));

    download(&config).await.expect_err("a wrong proxy password must not get through");
}

#[tokio::test]
async fn the_sites_own_password_is_not_handed_to_the_proxy() {
    // A proxy sits between this machine and every download, so a client
    // that sent the site's credential on every request would hand it to
    // whoever runs the proxy. The script answers 400 if it ever sees an
    // `Authorization` header, so this fails loudly rather than leaking.
    let proxy = MockServer::start_proxy();
    let dir = tempfile::tempdir().unwrap();

    let mut config = config_through(&proxy, dir.path());
    config.http.proxy_auth = Some(HttpCredential::new(PROXY_USER, PROXY_PASS));
    // A site credential that has never been challenged for, which is the
    // state every download starts in.
    let site = Arc::new(AuthState::new(HttpCredential::new("someone", "site-secret")));
    config.http.auth = Some(Arc::clone(&site));
    config.fetch.auth = Some(site);

    let report = download(&config).await.expect("the proxy password alone should suffice");
    assert_content(&report.path);
}
