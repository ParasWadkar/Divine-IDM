//! Exercises the code around ffmpeg, using a stand-in for it.
//!
//! Real ffmpeg is a hundred-megabyte download and converts media correctly
//! whether or not this project is right about anything. What is worth
//! testing is everything on this side of the boundary: finding the right
//! executable, refusing to trust one that cannot run, passing arguments as
//! arguments, and reporting a failure rather than swallowing it.
//!
//! The stand-in records the exact argument vector it was given, which is
//! how the no-shell property is checked rather than asserted.

use std::path::{Path, PathBuf};

use idm_media::sidecar::{self, Tool};

/// Puts the stand-in where `find` will look for ffmpeg.
fn bundled_stub(dir: &Path) -> PathBuf {
    let bundled = dir.join("sidecars");
    std::fs::create_dir_all(&bundled).unwrap();
    let target = bundled.join(format!("ffmpeg{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(env!("CARGO_BIN_EXE_stub-ffmpeg"), &target).unwrap();
    bundled
}

#[test]
fn a_sidecar_is_found_and_asked_what_it_is() {
    let dir = tempfile::tempdir().unwrap();
    let bundled = bundled_stub(dir.path());

    let ffmpeg = sidecar::probe(Tool::Ffmpeg, Some(&bundled)).expect("the stub should be found");
    assert_eq!(ffmpeg.path.parent().unwrap(), bundled);
    assert!(ffmpeg.version.contains("ffmpeg version"), "{}", ffmpeg.version);
}

#[test]
fn something_that_cannot_run_is_refused_before_it_is_used() {
    // A truncated download, a stub, a build for another architecture: all
    // look like an executable and none of them are one. Catching it at the
    // version call means one clear failure instead of a baffling one in the
    // middle of a mux.
    let dir = tempfile::tempdir().unwrap();
    let bundled = dir.path().join("sidecars");
    std::fs::create_dir_all(&bundled).unwrap();
    std::fs::write(bundled.join(format!("ffmpeg{}", std::env::consts::EXE_SUFFIX)), b"not an exe")
        .unwrap();

    let error = sidecar::probe(Tool::Ffmpeg, Some(&bundled)).unwrap_err().to_string();
    assert!(error.contains("ffmpeg"), "{error}");
}

#[test]
fn muxing_passes_the_files_through_and_produces_the_output() {
    let dir = tempfile::tempdir().unwrap();
    let bundled = bundled_stub(dir.path());
    let ffmpeg = sidecar::probe(Tool::Ffmpeg, Some(&bundled)).unwrap();

    let video = dir.path().join("video.ts");
    let audio = dir.path().join("audio.m4a");
    let output = dir.path().join("joined.mp4");
    std::fs::write(&video, b"video").unwrap();
    std::fs::write(&audio, b"audio").unwrap();

    ffmpeg.run(sidecar::mux_arguments(&video, Some(&audio), &output)).expect("the mux should run");
    assert!(output.is_file(), "the output should exist");

    let argv: Vec<String> = std::fs::read_to_string(format!("{}.argv", output.display()))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();

    assert!(argv.contains(&video.display().to_string()), "{argv:?}");
    assert!(argv.contains(&audio.display().to_string()), "{argv:?}");
    assert_eq!(argv.last().unwrap(), &output.display().to_string());
    // Copying rather than re-encoding is the whole point of remuxing.
    assert!(argv.windows(2).any(|pair| pair == ["-c", "copy"]), "{argv:?}");
}

#[test]
fn a_path_that_looks_like_shell_text_arrives_as_one_argument() {
    // The property that makes a filename taken from a web page safe to pass
    // along. A shell would read this as several commands; CreateProcess
    // reads it as a name.
    let dir = tempfile::tempdir().unwrap();
    let bundled = bundled_stub(dir.path());
    let ffmpeg = sidecar::probe(Tool::Ffmpeg, Some(&bundled)).unwrap();

    // Spaces, an ampersand, a semicolon and parentheses: all legal in a
    // Windows filename, and all syntax to a shell. Pipes and quotes would
    // be more pointed still, but Windows will not let a file be named with
    // them, so this is as hostile as a real name can be.
    let awkward = dir.path().join("a b & echo hi (x); done.ts");
    std::fs::write(&awkward, b"video").unwrap();
    let output = dir.path().join("out.mp4");

    ffmpeg.run(sidecar::mux_arguments(&awkward, None, &output)).expect("the mux should run");

    let argv: Vec<String> = std::fs::read_to_string(format!("{}.argv", output.display()))
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    // One element, not several: the recorder writes one argument per line.
    assert!(
        argv.contains(&awkward.display().to_string()),
        "the awkward name should arrive whole: {argv:?}"
    );
}

#[test]
fn a_failing_mux_is_reported_with_the_part_that_matters() {
    // ffmpeg writes progress to stderr as well as errors, so a report that
    // kept the first lines would show frame counts and hide the reason.
    let dir = tempfile::tempdir().unwrap();
    let bundled = bundled_stub(dir.path());
    let ffmpeg = sidecar::probe(Tool::Ffmpeg, Some(&bundled)).unwrap();

    let video = dir.path().join("video.ts");
    std::fs::write(&video, b"video").unwrap();

    // The signal rides on the output path, not the environment: tests share
    // a process, so a variable set here would leak into whatever else is
    // running at the same time.
    let error = ffmpeg
        .run(sidecar::mux_arguments(&video, None, &dir.path().join("should-fail.mp4")))
        .unwrap_err()
        .to_string();

    assert!(error.contains("Conversion failed"), "the reason should survive: {error}");
}

#[test]
fn a_bundled_sidecar_wins_over_one_on_the_path() {
    // The other order lets an unrelated ffmpeg somewhere in the user's
    // environment quietly replace the build that was verified.
    let dir = tempfile::tempdir().unwrap();
    let bundled = bundled_stub(dir.path());

    let found = sidecar::find(Tool::Ffmpeg, Some(&bundled)).expect("a copy should be found");
    assert_eq!(
        found.parent().unwrap(),
        bundled,
        "the bundled copy should win even if ffmpeg is installed"
    );
}

#[test]
fn a_pinned_checksum_is_checked_against_the_file_that_will_run() {
    let dir = tempfile::tempdir().unwrap();
    let bundled = bundled_stub(dir.path());
    let path = bundled.join(format!("ffmpeg{}", std::env::consts::EXE_SUFFIX));

    // Wrong pin: refused.
    assert!(sidecar::verify_checksum(&path, &"a".repeat(64)).is_err());

    // Replacing the file after a check must change the answer, which is why
    // the check belongs next to the run rather than at install time.
    std::fs::write(&path, b"something else entirely").unwrap();
    assert!(sidecar::verify_checksum(&path, &"a".repeat(64)).is_err());
}
