//! Looking inside a zip before committing to downloading it.
//!
//! A zip keeps its index at the *end*: a short "end of central directory"
//! record, pointing back at a list of entries, each naming a file and its
//! sizes. That layout is what makes this possible at all — the last few
//! kilobytes of a four-gigabyte archive are enough to list every file in
//! it, so somebody can see what they are about to spend an hour fetching.
//!
//! # A parser rather than the `zip` crate
//!
//! `zip` wants `Read + Seek`. Giving it that over HTTP means a blocking
//! seekable reader, a caching window, and `spawn_blocking` to keep it off
//! the async workers — more code than the parser below, and a far worse
//! failure story when a server lies about ranges, because the lie surfaces
//! as a seek returning the wrong bytes rather than as a refusal.
//!
//! `zip` is still the right answer for *extraction*, where real inflate is
//! needed. This is only the index.
//!
//! # Everything here is hostile input
//!
//! The bytes came off the internet and nothing has vouched for them. So
//! every read is bounds-checked, every count is capped, and a field that
//! does not make sense ends the parse rather than being worked around. A
//! name is returned exactly as stored and is **not** a path: it is shown
//! to a person, and anything that unpacks one has its own rules about
//! what a name may be.

/// How far back the end-of-directory record is looked for.
///
/// It sits at the very end unless the archive carries a comment, and the
/// comment length is sixteen bits — so 64 KiB plus the record itself is
/// the whole of the space it can hide in. Looking further would be looking
/// at file data.
pub const TAIL_BYTES: usize = (1 << 16) + 22;

/// The most entries that will be listed.
///
/// A zip may legitimately hold more; a preview may not. The count comes
/// out of the archive, so without a cap it decides how much memory this
/// takes.
const MOST_ENTRIES: usize = 5_000;

/// The longest name that will be kept, in bytes.
const LONGEST_NAME: usize = 1024;

const END_OF_DIRECTORY: u32 = 0x0605_4b50;
const ZIP64_LOCATOR: u32 = 0x0706_4b50;
const ZIP64_END: u32 = 0x0606_4b50;
const DIRECTORY_ENTRY: u32 = 0x0201_4b50;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The name exactly as the archive stores it.
    ///
    /// Not a path, and not sanitised: it is for showing. Whatever unpacks
    /// an archive applies its own rules, and doing it here would make this
    /// look as if it had.
    pub name: String,
    pub compressed: u64,
    pub uncompressed: u64,
    /// True when the stored name ends in a separator, which is how a zip
    /// marks a directory.
    pub is_directory: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    pub entries: Vec<Entry>,
    /// True when the archive holds more entries than were listed.
    pub truncated: bool,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ArchiveError {
    #[error("this does not look like a zip file")]
    NotAZip,
    #[error("the file's index is further back than the part that was read")]
    IndexOutOfReach,
    #[error("the file's index is damaged, or this is not a zip after all")]
    Damaged,
}

type Result<T> = std::result::Result<T, ArchiveError>;

/// Reads little-endian fields without ever reading past the end.
struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8], at: usize) -> Self {
        Self { bytes, at }
    }

    fn u16(&mut self) -> Result<u16> {
        let end = self.at.checked_add(2).ok_or(ArchiveError::Damaged)?;
        let slice = self.bytes.get(self.at..end).ok_or(ArchiveError::Damaged)?;
        self.at = end;
        Ok(u16::from_le_bytes([slice[0], slice[1]]))
    }

    fn u32(&mut self) -> Result<u32> {
        let end = self.at.checked_add(4).ok_or(ArchiveError::Damaged)?;
        let slice = self.bytes.get(self.at..end).ok_or(ArchiveError::Damaged)?;
        self.at = end;
        Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
    }

    fn u64(&mut self) -> Result<u64> {
        let end = self.at.checked_add(8).ok_or(ArchiveError::Damaged)?;
        let slice = self.bytes.get(self.at..end).ok_or(ArchiveError::Damaged)?;
        self.at = end;
        let mut wide = [0u8; 8];
        wide.copy_from_slice(slice);
        Ok(u64::from_le_bytes(wide))
    }

    fn skip(&mut self, by: usize) -> Result<()> {
        self.at = self.at.checked_add(by).ok_or(ArchiveError::Damaged)?;
        if self.at > self.bytes.len() {
            return Err(ArchiveError::Damaged);
        }
        Ok(())
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self.at.checked_add(len).ok_or(ArchiveError::Damaged)?;
        let slice = self.bytes.get(self.at..end).ok_or(ArchiveError::Damaged)?;
        self.at = end;
        Ok(slice)
    }
}

/// Where the central directory is, read from the tail of the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Index {
    /// Where the directory starts, as an offset into the whole file.
    pub offset: u64,
    pub size: u64,
    pub entries: u64,
}

/// Finds the central directory, given the last bytes of the file.
///
/// `tail_at` is where `tail` begins in the whole file, which is what turns
/// the offsets inside the record — which are absolute — into something the
/// caller can ask for.
pub fn find_index(tail: &[u8], tail_at: u64) -> Result<Index> {
    // Scanned backwards: the record is at the end, and a file that happens
    // to *contain* the signature earlier would otherwise be found first.
    // Twenty-two bytes is the record with no comment.
    let found = (0..=tail.len().saturating_sub(22))
        .rev()
        .find(|&at| {
            tail.get(at..at + 4).is_some_and(|sig| {
                u32::from_le_bytes([sig[0], sig[1], sig[2], sig[3]]) == END_OF_DIRECTORY
            })
        })
        .ok_or(ArchiveError::NotAZip)?;

    let mut cursor = Cursor::new(tail, found + 4);
    let _disk = cursor.u16()?;
    let _directory_disk = cursor.u16()?;
    let _here = cursor.u16()?;
    let entries = cursor.u16()? as u64;
    let size = cursor.u32()? as u64;
    let offset = cursor.u32()? as u64;

    // The sentinel that means "this is a Zip64 archive and the real
    // numbers are elsewhere". Checked on every field that can carry it,
    // because an archive can exceed one limit without exceeding another.
    let overflowed = entries == u64::from(u16::MAX)
        || size == u64::from(u32::MAX)
        || offset == u64::from(u32::MAX);
    if overflowed && let Some(wide) = find_zip64(tail, tail_at, found)? {
        return Ok(wide);
    }

    Ok(Index { offset, size, entries })
}

/// Follows the Zip64 locator that sits just before the ordinary record.
///
/// `tail_at` is where the slice begins in the whole file, and it is what
/// makes this exact: the locator gives the Zip64 record's position as an
/// absolute offset, so subtracting `tail_at` turns it into an index into
/// the bytes in hand. Hunting for the record's signature instead would
/// find any file data that happened to contain it.
fn find_zip64(tail: &[u8], tail_at: u64, end_record_at: usize) -> Result<Option<Index>> {
    // The locator is a fixed twenty bytes immediately before the ordinary
    // end record. Its absence simply means this is not a Zip64 archive.
    let Some(locator_at) = end_record_at.checked_sub(20) else {
        return Ok(None);
    };
    let mut cursor = Cursor::new(tail, locator_at);
    if cursor.u32()? != ZIP64_LOCATOR {
        return Ok(None);
    }
    let _disk = cursor.u32()?;
    let record_at = cursor.u64()?;

    // Before the window that was read. Not damage: a Zip64 archive with a
    // large comment simply needs a longer tail, and saying which it is
    // lets the caller ask again rather than conclude the file is broken.
    let Some(within) = record_at.checked_sub(tail_at) else {
        return Err(ArchiveError::IndexOutOfReach);
    };
    let within = usize::try_from(within).map_err(|_| ArchiveError::IndexOutOfReach)?;

    let mut cursor = Cursor::new(tail, within);
    if cursor.u32()? != ZIP64_END {
        // The locator pointed somewhere that is not the record. Damage
        // rather than "no Zip64", because the locator said there was one.
        return Err(ArchiveError::Damaged);
    }
    let _record_size = cursor.u64()?;
    let _made_by = cursor.u16()?;
    let _needed = cursor.u16()?;
    let _disk = cursor.u32()?;
    let _directory_disk = cursor.u32()?;
    let _here = cursor.u64()?;
    let entries = cursor.u64()?;
    let size = cursor.u64()?;
    let offset = cursor.u64()?;

    Ok(Some(Index { offset, size, entries }))
}

/// Reads the central directory into a listing.
///
/// `directory` is the bytes the index pointed at. Entries that run past
/// the end stop the walk rather than failing it: a truncated read is
/// common when the directory is larger than the cap, and the entries that
/// did arrive are still worth showing.
pub fn list(directory: &[u8], stated: u64) -> Result<Listing> {
    let mut entries = Vec::new();
    let mut cursor = Cursor::new(directory, 0);
    let wanted = stated.min(MOST_ENTRIES as u64) as usize;

    while entries.len() < wanted {
        match cursor.u32() {
            Ok(DIRECTORY_ENTRY) => {}
            // Anything else means the end of the entries, which includes
            // running out of bytes partway.
            _ => break,
        }

        let entry = match read_entry(&mut cursor) {
            Ok(entry) => entry,
            Err(_) => break,
        };
        entries.push(entry);
    }

    if entries.is_empty() && stated > 0 {
        return Err(ArchiveError::Damaged);
    }

    Ok(Listing { truncated: (entries.len() as u64) < stated, entries })
}

fn read_entry(cursor: &mut Cursor<'_>) -> Result<Entry> {
    let _made_by = cursor.u16()?;
    let _needed = cursor.u16()?;
    let _flags = cursor.u16()?;
    let _method = cursor.u16()?;
    let _time = cursor.u16()?;
    let _date = cursor.u16()?;
    let _crc = cursor.u32()?;
    let compressed = cursor.u32()? as u64;
    let uncompressed = cursor.u32()? as u64;
    let name_len = cursor.u16()? as usize;
    let extra_len = cursor.u16()? as usize;
    let comment_len = cursor.u16()? as usize;
    let _disk = cursor.u16()?;
    let _internal = cursor.u16()?;
    let _external = cursor.u32()?;
    let _local_at = cursor.u32()?;

    let name_bytes = cursor.take(name_len.min(LONGEST_NAME))?;
    // A name longer than the cap still has to be stepped over, or every
    // entry after it is read from the wrong offset.
    if name_len > LONGEST_NAME {
        cursor.skip(name_len - LONGEST_NAME)?;
    }
    let extra = cursor.take(extra_len)?;
    cursor.skip(comment_len)?;

    // Names are stored as UTF-8 when a flag says so and in an unspecified
    // code page otherwise. Lossy either way: a name that will not decode
    // should show with replacement characters rather than hide the entry.
    let name = String::from_utf8_lossy(name_bytes).into_owned();
    let is_directory = name.ends_with('/') || name.ends_with('\\');

    // Zip64 puts the real sizes in the extra field when the ordinary ones
    // have overflowed, so a four-gigabyte file does not list as 4294967295.
    let (compressed, uncompressed) = widen(extra, compressed, uncompressed);

    Ok(Entry { name, compressed, uncompressed, is_directory })
}

/// Replaces overflowed sizes with the Zip64 ones, when they are there.
fn widen(extra: &[u8], compressed: u64, uncompressed: u64) -> (u64, u64) {
    const ZIP64_EXTRA: u16 = 0x0001;
    let overflowed = u64::from(u32::MAX);
    if compressed != overflowed && uncompressed != overflowed {
        return (compressed, uncompressed);
    }

    let mut cursor = Cursor::new(extra, 0);
    while let (Ok(tag), Ok(len)) = (cursor.u16(), cursor.u16()) {
        if tag != ZIP64_EXTRA {
            if cursor.skip(len as usize).is_err() {
                break;
            }
            continue;
        }
        // The fields are present only for the ones that overflowed, in a
        // fixed order, so they are read in that order and only when the
        // matching ordinary field was the sentinel.
        let wide_uncompressed = if uncompressed == overflowed { cursor.u64().ok() } else { None };
        let wide_compressed = if compressed == overflowed { cursor.u64().ok() } else { None };
        return (wide_compressed.unwrap_or(compressed), wide_uncompressed.unwrap_or(uncompressed));
    }
    (compressed, uncompressed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds an end-of-directory record. Hand-assembled rather than
    /// produced by a zip library, so the test states what the bytes are
    /// rather than agreeing with whatever a library happened to write.
    fn end_record(entries: u16, size: u32, offset: u32, comment: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&END_OF_DIRECTORY.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // this disk
        out.extend_from_slice(&0u16.to_le_bytes()); // directory's disk
        out.extend_from_slice(&entries.to_le_bytes()); // entries here
        out.extend_from_slice(&entries.to_le_bytes()); // entries total
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        out.extend_from_slice(&(comment.len() as u16).to_le_bytes());
        out.extend_from_slice(comment);
        out
    }

    fn directory_entry(name: &str, compressed: u32, uncompressed: u32) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&DIRECTORY_ENTRY.to_le_bytes());
        out.extend_from_slice(&0x031eu16.to_le_bytes()); // made by
        out.extend_from_slice(&20u16.to_le_bytes()); // needed
        out.extend_from_slice(&0u16.to_le_bytes()); // flags
        out.extend_from_slice(&8u16.to_le_bytes()); // deflate
        out.extend_from_slice(&0u16.to_le_bytes()); // time
        out.extend_from_slice(&0u16.to_le_bytes()); // date
        out.extend_from_slice(&0u32.to_le_bytes()); // crc
        out.extend_from_slice(&compressed.to_le_bytes());
        out.extend_from_slice(&uncompressed.to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra
        out.extend_from_slice(&0u16.to_le_bytes()); // comment
        out.extend_from_slice(&0u16.to_le_bytes()); // disk
        out.extend_from_slice(&0u16.to_le_bytes()); // internal
        out.extend_from_slice(&0u32.to_le_bytes()); // external
        out.extend_from_slice(&0u32.to_le_bytes()); // local header
        out.extend_from_slice(name.as_bytes());
        out
    }

    #[test]
    fn the_index_is_found_at_the_very_end() {
        let tail = end_record(3, 180, 4096, b"");
        let index = find_index(&tail, 0).unwrap();
        assert_eq!(index, Index { offset: 4096, size: 180, entries: 3 });
    }

    #[test]
    fn a_comment_does_not_hide_the_index() {
        // The record is at the end *unless* there is a comment, which is
        // why it is searched for rather than read from a fixed position.
        let tail = end_record(1, 60, 100, b"made with something or other");
        let index = find_index(&tail, 0).unwrap();
        assert_eq!(index.entries, 1);
    }

    #[test]
    fn the_last_signature_wins_not_the_first() {
        // File data can contain the signature. Scanning forwards would find
        // that one and read four random bytes as the entry count.
        let mut tail = end_record(9, 999, 999, b"");
        tail.extend_from_slice(&end_record(2, 60, 100, b""));
        let index = find_index(&tail, 0).unwrap();
        assert_eq!(index.entries, 2, "the real record is the later one");
    }

    #[test]
    fn something_that_is_not_a_zip_says_so() {
        assert_eq!(find_index(b"this is a jpeg, honestly", 0), Err(ArchiveError::NotAZip));
        assert_eq!(find_index(b"", 0), Err(ArchiveError::NotAZip));
        // Long enough to scan, with nothing in it.
        assert_eq!(find_index(&vec![0u8; 4096], 0), Err(ArchiveError::NotAZip));
    }

    #[test]
    fn a_truncated_record_is_damaged_rather_than_read_past() {
        // The signature is there and the fields after it are not. Reading
        // past would be reading whatever is next in memory.
        let full = end_record(1, 60, 100, b"");
        for cut in 4..full.len() {
            let _ = find_index(&full[..cut], 0);
        }
        // A record that is only its signature: the marker is there and
        // the fields are not, which is damage rather than "not a zip".
        // The difference matters to whoever reads the message.
        assert_eq!(find_index(&END_OF_DIRECTORY.to_le_bytes(), 0), Err(ArchiveError::Damaged));
    }

    #[test]
    fn entries_come_back_with_their_names_and_sizes() {
        let mut directory = Vec::new();
        directory.extend_from_slice(&directory_entry("readme.txt", 120, 400));
        directory.extend_from_slice(&directory_entry("src/", 0, 0));
        directory.extend_from_slice(&directory_entry("src/main.rs", 900, 3000));

        let listing = list(&directory, 3).unwrap();
        assert!(!listing.truncated);
        assert_eq!(listing.entries.len(), 3);

        assert_eq!(listing.entries[0].name, "readme.txt");
        assert_eq!(listing.entries[0].compressed, 120);
        assert_eq!(listing.entries[0].uncompressed, 400);
        assert!(!listing.entries[0].is_directory);

        assert!(listing.entries[1].is_directory, "a trailing slash is how a zip says so");
        assert_eq!(listing.entries[2].name, "src/main.rs");
    }

    #[test]
    fn a_directory_cut_short_lists_what_arrived_and_says_so() {
        // The normal case when the archive's index is bigger than the cap.
        // The entries that did arrive are still worth showing.
        let mut directory = Vec::new();
        directory.extend_from_slice(&directory_entry("one.txt", 1, 1));
        directory.extend_from_slice(&directory_entry("two.txt", 1, 1));
        directory.truncate(directory.len() - 10);

        let listing = list(&directory, 2).unwrap();
        assert_eq!(listing.entries.len(), 1);
        assert!(listing.truncated, "it should admit there is more");
    }

    #[test]
    fn a_name_is_returned_exactly_as_stored() {
        // Not sanitised, and not a path. Whatever unpacks an archive
        // applies its own rules; doing it here would make this look as if
        // it had, and a listing that silently rewrites names is worse than
        // one that shows them.
        let directory = directory_entry("../../etc/passwd", 1, 1);
        let listing = list(&directory, 1).unwrap();
        assert_eq!(listing.entries[0].name, "../../etc/passwd");
    }

    #[test]
    fn a_name_that_is_not_utf8_still_lists() {
        // Zip stores names in an unspecified code page unless a flag says
        // UTF-8. Hiding the entry would be worse than showing it with
        // replacement characters.
        let mut entry = directory_entry("x", 1, 1);
        let at = entry.len() - 1;
        entry[at] = 0xff;
        let listing = list(&entry, 1).unwrap();
        assert_eq!(listing.entries.len(), 1);
        assert!(!listing.entries[0].name.is_empty());
    }

    #[test]
    fn an_entry_count_far_beyond_reason_is_capped() {
        // The count comes out of the archive, so without a cap it decides
        // how much memory this takes.
        let directory = directory_entry("one.txt", 1, 1);
        let listing = list(&directory, u64::MAX).unwrap();
        assert_eq!(listing.entries.len(), 1, "it stops when the bytes run out");
        assert!(listing.truncated);
    }

    #[test]
    fn a_directory_of_rubbish_is_damaged_rather_than_empty() {
        // Claiming entries and producing none means the bytes were not a
        // central directory, which is worth saying.
        assert_eq!(list(b"not a directory at all", 4), Err(ArchiveError::Damaged));
        // But genuinely zero entries is an empty zip, which is fine.
        assert_eq!(list(b"", 0).unwrap().entries.len(), 0);
    }

    #[test]
    fn a_zip64_archive_reports_its_real_numbers() {
        // The ordinary record can only hold 16- and 32-bit fields, so a
        // large archive fills them with sentinels and puts the truth in a
        // Zip64 record. Reading the sentinels would report an archive of
        // 65535 entries starting at 4294967295.
        let huge_offset = 6_000_000_000u64;
        let huge_entries = 200_000u64;

        let mut tail = Vec::new();
        // The Zip64 end record, at offset 0 of this slice.
        tail.extend_from_slice(&ZIP64_END.to_le_bytes());
        tail.extend_from_slice(&44u64.to_le_bytes()); // record size
        tail.extend_from_slice(&45u16.to_le_bytes()); // made by
        tail.extend_from_slice(&45u16.to_le_bytes()); // needed
        tail.extend_from_slice(&0u32.to_le_bytes()); // this disk
        tail.extend_from_slice(&0u32.to_le_bytes()); // directory's disk
        tail.extend_from_slice(&huge_entries.to_le_bytes()); // entries here
        tail.extend_from_slice(&huge_entries.to_le_bytes()); // entries total
        tail.extend_from_slice(&9_000u64.to_le_bytes()); // directory size
        tail.extend_from_slice(&huge_offset.to_le_bytes()); // directory at

        // The locator, which must sit immediately before the end record.
        let record_at = 1_000_000u64; // where this slice begins in the file
        tail.extend_from_slice(&ZIP64_LOCATOR.to_le_bytes());
        tail.extend_from_slice(&0u32.to_le_bytes()); // disk
        tail.extend_from_slice(&record_at.to_le_bytes()); // absolute
        tail.extend_from_slice(&1u32.to_le_bytes()); // disks

        // And the ordinary record, all sentinels.
        tail.extend_from_slice(&end_record(u16::MAX, u32::MAX, u32::MAX, b""));

        let index = find_index(&tail, record_at).unwrap();
        assert_eq!(index.entries, huge_entries);
        assert_eq!(index.offset, huge_offset);
        assert_eq!(index.size, 9_000);
    }

    #[test]
    fn a_zip64_record_before_the_window_says_so_rather_than_failing() {
        // A Zip64 archive with a large comment needs a longer tail. Saying
        // which it is lets the caller ask again instead of concluding the
        // file is broken.
        let mut tail = Vec::new();
        tail.extend_from_slice(&ZIP64_LOCATOR.to_le_bytes());
        tail.extend_from_slice(&0u32.to_le_bytes());
        // Before where this slice starts.
        tail.extend_from_slice(&10u64.to_le_bytes());
        tail.extend_from_slice(&1u32.to_le_bytes());
        tail.extend_from_slice(&end_record(u16::MAX, u32::MAX, u32::MAX, b""));

        assert_eq!(find_index(&tail, 50_000), Err(ArchiveError::IndexOutOfReach));
    }

    #[test]
    fn zip64_sizes_replace_the_overflowed_ones() {
        // Without this a five-gigabyte file lists as 4294967295 bytes,
        // which is both wrong and exactly the kind of wrong that looks
        // plausible.
        let huge = 5_000_000_000u64;
        let mut extra = Vec::new();
        extra.extend_from_slice(&1u16.to_le_bytes()); // Zip64 tag
        extra.extend_from_slice(&16u16.to_le_bytes()); // length
        extra.extend_from_slice(&huge.to_le_bytes()); // uncompressed
        extra.extend_from_slice(&(huge - 1).to_le_bytes()); // compressed

        let (compressed, uncompressed) = widen(&extra, u32::MAX as u64, u32::MAX as u64);
        assert_eq!(uncompressed, huge);
        assert_eq!(compressed, huge - 1);
    }

    #[test]
    fn sizes_that_did_not_overflow_are_left_alone() {
        // The extra field is only consulted for a field carrying the
        // sentinel; reading it otherwise would overwrite good numbers.
        let extra = [1u8, 0, 16, 0, 9, 9, 9, 9, 9, 9, 9, 9, 8, 8, 8, 8, 8, 8, 8, 8];
        assert_eq!(widen(&extra, 100, 200), (100, 200));
    }

    #[test]
    fn the_tail_window_covers_everywhere_the_record_can_hide() {
        // The comment length is sixteen bits, so the record can start at
        // most 65535 bytes before the end, plus its own 22.
        assert_eq!(TAIL_BYTES, 65535 + 1 + 22);
    }
}
