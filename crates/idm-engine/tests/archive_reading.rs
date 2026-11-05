//! The zip reader, checked against zips somebody else wrote.
//!
//! The unit tests in `archive` are built from hand-assembled bytes, which
//! is the right way to test the odd cases — a truncated record, a name
//! that is not UTF-8, a count beyond reason — because the test states what
//! the bytes are rather than agreeing with whatever a library happened to
//! produce.
//!
//! That leaves one thing unproven: whether the reader agrees with reality.
//! A parser can be perfectly self-consistent and wrong about the format.
//! So these build real archives with the `zip` crate — already in the tree
//! for extraction — and read them back with the parser, which makes this a
//! comparison between two independent implementations rather than a
//! restatement of one.

use std::io::{Cursor, Write};

use idm_engine::archive::{TAIL_BYTES, find_index, list};
use zip::write::SimpleFileOptions;

/// Builds a real zip in memory.
fn build(files: &[(&str, usize)], comment: &str) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut writer = zip::ZipWriter::new(Cursor::new(&mut out));
        writer.set_comment(comment).unwrap();
        for (name, size) in files {
            if name.ends_with('/') {
                writer.add_directory(*name, SimpleFileOptions::default()).unwrap();
                continue;
            }
            writer.start_file(*name, SimpleFileOptions::default()).unwrap();
            // Compressible on purpose, so the two sizes differ and a test
            // that mixed them up would show it.
            writer.write_all(&vec![b'a'; *size]).unwrap();
        }
        writer.finish().unwrap();
    }
    out
}

/// Reads an archive the way the engine will: a tail, then the directory.
fn read(archive: &[u8]) -> idm_engine::archive::Listing {
    let tail_len = TAIL_BYTES.min(archive.len());
    let tail_at = (archive.len() - tail_len) as u64;
    let index = find_index(&archive[archive.len() - tail_len..], tail_at)
        .expect("the index should be found");

    let from = usize::try_from(index.offset).unwrap();
    let to = from + usize::try_from(index.size).unwrap();
    list(&archive[from..to], index.entries).expect("the directory should parse")
}

#[test]
fn a_real_archive_lists_the_files_it_holds() {
    let archive = build(&[("readme.txt", 400), ("src/", 0), ("src/main.rs", 3000)], "");
    let listing = read(&archive);

    assert!(!listing.truncated);
    let names: Vec<&str> = listing.entries.iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(names, vec!["readme.txt", "src/", "src/main.rs"]);

    let readme = &listing.entries[0];
    assert_eq!(readme.uncompressed, 400, "the size that was written");
    assert!(readme.compressed < readme.uncompressed, "and it compressed");
    assert!(!readme.is_directory);
    assert!(listing.entries[1].is_directory, "a trailing slash is how a zip says so");
}

#[test]
fn a_comment_does_not_hide_the_index_in_a_real_archive() {
    // The unit tests cover this with assembled bytes; this proves the
    // backwards scan copes with what a writer actually produces.
    let archive = build(&[("one.txt", 10)], "packed by something or other");
    let listing = read(&archive);
    assert_eq!(listing.entries.len(), 1);
    assert_eq!(listing.entries[0].name, "one.txt");
}

#[test]
fn an_archive_with_no_files_in_it_lists_nothing() {
    let archive = build(&[], "");
    let listing = read(&archive);
    assert!(listing.entries.is_empty());
    assert!(!listing.truncated);
}

#[test]
fn a_name_with_characters_outside_ascii_survives_the_round_trip() {
    // Zip stores names in an unspecified code page unless a flag says
    // UTF-8, and the writer sets that flag. Getting the decoding wrong
    // would show as mojibake in the preview rather than as a failure.
    let archive = build(&[("日本語/ファイル.txt", 20), ("ünïcode.bin", 20)], "");
    let listing = read(&archive);
    let names: Vec<&str> = listing.entries.iter().map(|entry| entry.name.as_str()).collect();
    assert!(names.contains(&"日本語/ファイル.txt"), "{names:?}");
    assert!(names.contains(&"ünïcode.bin"), "{names:?}");
}

#[test]
fn many_files_all_come_back() {
    // Enough entries that the directory spans more than one read's worth
    // of anything, and that an off-by-one in the walk would show.
    let names: Vec<String> = (0..500).map(|n| format!("file-{n:04}.bin")).collect();
    let files: Vec<(&str, usize)> = names.iter().map(|name| (name.as_str(), 16usize)).collect();

    let archive = build(&files, "");
    let listing = read(&archive);

    assert_eq!(listing.entries.len(), 500);
    assert!(!listing.truncated);
    assert_eq!(listing.entries[0].name, "file-0000.bin");
    assert_eq!(listing.entries[499].name, "file-0499.bin");
}

#[test]
fn only_the_tail_is_needed_to_list_a_large_archive() {
    // The whole point of the feature: the index is at the end, so a
    // listing costs a few kilobytes however large the archive is. This
    // checks that the tail window really is enough rather than the test
    // quietly handing over the whole file.
    // Stored rather than deflated: two megabytes of one byte compresses to
    // nothing, and an archive smaller than the window would make this pass
    // without proving anything.
    let mut archive = Vec::new();
    {
        let mut writer = zip::ZipWriter::new(Cursor::new(&mut archive));
        let stored =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        writer.start_file("big.bin", stored).unwrap();
        writer.write_all(&vec![b'a'; 2 << 20]).unwrap();
        writer.start_file("small.txt", stored).unwrap();
        writer.write_all(&[b'b'; 64]).unwrap();
        writer.finish().unwrap();
    }
    assert!(archive.len() > TAIL_BYTES, "the archive must be larger than the window");

    let tail = &archive[archive.len() - TAIL_BYTES..];
    let index = find_index(tail, (archive.len() - TAIL_BYTES) as u64).expect("found from the tail");
    assert_eq!(index.entries, 2);

    let from = usize::try_from(index.offset).unwrap();
    let listing =
        list(&archive[from..from + usize::try_from(index.size).unwrap()], index.entries).unwrap();
    assert_eq!(listing.entries.len(), 2);
    assert_eq!(listing.entries[0].uncompressed, 2 << 20);
}
