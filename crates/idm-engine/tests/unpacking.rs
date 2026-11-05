//! Unpacking a real zip, built here, onto a real disk.
//!
//! `unpack`'s own tests cover `safe_name`, which is where the rules live,
//! and they prove the rules without touching a file system. What they
//! cannot prove is that the rules are reached: a checker nothing calls
//! passes every test it has. So this writes archives containing the names
//! that matter and then looks at what ended up on disk — including, for the
//! traversal case, at what did *not* end up outside the folder.

use std::io::{Cursor, Write};
use std::path::Path;

use idm_engine::unpack::{self, Unpacked};
use zip::write::SimpleFileOptions;

/// A zip holding exactly the entries given, stored rather than deflated.
///
/// Stored because the entry *names* are what is being tested and inflate
/// would only slow it down — except where a test wants compression, which
/// says so.
fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut writer = zip::ZipWriter::new(Cursor::new(&mut out));
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, body) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(body).unwrap();
        }
        writer.finish().unwrap();
    }
    out
}

fn written(dir: &Path, bytes: &[u8]) -> std::path::PathBuf {
    let archive = dir.join("pack.zip");
    std::fs::write(&archive, bytes).unwrap();
    archive
}

#[tokio::test]
async fn an_ordinary_archive_is_unpacked_beside_itself() {
    let dir = tempfile::tempdir().unwrap();
    let archive =
        written(dir.path(), &zip_of(&[("readme.txt", b"hello"), ("docs/guide.txt", b"more")]));

    match unpack::unpack(&archive).await {
        Unpacked::Into { folder, files, skipped } => {
            assert_eq!((files, skipped), (2, 0));
            assert_eq!(folder, dir.path().join("pack"));
            assert_eq!(std::fs::read(folder.join("readme.txt")).unwrap(), b"hello");
            assert_eq!(std::fs::read(folder.join("docs/guide.txt")).unwrap(), b"more");
        }
        other => panic!("expected it to unpack: {other:?}"),
    }

    // Never removed. Somebody may want the archive as well as its contents,
    // and deleting what was just downloaded is not this feature's to do.
    assert!(archive.is_file());
}

#[tokio::test]
async fn a_name_that_climbs_writes_nothing_outside_the_folder() {
    // The one that matters. An entry called `../escaped.txt` in a program
    // that joins names onto a folder lands one level up — and `..\..\` far
    // enough up to reach the Startup folder.
    let dir = tempfile::tempdir().unwrap();
    let inside = dir.path().join("inside");
    std::fs::create_dir(&inside).unwrap();
    let archive = written(
        &inside,
        &zip_of(&[
            ("../escaped.txt", b"out"),
            (r"..\..\escaped-too.txt", b"further"),
            ("kept.txt", b"in"),
        ]),
    );

    match unpack::unpack(&archive).await {
        Unpacked::Into { folder, files, skipped } => {
            assert_eq!((files, skipped), (1, 2));
            assert!(folder.join("kept.txt").is_file());
        }
        other => panic!("expected it to unpack what it could: {other:?}"),
    }

    assert!(!dir.path().join("escaped.txt").exists(), "it escaped the folder");
    assert!(!inside.join("escaped.txt").exists(), "it escaped the folder");
    assert!(!dir.path().parent().unwrap().join("escaped-too.txt").exists());
}

#[tokio::test]
async fn an_archive_that_claims_to_be_enormous_is_not_unpacked() {
    // A zip bomb refused on what it says about itself, before a byte of it
    // is written. Deflate of a megabyte of zeroes is about a kilobyte, so
    // this is a thousand to one -- well past the cap without taking long
    // to build.
    let mut out = Vec::new();
    {
        let mut writer = zip::ZipWriter::new(Cursor::new(&mut out));
        let options =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for n in 0..8 {
            writer.start_file(format!("zeroes{n}.bin"), options).unwrap();
            writer.write_all(&vec![0u8; 4 << 20]).unwrap();
        }
        writer.finish().unwrap();
    }

    let dir = tempfile::tempdir().unwrap();
    let archive = written(dir.path(), &out);
    match unpack::unpack(&archive).await {
        Unpacked::Not { why } => assert!(why.contains("expand"), "{why}"),
        other => panic!("a bomb must be refused: {other:?}"),
    }
    assert!(!dir.path().join("pack").exists(), "nothing should have been written");
}

#[tokio::test]
async fn something_that_is_not_a_zip_is_reported_rather_than_unpacked() {
    let dir = tempfile::tempdir().unwrap();
    let archive = written(dir.path(), b"this is not a zip");
    match unpack::unpack(&archive).await {
        Unpacked::Not { why } => assert!(why.contains("could not be read"), "{why}"),
        other => panic!("expected a refusal: {other:?}"),
    }
}

#[tokio::test]
async fn unpacking_twice_does_not_overwrite_the_first_folder() {
    let dir = tempfile::tempdir().unwrap();
    let archive = written(dir.path(), &zip_of(&[("readme.txt", b"first")]));
    let Unpacked::Into { folder: first, .. } = unpack::unpack(&archive).await else {
        panic!("the first should unpack");
    };

    std::fs::write(dir.path().join("pack.zip"), zip_of(&[("readme.txt", b"second")])).unwrap();
    let Unpacked::Into { folder: again, .. } = unpack::unpack(&archive).await else {
        panic!("the second should unpack");
    };

    assert_ne!(first, again);
    assert_eq!(std::fs::read(first.join("readme.txt")).unwrap(), b"first");
    assert_eq!(std::fs::read(again.join("readme.txt")).unwrap(), b"second");
}
