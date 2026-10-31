//! Preallocating the `.part` file, and the NTFS trap that makes it necessary.
//!
//! # Why this exists
//!
//! NTFS tracks a **Valid Data Length**: how far into a file real data has been
//! written. Bytes past it read as zero without occupying disk. A write *beyond*
//! the VDL forces NTFS to zero-fill the gap first, under a lock, before the
//! write lands. So after `set_len(20 GB)`, the first write at offset 7 GB
//! zero-fills seven gigabytes before storing one megabyte.
//!
//! That is fatal for a parallel downloader, whose whole access pattern is
//! out-of-order writes at scattered offsets. Done naively, "16 parallel
//! connections" become a serialised multi-gigabyte zeroing storm, and the
//! symptom — high kernel CPU, throughput collapsing on large files — looks
//! nothing like its cause.
//!
//! # The chain
//!
//! In order, each step falling through to the next:
//!
//! 1. `FSCTL_SET_SPARSE` **before** sizing. Holes cost no disk and no zeroing.
//!    The preferred path: no privilege, no data disclosure. *(Mode: Sparse)*
//! 2. Otherwise `SetFileValidData`, if the user opted in, the process holds
//!    `SeManageVolumePrivilege` and the volume is a local fixed disk. Skips the
//!    zeroing by declaring the file initialised — which exposes stale disk
//!    content, so it is never the default. *(Mode: ValidDataSet)*
//! 3. Otherwise bound the damage instead of eliminating it: the scheduler
//!    refuses any claim beyond a window ahead of the contiguous durable prefix,
//!    so the zero-fill gap is amortised once per window advance rather than
//!    paid on every out-of-order write. *(Mode: OrderedWindow)*
//!
//! Step 3 is the part worth noticing: the VDL problem is solved in the
//! **scheduler's claim policy**, not in the writer. No amount of cleverness at
//! the write call can avoid a cost the filesystem imposes on the offset being
//! written; only choosing different offsets can.

use std::fs::File;
use std::io;
use std::path::Path;

use crate::extent::ByteRange;
use crate::win::volume::{self, VolumeCaps};

/// How far ahead of the durable prefix a claim may be issued in
/// [`WriteMode::OrderedWindow`]. Large enough to keep many connections busy,
/// small enough that each window advance zero-fills a bounded amount.
pub const ORDERED_WINDOW: u64 = 256 << 20;

/// The tighter window used after a quota actually rejects a sparse write.
pub const QUOTA_FALLBACK_WINDOW: u64 = 64 << 20;

/// Refuse to start unless this much slack remains beyond the file itself.
pub const FREE_SPACE_HEADROOM: u64 = 64 << 20;

/// How the `.part` file was prepared, and therefore how the scheduler may hand
/// out work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteMode {
    /// Sparse: writes may go anywhere, in any order, with no zero-fill.
    Sparse,
    /// `SetFileValidData` was applied: writes may go anywhere, but unwritten
    /// regions hold stale disk content, so resume must verify in full.
    ValidDataSet,
    /// Neither was available: claims are confined to a sliding window ahead of
    /// the contiguous durable prefix.
    OrderedWindow { window: u64 },
}

impl WriteMode {
    /// `Some(window)` when claims must stay within that many bytes of the
    /// contiguous durable prefix, `None` when the whole file is fair game.
    pub fn claim_window(&self) -> Option<u64> {
        match self {
            Self::Sparse | Self::ValidDataSet => None,
            Self::OrderedWindow { window } => Some(*window),
        }
    }

    /// Whether a resume must re-verify every recorded block rather than a
    /// sample. True only for `ValidDataSet`, where "unwritten" is
    /// indistinguishable from "plausible stale data".
    pub fn requires_full_verification(&self) -> bool {
        matches!(self, Self::ValidDataSet)
    }

    pub fn journal_flags(&self) -> u32 {
        use crate::journal::flags;
        match self {
            Self::Sparse => flags::SPARSE_USED,
            Self::ValidDataSet => flags::VALID_DATA_SET,
            Self::OrderedWindow { .. } => flags::ORDERED_MODE,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PreallocOptions {
    pub total_len: u64,
    /// Opt in to `SetFileValidData`. Off by default: it skips zero-filling by
    /// exposing previously deleted on-disk content through a file the user can
    /// read, and forces full verification on every resume.
    pub allow_set_valid_data: bool,
}

impl PreallocOptions {
    pub fn new(total_len: u64) -> Self {
        Self { total_len, allow_set_valid_data: false }
    }
}

#[derive(Debug, Clone)]
pub struct Preallocation {
    pub mode: WriteMode,
    pub caps: VolumeCaps,
    /// How many writer shards this device can usefully take. More than one on a
    /// device with a seek penalty is seek thrash, not parallelism.
    pub max_writer_shards: u8,
}

/// Bytes as the window says them, so one number reads the same wherever it
/// is shown: 1024-based, one decimal until the figure reaches three digits.
///
/// This one ends up in front of someone who is being told to go and free
/// up disk space, and `190950628030 bytes` leaves them to work out for
/// themselves how much that is and how far short they are.
fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut power = 0;
    while value >= 1024.0 && power < UNITS.len() - 1 {
        value /= 1024.0;
        power += 1;
    }
    if power == 0 || value >= 100.0 {
        format!("{} {}", value.round() as u64, UNITS[power])
    } else {
        format!("{value:.1} {}", UNITS[power])
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PreallocError {
    #[error("preallocation I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error(
        "not enough space on {volume}: needs {} (the file plus {} to work in) and {} is free — free up {} or save it somewhere else",
        human(*needed),
        human(*headroom),
        human(*available),
        human(needed.saturating_sub(*available)),
    )]
    InsufficientSpace { volume: String, needed: u64, available: u64, headroom: u64 },
}

/// Prepares a freshly created `.part` file for out-of-order parallel writes.
///
/// `file` must already exist **on the destination volume**, so that finalising
/// is a rename rather than a cross-volume copy.
pub fn preallocate(
    file: &File,
    path: &Path,
    options: &PreallocOptions,
) -> Result<Preallocation, PreallocError> {
    let caps = volume::caps_for(path)?;

    // A sparse file reserves nothing, so the filesystem will not catch us
    // running out of room. Checked here before a byte is written, and again
    // by the writer every SPACE_CHECK_INTERVAL bytes, because this check
    // cannot know what another program will do during the hour that
    // follows.
    let space = volume::disk_space(path)?;
    let needed = options.total_len.saturating_add(FREE_SPACE_HEADROOM);
    if space.available_to_caller < needed {
        return Err(PreallocError::InsufficientSpace {
            volume: caps.root.display().to_string(),
            needed,
            available: space.available_to_caller,
            headroom: FREE_SPACE_HEADROOM,
        });
    }

    // Step 1: sparse, before any sizing or writing.
    let sparse = if caps.should_try_sparse() {
        match platform::set_sparse(file, true) {
            Ok(()) => true,
            Err(error) => {
                tracing::debug!(%error, "FSCTL_SET_SPARSE failed; falling through");
                false
            }
        }
    } else {
        false
    };

    // Step 2: the logical size. On a sparse file this reserves no space.
    file.set_len(options.total_len)?;

    // On a non-sparse file, reserving clusters up front limits fragmentation.
    // It does not help a sparse file, where allocation follows the writes.
    if !sparse && let Err(error) = platform::set_allocation_size(file, options.total_len) {
        tracing::debug!(%error, "FileAllocationInfo failed; continuing");
    }

    // Step 3: SetFileValidData, only when everything lines up and the user
    // asked for it.
    let valid_data_set = !sparse
        && options.allow_set_valid_data
        && caps.is_fixed
        && super::privilege::have_manage_volume_privilege()
        && match platform::set_valid_data(file, options.total_len) {
            Ok(()) => true,
            Err(error) => {
                tracing::debug!(%error, "SetFileValidData failed; falling through");
                false
            }
        };

    let mode = if sparse {
        WriteMode::Sparse
    } else if valid_data_set {
        WriteMode::ValidDataSet
    } else {
        WriteMode::OrderedWindow { window: ORDERED_WINDOW }
    };

    // A seek penalty, or a device that will not say, means one shard.
    let max_writer_shards = match volume::has_seek_penalty(path) {
        Ok(Some(false)) => 4,
        _ => 1,
    };

    Ok(Preallocation { mode, caps, max_writer_shards })
}

/// Drops sparseness once the file is complete and has no holes left.
///
/// Only safe when every byte has been written: on a file with holes this would
/// make NTFS materialise them, consuming the full size on disk.
pub fn clear_sparse(file: &File) -> io::Result<()> {
    platform::set_sparse(file, false)
}

/// Regions NTFS has actually allocated within `query`.
///
/// Used as a cheap recovery cross-check: the allocated set must be a superset
/// of the journal's durable extents. It reads no file data, so it costs almost
/// nothing on a multi-gigabyte file.
pub fn allocated_ranges(file: &File, query: ByteRange) -> io::Result<Vec<ByteRange>> {
    platform::allocated_ranges(file, query)
}

/// Whether an error is a disk quota rejection, which is how a sparse write
/// fails when a quota applies. The caller responds by clearing sparseness and
/// dropping to [`QUOTA_FALLBACK_WINDOW`], rather than failing the download.
pub fn is_quota_exceeded(error: &io::Error) -> bool {
    platform::is_quota_exceeded(error)
}

/// Moves the finished file into place and marks it as internet-sourced.
///
/// The rename is write-through so the directory entry is durable. The
/// `Zone.Identifier` stream is what makes Windows and SmartScreen treat the
/// file as downloaded: omitting it silently strips a protection the user
/// expects to have, so a failure to write it is reported rather than ignored.
pub fn finalize(
    part_path: &Path,
    final_path: &Path,
    host_url: &str,
    referrer_url: &str,
) -> io::Result<()> {
    platform::move_write_through(part_path, final_path)?;
    write_mark_of_the_web(final_path, host_url, referrer_url)
}

/// Writes the `Zone.Identifier` alternate data stream (zone 3 = Internet).
pub fn write_mark_of_the_web(path: &Path, host_url: &str, referrer_url: &str) -> io::Result<()> {
    use std::io::Write;

    let mut stream_path = path.as_os_str().to_os_string();
    stream_path.push(":Zone.Identifier");

    let mut contents = String::from("[ZoneTransfer]\r\nZoneId=3\r\n");
    if !host_url.is_empty() {
        contents.push_str(&format!("HostUrl={host_url}\r\n"));
    }
    if !referrer_url.is_empty() {
        contents.push_str(&format!("ReferrerUrl={referrer_url}\r\n"));
    }

    let mut stream = File::create(stream_path)?;
    stream.write_all(contents.as_bytes())?;
    stream.sync_data()
}

#[cfg(windows)]
mod platform {
    use super::{ByteRange, File, Path, io};
    use std::os::windows::io::AsRawHandle;

    use windows::Win32::Foundation::{ERROR_DISK_QUOTA_EXCEEDED, ERROR_MORE_DATA, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        FILE_ALLOCATION_INFO, FileAllocationInfo, MOVEFILE_REPLACE_EXISTING,
        MOVEFILE_WRITE_THROUGH, MoveFileExW, SetFileInformationByHandle, SetFileValidData,
    };
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::{
        FILE_ALLOCATED_RANGE_BUFFER, FILE_SET_SPARSE_BUFFER, FSCTL_QUERY_ALLOCATED_RANGES,
        FSCTL_SET_SPARSE,
    };
    use windows::core::PCWSTR;

    fn handle(file: &File) -> HANDLE {
        HANDLE(file.as_raw_handle())
    }

    pub(super) fn set_sparse(file: &File, sparse: bool) -> io::Result<()> {
        let input = FILE_SET_SPARSE_BUFFER { SetSparse: sparse };
        let mut returned: u32 = 0;
        // SAFETY: the handle is live for the call; `input` is a correctly sized
        // FILE_SET_SPARSE_BUFFER and the length passed matches it. No output
        // buffer is requested.
        unsafe {
            DeviceIoControl(
                handle(file),
                FSCTL_SET_SPARSE,
                Some((&raw const input).cast()),
                size_of::<FILE_SET_SPARSE_BUFFER>() as u32,
                None,
                0,
                Some(&raw mut returned),
                None,
            )
        }
        .map_err(|error| io::Error::other(format!("FSCTL_SET_SPARSE: {error}")))
    }

    pub(super) fn set_allocation_size(file: &File, size: u64) -> io::Result<()> {
        let info = FILE_ALLOCATION_INFO { AllocationSize: size as i64 };
        // SAFETY: the handle is live; `info` matches the class passed and the
        // declared size is exactly its own.
        unsafe {
            SetFileInformationByHandle(
                handle(file),
                FileAllocationInfo,
                (&raw const info).cast(),
                size_of::<FILE_ALLOCATION_INFO>() as u32,
            )
        }
        .map_err(|error| io::Error::other(format!("FileAllocationInfo: {error}")))
    }

    pub(super) fn set_valid_data(file: &File, len: u64) -> io::Result<()> {
        // SAFETY: the handle is live and opened for writing. Requires
        // SeManageVolumePrivilege, which the caller has already confirmed.
        unsafe { SetFileValidData(handle(file), len as i64) }
            .map_err(|error| io::Error::other(format!("SetFileValidData: {error}")))
    }

    pub(super) fn allocated_ranges(file: &File, query: ByteRange) -> io::Result<Vec<ByteRange>> {
        const BATCH: usize = 256;
        let entry_size = size_of::<FILE_ALLOCATED_RANGE_BUFFER>();

        let mut found = Vec::new();
        let mut cursor = query.start();

        while cursor < query.end() {
            let input = FILE_ALLOCATED_RANGE_BUFFER {
                FileOffset: cursor as i64,
                Length: (query.end() - cursor) as i64,
            };
            let mut batch = [FILE_ALLOCATED_RANGE_BUFFER::default(); BATCH];
            let mut returned: u32 = 0;

            // SAFETY: the handle is live; `input` is one correctly sized entry,
            // and `batch` is BATCH contiguous entries whose byte length is
            // passed exactly, so the kernel cannot overrun it.
            let result = unsafe {
                DeviceIoControl(
                    handle(file),
                    FSCTL_QUERY_ALLOCATED_RANGES,
                    Some((&raw const input).cast()),
                    entry_size as u32,
                    Some(batch.as_mut_ptr().cast()),
                    (entry_size * BATCH) as u32,
                    Some(&raw mut returned),
                    None,
                )
            };

            // ERROR_MORE_DATA is success with a truncated answer: the entries
            // returned are valid and the query resumes past them.
            let truncated = match result {
                Ok(()) => false,
                Err(error) if error.code() == ERROR_MORE_DATA.to_hresult() => true,
                Err(error) => {
                    return Err(io::Error::other(format!("FSCTL_QUERY_ALLOCATED_RANGES: {error}")));
                }
            };

            let count = returned as usize / entry_size;
            if count == 0 {
                break;
            }
            for entry in &batch[..count] {
                let start = entry.FileOffset as u64;
                let end = start.saturating_add(entry.Length as u64);
                found.push(ByteRange::try_new(start, end).map_err(io::Error::other)?);
            }

            if !truncated {
                break;
            }
            // Resume from the end of the last range reported, which must move
            // forward or the loop would not terminate.
            let next = found.last().map_or(query.end(), ByteRange::end);
            if next <= cursor {
                break;
            }
            cursor = next;
        }

        Ok(found)
    }

    pub(super) fn is_quota_exceeded(error: &io::Error) -> bool {
        error.raw_os_error() == Some(ERROR_DISK_QUOTA_EXCEEDED.0 as i32)
    }

    pub(super) fn move_write_through(from: &Path, to: &Path) -> io::Result<()> {
        let from_wide = crate::win::wide(from);
        let to_wide = crate::win::wide(to);
        // SAFETY: both buffers are NUL-terminated and outlive the call.
        unsafe {
            MoveFileExW(
                PCWSTR(from_wide.as_ptr()),
                PCWSTR(to_wide.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        }
        .map_err(|error| io::Error::other(format!("MoveFileExW: {error}")))
    }
}

#[cfg(not(windows))]
mod platform {
    use super::{ByteRange, File, Path, io};

    pub(super) fn set_sparse(_file: &File, _sparse: bool) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "sparse files are Windows-only here"))
    }

    pub(super) fn set_allocation_size(_file: &File, _size: u64) -> io::Result<()> {
        Ok(())
    }

    pub(super) fn set_valid_data(_file: &File, _len: u64) -> io::Result<()> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "SetFileValidData is Windows-only"))
    }

    pub(super) fn allocated_ranges(_file: &File, query: ByteRange) -> io::Result<Vec<ByteRange>> {
        Ok(vec![query])
    }

    pub(super) fn is_quota_exceeded(_error: &io::Error) -> bool {
        false
    }

    pub(super) fn move_write_through(from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pio;

    const TOTAL: u64 = 64 << 20;

    #[test]
    fn running_out_of_room_says_how_much_in_words_a_person_reads() {
        // The person reading this is about to go and clear disk space, so
        // the number they need is how far short they are -- not three
        // twelve-digit byte counts to subtract from each other.
        let error = PreallocError::InsufficientSpace {
            volume: "D:\\".into(),
            needed: 190_950_628_030,
            available: 180_735_242_240,
            headroom: 64 << 20,
        };
        let said = error.to_string();

        assert!(said.contains("178 GB"), "the size wanted, in words: {said}");
        assert!(said.contains("168 GB"), "the space there is, in words: {said}");
        assert!(said.contains("9.5 GB"), "how far short, so nobody has to subtract: {said}");
        assert!(said.contains("save it somewhere else"), "and a way out: {said}");
        assert!(!said.contains("190950628030"), "no raw byte counts: {said}");
    }

    fn fresh_part(dir: &Path) -> (std::path::PathBuf, File) {
        let path = dir.join("download.bin.idmpart");
        let file =
            File::options().create(true).truncate(true).read(true).write(true).open(&path).unwrap();
        (path, file)
    }

    #[test]
    fn takes_the_sparse_path_on_an_ordinary_ntfs_volume() {
        let dir = tempfile::tempdir().unwrap();
        let (path, file) = fresh_part(dir.path());

        let prepared =
            preallocate(&file, &path, &PreallocOptions::new(TOTAL)).expect("preallocation failed");

        assert_eq!(file.metadata().unwrap().len(), TOTAL, "logical size must be set");
        if prepared.caps.filesystem == "NTFS" && !prepared.caps.quota_restricted {
            assert_eq!(
                prepared.mode,
                WriteMode::Sparse,
                "NTFS without an active quota must reach the sparse fast path"
            );
            assert_eq!(prepared.mode.claim_window(), None, "sparse puts no bound on claims");
            assert!(!prepared.mode.requires_full_verification());
        }
    }

    #[test]
    fn a_sparse_file_occupies_almost_nothing_until_written() {
        let dir = tempfile::tempdir().unwrap();
        let (path, file) = fresh_part(dir.path());
        let prepared = preallocate(&file, &path, &PreallocOptions::new(TOTAL)).unwrap();
        if prepared.mode != WriteMode::Sparse {
            return; // nothing to assert on a volume without sparse support
        }

        // This is the whole point: 64 MiB of logical size, no allocation and no
        // zero-fill, so an out-of-order write costs nothing extra.
        let allocated = allocated_ranges(&file, ByteRange::new(0, TOTAL)).unwrap();
        assert!(allocated.is_empty(), "a fresh sparse file should allocate nothing: {allocated:?}");

        // Write one block far into the file -- the access pattern that would
        // otherwise trigger a 48 MiB zero-fill.
        pio::write_all_at(&file, 48 << 20, &[0xAB; 4096]).unwrap();
        file.sync_data().unwrap();

        let allocated = allocated_ranges(&file, ByteRange::new(0, TOTAL)).unwrap();
        let total: u64 = allocated.iter().map(ByteRange::len).sum();
        assert!(!allocated.is_empty(), "the written region should now be allocated");
        assert!(
            total < 1 << 20,
            "only the written region should be allocated, got {total} bytes in {allocated:?}"
        );
        assert!(
            allocated.iter().any(|range| range.contains(48 << 20)),
            "the allocated range should cover the offset written: {allocated:?}"
        );
    }

    #[test]
    fn allocated_ranges_are_a_superset_of_what_was_written() {
        // The recovery cross-check: cheap, reads no data, and catches a journal
        // claiming bytes NTFS never allocated.
        let dir = tempfile::tempdir().unwrap();
        let (path, file) = fresh_part(dir.path());
        let prepared = preallocate(&file, &path, &PreallocOptions::new(TOTAL)).unwrap();
        if prepared.mode != WriteMode::Sparse {
            return;
        }

        let written = [ByteRange::new(0, 1 << 20), ByteRange::new(16 << 20, 17 << 20)];
        for range in written {
            pio::write_all_at(&file, range.start(), &vec![1u8; range.len() as usize]).unwrap();
        }
        file.sync_data().unwrap();

        let allocated: crate::RangeSet =
            allocated_ranges(&file, ByteRange::new(0, TOTAL)).unwrap().into_iter().collect();
        for range in written {
            assert!(
                allocated.contains_range(range),
                "{range} was written but is not reported allocated"
            );
        }
    }

    #[test]
    fn refuses_to_start_without_room_for_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let (path, file) = fresh_part(dir.path());

        // Ask for more than any volume has.
        let options = PreallocOptions::new(u64::MAX - FREE_SPACE_HEADROOM);
        let error = preallocate(&file, &path, &options).expect_err("should refuse");
        assert!(
            matches!(error, PreallocError::InsufficientSpace { .. }),
            "expected a space refusal, got {error}"
        );
    }

    #[test]
    fn clearing_sparse_on_a_complete_file_keeps_its_contents() {
        let dir = tempfile::tempdir().unwrap();
        let (path, file) = fresh_part(dir.path());
        let small = 1 << 20;
        let prepared = preallocate(&file, &path, &PreallocOptions::new(small)).unwrap();
        if prepared.mode != WriteMode::Sparse {
            return;
        }

        let payload: Vec<u8> = (0..small).map(|index| (index % 251) as u8).collect();
        pio::write_all_at(&file, 0, &payload).unwrap();
        file.sync_data().unwrap();

        // Finalisation drops sparseness, which is only safe with no holes left.
        clear_sparse(&file).unwrap();

        let mut read_back = vec![0u8; small as usize];
        pio::read_exact_at(&file, 0, &mut read_back).unwrap();
        assert_eq!(read_back, payload, "clearing sparseness must not alter content");
    }

    #[test]
    fn write_modes_describe_their_own_constraints() {
        assert_eq!(WriteMode::Sparse.claim_window(), None);
        assert_eq!(WriteMode::ValidDataSet.claim_window(), None);
        assert_eq!(
            WriteMode::OrderedWindow { window: ORDERED_WINDOW }.claim_window(),
            Some(ORDERED_WINDOW)
        );

        // Only ValidDataSet cannot distinguish "unwritten" from "stale data".
        assert!(WriteMode::ValidDataSet.requires_full_verification());
        assert!(!WriteMode::Sparse.requires_full_verification());
        assert!(!WriteMode::OrderedWindow { window: 1 }.requires_full_verification());
    }

    #[test]
    fn set_valid_data_is_never_used_without_an_explicit_opt_in() {
        let dir = tempfile::tempdir().unwrap();
        let (path, file) = fresh_part(dir.path());

        // Default options must never produce ValidDataSet, whatever privileges
        // this process happens to hold -- it exposes stale disk content.
        let prepared = preallocate(&file, &path, &PreallocOptions::new(TOTAL)).unwrap();
        assert_ne!(prepared.mode, WriteMode::ValidDataSet);
    }

    #[test]
    fn finalize_renames_and_marks_the_file_as_internet_sourced() {
        let dir = tempfile::tempdir().unwrap();
        let (part_path, file) = fresh_part(dir.path());
        let final_path = dir.path().join("download.bin");

        preallocate(&file, &part_path, &PreallocOptions::new(4096)).unwrap();
        pio::write_all_at(&file, 0, &[7u8; 4096]).unwrap();
        file.sync_all().unwrap();
        drop(file);

        finalize(
            &part_path,
            &final_path,
            "https://example.invalid/download.bin",
            "https://example.invalid/page",
        )
        .unwrap();

        assert!(final_path.exists(), "finished file should be at its final name");
        assert!(!part_path.exists(), "the .part file should be gone");

        // Without this stream Windows treats the file as locally created and
        // SmartScreen stays out of the way -- a protection quietly removed.
        let mut stream_path = final_path.as_os_str().to_os_string();
        stream_path.push(":Zone.Identifier");
        let zone = std::fs::read_to_string(stream_path).expect("Zone.Identifier missing");
        assert!(zone.contains("ZoneId=3"), "expected the internet zone, got: {zone}");
        assert!(zone.contains("HostUrl=https://example.invalid/download.bin"));
        assert!(zone.contains("ReferrerUrl=https://example.invalid/page"));
    }
}
