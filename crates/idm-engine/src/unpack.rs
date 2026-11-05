//! Unpacking a finished zip, in this process, into a folder beside it.
//!
//! The counterpart to `archive`, which reads a zip's index over HTTP to show
//! somebody what is in it. This one has the whole file and does the real
//! work, so it uses the `zip` crate rather than a parser of our own: the
//! index is a hundred lines of bounds checks, and inflate is not.
//!
//! What is ours is everything around it, because an archive's contents are
//! chosen by whoever made the archive.
//!
//! # Every name in a zip is hostile
//!
//! A zip entry's name is free text. It can be `..\..\Startup\x.exe`, it can
//! be `C:\Windows\System32\x.dll`, it can be `\\server\share\x`, it can be
//! `x.exe.` — which Windows opens as `x.exe`, having quietly dropped the
//! dot — or `readme.txt:hidden.exe`, which writes an alternate data stream
//! on an NTFS volume. None of those is a file name, and every one of them
//! has been used.
//!
//! So [`safe_name`] builds the destination out of components it has
//! individually approved, and the result is checked against the destination
//! folder once more before anything is opened. An entry that does not
//! survive that is skipped and counted, never repaired into something
//! close to what it said: guessing at what a hostile name meant is how
//! these holes get reopened.
//!
//! # A zip can be much larger than it looks
//!
//! Forty-two kilobytes of zeroes deflates to about forty bytes. An archive
//! can therefore promise a terabyte from a small download, and the file
//! system will cheerfully try. Three caps rather than one: how many entries
//! there may be, how many bytes may be written in total, and how far the
//! archive may expand overall — because an archive that is genuinely four
//! gigabytes of text has a high ratio and is not an attack, and one that is
//! a few kilobytes claiming a terabyte is.
//!
//! # Nothing is ever deleted, and nothing fails the download
//!
//! The archive stays exactly where it was: unpacking it is not a reason to
//! remove it, and the person may want both. And a zip that could not be
//! unpacked is still a zip that downloaded correctly, so the outcome is
//! recorded and the download is delivered either way.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

/// How many entries will be taken out of one archive.
pub const MOST_ENTRIES: usize = 20_000;

/// How many bytes will be written in total.
pub const MOST_UNPACKED_BYTES: u64 = 4 << 30;

/// How much larger than the archive the contents may be, in total.
///
/// Generous: text and logs really do compress by a hundred to one, and a
/// cap that refused those would be a cap that mostly refuses honest
/// archives. The number that stops a bomb is the byte cap above; this one
/// stops the small-archive case, where the byte cap alone would still mean
/// writing four gigabytes from a two-kilobyte file.
pub const MOST_EXPANSION: u64 = 500;

/// How long one component of a name may be.
pub const LONGEST_COMPONENT: usize = 200;

/// What happened to the archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unpacked {
    Into {
        folder: PathBuf,
        /// How many entries were written.
        files: usize,
        /// How many were refused — a hostile name, a symlink, a cap.
        skipped: usize,
    },
    /// It was not unpacked, and why. Never fails the download.
    Not { why: String },
}

/// Whether this is a file worth offering to unpack.
///
/// Extension only. Sniffing the first bytes would be a better test of what
/// the file *is*, and a worse test of what the user expects: somebody who
/// renamed something to `.bin` did not want it unpacked, and a `.docx` is a
/// zip that nobody means to explode into a folder.
pub fn looks_like_a_zip(file: &Path) -> bool {
    file.extension()
        .and_then(|part| part.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("zip"))
}

/// Turns an entry's stored name into a relative path, or refuses it.
///
/// Pure, and the piece that matters. See the module docs: the rule is that
/// a component is approved whole or the entry is dropped, never trimmed
/// into something acceptable.
pub fn safe_name(name: &str) -> Option<PathBuf> {
    // Zip says forward slashes; plenty of writers use backslashes anyway,
    // and on Windows both separate. Treated as the same so that a name
    // cannot smuggle a separator past the component check below.
    let name = name.replace('\\', "/");

    // `C:/x`, `//server/share`, `/etc/x` — all of them say where to write
    // rather than what to call it.
    if name.starts_with('/') || name.contains(':') {
        return None;
    }

    let mut path = PathBuf::new();
    let mut parts = 0;
    for part in name.split('/') {
        // A trailing empty part is the directory marker, and a doubled
        // slash is nothing at all.
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            return None;
        }
        if part.len() > LONGEST_COMPONENT {
            return None;
        }
        // Windows strips trailing dots and spaces when opening a file, so
        // `x.exe.` and `x.exe ` are both `x.exe` — which is how a name that
        // looked harmless becomes an executable.
        if part.ends_with('.') || part.ends_with(' ') || part.starts_with(' ') {
            return None;
        }
        // `readme.txt:hidden.exe` is caught above by the colon check, and
        // the rest of these cannot appear in a Windows file name at all.
        if part.chars().any(|c| c.is_control() || matches!(c, '<' | '>' | '"' | '|' | '?' | '*')) {
            return None;
        }
        if is_reserved(part) {
            return None;
        }
        path.push(part);
        parts += 1;
    }

    (parts > 0).then_some(path)
}

/// Whether a component is one of the names Windows keeps for devices.
///
/// `CON`, `NUL`, `COM3` and the rest are devices whatever folder they are
/// written in, and whatever extension is put on them: `nul.txt` is still
/// the null device. Writing one does not land a file anywhere, so an
/// archive containing one is either broken or probing.
fn is_reserved(part: &str) -> bool {
    const RESERVED: [&str; 4] = ["CON", "PRN", "AUX", "NUL"];
    let stem = part.split('.').next().unwrap_or_default().trim_end();
    if RESERVED.iter().any(|name| stem.eq_ignore_ascii_case(name)) {
        return true;
    }
    // COM1..COM9, LPT1..LPT9 — and COM0/LPT0, which Windows also reserves.
    for prefix in ["COM", "LPT"] {
        if stem.len() == prefix.len() + 1
            && stem[..prefix.len()].eq_ignore_ascii_case(prefix)
            && stem.as_bytes()[prefix.len()].is_ascii_digit()
        {
            return true;
        }
    }
    false
}

/// The folder an archive is unpacked into: its own name, beside it.
///
/// Numbered rather than merged if something is already there. Unpacking
/// twice into one folder would silently overwrite whatever the first one
/// left, and the two archives need not be the same archive.
pub fn folder_for(archive: &Path) -> Option<PathBuf> {
    let parent = archive.parent()?;
    let stem = archive.file_stem()?;
    let first = parent.join(stem);
    if !first.exists() {
        return Some(first);
    }
    (2..100)
        .map(|n| parent.join(format!("{} ({n})", stem.to_string_lossy())))
        .find(|candidate| !candidate.exists())
}

/// Unpacks a zip into a new folder beside it.
///
/// On a blocking thread, because inflate is real CPU work and a four-
/// gigabyte archive would otherwise hold an async worker for the duration.
pub async fn unpack(archive: &Path) -> Unpacked {
    let archive = archive.to_path_buf();
    match tokio::task::spawn_blocking(move || unpack_now(&archive)).await {
        Ok(outcome) => outcome,
        Err(error) => Unpacked::Not { why: error.to_string() },
    }
}

fn unpack_now(archive: &Path) -> Unpacked {
    let file = match std::fs::File::open(archive) {
        Ok(file) => file,
        Err(error) => return Unpacked::Not { why: error.to_string() },
    };
    let packed = file.metadata().map(|data| data.len()).unwrap_or(0);

    let mut zip = match zip::ZipArchive::new(std::io::BufReader::new(file)) {
        Ok(zip) => zip,
        Err(error) => return Unpacked::Not { why: format!("it could not be read: {error}") },
    };

    if zip.len() > MOST_ENTRIES {
        return Unpacked::Not { why: format!("it holds more than {MOST_ENTRIES} entries") };
    }

    // Before anything is written: a bomb should be refused on what it says
    // about itself, not discovered four gigabytes in.
    //
    // `by_index_data` rather than `by_index_raw`: the sizes are in the
    // central directory, which `ZipArchive::new` has already read, and the
    // raw accessor parses each entry's *local* header and seeks the reader
    // to it. That is a seek and a read per entry — up to twenty thousand of
    // them, each throwing away the buffer's window — to learn a number that
    // is already in memory.
    let mut promised: u64 = 0;
    for index in 0..zip.len() {
        if let Ok(entry) = zip.by_index_data(index) {
            promised = promised.saturating_add(entry.size());
        }
    }
    if promised > MOST_UNPACKED_BYTES {
        return Unpacked::Not {
            why: format!(
                "it would unpack to {promised} bytes, over the {MOST_UNPACKED_BYTES} limit"
            ),
        };
    }
    if packed > 0 && promised / packed.max(1) > MOST_EXPANSION {
        return Unpacked::Not {
            why: format!("it claims to expand more than {MOST_EXPANSION} times over"),
        };
    }

    let Some(folder) = folder_for(archive) else {
        return Unpacked::Not { why: "there was nowhere to put it".into() };
    };
    if let Err(error) = std::fs::create_dir_all(&folder) {
        return Unpacked::Not { why: format!("could not make {}: {error}", folder.display()) };
    }
    // Resolved once, and every destination is checked against this rather
    // than against the path as written: a folder reached through a junction
    // is a different string and the same place.
    let root = std::fs::canonicalize(&folder).unwrap_or_else(|_| folder.clone());

    let mut files = 0;
    let mut skipped = 0;
    let mut written: u64 = 0;
    // Allocated once rather than per entry: a fresh 64 KiB vector for each
    // of twenty thousand entries is a gigabyte of zero-filling to no end.
    let mut buffer = vec![0u8; 64 << 10];
    // Parent folder as written, to the same folder as the file system calls
    // it. Nearly every entry shares a parent with the one before, and
    // canonicalising is an open-and-resolve each time.
    let mut settled: HashMap<PathBuf, PathBuf> = HashMap::new();

    for index in 0..zip.len() {
        match one_entry(&mut zip, index, &folder, &root, &mut settled, &mut buffer, written) {
            Taken::Wrote(bytes) => {
                written += bytes;
                files += 1;
            }
            Taken::Folder => {}
            Taken::Skipped => skipped += 1,
        }
    }

    Unpacked::Into { folder, files, skipped }
}

/// What became of one entry.
enum Taken {
    Wrote(u64),
    /// A directory entry, which is neither written nor refused.
    Folder,
    Skipped,
}

/// Takes one entry out, or refuses it.
///
/// Its own function because the guards are the substance here: eight of
/// them, each of which has to both refuse the entry and count it. Inline,
/// that was eight copies of `skipped += 1; continue;` with the line that
/// does the work buried underneath, and a guard added in the middle could
/// easily be written without the counter.
#[allow(clippy::too_many_arguments)]
fn one_entry<R: Read + std::io::Seek>(
    zip: &mut zip::ZipArchive<R>,
    index: usize,
    folder: &Path,
    root: &Path,
    settled: &mut HashMap<PathBuf, PathBuf>,
    buffer: &mut [u8],
    already: u64,
) -> Taken {
    let Ok(mut entry) = zip.by_index(index) else {
        return Taken::Skipped;
    };
    // A symlink in a zip is a path pointing wherever it likes, and
    // following one after the fact would undo every check above.
    if entry.is_symlink() {
        return Taken::Skipped;
    }
    // A name that is not even valid text is one nobody meant.
    let Ok(stored) = entry.name() else {
        return Taken::Skipped;
    };
    let Some(relative) = safe_name(&stored) else {
        return Taken::Skipped;
    };
    let destination = folder.join(&relative);

    if entry.is_dir() {
        if std::fs::create_dir_all(&destination).is_err() {
            return Taken::Skipped;
        }
        return Taken::Folder;
    }

    let Some(parent) = destination.parent() else {
        return Taken::Skipped;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return Taken::Skipped;
    }

    // The belt to `safe_name`'s braces. `safe_name` already refuses
    // everything that could climb; this proves it on the path that was
    // actually built, after the file system has had its say about short
    // names, junctions and the rest.
    let real = match settled.get(parent) {
        Some(known) => known.clone(),
        None => {
            let Ok(found) = std::fs::canonicalize(parent) else {
                return Taken::Skipped;
            };
            settled.insert(parent.to_path_buf(), found.clone());
            found
        }
    };
    if !real.starts_with(root) {
        return Taken::Skipped;
    }

    match write_entry(&mut entry, &destination, buffer, already) {
        Ok(bytes) => Taken::Wrote(bytes),
        Err(_) => Taken::Skipped,
    }
}

/// Writes one entry, stopping if the running total would pass the cap.
///
/// Copied by hand rather than with `std::io::copy` so the total is checked
/// while the bytes arrive: an entry that *states* a small size and then
/// produces a terabyte is the oldest trick in this format, and a size read
/// from the archive is a claim by the archive.
fn write_entry(
    entry: &mut impl Read,
    destination: &Path,
    buffer: &mut [u8],
    already: u64,
) -> Result<u64, std::io::Error> {
    use std::io::Write;

    let mut out = std::io::BufWriter::new(std::fs::File::create(destination)?);
    let mut written = 0u64;
    loop {
        let read = entry.read(buffer)?;
        if read == 0 {
            break;
        }
        written += read as u64;
        if already + written > MOST_UNPACKED_BYTES {
            return Err(std::io::Error::other("over the unpacking limit"));
        }
        out.write_all(&buffer[..read])?;
    }
    out.flush()?;
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ordinary_name_comes_through() {
        assert_eq!(safe_name("readme.txt"), Some(PathBuf::from("readme.txt")));
        assert_eq!(safe_name("docs/readme.txt"), Some(PathBuf::from("docs/readme.txt")));
        assert_eq!(safe_name("./docs//readme.txt"), Some(PathBuf::from("docs/readme.txt")));
    }

    #[test]
    fn a_name_that_climbs_is_refused() {
        // The classic. `..\..\Startup\x.exe` in a zip opened by a program
        // that joins names onto a folder writes to the Startup folder.
        assert_eq!(safe_name("../x.exe"), None);
        assert_eq!(safe_name(r"..\..\Startup\x.exe"), None);
        assert_eq!(safe_name("docs/../../x"), None);
    }

    #[test]
    fn an_absolute_name_is_refused() {
        assert_eq!(safe_name("/etc/passwd"), None);
        assert_eq!(safe_name(r"C:\Windows\System32\x.dll"), None);
        assert_eq!(safe_name(r"\\server\share\x"), None);
    }

    #[test]
    fn an_alternate_data_stream_is_refused() {
        // `readme.txt:hidden.exe` writes a stream on the file rather than a
        // file, which nothing shows and plenty will run.
        assert_eq!(safe_name("readme.txt:hidden.exe"), None);
    }

    #[test]
    fn a_trailing_dot_or_space_is_refused() {
        // Windows drops both when opening, so these are `x.exe` wearing a
        // disguise that only the check sees through.
        assert_eq!(safe_name("x.exe."), None);
        assert_eq!(safe_name("x.exe "), None);
        assert_eq!(safe_name(" x.exe"), None);
    }

    #[test]
    fn a_device_name_is_refused() {
        for name in ["CON", "nul", "NUL.txt", "com1", "LPT9.dat", "aux", "prn"] {
            assert_eq!(safe_name(name), None, "{name} is a device");
        }
        // And names that merely start the same way are not devices.
        for name in ["console.log", "communicate.txt", "nulls.bin", "com10.txt"] {
            assert!(safe_name(name).is_some(), "{name} is an ordinary file");
        }
    }

    #[test]
    fn a_name_with_nothing_left_is_refused() {
        assert_eq!(safe_name(""), None);
        assert_eq!(safe_name("/"), None);
        assert_eq!(safe_name("."), None);
        // A trailing slash is how a zip marks a directory entry, so this
        // one is a name and not nothing.
        assert_eq!(safe_name("docs/"), Some(PathBuf::from("docs")));
    }

    #[test]
    fn a_very_long_component_is_refused() {
        let long = "x".repeat(LONGEST_COMPONENT + 1);
        assert_eq!(safe_name(&long), None);
        assert!(safe_name(&"x".repeat(LONGEST_COMPONENT)).is_some());
    }

    #[test]
    fn the_folder_is_named_after_the_archive() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("pack.zip");
        assert_eq!(folder_for(&archive), Some(dir.path().join("pack")));
    }

    #[test]
    fn a_folder_already_there_is_not_unpacked_into() {
        // Two archives of the same name are not the same archive, and the
        // second one quietly overwriting the first is the worst outcome.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("pack")).unwrap();
        let archive = dir.path().join("pack.zip");
        assert_eq!(folder_for(&archive), Some(dir.path().join("pack (2)")));
    }

    #[test]
    fn only_a_zip_is_offered() {
        assert!(looks_like_a_zip(Path::new("x.zip")));
        assert!(looks_like_a_zip(Path::new("X.ZIP")));
        assert!(!looks_like_a_zip(Path::new("x.docx")));
        assert!(!looks_like_a_zip(Path::new("x.bin")));
        assert!(!looks_like_a_zip(Path::new("x")));
    }
}
