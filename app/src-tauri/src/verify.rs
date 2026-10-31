//! Checking a finished download against the hash its source published.
//!
//! The point is not that the bytes arrived intact -- the journal's own
//! block checksums already say that, and a download that failed its way
//! through never finishes. The point is that the bytes are the ones the
//! publisher meant to send: a mirror serving an older build, a truncated
//! upload, a file somebody swapped. Nothing this side of the wire can tell
//! you that, which is why it takes a figure from the page the link came
//! from.
//!
//! Which algorithm is worked out from the length of what gets pasted,
//! because that is the one thing about a hash a person reliably has in
//! front of them, and so the least they should have to say.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use digest::Digest;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Algorithm {
    Md5,
    Sha1,
    Sha256,
}

impl Algorithm {
    pub fn name(self) -> &'static str {
        match self {
            Self::Md5 => "MD5",
            Self::Sha1 => "SHA-1",
            Self::Sha256 => "SHA-256",
        }
    }

    /// Which algorithm produces a hash written like this one.
    ///
    /// The three lengths do not collide, so whoever pastes it does not
    /// have to know or say which kind they have.
    pub fn for_hash(hash: &str) -> Option<Self> {
        match hash.len() {
            32 => Some(Self::Md5),
            40 => Some(Self::Sha1),
            64 => Some(Self::Sha256),
            _ => None,
        }
    }
}

#[derive(Debug, serde::Serialize)]
pub struct Verdict {
    pub matched: bool,
    pub algorithm: Algorithm,
    /// What the file actually hashes to, so a mismatch can be read rather
    /// than only reported.
    pub actual: String,
}

/// What was pasted, with the noise people paste around it taken off.
///
/// A published line is usually the hash followed by the filename, and one
/// copied out of a table arrives with spaces or a `sha256:` label on it.
fn tidy(expected: &str) -> String {
    let trimmed = expected.trim();
    let without_label = trimmed.rsplit_once(':').map_or(trimmed, |(_, rest)| rest);
    without_label
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches(|c: char| !c.is_ascii_alphanumeric())
        .to_ascii_lowercase()
}

/// Hashes `path` and says whether it matches `expected`.
///
/// Blocking, and deliberately so: this reads the whole file, which for a
/// large download is minutes of work with no business on an async
/// runtime's workers. The caller hands it to a blocking thread.
pub fn check(path: &Path, expected: &str) -> Result<Verdict, String> {
    let wanted = tidy(expected);
    if wanted.is_empty() {
        return Err("no hash given".into());
    }
    if !wanted.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("that does not look like a hash: it should be hexadecimal".into());
    }
    let Some(algorithm) = Algorithm::for_hash(&wanted) else {
        return Err(format!(
            "a hash of {} characters is not one this knows; MD5 is 32, SHA-1 is 40, SHA-256 is 64",
            wanted.len()
        ));
    };

    let actual = hash(path, algorithm)?;
    Ok(Verdict { matched: actual == wanted, algorithm, actual })
}

fn digest_file<D: Digest>(file: &mut File, buffer: &mut [u8], path: &Path) -> Result<String, String> {
    let mut digest = D::new();
    loop {
        let read = file.read(buffer).map_err(|error| format!("{}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(digest.finalize().iter().map(|byte| format!("{byte:02x}")).collect())
}

fn hash(path: &Path, algorithm: Algorithm) -> Result<String, String> {
    let mut file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    // Big enough that a large file is not thousands of syscalls, small
    // enough to stay out of the way of whatever else is downloading.
    let mut buffer = vec![0u8; 1 << 20];

    match algorithm {
        Algorithm::Md5 => digest_file::<md5::Md5>(&mut file, &mut buffer, path),
        Algorithm::Sha1 => digest_file::<sha1::Sha1>(&mut file, &mut buffer, path),
        Algorithm::Sha256 => digest_file::<sha2::Sha256>(&mut file, &mut buffer, path),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    /// The published hashes of nothing at all, which each of these
    /// algorithms has a well-known value for.
    const EMPTY_MD5: &str = "d41d8cd98f00b204e9800998ecf8427e";
    const EMPTY_SHA1: &str = "da39a3ee5e6b4b0d3255bfef95601890afd80709";
    const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn file_of(bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("payload.bin");
        File::create(&path).unwrap().write_all(bytes).unwrap();
        (dir, path)
    }

    #[test]
    fn each_algorithm_is_told_apart_by_the_length_of_what_was_pasted() {
        assert_eq!(Algorithm::for_hash(EMPTY_MD5), Some(Algorithm::Md5));
        assert_eq!(Algorithm::for_hash(EMPTY_SHA1), Some(Algorithm::Sha1));
        assert_eq!(Algorithm::for_hash(EMPTY_SHA256), Some(Algorithm::Sha256));
        assert_eq!(Algorithm::for_hash("abc"), None);
    }

    #[test]
    fn a_file_matching_its_published_hash_is_said_to_match() {
        let (_dir, path) = file_of(b"");
        for published in [EMPTY_MD5, EMPTY_SHA1, EMPTY_SHA256] {
            let verdict = check(&path, published).unwrap();
            assert!(verdict.matched, "{} should match: {verdict:?}", verdict.algorithm.name());
            assert_eq!(verdict.actual, published);
        }
    }

    #[test]
    fn a_file_that_does_not_match_says_what_it_actually_is() {
        // The figure it actually hashes to is the useful half of a
        // mismatch: without it there is nothing to compare by eye.
        let (_dir, path) = file_of(b"not empty");
        let verdict = check(&path, EMPTY_SHA256).unwrap();
        assert!(!verdict.matched);
        assert_ne!(verdict.actual, EMPTY_SHA256);
        assert_eq!(verdict.actual.len(), 64);
    }

    #[test]
    fn the_noise_people_paste_around_a_hash_comes_off() {
        let (_dir, path) = file_of(b"");
        for pasted in [
            EMPTY_SHA256.to_owned(),
            format!("  {EMPTY_SHA256}  "),
            // What `sha256sum` prints, and what a web page tends to give.
            format!("{EMPTY_SHA256}  payload.bin"),
            format!("sha256:{EMPTY_SHA256}"),
            EMPTY_SHA256.to_ascii_uppercase(),
        ] {
            assert!(check(&path, &pasted).unwrap().matched, "should match: {pasted:?}");
        }
    }

    #[test]
    fn something_that_is_not_a_hash_is_refused_in_words() {
        let (_dir, path) = file_of(b"");
        let error = check(&path, "zzzz").unwrap_err();
        assert!(error.contains("hexadecimal"), "{error}");

        let error = check(&path, "abcdef123456").unwrap_err();
        assert!(error.contains("32"), "the lengths it does know should be said: {error}");
        assert!(error.contains("64"), "{error}");

        assert!(check(&path, "   ").unwrap_err().contains("no hash"));
    }

    #[test]
    fn a_file_larger_than_the_read_buffer_hashes_the_whole_of_itself() {
        // Every other case here fits in one read, so none of them would
        // notice the loop dropping or repeating a chunk. This one spans
        // several buffers and ends part way through one.
        let size = (1 << 20) * 3 + 12_345;
        let bytes: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let (_dir, path) = file_of(&bytes);

        // The same digest, computed in one go rather than in chunks.
        let expected: String =
            sha2::Sha256::digest(&bytes).iter().map(|byte| format!("{byte:02x}")).collect();

        let verdict = check(&path, &expected).unwrap();
        assert!(verdict.matched, "chunked and whole-file digests must agree: {verdict:?}");
        assert_eq!(verdict.actual, expected);
    }

    #[test]
    fn a_file_that_is_not_there_says_so_with_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("gone.bin");
        let error = check(&missing, EMPTY_SHA256).unwrap_err();
        assert!(error.contains("gone.bin"), "{error}");
    }
}
