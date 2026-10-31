//! The resume journal.
//!
//! # The invariant
//!
//! **The journal may understate progress; it must never overstate it.**
//! Understating costs a few re-downloaded megabytes. Overstating produces a
//! file that passes every length check and is silently corrupt.
//!
//! [`Journal::commit`] enforces it by ordering: the data file's flush completes
//! *before* any record claiming those bytes is even submitted. Flushing the
//! journal afterwards cannot resurrect an unflushed data write, so no
//! interleaving exists in which a durable record describes non-durable data.
//! Every crash point lands on the safe side:
//!
//! | Crash between | Result |
//! |---|---|
//! | data flush and record append | data on disk, journal unaware -> re-download a little |
//! | record append and journal flush | same |
//! | mid-record | trailing record fails CRC, discarded on replay |
//! | mid-header | the other slot is still valid; log replay rebuilds forward |
//!
//! # The hardware caveat
//!
//! If the volume has write caching on *and* "turn off Windows write-cache
//! buffer flushing" checked, `FlushFileBuffers` does not reach the device and
//! the argument above degrades. There is no clean API to detect that. The
//! mitigation is not a better flush protocol, it is [`Journal::verify`]:
//! recovery *verifies* the bytes against per-block CRC32C rather than trusting
//! the journal. That also covers silent truncation, antivirus interference and
//! a user copying the `.part` file around, none of which any ordering
//! discipline can catch.
//!
//! # On-disk layout
//!
//! ```text
//! 0                    header slot A (4 KiB)
//! 4096                 header slot B (4 KiB)   <- alternating; highest valid seq wins
//! 8192                 block CRC table, 4 bytes per MiB of the file, updated in place
//! (aligned)            snapshot slot A (256 KiB)
//!                      snapshot slot B (256 KiB)
//!                      append-only record log
//! ```
//!
//! Records are fixed 32 bytes and individually CRC'd, so replay can stop at the
//! first bad or short one. The block CRC table is deliberately *advisory*: a
//! torn entry can only cause a conservative re-download, never acceptance of
//! bad data.

use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use crate::extent::{ByteRange, ExtentError, RangeSet};
use crate::pio;
use crate::win::FileIdentity;

const MAGIC: &[u8; 8] = b"IDMJRNL1";
const FORMAT_VERSION: u32 = 1;

const HEADER_SLOT_LEN: u64 = 4096;
const HEADER_SLOT_COUNT: u64 = 2;
/// CRC32C over bytes `[0, HEADER_CRC_OFFSET)` of a slot.
const HEADER_CRC_OFFSET: usize = 4092;

/// Granularity of the verification checksums: 4 bytes of table per MiB of file.
pub const BLOCK_SIZE: u64 = 1 << 20;

const SNAPSHOT_SLOT_LEN: u64 = 256 * 1024;
const SNAPSHOT_HEADER_LEN: usize = 8;
const MAX_SNAPSHOT_EXTENTS: usize =
    (SNAPSHOT_SLOT_LEN as usize - SNAPSHOT_HEADER_LEN) / size_of::<[u64; 2]>();

const RECORD_LEN: usize = 32;
/// CRC32C over bytes `[0, RECORD_CRC_OFFSET)` of a record.
const RECORD_CRC_OFFSET: usize = 28;

/// Compact the log once it exceeds this, to bound replay time on recovery.
const SNAPSHOT_AFTER_LOG_BYTES: u64 = 1 << 20;

/// Blocks beyond the tail window are sampled at this stride in [`VerifyMode::Fast`].
/// Prime so the sampled set does not align with any natural extent boundary.
const FAST_VERIFY_STRIDE: u64 = 97;
/// How much of each extent's tail is always verified: the most recently written
/// bytes are the ones a crash is most likely to have left half-done.
const FAST_VERIFY_TAIL_BYTES: u64 = 32 << 20;

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("journal I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("not a DivinePulse journal: bad magic")]
    BadMagic,
    #[error("journal format version {found} is newer than the supported {supported}")]
    UnsupportedVersion { found: u32, supported: u32 },
    #[error("both journal header slots are corrupt")]
    NoValidHeader,
    #[error("journal belongs to a different file: part identity does not match")]
    IdentityMismatch,
    #[error("part file is {actual} bytes but the journal expects {expected}")]
    SizeMismatch { actual: u64, expected: u64 },
    #[error("journal snapshot failed its checksum")]
    SnapshotCorrupt,
    #[error("{extents} extents will not fit in a snapshot slot (max {max})")]
    SnapshotOverflow { extents: usize, max: usize },
    #[error(transparent)]
    Extent(#[from] ExtentError),
}

/// Which validator the server gave us, and therefore how a resume may be
/// checked. A weak ETag is recorded as such because it is **not** usable with
/// `If-Range`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum ValidatorKind {
    #[default]
    None = 0,
    StrongETag = 1,
    WeakETag = 2,
    LastModified = 3,
    LengthOnly = 4,
}

impl ValidatorKind {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::StrongETag,
            2 => Self::WeakETag,
            3 => Self::LastModified,
            4 => Self::LengthOnly,
            _ => Self::None,
        }
    }

    /// Only a strong validator may be used with `If-Range`: a weak one permits
    /// byte-different representations, which is exactly what would corrupt a
    /// resumed file.
    pub fn usable_for_if_range(self) -> bool {
        matches!(self, Self::StrongETag | Self::LastModified)
    }
}

/// How the `.part` file was preallocated, which recovery needs to know.
pub mod flags {
    /// `FSCTL_SET_SPARSE` succeeded; holes read as zeros.
    pub const SPARSE_USED: u32 = 1 << 0;
    /// `SetFileValidData` was used; **unwritten regions contain stale disk
    /// content**, so recovery must verify in full rather than fast mode.
    pub const VALID_DATA_SET: u32 = 1 << 1;
    /// Claims were restricted to a sliding window ahead of the durable prefix.
    pub const ORDERED_MODE: u32 = 1 << 2;
    /// Total length was never known; append-only, no preallocation.
    pub const LENGTH_UNKNOWN: u32 = 1 << 3;
    /// Server offered no validator strong enough to trust for a resume.
    pub const WEAK_VALIDATOR: u32 = 1 << 4;
    /// Server does not honour ranges; single stream only.
    pub const RANGES_DENIED: u32 = 1 << 5;
}

/// Strings carried in the header arena.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JournalStrings {
    /// The URL the user gave, re-resolvable when a signed link expires.
    pub request_url: String,
    /// The post-redirect URL all segment requests use.
    pub pinned_url: String,
    pub final_name: String,
    pub etag: String,
    pub last_modified: String,
    pub content_type: String,
}

impl JournalStrings {
    fn encode(&self, out: &mut Vec<u8>) {
        for field in self.as_array() {
            out.extend_from_slice(&(field.len() as u32).to_le_bytes());
            out.extend_from_slice(field.as_bytes());
        }
    }

    fn as_array(&self) -> [&String; 6] {
        [
            &self.request_url,
            &self.pinned_url,
            &self.final_name,
            &self.etag,
            &self.last_modified,
            &self.content_type,
        ]
    }

    fn decode(bytes: &[u8]) -> Result<Self, JournalError> {
        let mut cursor = 0usize;
        let mut fields: [String; 6] = Default::default();
        for field in &mut fields {
            if cursor + 4 > bytes.len() {
                return Err(JournalError::NoValidHeader);
            }
            let len = u32::from_le_bytes(bytes[cursor..cursor + 4].try_into().unwrap()) as usize;
            cursor += 4;
            if cursor + len > bytes.len() {
                return Err(JournalError::NoValidHeader);
            }
            *field = String::from_utf8_lossy(&bytes[cursor..cursor + len]).into_owned();
            cursor += len;
        }
        let [request_url, pinned_url, final_name, etag, last_modified, content_type] = fields;
        Ok(Self { request_url, pinned_url, final_name, etag, last_modified, content_type })
    }
}

/// Everything needed to start a journal for a fresh download.
#[derive(Debug, Clone)]
pub struct JournalMeta {
    pub total_len: u64,
    pub generation: u64,
    pub flags: u32,
    pub validator: ValidatorKind,
    pub part_identity: FileIdentity,
    pub strings: JournalStrings,
}

/// Where each region lives, derived purely from the total length so that
/// `create` and `open` can never disagree about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Layout {
    block_count: u32,
    block_crc_off: u64,
    snapshot_slots: [u64; 2],
    log_start: u64,
}

fn align_up(value: u64, to: u64) -> u64 {
    value.div_ceil(to) * to
}

impl Layout {
    fn for_total(total_len: u64) -> Self {
        let block_count = total_len.div_ceil(BLOCK_SIZE);
        let block_crc_off = HEADER_SLOT_LEN * HEADER_SLOT_COUNT;
        let table_end = block_crc_off + block_count * 4;
        let slot_a = align_up(table_end, HEADER_SLOT_LEN);
        let slot_b = slot_a + SNAPSHOT_SLOT_LEN;
        Self {
            block_count: block_count as u32,
            block_crc_off,
            snapshot_slots: [slot_a, slot_b],
            log_start: slot_b + SNAPSHOT_SLOT_LEN,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Header {
    seq: u64,
    flags: u32,
    generation: u64,
    total_len: u64,
    committed_bytes: u64,
    part_identity: FileIdentity,
    /// Seq at the last snapshot. Replay accepts only records above this, which
    /// is what makes stale records left behind by a log reset unusable.
    snapshot_seq: u64,
    snapshot_off: u64,
    snapshot_len: u32,
    snapshot_crc: u32,
    validator: ValidatorKind,
    strings: JournalStrings,
}

impl Header {
    fn encode(&self) -> Result<[u8; HEADER_SLOT_LEN as usize], JournalError> {
        let mut buf = [0u8; HEADER_SLOT_LEN as usize];
        let mut body: Vec<u8> = Vec::with_capacity(256);

        body.extend_from_slice(MAGIC);
        body.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        body.extend_from_slice(&self.flags.to_le_bytes());
        body.extend_from_slice(&self.seq.to_le_bytes());
        body.extend_from_slice(&self.generation.to_le_bytes());
        body.extend_from_slice(&self.total_len.to_le_bytes());
        body.extend_from_slice(&self.committed_bytes.to_le_bytes());
        body.extend_from_slice(&self.part_identity.to_bytes());
        body.extend_from_slice(&self.snapshot_seq.to_le_bytes());
        body.extend_from_slice(&self.snapshot_off.to_le_bytes());
        body.extend_from_slice(&self.snapshot_len.to_le_bytes());
        body.extend_from_slice(&self.snapshot_crc.to_le_bytes());
        body.push(self.validator as u8);
        body.extend_from_slice(&[0u8; 3]); // pad to a 4-byte boundary
        self.strings.encode(&mut body);

        if body.len() > HEADER_CRC_OFFSET {
            // Only reachable with an absurdly long URL; truncating metadata
            // would be worse than refusing.
            return Err(JournalError::SnapshotOverflow {
                extents: body.len(),
                max: HEADER_CRC_OFFSET,
            });
        }
        buf[..body.len()].copy_from_slice(&body);
        let crc = crc32c::crc32c(&buf[..HEADER_CRC_OFFSET]);
        buf[HEADER_CRC_OFFSET..].copy_from_slice(&crc.to_le_bytes());
        Ok(buf)
    }

    fn decode(buf: &[u8; HEADER_SLOT_LEN as usize]) -> Result<Self, JournalError> {
        let stored_crc = u32::from_le_bytes(buf[HEADER_CRC_OFFSET..].try_into().unwrap());
        if crc32c::crc32c(&buf[..HEADER_CRC_OFFSET]) != stored_crc {
            return Err(JournalError::NoValidHeader);
        }
        if &buf[..8] != MAGIC {
            return Err(JournalError::BadMagic);
        }
        let version = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        if version > FORMAT_VERSION {
            return Err(JournalError::UnsupportedVersion {
                found: version,
                supported: FORMAT_VERSION,
            });
        }

        let u32_at = |at: usize| u32::from_le_bytes(buf[at..at + 4].try_into().unwrap());
        let u64_at = |at: usize| u64::from_le_bytes(buf[at..at + 8].try_into().unwrap());

        let mut identity = [0u8; 24];
        identity.copy_from_slice(&buf[48..72]);

        Ok(Self {
            flags: u32_at(12),
            seq: u64_at(16),
            generation: u64_at(24),
            total_len: u64_at(32),
            committed_bytes: u64_at(40),
            part_identity: FileIdentity::from_bytes(identity),
            snapshot_seq: u64_at(72),
            snapshot_off: u64_at(80),
            snapshot_len: u32_at(88),
            snapshot_crc: u32_at(92),
            validator: ValidatorKind::from_u8(buf[96]),
            strings: JournalStrings::decode(&buf[100..HEADER_CRC_OFFSET])?,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordKind {
    Extent = 1,
    Checkpoint = 2,
    Trim = 3,
}

impl RecordKind {
    fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Extent),
            2 => Some(Self::Checkpoint),
            3 => Some(Self::Trim),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Record {
    kind: RecordKind,
    seq: u64,
    /// `Extent`/`Trim`: range start. `Checkpoint`: committed byte count.
    a: u64,
    /// `Extent`/`Trim`: range end. `Checkpoint`: unused.
    b: u64,
}

impl Record {
    fn encode(&self) -> [u8; RECORD_LEN] {
        let mut buf = [0u8; RECORD_LEN];
        buf[0] = self.kind as u8;
        buf[4..12].copy_from_slice(&self.seq.to_le_bytes());
        buf[12..20].copy_from_slice(&self.a.to_le_bytes());
        buf[20..28].copy_from_slice(&self.b.to_le_bytes());
        let crc = crc32c::crc32c(&buf[..RECORD_CRC_OFFSET]);
        buf[RECORD_CRC_OFFSET..].copy_from_slice(&crc.to_le_bytes());
        buf
    }

    /// Returns `None` for any record that fails its checksum or carries an
    /// unknown kind. Replay stops at the first `None`.
    fn decode(buf: &[u8; RECORD_LEN]) -> Option<Self> {
        let stored = u32::from_le_bytes(buf[RECORD_CRC_OFFSET..].try_into().unwrap());
        if crc32c::crc32c(&buf[..RECORD_CRC_OFFSET]) != stored {
            return None;
        }
        Some(Self {
            kind: RecordKind::from_u8(buf[0])?,
            seq: u64::from_le_bytes(buf[4..12].try_into().unwrap()),
            a: u64::from_le_bytes(buf[12..20].try_into().unwrap()),
            b: u64::from_le_bytes(buf[20..28].try_into().unwrap()),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyMode {
    /// Every block in each extent's tail window, plus a strided sample of the
    /// rest. The default: a crash damages recent writes, not old ones.
    Fast,
    /// Every recorded block. Forced when `SetFileValidData` was used, because
    /// unwritten regions then hold plausible-looking stale content.
    Full,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifyReport {
    pub blocks_checked: u64,
    pub blocks_failed: u64,
    /// Ranges removed from the durable set because they failed verification.
    /// An extent is trimmed back to the last good block boundary rather than
    /// discarded, so a single bad block never throws away a whole download.
    pub trimmed: Vec<ByteRange>,
}

/// An open journal plus the durable extent set it describes.
pub struct Journal {
    file: File,
    path: PathBuf,
    header: Header,
    layout: Layout,
    durable: RangeSet,
    /// `0` means "no checksum recorded" — a real CRC32C of zero is possible but
    /// at 2^-32 the only cost is skipping one verifiable block.
    block_crc: Vec<u32>,
    log_end: u64,
    next_seq: u64,
}

impl Journal {
    /// Creates a new journal, overwriting any existing one at `path`.
    pub fn create(path: impl AsRef<Path>, meta: &JournalMeta) -> Result<Self, JournalError> {
        let path = path.as_ref().to_path_buf();
        let file =
            File::options().create(true).truncate(true).read(true).write(true).open(&path)?;

        let layout = Layout::for_total(meta.total_len);
        let header = Header {
            seq: 1,
            flags: meta.flags,
            generation: meta.generation,
            total_len: meta.total_len,
            committed_bytes: 0,
            part_identity: meta.part_identity,
            snapshot_seq: 0,
            snapshot_off: layout.snapshot_slots[0],
            snapshot_len: 0,
            snapshot_crc: 0,
            validator: meta.validator,
            strings: meta.strings.clone(),
        };

        let mut journal = Self {
            file,
            path,
            header,
            layout,
            durable: RangeSet::new(),
            block_crc: vec![0; layout.block_count as usize],
            log_end: layout.log_start,
            next_seq: 1,
        };

        // Reserve the fixed regions so later in-place writes never extend the
        // file, and zero the first log slot so replay of a fresh journal stops
        // immediately.
        journal.file.set_len(layout.log_start + RECORD_LEN as u64)?;
        pio::write_all_at(&journal.file, layout.log_start, &[0u8; RECORD_LEN])?;
        // Slot is always `seq % 2`, here as everywhere, so consecutive headers
        // land in different slots. Writing slot 0 here instead would collide
        // with the first snapshot (seq 2) and leave slot 1 permanently empty,
        // which quietly removes the redundancy this design exists for.
        let slot = journal.header.seq % HEADER_SLOT_COUNT;
        journal.write_header_slot(slot)?;
        journal.file.sync_all()?;
        Ok(journal)
    }

    /// Opens an existing journal and replays it.
    ///
    /// Does **not** touch the data file; call [`verify`](Self::verify) for that.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, JournalError> {
        let path = path.as_ref().to_path_buf();
        let file = File::options().read(true).write(true).open(&path)?;

        // Take whichever slot has the highest seq and a valid checksum, so a
        // torn header write costs at most the last snapshot.
        let mut best: Option<Header> = None;
        let mut bad_magic = false;
        for slot in 0..HEADER_SLOT_COUNT {
            let mut buf = [0u8; HEADER_SLOT_LEN as usize];
            if pio::read_exact_at(&file, slot * HEADER_SLOT_LEN, &mut buf).is_err() {
                continue;
            }
            match Header::decode(&buf) {
                Ok(header) => {
                    if best.as_ref().is_none_or(|current| header.seq > current.seq) {
                        best = Some(header);
                    }
                }
                Err(JournalError::BadMagic) => bad_magic = true,
                Err(JournalError::UnsupportedVersion { found, supported }) => {
                    return Err(JournalError::UnsupportedVersion { found, supported });
                }
                Err(_) => {}
            }
        }
        let header = match best {
            Some(header) => header,
            None if bad_magic => return Err(JournalError::BadMagic),
            None => return Err(JournalError::NoValidHeader),
        };

        let layout = Layout::for_total(header.total_len);
        let mut journal = Self {
            file,
            path,
            header,
            layout,
            durable: RangeSet::new(),
            block_crc: vec![0; layout.block_count as usize],
            log_end: layout.log_start,
            next_seq: 1,
        };

        journal.load_block_crc_table()?;
        journal.load_snapshot()?;
        journal.replay_log()?;
        Ok(journal)
    }

    fn write_header_slot(&mut self, slot: u64) -> Result<(), JournalError> {
        let encoded = self.header.encode()?;
        pio::write_all_at(&self.file, slot * HEADER_SLOT_LEN, &encoded)?;
        Ok(())
    }

    fn load_block_crc_table(&mut self) -> Result<(), JournalError> {
        if self.layout.block_count == 0 {
            return Ok(());
        }
        let mut bytes = vec![0u8; self.layout.block_count as usize * 4];
        // A short read means the table was never fully written; the zeros left
        // behind simply read as "not recorded".
        let read = pio::read_at(&self.file, self.layout.block_crc_off, &mut bytes)?;
        // `as_chunks` gives fixed-size arrays, so there is no fallible
        // conversion to unwrap on every entry.
        let (whole, _) = bytes[..read - read % 4].as_chunks::<4>();
        for (index, chunk) in whole.iter().enumerate() {
            self.block_crc[index] = u32::from_le_bytes(*chunk);
        }
        Ok(())
    }

    fn load_snapshot(&mut self) -> Result<(), JournalError> {
        let len = self.header.snapshot_len as usize;
        if len == 0 {
            return Ok(());
        }
        let mut bytes = vec![0u8; len];
        pio::read_exact_at(&self.file, self.header.snapshot_off, &mut bytes)?;
        if crc32c::crc32c(&bytes) != self.header.snapshot_crc {
            return Err(JournalError::SnapshotCorrupt);
        }
        let count = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        for index in 0..count {
            let at = SNAPSHOT_HEADER_LEN + index * 16;
            let start = u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
            let end = u64::from_le_bytes(bytes[at + 8..at + 16].try_into().unwrap());
            self.durable.insert(ByteRange::try_new(start, end)?);
        }
        Ok(())
    }

    /// Replays records above the snapshot's seq, stopping at the first record
    /// that is short, fails its CRC, or does not advance the sequence.
    ///
    /// The `seq > last` requirement is what makes stale records beyond a reset
    /// log unusable: they always carry a seq at or below `snapshot_seq`.
    fn replay_log(&mut self) -> Result<(), JournalError> {
        let mut offset = self.layout.log_start;
        let mut last_seq = self.header.snapshot_seq;

        loop {
            let mut buf = [0u8; RECORD_LEN];
            match pio::read_at(&self.file, offset, &mut buf) {
                Ok(RECORD_LEN) => {}
                // Short read: a torn tail record, or simply end of file.
                Ok(_) | Err(_) => break,
            }
            let Some(record) = Record::decode(&buf) else { break };
            if record.seq <= last_seq {
                break;
            }

            match record.kind {
                RecordKind::Extent => self.durable.insert(ByteRange::try_new(record.a, record.b)?),
                RecordKind::Trim => self.durable.remove(ByteRange::try_new(record.a, record.b)?),
                RecordKind::Checkpoint => self.header.committed_bytes = record.a,
            }

            last_seq = record.seq;
            offset += RECORD_LEN as u64;
        }

        self.durable.assert_invariants();
        self.log_end = offset;
        self.next_seq = last_seq + 1;
        self.header.committed_bytes = self.durable.total_bytes();
        Ok(())
    }

    /// Commits newly durable extents.
    ///
    /// `newly_durable` must contain only ranges whose writes were issued before
    /// this call; the caller (the writer's sequence fence) guarantees that.
    /// This is where the module's ordering invariant is enforced.
    pub fn commit(
        &mut self,
        data_file: &File,
        newly_durable: &[ByteRange],
        block_crcs: &[(u32, u32)],
    ) -> Result<(), JournalError> {
        // 1. The data must reach the device before anything claims it does.
        //    Every later step only touches the journal file, which cannot undo
        //    or reorder this.
        data_file.sync_data()?;

        // 2. Checksums for blocks that are now durable. Advisory, written in
        //    place; a torn entry can only cause a conservative re-download.
        for &(index, crc) in block_crcs {
            if let Some(slot) = self.block_crc.get_mut(index as usize) {
                *slot = crc;
                pio::write_all_at(
                    &self.file,
                    self.layout.block_crc_off + u64::from(index) * 4,
                    &crc.to_le_bytes(),
                )?;
            }
        }

        // 3. Append the records, then a checkpoint marking the new total.
        let mut batch = Vec::with_capacity((newly_durable.len() + 1) * RECORD_LEN);
        let mut provisional = self.durable.clone();
        for range in newly_durable {
            if range.is_empty() {
                continue;
            }
            provisional.insert(*range);
            batch.extend_from_slice(
                &Record {
                    kind: RecordKind::Extent,
                    seq: self.next_seq,
                    a: range.start(),
                    b: range.end(),
                }
                .encode(),
            );
            self.next_seq += 1;
        }
        batch.extend_from_slice(
            &Record {
                kind: RecordKind::Checkpoint,
                seq: self.next_seq,
                a: provisional.total_bytes(),
                b: 0,
            }
            .encode(),
        );
        self.next_seq += 1;

        pio::write_all_at(&self.file, self.log_end, &batch)?;
        // Zero the slot after the batch so a replay cannot run into whatever
        // stale bytes happen to follow.
        pio::write_all_at(&self.file, self.log_end + batch.len() as u64, &[0u8; RECORD_LEN])?;

        // 4. Make the records durable.
        self.file.sync_data()?;

        self.log_end += batch.len() as u64;
        self.durable = provisional;
        self.header.committed_bytes = self.durable.total_bytes();
        self.durable.assert_invariants();

        if self.log_end - self.layout.log_start > SNAPSHOT_AFTER_LOG_BYTES {
            self.snapshot()?;
        }
        Ok(())
    }

    /// Compacts the log into a snapshot and resets it.
    ///
    /// Writes the snapshot to the slot the live header does *not* reference and
    /// the header to the other slot, so a crash at any point leaves a complete,
    /// self-consistent pair behind.
    pub fn snapshot(&mut self) -> Result<(), JournalError> {
        let extents: Vec<ByteRange> = self.durable.iter().collect();
        if extents.len() > MAX_SNAPSHOT_EXTENTS {
            return Err(JournalError::SnapshotOverflow {
                extents: extents.len(),
                max: MAX_SNAPSHOT_EXTENTS,
            });
        }

        let mut bytes = Vec::with_capacity(SNAPSHOT_HEADER_LEN + extents.len() * 16);
        bytes.extend_from_slice(&(extents.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes()); // reserved
        for extent in &extents {
            bytes.extend_from_slice(&extent.start().to_le_bytes());
            bytes.extend_from_slice(&extent.end().to_le_bytes());
        }

        let target_slot = if self.header.snapshot_off == self.layout.snapshot_slots[0] {
            self.layout.snapshot_slots[1]
        } else {
            self.layout.snapshot_slots[0]
        };
        pio::write_all_at(&self.file, target_slot, &bytes)?;
        self.file.sync_data()?;

        self.header.snapshot_off = target_slot;
        self.header.snapshot_len = bytes.len() as u32;
        self.header.snapshot_crc = crc32c::crc32c(&bytes);
        // Records at or below this seq are stale once the log resets.
        self.header.snapshot_seq = self.next_seq;
        self.next_seq += 1;
        self.header.seq += 1;

        let slot = self.header.seq % HEADER_SLOT_COUNT;
        self.write_header_slot(slot)?;
        self.file.sync_data()?;

        // Deliberately *not* erasing the absorbed records. `snapshot_seq`
        // already makes them unusable to the new header, and leaving them in
        // place means that if this header write is ever torn, the fallback
        // header (which predates the snapshot) can still replay them and
        // recover the progress instead of starting from zero.

        self.log_end = self.layout.log_start;
        Ok(())
    }

    /// Checks the data file against the recorded block checksums, trimming any
    /// extent whose content does not match.
    ///
    /// Trimming stops at the last verified block boundary, so one bad block
    /// costs that block and whatever follows it in the same extent — never the
    /// whole download.
    pub fn verify(
        &mut self,
        data_file: &File,
        mode: VerifyMode,
    ) -> Result<VerifyReport, JournalError> {
        let mode = if self.header.flags & flags::VALID_DATA_SET != 0 {
            // Unwritten regions hold stale disk content rather than zeros, so a
            // sampled check is not good enough.
            VerifyMode::Full
        } else {
            mode
        };

        let mut report = VerifyReport::default();
        let mut buffer = vec![0u8; BLOCK_SIZE as usize];
        let mut trims: Vec<ByteRange> = Vec::new();

        for extent in self.durable.iter() {
            let first_block = extent.start().div_ceil(BLOCK_SIZE);
            let last_block = extent.end() / BLOCK_SIZE;
            let tail_from = extent.end().saturating_sub(FAST_VERIFY_TAIL_BYTES) / BLOCK_SIZE;

            for block in first_block..last_block {
                let in_tail = block >= tail_from;
                let sampled = block % FAST_VERIFY_STRIDE == 0;
                if mode == VerifyMode::Fast && !in_tail && !sampled {
                    continue;
                }
                let Some(&expected) = self.block_crc.get(block as usize) else { continue };
                if expected == 0 {
                    continue; // never recorded
                }

                let start = block * BLOCK_SIZE;
                let len = BLOCK_SIZE.min(self.header.total_len - start) as usize;
                pio::read_exact_at(data_file, start, &mut buffer[..len])?;
                report.blocks_checked += 1;

                if crc32c::crc32c(&buffer[..len]) != expected {
                    report.blocks_failed += 1;
                    // Everything from this block to the end of the extent is
                    // now suspect.
                    trims.push(ByteRange::try_new(start, extent.end())?);
                    break;
                }
            }
        }

        for trim in &trims {
            self.durable.remove(*trim);
            let record = Record {
                kind: RecordKind::Trim,
                seq: self.next_seq,
                a: trim.start(),
                b: trim.end(),
            };
            self.next_seq += 1;
            pio::write_all_at(&self.file, self.log_end, &record.encode())?;
            self.log_end += RECORD_LEN as u64;
        }
        if !trims.is_empty() {
            pio::write_all_at(&self.file, self.log_end, &[0u8; RECORD_LEN])?;
            self.file.sync_data()?;
            self.header.committed_bytes = self.durable.total_bytes();
        }

        report.trimmed = trims;
        self.durable.assert_invariants();
        Ok(report)
    }

    /// Confirms the journal actually describes this file, by identity and size.
    ///
    /// A path is not an identity: the `.part` may have been replaced, restored
    /// from backup or recreated, in which case the extent map describes bytes
    /// that are not there.
    pub fn check_part_file(&self, data_file: &File) -> Result<(), JournalError> {
        if let Ok(identity) = FileIdentity::of(data_file)
            && identity != self.header.part_identity
        {
            return Err(JournalError::IdentityMismatch);
        }
        if self.header.flags & flags::LENGTH_UNKNOWN == 0 {
            let actual = data_file.metadata()?.len();
            if actual != self.header.total_len {
                return Err(JournalError::SizeMismatch { actual, expected: self.header.total_len });
            }
        }
        Ok(())
    }

    /// Flushes and compacts, for a clean pause or shutdown.
    pub fn checkpoint_now(&mut self, data_file: &File) -> Result<(), JournalError> {
        self.commit(data_file, &[], &[])?;
        self.snapshot()
    }

    pub fn durable(&self) -> &RangeSet {
        &self.durable
    }

    /// The work still to do, as the complement of the durable set.
    pub fn remaining(&self) -> Vec<ByteRange> {
        match ByteRange::try_new(0, self.header.total_len) {
            Ok(whole) => self.durable.gaps_in(whole),
            Err(_) => Vec::new(),
        }
    }

    pub fn is_complete(&self) -> bool {
        self.header.total_len > 0 && self.durable.is_complete(self.header.total_len)
    }

    pub fn total_len(&self) -> u64 {
        self.header.total_len
    }

    pub fn committed_bytes(&self) -> u64 {
        self.header.committed_bytes
    }

    pub fn generation(&self) -> u64 {
        self.header.generation
    }

    pub fn flags(&self) -> u32 {
        self.header.flags
    }

    pub fn validator(&self) -> ValidatorKind {
        self.header.validator
    }

    pub fn strings(&self) -> &JournalStrings {
        &self.header.strings
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// CRC32C of a block of `data`, keyed by absolute file offset, for the
    /// writer to hand back through [`commit`](Self::commit).
    pub fn block_index_of(offset: u64) -> u32 {
        (offset / BLOCK_SIZE) as u32
    }

    /// Block checksums for a fully durable range, read back from the data file.
    /// The writer normally computes these from the buffers it already holds;
    /// this exists for recovery and tests.
    pub fn compute_block_crcs(
        &self,
        data_file: &File,
        range: ByteRange,
    ) -> Result<Vec<(u32, u32)>, JournalError> {
        let mut out = Vec::new();
        let mut buffer = vec![0u8; BLOCK_SIZE as usize];
        let first = range.start().div_ceil(BLOCK_SIZE);
        let last = range.end() / BLOCK_SIZE;
        for block in first..last {
            let start = block * BLOCK_SIZE;
            let len = BLOCK_SIZE.min(self.header.total_len - start) as usize;
            pio::read_exact_at(data_file, start, &mut buffer[..len])?;
            out.push((block as u32, crc32c::crc32c(&buffer[..len])));
        }
        Ok(out)
    }

    /// Lets a partially written `BTreeMap` of checksums be handed to `commit`.
    pub fn flatten_crcs(map: &BTreeMap<u32, u32>) -> Vec<(u32, u32)> {
        map.iter().map(|(&index, &crc)| (index, crc)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const TOTAL: u64 = 8 * BLOCK_SIZE;

    struct Fixture {
        _dir: tempfile::TempDir,
        journal_path: PathBuf,
        data_path: PathBuf,
    }

    /// A `.part` file whose byte at offset `i` is a deterministic function of
    /// `i`, so any corruption is detectable without storing a reference copy.
    fn byte_at(offset: u64) -> u8 {
        let mut x = (offset as u32).wrapping_add(0x9e37_79b9);
        x = (x ^ (x >> 16)).wrapping_mul(0x85eb_ca6b);
        x = (x ^ (x >> 13)).wrapping_mul(0xc2b2_ae35);
        (x ^ (x >> 16)) as u8
    }

    fn fixture() -> (Fixture, Journal, File) {
        let dir = tempfile::tempdir().unwrap();
        let data_path = dir.path().join("file.bin.idmpart");
        let journal_path = dir.path().join("file.bin.idmjrn");

        let mut data = File::options()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&data_path)
            .unwrap();
        let content: Vec<u8> = (0..TOTAL).map(byte_at).collect();
        data.write_all(&content).unwrap();
        data.sync_all().unwrap();

        let meta = JournalMeta {
            total_len: TOTAL,
            generation: 1,
            flags: flags::SPARSE_USED,
            validator: ValidatorKind::StrongETag,
            part_identity: FileIdentity::of(&data).unwrap(),
            strings: JournalStrings {
                request_url: "https://example.invalid/big.bin".into(),
                pinned_url: "https://edge7.example.invalid/big.bin".into(),
                final_name: "big.bin".into(),
                etag: "\"abc123\"".into(),
                last_modified: "Wed, 21 Oct 2020 07:28:00 GMT".into(),
                content_type: "application/octet-stream".into(),
            },
        };
        let journal = Journal::create(&journal_path, &meta).unwrap();
        (Fixture { _dir: dir, journal_path, data_path }, journal, data)
    }

    fn reopen_data(fixture: &Fixture) -> File {
        File::options().read(true).write(true).open(&fixture.data_path).unwrap()
    }

    #[test]
    fn metadata_survives_a_round_trip() {
        let (fixture, mut journal, data) = fixture();
        journal.snapshot().unwrap();
        drop(journal);

        let reopened = Journal::open(&fixture.journal_path).unwrap();
        assert_eq!(reopened.total_len(), TOTAL);
        assert_eq!(reopened.generation(), 1);
        assert_eq!(reopened.validator(), ValidatorKind::StrongETag);
        assert_eq!(reopened.strings().final_name, "big.bin");
        assert_eq!(reopened.strings().pinned_url, "https://edge7.example.invalid/big.bin");
        assert_eq!(reopened.flags() & flags::SPARSE_USED, flags::SPARSE_USED);
        reopened.check_part_file(&data).unwrap();
    }

    #[test]
    fn committed_extents_survive_a_reopen() {
        let (fixture, mut journal, data) = fixture();
        journal.commit(&data, &[ByteRange::new(0, BLOCK_SIZE)], &[]).unwrap();
        journal.commit(&data, &[ByteRange::new(4 * BLOCK_SIZE, 5 * BLOCK_SIZE)], &[]).unwrap();
        drop(journal);

        let reopened = Journal::open(&fixture.journal_path).unwrap();
        assert_eq!(reopened.durable().total_bytes(), 2 * BLOCK_SIZE);
        assert!(reopened.durable().contains_range(ByteRange::new(0, BLOCK_SIZE)));
        assert_eq!(
            reopened.remaining(),
            vec![ByteRange::new(BLOCK_SIZE, 4 * BLOCK_SIZE), ByteRange::new(5 * BLOCK_SIZE, TOTAL),]
        );
        assert!(!reopened.is_complete());
    }

    #[test]
    fn a_snapshot_compacts_the_log_and_discards_stale_records() {
        let (fixture, mut journal, data) = fixture();
        // Enough commits to leave plenty of records behind the snapshot.
        for block in 0..6 {
            journal
                .commit(&data, &[ByteRange::new(block * BLOCK_SIZE, (block + 1) * BLOCK_SIZE)], &[])
                .unwrap();
        }
        journal.snapshot().unwrap();
        // One more commit after the reset writes over the stale region.
        journal.commit(&data, &[ByteRange::new(6 * BLOCK_SIZE, 7 * BLOCK_SIZE)], &[]).unwrap();
        drop(journal);

        let reopened = Journal::open(&fixture.journal_path).unwrap();
        // 7 MiB, and crucially not more: the overwritten stale records must not
        // have been replayed on top.
        assert_eq!(reopened.durable().total_bytes(), 7 * BLOCK_SIZE);
        assert_eq!(reopened.durable().extent_count(), 1);
    }

    #[test]
    fn a_torn_header_slot_falls_back_to_the_other() {
        let (fixture, mut journal, data) = fixture();
        journal.commit(&data, &[ByteRange::new(0, BLOCK_SIZE)], &[]).unwrap();
        journal.snapshot().unwrap(); // now slot 0 holds seq 2
        drop(journal);

        // Shred the live slot; the previous one must carry the day.
        let file = File::options().write(true).open(&fixture.journal_path).unwrap();
        pio::write_all_at(&file, 0, &[0xAB; 512]).unwrap();
        drop(file);

        let reopened = Journal::open(&fixture.journal_path).unwrap();
        assert_eq!(reopened.total_len(), TOTAL);
        // The surviving slot predates the snapshot, so it carries no snapshot
        // of its own -- but because the snapshot left the absorbed records in
        // place, replay recovers them from the log rather than starting over.
        assert_eq!(
            reopened.durable().total_bytes(),
            BLOCK_SIZE,
            "a torn header must cost at most the last snapshot, not all progress"
        );
    }

    #[test]
    fn consecutive_headers_alternate_between_the_two_slots() {
        // Regression guard: `create` once wrote seq 1 to slot 0 while the first
        // snapshot (seq 2) also resolved to slot 0, so slot 1 stayed empty and
        // the redundancy silently did not exist.
        let (fixture, mut journal, data) = fixture();
        journal.commit(&data, &[ByteRange::new(0, BLOCK_SIZE)], &[]).unwrap();
        journal.snapshot().unwrap();
        journal.commit(&data, &[ByteRange::new(BLOCK_SIZE, 2 * BLOCK_SIZE)], &[]).unwrap();
        journal.snapshot().unwrap();
        drop(journal);

        let file = File::open(&fixture.journal_path).unwrap();
        let mut seqs = Vec::new();
        for slot in 0..HEADER_SLOT_COUNT {
            let mut buf = [0u8; HEADER_SLOT_LEN as usize];
            pio::read_exact_at(&file, slot * HEADER_SLOT_LEN, &mut buf).unwrap();
            seqs.push(Header::decode(&buf).map(|header| header.seq));
        }

        assert!(
            seqs.iter().all(Result::is_ok),
            "both slots must hold a valid header or there is no redundancy: {seqs:?}"
        );
        assert_ne!(
            seqs[0].as_ref().unwrap(),
            seqs[1].as_ref().unwrap(),
            "the slots must hold different generations"
        );
    }

    #[test]
    fn shredding_either_single_slot_still_leaves_the_journal_openable() {
        for slot in 0..HEADER_SLOT_COUNT {
            let (fixture, mut journal, data) = fixture();
            journal.commit(&data, &[ByteRange::new(0, BLOCK_SIZE)], &[]).unwrap();
            journal.snapshot().unwrap();
            journal.commit(&data, &[ByteRange::new(BLOCK_SIZE, 2 * BLOCK_SIZE)], &[]).unwrap();
            journal.snapshot().unwrap();
            drop(journal);

            let file = File::options().write(true).open(&fixture.journal_path).unwrap();
            pio::write_all_at(&file, slot * HEADER_SLOT_LEN, &[0xAB; 4096]).unwrap();
            drop(file);

            let reopened = Journal::open(&fixture.journal_path)
                .unwrap_or_else(|error| panic!("slot {slot} shredded: {error}"));
            // Never more than was really committed -- understating is safe,
            // overstating is the corruption this design exists to prevent.
            assert!(
                reopened.durable().total_bytes() <= 2 * BLOCK_SIZE,
                "slot {slot}: journal overstated progress"
            );
        }
    }

    #[test]
    fn both_headers_corrupt_is_an_error_not_a_wrong_answer() {
        let (fixture, journal, _data) = fixture();
        drop(journal);
        let file = File::options().write(true).open(&fixture.journal_path).unwrap();
        pio::write_all_at(&file, 0, &[0u8; 8192]).unwrap();
        drop(file);

        assert!(matches!(
            Journal::open(&fixture.journal_path),
            Err(JournalError::NoValidHeader | JournalError::BadMagic)
        ));
    }

    #[test]
    fn a_torn_trailing_record_is_discarded_and_earlier_ones_survive() {
        let (fixture, mut journal, data) = fixture();
        journal.commit(&data, &[ByteRange::new(0, BLOCK_SIZE)], &[]).unwrap();
        journal.commit(&data, &[ByteRange::new(BLOCK_SIZE, 2 * BLOCK_SIZE)], &[]).unwrap();
        let log_end = journal.log_end;
        drop(journal);

        // Simulate power loss partway through writing the next record.
        let file = File::options().write(true).open(&fixture.journal_path).unwrap();
        pio::write_all_at(&file, log_end, &[0x5A; 11]).unwrap();
        drop(file);

        let reopened = Journal::open(&fixture.journal_path).unwrap();
        assert_eq!(
            reopened.durable().total_bytes(),
            2 * BLOCK_SIZE,
            "a torn tail must not cost committed extents"
        );
    }

    #[test]
    fn a_record_failing_its_checksum_stops_replay_there() {
        let (fixture, mut journal, data) = fixture();
        journal.commit(&data, &[ByteRange::new(0, BLOCK_SIZE)], &[]).unwrap();
        let after_first = journal.log_end;
        journal.commit(&data, &[ByteRange::new(BLOCK_SIZE, 2 * BLOCK_SIZE)], &[]).unwrap();
        drop(journal);

        // Flip a byte inside the second commit's first record.
        let file = File::options().read(true).write(true).open(&fixture.journal_path).unwrap();
        let mut byte = [0u8; 1];
        pio::read_exact_at(&file, after_first + 13, &mut byte).unwrap();
        byte[0] ^= 0xFF;
        pio::write_all_at(&file, after_first + 13, &byte).unwrap();
        drop(file);

        let reopened = Journal::open(&fixture.journal_path).unwrap();
        assert_eq!(
            reopened.durable().total_bytes(),
            BLOCK_SIZE,
            "replay must stop at the corrupt record, keeping only what preceded it"
        );
    }

    #[test]
    fn a_replaced_part_file_is_refused() {
        let (fixture, mut journal, data) = fixture();
        journal.commit(&data, &[ByteRange::new(0, BLOCK_SIZE)], &[]).unwrap();
        drop(journal);
        drop(data);

        // Recreate the file at the same path with the same size: only its
        // identity differs.
        std::fs::remove_file(&fixture.data_path).unwrap();
        let replacement = File::options()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&fixture.data_path)
            .unwrap();
        replacement.set_len(TOTAL).unwrap();

        let reopened = Journal::open(&fixture.journal_path).unwrap();
        assert!(matches!(
            reopened.check_part_file(&replacement),
            Err(JournalError::IdentityMismatch)
        ));
    }

    #[test]
    fn a_truncated_part_file_is_refused() {
        let (fixture, journal, data) = fixture();
        drop(journal);
        data.set_len(TOTAL / 2).unwrap();

        let reopened = Journal::open(&fixture.journal_path).unwrap();
        assert!(matches!(
            reopened.check_part_file(&data),
            Err(JournalError::SizeMismatch { expected: TOTAL, .. })
        ));
    }

    #[test]
    fn verification_accepts_intact_data() {
        let (_fixture, mut journal, data) = fixture();
        let whole = ByteRange::new(0, TOTAL);
        let crcs = journal.compute_block_crcs(&data, whole).unwrap();
        journal.commit(&data, &[whole], &crcs).unwrap();

        let report = journal.verify(&data, VerifyMode::Full).unwrap();
        assert_eq!(report.blocks_failed, 0);
        assert_eq!(report.blocks_checked, 8);
        assert!(report.trimmed.is_empty());
        assert!(journal.is_complete());
    }

    #[test]
    fn verification_trims_to_the_last_good_block_rather_than_discarding() {
        let (fixture, mut journal, data) = fixture();
        let whole = ByteRange::new(0, TOTAL);
        let crcs = journal.compute_block_crcs(&data, whole).unwrap();
        journal.commit(&data, &[whole], &crcs).unwrap();
        drop(journal);

        // Corrupt one byte inside block 5.
        let data = reopen_data(&fixture);
        pio::write_all_at(&data, 5 * BLOCK_SIZE + 999, &[0xFF]).unwrap();
        data.sync_all().unwrap();

        let mut reopened = Journal::open(&fixture.journal_path).unwrap();
        let report = reopened.verify(&data, VerifyMode::Full).unwrap();

        assert_eq!(report.blocks_failed, 1);
        assert_eq!(report.trimmed, vec![ByteRange::new(5 * BLOCK_SIZE, TOTAL)]);
        // Blocks 0..5 are kept: one bad block costs the rest of its extent, not
        // the whole download.
        assert_eq!(reopened.durable().total_bytes(), 5 * BLOCK_SIZE);
        assert!(!reopened.is_complete());
        assert_eq!(reopened.remaining(), vec![ByteRange::new(5 * BLOCK_SIZE, TOTAL)]);
    }

    #[test]
    fn a_trim_is_durable_across_a_reopen() {
        let (fixture, mut journal, data) = fixture();
        let whole = ByteRange::new(0, TOTAL);
        let crcs = journal.compute_block_crcs(&data, whole).unwrap();
        journal.commit(&data, &[whole], &crcs).unwrap();
        drop(journal);

        let data = reopen_data(&fixture);
        pio::write_all_at(&data, 2 * BLOCK_SIZE, &[0xFF; 4]).unwrap();
        data.sync_all().unwrap();

        let mut first = Journal::open(&fixture.journal_path).unwrap();
        first.verify(&data, VerifyMode::Full).unwrap();
        drop(first);

        // The trim must not have to be rediscovered.
        let second = Journal::open(&fixture.journal_path).unwrap();
        assert_eq!(second.durable().total_bytes(), 2 * BLOCK_SIZE);
    }

    #[test]
    fn valid_data_set_forces_full_verification() {
        // With SetFileValidData, unwritten regions hold stale disk content
        // rather than zeros, so a sampled check is not sufficient.
        let dir = tempfile::tempdir().unwrap();
        let data_path = dir.path().join("f.idmpart");
        let journal_path = dir.path().join("f.idmjrn");

        let data = File::options()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&data_path)
            .unwrap();
        data.set_len(TOTAL).unwrap();

        let meta = JournalMeta {
            total_len: TOTAL,
            generation: 1,
            flags: flags::VALID_DATA_SET,
            validator: ValidatorKind::StrongETag,
            part_identity: FileIdentity::of(&data).unwrap(),
            strings: JournalStrings::default(),
        };
        let mut journal = Journal::create(&journal_path, &meta).unwrap();
        let whole = ByteRange::new(0, TOTAL);
        let crcs = journal.compute_block_crcs(&data, whole).unwrap();
        journal.commit(&data, &[whole], &crcs).unwrap();

        // Fast is requested, but the flag must escalate it to Full: all 8
        // blocks get checked, not just the tail and the stride sample.
        let report = journal.verify(&data, VerifyMode::Fast).unwrap();
        assert_eq!(report.blocks_checked, 8);
    }

    #[test]
    fn unrecorded_block_checksums_are_skipped_not_failed() {
        let (_fixture, mut journal, data) = fixture();
        // Commit the extent without any checksums at all.
        journal.commit(&data, &[ByteRange::new(0, TOTAL)], &[]).unwrap();
        let report = journal.verify(&data, VerifyMode::Full).unwrap();
        assert_eq!(report.blocks_checked, 0);
        assert_eq!(report.blocks_failed, 0);
        assert!(report.trimmed.is_empty());
    }

    #[test]
    fn checkpoint_now_leaves_a_clean_resumable_state() {
        let (fixture, mut journal, data) = fixture();
        journal.commit(&data, &[ByteRange::new(0, 3 * BLOCK_SIZE)], &[]).unwrap();
        journal.checkpoint_now(&data).unwrap();
        drop(journal);

        let reopened = Journal::open(&fixture.journal_path).unwrap();
        assert_eq!(reopened.durable().total_bytes(), 3 * BLOCK_SIZE);
        assert_eq!(reopened.committed_bytes(), 3 * BLOCK_SIZE);
    }

    #[test]
    fn header_and_record_encodings_round_trip() {
        let header = Header {
            seq: 42,
            flags: flags::SPARSE_USED | flags::ORDERED_MODE,
            generation: 7,
            total_len: 123_456_789,
            committed_bytes: 1024,
            part_identity: FileIdentity { volume_serial: 99, file_id: [3u8; 16] },
            snapshot_seq: 11,
            snapshot_off: 65536,
            snapshot_len: 128,
            snapshot_crc: 0xDEAD_BEEF,
            validator: ValidatorKind::WeakETag,
            strings: JournalStrings {
                request_url: "https://a.invalid/x".into(),
                pinned_url: "https://b.invalid/x".into(),
                final_name: "x".into(),
                etag: "W/\"w\"".into(),
                last_modified: String::new(),
                content_type: "video/mp4".into(),
            },
        };
        assert_eq!(Header::decode(&header.encode().unwrap()).unwrap(), header);

        let record = Record { kind: RecordKind::Trim, seq: 5, a: 100, b: 200 };
        assert_eq!(Record::decode(&record.encode()).unwrap(), record);

        // A single flipped bit must be rejected, not misread.
        let mut encoded = record.encode();
        encoded[12] ^= 0x01;
        assert!(Record::decode(&encoded).is_none());
    }

    #[test]
    fn weak_validators_are_not_usable_for_if_range() {
        assert!(!ValidatorKind::WeakETag.usable_for_if_range());
        assert!(!ValidatorKind::None.usable_for_if_range());
        assert!(!ValidatorKind::LengthOnly.usable_for_if_range());
        assert!(ValidatorKind::StrongETag.usable_for_if_range());
    }
}
