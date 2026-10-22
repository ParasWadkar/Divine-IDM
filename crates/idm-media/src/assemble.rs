//! Putting fetched segments back into one file.
//!
//! Joining segments of a single track is concatenation and nothing more —
//! MPEG-TS is designed for it, and fragmented MP4 works the same way once
//! its initialisation segment goes first. That matters because it means a
//! stream can be downloaded and played without ffmpeg being installed at
//! all. ffmpeg is needed only to combine two tracks, or to change container.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::{MediaError, Result};

/// Concatenates `parts` into `output`, in the order given.
///
/// Returns the number of bytes written. Missing parts are an error rather
/// than a gap: a video assembled from most of its segments plays, looks
/// almost right, and is wrong in a way nobody notices until much later.
pub fn concatenate(parts: &[PathBuf], output: &Path) -> Result<u64> {
    if parts.is_empty() {
        return Err(MediaError::Empty);
    }

    for part in parts {
        if !part.is_file() {
            return Err(MediaError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("{} is missing, so the result would have a hole in it", part.display()),
            )));
        }
    }

    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut written = 0u64;
    let mut sink = BufWriter::with_capacity(1 << 20, File::create(output)?);
    for part in parts {
        let mut source = File::open(part)?;
        written += std::io::copy(&mut source, &mut sink)?;
    }
    sink.flush()?;
    Ok(written)
}

/// The order segments have to be joined in.
///
/// The initialisation segment first, always: for fragmented MP4 it carries
/// the `moov` box, and without it at the front the file describes nothing
/// and no player will open it.
pub fn ordered(init: Option<&Path>, segments: &[PathBuf]) -> Vec<PathBuf> {
    let mut parts = Vec::with_capacity(segments.len() + 1);
    parts.extend(init.map(Path::to_path_buf));
    parts.extend(segments.iter().cloned());
    parts
}

/// What a joined track should be called before anything is muxed.
pub fn part_name(directory: &Path, stem: &str, kind: &str, container: &str) -> PathBuf {
    directory.join(format!("{stem}.{kind}.{container}"))
}

/// Whether a joined file can be delivered as-is, or needs remuxing first.
///
/// A single TS track is playable by anything, but `.ts` is an awkward thing
/// to be handed; MP4 is what people expect. This says what is merely
/// preferable, so the caller can carry on without ffmpeg rather than fail.
pub fn wants_remux(container: &str) -> bool {
    container == "ts"
}

/// Removes the scratch files once the result is safely in place.
///
/// Only after: a cleanup that runs before the output is known good is how a
/// failed mux turns into a lost download.
pub fn clean_up(parts: &[PathBuf]) {
    for part in parts {
        let _ = std::fs::remove_file(part);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, contents: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn segments_are_joined_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let parts = vec![
            write(dir.path(), "a", b"one"),
            write(dir.path(), "b", b"two"),
            write(dir.path(), "c", b"three"),
        ];
        let output = dir.path().join("joined.ts");

        assert_eq!(concatenate(&parts, &output).unwrap(), 11);
        assert_eq!(std::fs::read(&output).unwrap(), b"onetwothree");
    }

    #[test]
    fn a_missing_segment_is_refused_rather_than_skipped() {
        // A video assembled from most of its segments plays, looks almost
        // right, and is wrong in a way nobody notices for a long time.
        let dir = tempfile::tempdir().unwrap();
        let parts = vec![write(dir.path(), "a", b"one"), dir.path().join("gone")];
        let error = concatenate(&parts, &dir.path().join("out.ts")).unwrap_err();
        assert!(error.to_string().contains("hole"), "{error}");
    }

    #[test]
    fn nothing_to_join_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(concatenate(&[], &dir.path().join("out.ts")), Err(MediaError::Empty)));
    }

    #[test]
    fn the_initialisation_segment_goes_first() {
        // It carries the moov box. Anywhere but the front and the file
        // describes nothing.
        let init = PathBuf::from("init.mp4");
        let segments = vec![PathBuf::from("1.m4s"), PathBuf::from("2.m4s")];
        let with_init = ordered(Some(&init), &segments);
        assert_eq!(with_init[0], init);
        assert_eq!(with_init.len(), 3);

        let without = ordered(None, &segments);
        assert_eq!(without.len(), 2);
    }

    #[test]
    fn joining_works_without_ffmpeg_being_anywhere_near() {
        // The property the whole design rests on: one track is a byte-wise
        // join, so a stream downloads and plays on a machine with no
        // sidecars installed at all.
        let dir = tempfile::tempdir().unwrap();
        let init = write(dir.path(), "init.mp4", b"MOOV");
        let segments =
            vec![write(dir.path(), "1.m4s", b"AAAA"), write(dir.path(), "2.m4s", b"BBBB")];
        let output = dir.path().join("video.mp4");

        let parts = ordered(Some(&init), &segments);
        concatenate(&parts, &output).unwrap();
        assert_eq!(std::fs::read(&output).unwrap(), b"MOOVAAAABBBB");
    }

    #[test]
    fn transport_streams_would_rather_be_remuxed() {
        assert!(wants_remux("ts"));
        assert!(!wants_remux("mp4"));
    }

    #[test]
    fn cleanup_removes_the_scratch_files() {
        let dir = tempfile::tempdir().unwrap();
        let parts = vec![write(dir.path(), "a", b"x"), write(dir.path(), "b", b"y")];
        clean_up(&parts);
        assert!(parts.iter().all(|part| !part.exists()));
    }

    #[test]
    fn part_names_do_not_collide_between_tracks() {
        let dir = Path::new("D:/downloads");
        let video = part_name(dir, "lecture", "video", "ts");
        let audio = part_name(dir, "lecture", "audio", "m4a");
        assert_ne!(video, audio);
        assert!(video.to_string_lossy().ends_with("lecture.video.ts"));
    }
}
