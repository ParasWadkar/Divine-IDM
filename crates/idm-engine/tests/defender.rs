//! The real scanner, on whatever machine this is running on.
//!
//! The unit tests in `scan` prove the protocol — which exit code means
//! what, and that a `2` with no threat in the report is a scanning error
//! rather than a detection. What they cannot prove is that the tool is
//! where this looks for it, that the arguments are accepted, and that a
//! scan of an ordinary file comes back clean in reasonable time.
//!
//! # Why there is no test with a real detection
//!
//! The usual way is the EICAR test file, a harmless string every scanner
//! is required to flag. It is not used here, for two reasons. Real-time
//! protection acts on it the moment it is written, so the file would
//! usually be quarantined before this process could scan it — the test
//! would be flaky by construction. And it would leave a threat
//! notification and a quarantine entry on the machine of whoever ran the
//! test suite, which is not a thing a test should do to somebody.
//!
//! So the detection path is proven from captured report text, in `scan`'s
//! unit tests, and this proves the half that needs a real scanner.

use std::io::Write;

use idm_engine::scan::{self, Verdict};

/// Skips rather than fails where there is no Defender.
///
/// CI runners, machines with another product installed, and anywhere this
/// is cross-compiled. A test that cannot run is not a test that failed,
/// and making it one would mean the suite is red on half the machines it
/// is meant to protect.
fn scanner() -> Option<std::path::PathBuf> {
    match scan::tool() {
        Some(tool) => Some(tool),
        None => {
            eprintln!("no MpCmdRun.exe on this machine; skipping");
            None
        }
    }
}

#[tokio::test]
async fn an_ordinary_file_comes_back_clean() {
    let Some(tool) = scanner() else { return };

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ordinary.bin");
    let mut file = std::fs::File::create(&path).unwrap();
    // Not random: random bytes are what packed malware looks like, and the
    // point of this test is the uninteresting case.
    file.write_all(&vec![b'-'; 64 * 1024]).unwrap();
    file.sync_all().unwrap();
    drop(file);

    let began = std::time::Instant::now();
    let verdict = scan::scan(&tool, &path).await;
    assert!(began.elapsed() < scan::LONGEST_SCAN, "a small file should not take the full budget");

    match verdict {
        Verdict::Clean => {}
        // Acceptable: Defender may be switched off by policy, in which case
        // it answers rather than scanning. What must never happen is a
        // detection on a file of hyphens.
        Verdict::Unknown { why } => eprintln!("no verdict on this machine: {why}"),
        Verdict::Infected { name } => panic!("64KB of hyphens was called {name}"),
    }
}

#[tokio::test]
async fn a_file_that_is_not_there_is_not_a_detection() {
    let Some(tool) = scanner() else { return };

    let dir = tempfile::tempdir().unwrap();
    match scan::scan(&tool, &dir.path().join("never-written.bin")).await {
        Verdict::Unknown { .. } => {}
        other => panic!("a missing file must not produce a verdict about it: {other:?}"),
    }
}

#[tokio::test]
async fn something_too_large_is_not_scanned_rather_than_called_clean() {
    // "Not scanned" and "scanned and fine" are different facts, and a
    // forty-gigabyte disc image read twice is a long stall for a verdict
    // that is almost always the second one.
    let Some(tool) = scanner() else { return };

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("huge.bin");
    let file = std::fs::File::create(&path).unwrap();
    // Sparse: the length is what the check reads, and writing nine
    // gigabytes to prove a size check would be absurd.
    file.set_len(scan::LARGEST_SCAN_BYTES + 1).unwrap();
    drop(file);

    match scan::scan(&tool, &path).await {
        Verdict::Unknown { why } => assert!(why.contains("large"), "{why}"),
        other => panic!("expected it to be left alone, got {other:?}"),
    }
}
