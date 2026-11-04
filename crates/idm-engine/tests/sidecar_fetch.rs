//! The real fetch, against the real publishers.
//!
//! Ignored by default and deliberately out of CI: it downloads something
//! over a hundred megabytes from two third-party hosts, so a shared runner
//! would be fetching it on every push and a failure would as often be the
//! publisher's outage as this project's bug.
//!
//! Run it by hand when the manifest changes, which is the moment it is
//! worth anything:
//!
//! ```text
//! cargo test -p idm-engine --test sidecar_fetch -- --ignored --nocapture
//! ```
//!
//! `IDM_STATE_DIR` is set to a temporary folder first, so this never writes
//! into the state folder of the copy the user is actually running.

use idm_media::sidecar::{self, Tool};

/// Points the whole engine's state at a scratch folder for this process.
///
/// Set rather than restored: these tests are the only thing in this binary,
/// and a restore would race the other one anyway.
fn scratch() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("a temporary folder");
    // SAFETY: single-threaded setup, before anything reads the variable.
    unsafe { std::env::set_var("IDM_STATE_DIR", dir.path()) };
    dir
}

#[tokio::test]
#[ignore = "downloads over a hundred megabytes from the publishers"]
async fn ffmpeg_arrives_verified_and_then_runs() {
    let _scratch = scratch();

    let done = idm_engine::sidecars::fetch("ffmpeg").await.expect("the fetch should succeed");
    println!("{done}");

    let installed = idm_engine::sidecars::dir().join("ffmpeg.exe");
    assert!(installed.is_file(), "{} should exist", installed.display());

    // The point of the whole exercise: not that a file arrived, but that the
    // runner -- which checks it against the lock again before executing it
    // -- accepts and runs this one.
    let found = sidecar::probe_managed(Tool::Ffmpeg, &idm_engine::sidecars::dir())
        .expect("the installed copy should probe cleanly");
    assert_eq!(found.path, installed, "the fetched copy should be the one found");
    assert!(found.version.contains("ffmpeg"), "it should say what it is: {}", found.version);

    // And the status the window shows agrees with all of that.
    let view = idm_engine::sidecars::views()
        .into_iter()
        .find(|view| view.name == "ffmpeg")
        .expect("ffmpeg should be in the manifest");
    assert!(view.present, "{:?}", view.detail);
    assert!(
        view.detail.as_deref().is_some_and(|detail| detail.contains("fetched by this app")),
        "it should say where it came from: {:?}",
        view.detail,
    );
}

#[tokio::test]
#[ignore = "downloads twenty-odd megabytes from the publishers"]
async fn yt_dlp_arrives_verified_too() {
    // The other archive shape -- the download *is* the program, with no
    // unpacking step -- which is the half of `fetch_into` the ffmpeg case
    // never reaches.
    let _scratch = scratch();

    idm_engine::sidecars::fetch("yt-dlp").await.expect("the fetch should succeed");

    let found = sidecar::probe_managed(Tool::YtDlp, &idm_engine::sidecars::dir())
        .expect("the installed copy should probe cleanly");
    assert!(found.version.starts_with("20"), "a yt-dlp version is a date: {}", found.version);
}
