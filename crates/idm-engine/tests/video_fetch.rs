//! Driving yt-dlp, against something that behaves like it.
//!
//! The unit tests in `video` cover the argument list and the progress
//! parsing as text. What they cannot cover is the part that only exists
//! once a process is involved: that the arguments are understood by
//! something reading them as yt-dlp would, that progress arrives as the
//! download runs rather than in a lump at the end, that the final path is
//! believed, and that a refusal comes back as a refusal.
//!
//! The stub is not yt-dlp and this does not pretend otherwise -- see
//! `stub-ytdlp`. What it does prove is that nothing between here and a real
//! one is wrong about the contract.

use std::path::Path;

use idm_engine::video::{self, Progress};

fn stub() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_stub-ytdlp"))
}

#[tokio::test]
async fn a_video_is_fetched_and_its_path_comes_back() {
    let dir = tempfile::tempdir().unwrap();
    let got = video::fetch_with(stub(), "https://example.invalid/watch", dir.path(), None, |_| {})
        .await
        .expect("the fetch should succeed");

    assert!(got.path.exists(), "the file it named should be there: {:?}", got.path);
    assert_eq!(got.bytes, 4096, "the size comes from the file, not the progress line");
    assert!(
        got.path.starts_with(dir.path().canonicalize().unwrap_or_else(|_| dir.path().into())),
        "it should land in the folder we asked for: {:?}",
        got.path,
    );
}

#[tokio::test]
async fn progress_arrives_while_it_runs() {
    // Not one lump at the end: a progress bar that only moves once is the
    // same as no progress bar.
    let dir = tempfile::tempdir().unwrap();
    let mut seen: Vec<Progress> = Vec::new();
    video::fetch_with(stub(), "https://example.invalid/watch", dir.path(), None, |p| seen.push(p))
        .await
        .unwrap();

    assert!(seen.len() >= 3, "expected several updates, got {}", seen.len());
    assert!(
        seen.windows(2).all(|pair| pair[1].downloaded >= pair[0].downloaded),
        "progress should not go backwards: {seen:?}",
    );
    assert!(seen.last().unwrap().finished, "the last update should say it finished");
    assert_eq!(seen.last().unwrap().total, Some(4096));
}

#[tokio::test]
async fn a_stream_with_no_known_size_still_reports_what_it_has() {
    // A live stream has no total. The bytes still climb, and the absence of
    // a total has to survive the trip rather than arriving as a zero.
    let dir = tempfile::tempdir().unwrap();
    let mut seen: Vec<Progress> = Vec::new();
    video::fetch_with(stub(), "https://example.invalid/live", dir.path(), None, |p| seen.push(p))
        .await
        .unwrap();

    assert!(seen.iter().all(|p| p.total.is_none()), "no total was known: {seen:?}");
    assert!(seen.last().unwrap().downloaded > 0);
}

#[tokio::test]
async fn a_refusal_comes_back_as_one() {
    let dir = tempfile::tempdir().unwrap();
    let error =
        video::fetch_with(stub(), "https://example.invalid/refuse", dir.path(), None, |_| {})
            .await
            .expect_err("it should fail");

    // Carrying what the tool said, because "yt-dlp failed" on its own tells
    // somebody nothing about whether to try a different address.
    assert!(format!("{error}").contains("Unsupported URL"), "{error}");
}

#[tokio::test]
async fn a_chosen_name_is_honoured() {
    let dir = tempfile::tempdir().unwrap();
    let got = video::fetch_with(
        stub(),
        "https://example.invalid/watch",
        dir.path(),
        Some("holiday.mp4"),
        |_| {},
    )
    .await
    .unwrap();

    let name = got.path.file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.starts_with("holiday"), "{name}");
}

#[test]
fn yt_dlp_is_only_used_when_it_is_already_installed() {
    // Nothing here fetches it. A download manager that silently installs a
    // program in order to perform a download is not a trade anybody agreed
    // to, so an absent tool is a refusal that names it.
    let empty = tempfile::tempdir().unwrap();
    let error = video::find(empty.path()).expect_err("nothing is installed there");
    assert!(format!("{error}").contains("yt-dlp"), "{error}");
}
