//! Reading part of a file into memory, against a server that misbehaves.
//!
//! The parsing is proved in unit tests; this is about what a real server
//! does to a tail read. Three of these are failures rather than successes,
//! which is the point: a suffix read is only useful if "the server ignored
//! me" and "the server sent something else" are told apart from "here is
//! the tail". Reading the opening bytes of a file as its closing ones is a
//! silent wrong answer, not an error.

mod support;

use idm_net::client::HttpConfig;
use idm_net::range::{RangeError, read_at, read_suffix};
use support::MockServer;

const SIZE: u64 = 1 << 20;

/// Matches `byteAt` in tests/mock-server/server.mjs, recomputed here so the
/// comparison shares no code with the server that produced it.
fn byte_at(offset: u64) -> u8 {
    let mut x = (offset as u32).wrapping_add(0x9e37_79b9);
    x = (x ^ (x >> 16)).wrapping_mul(0x85eb_ca6b);
    x = (x ^ (x >> 13)).wrapping_mul(0xc2b2_ae35);
    (x ^ (x >> 16)) as u8
}

#[tokio::test]
async fn a_tail_read_comes_back_as_the_tail() {
    let server = MockServer::start();
    let config = HttpConfig::default();
    let url = server.url(&format!("/suffix-honest?size={SIZE}"));

    let window = read_suffix(&config, &url, 512, 64 << 10).await.expect("the tail");

    assert!(window.is_ranged(), "the server honoured the range");
    assert_eq!(window.bytes.len(), 512);
    assert_eq!(window.offset, Some(SIZE - 512));
    assert_eq!(window.total, Some(SIZE));

    // The bytes themselves, which is the only thing that proves the offset
    // was real rather than reported.
    let wanted: Vec<u8> = ((SIZE - 512)..SIZE).map(byte_at).collect();
    assert_eq!(window.bytes, wanted, "the last 512 bytes, not the first");
}

#[tokio::test]
async fn a_window_in_the_middle_is_the_part_that_was_asked_for() {
    let server = MockServer::start();
    let config = HttpConfig::default();
    let url = server.url(&format!("/honest?size={SIZE}"));

    let window = read_at(&config, &url, 1000, 256, 64 << 10).await.expect("a window");

    assert_eq!(window.offset, Some(1000));
    let wanted: Vec<u8> = (1000..1256).map(byte_at).collect();
    assert_eq!(window.bytes, wanted);
}

#[tokio::test]
async fn a_server_that_ignores_the_range_says_so_rather_than_lying() {
    // The failure that would be silent. A 200 means these are the *first*
    // bytes of the file, and a caller looking for a zip's index at the end
    // would otherwise parse the beginning and conclude it is not a zip --
    // or worse, find something that looks like an index.
    let server = MockServer::start();
    let config = HttpConfig::default();
    let url = server.url(&format!("/suffix-ignored?size={}", 4096));

    let window = read_suffix(&config, &url, 512, 64 << 10).await.expect("a body arrived");

    assert!(!window.is_ranged(), "it must not claim to be the tail");
    assert_eq!(window.offset, None);
    // And what came back really is the start, which is what makes the flag
    // worth checking.
    assert_eq!(window.bytes[..8], (0..8).map(byte_at).collect::<Vec<_>>()[..]);
}

#[tokio::test]
async fn a_server_that_sends_more_than_the_cap_is_cut_off() {
    // Without this the far end decides how much of this process's memory
    // to use. Capped while the body arrives rather than after, so the
    // gigabyte is never held.
    let server = MockServer::start();
    let config = HttpConfig::default();
    let url = server.url(&format!("/suffix-lies?size={}", 256 << 10));

    let error = read_suffix(&config, &url, 512, 8 << 10).await.expect_err("it oversent");
    assert!(matches!(error, RangeError::TooLarge { .. }), "{error}");
}

#[tokio::test]
async fn a_partial_answer_that_does_not_say_which_part_is_refused() {
    // Guessing is how the wrong part of a file gets read as the right one,
    // and nothing downstream could catch it.
    let server = MockServer::start();
    let config = HttpConfig::default();
    let url = server.url(&format!("/range-no-content-range?size={SIZE}"));

    let error = read_at(&config, &url, 0, 64, 64 << 10).await.expect_err("unusable");
    assert!(error.to_string().contains("which part"), "{error}");
}

#[tokio::test]
async fn an_address_that_is_not_there_is_an_error_not_an_empty_window() {
    let server = MockServer::start();
    let config = HttpConfig::default();

    let error =
        read_suffix(&config, &server.url("/nothing-here"), 512, 64 << 10).await.expect_err("404");
    assert!(error.to_string().contains("404"), "{error}");
}
