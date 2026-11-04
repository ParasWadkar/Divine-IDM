//! Signing in to a site, against a server that actually checks.
//!
//! The unit tests in `auth.rs` prove the hashing against the RFCs' own
//! worked examples. These prove the half that cannot be tested as text: the
//! 401-then-retry flow, the scheme being remembered so N segments do not
//! each cost a round trip, and the nonce count staying unique while every
//! lane sends at once.
//!
//! Every assertion is on content. A download that authenticated and then
//! wrote the wrong bytes has not worked, and a length check would pass it.

mod support;

use std::sync::Arc;

use idm_net::auth::{AuthState, HttpCredential};
use idm_net::download::{DownloadConfig, download};
use support::MockServer;

/// Matches the mock server's own credentials.
const USER: &str = "mufasa";
const PASS: &str = "circle-of-life";

/// Big enough to be split across lanes, so the Digest nonce count is taken
/// concurrently rather than once. A single-connection download would prove
/// nothing about the thing most likely to be wrong.
const SIZE: u64 = 24 << 20;

/// Matches `byteAt` in tests/mock-server/server.mjs, recomputed here so the
/// comparison shares no code with the server that produced it.
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
        Some(at) => panic!("the signed-in download differs from the source at offset {at}"),
    }
}

fn config_for(
    server: &MockServer,
    dir: &std::path::Path,
    route: &str,
    credential: Option<(&str, &str)>,
) -> DownloadConfig {
    let mut config = DownloadConfig::new(server.url(&format!("{route}?size={SIZE}")), dir);
    config.filename = Some("payload.bin".to_owned());
    if let Some((user, password)) = credential {
        // The same `Arc` in both halves, exactly as the engine does it: one
        // nonce counter across the probe and every lane.
        let state = Arc::new(AuthState::new(HttpCredential::new(user, password)));
        config.http.auth = Some(Arc::clone(&state));
        config.fetch.auth = Some(state);
    }
    config
}

async fn signs_in_and_downloads(route: &str) {
    let server = MockServer::start();
    let dir = tempfile::tempdir().unwrap();
    let config = config_for(&server, dir.path(), route, Some((USER, PASS)));

    let report = download(&config).await.unwrap_or_else(|error| {
        panic!("{route} should have signed in and downloaded, got {error}")
    });

    assert_eq!(report.total, SIZE);
    assert_content(&report.path);
}

#[tokio::test]
async fn basic_signs_in_and_the_bytes_are_right() {
    signs_in_and_downloads("/auth-basic").await;
}

#[tokio::test]
async fn digest_md5_with_qop_signs_in_and_the_bytes_are_right() {
    // The commonest Digest in the wild, and the one whose nonce count the
    // server rejects on a repeat -- so passing this means the counter is
    // genuinely shared across the lanes.
    signs_in_and_downloads("/auth-digest").await;
}

#[tokio::test]
async fn digest_sha256_signs_in_and_the_bytes_are_right() {
    signs_in_and_downloads("/auth-digest-sha256").await;
}

#[tokio::test]
async fn digest_without_qop_signs_in_and_the_bytes_are_right() {
    // RFC 2617's form, hashed differently: no nc and no cnonce in the sum.
    // Sending the qop form here produces a wrong response, so this catches
    // the two being confused.
    signs_in_and_downloads("/auth-digest-no-qop").await;
}

#[tokio::test]
async fn a_nonce_that_expires_is_retried_rather_than_treated_as_a_wrong_password() {
    // What happens to any download that outlives one nonce. Treating
    // `stale=true` as a refusal would fail exactly the long transfers this
    // feature exists for.
    signs_in_and_downloads("/auth-stale").await;
}

#[tokio::test]
async fn a_server_offering_windows_sign_in_alongside_basic_uses_the_basic() {
    // What a Windows server commonly offers. Taking the refusal would fail
    // a download that would have worked perfectly well.
    signs_in_and_downloads("/auth-both").await;
}

#[tokio::test]
async fn a_site_that_only_wants_windows_sign_in_says_so_in_words() {
    // The whole reason NTLM is parsed rather than ignored: "this server
    // wants Windows sign-in" is something a person can act on, and a bare
    // 401 is not.
    let server = MockServer::start();
    let dir = tempfile::tempdir().unwrap();

    let (notes, mut said) = tokio::sync::mpsc::unbounded_channel();
    let mut config = config_for(&server, dir.path(), "/auth-ntlm-only", Some((USER, PASS)));
    config.notes = Some(notes);

    let error = download(&config).await.expect_err("it cannot sign in, so it must fail");
    drop(config);

    let note = said.recv().await.expect("the refusal should have been passed on");
    assert!(note.contains("Windows sign-in"), "it should name what the server wanted: {note}");
    assert!(note.contains("does not do"), "and be plain that this cannot: {note}");
    // And the failure itself is the one that means "needs a password",
    // rather than a mystery status code.
    let said = error.to_string();
    assert!(said.contains("password"), "{said}");
}

#[tokio::test]
async fn a_site_that_wants_a_password_we_do_not_have_fails_as_needing_one() {
    // No credential configured at all, which is what every download from a
    // protected site looks like before somebody saves one.
    let server = MockServer::start();
    let dir = tempfile::tempdir().unwrap();
    let config = config_for(&server, dir.path(), "/auth-basic", None);

    let error = download(&config).await.expect_err("there is no password to send");
    let said = error.to_string();
    assert!(said.contains("password"), "it should say what is missing: {said}");
}

#[tokio::test]
async fn a_wrong_password_is_not_sent_over_and_over() {
    // A server that counts failures locks the account, so the second 401
    // has to be final rather than the start of a retry loop.
    let server = MockServer::start();
    let dir = tempfile::tempdir().unwrap();
    let config = config_for(&server, dir.path(), "/auth-wrong-password", Some((USER, "wrong")));

    let error = download(&config).await.expect_err("the password is wrong");
    assert!(error.to_string().contains("password"), "{error}");
}

#[tokio::test]
async fn nothing_is_sent_to_a_server_that_never_asked() {
    // The password must not go to every host the user downloads from --
    // only to the ones that have actually challenged. `/honest` never
    // does, so a correct run touches the credential not at all.
    let server = MockServer::start();
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(AuthState::new(HttpCredential::new(USER, PASS)));

    let mut config = config_for(&server, dir.path(), "/honest", None);
    config.http.auth = Some(Arc::clone(&state));
    config.fetch.auth = Some(Arc::clone(&state));

    let report = download(&config).await.expect("an unprotected download should just work");
    assert_content(&report.path);
    assert!(
        state.learned().is_none(),
        "a server that never challenged must not have taught us a scheme",
    );
}
