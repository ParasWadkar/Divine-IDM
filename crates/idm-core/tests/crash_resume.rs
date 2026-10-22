//! The crash-durability loop: kill the process mid-write, resume, repeat, and
//! assert the finished file is byte-for-byte correct.
//!
//! This is the test that actually proves resume works. The journal's ordering
//! argument says it may understate progress but must never overstate it, and
//! only this test can tell the difference: if the journal ever claims bytes
//! that were not written, resume skips them and the final content comparison
//! fails. A length check would not notice.
//!
//! The crash is a real `process::abort()` in a real child process — no
//! destructors, no unwinding, no flush on the way out — driven by re-executing
//! this test binary with `IDM_CRASH_ROLE=driver`. Simulating it in-process
//! would prove nothing, because the thing being tested is what survives when
//! cleanup code never runs.
//!
//! Note on scope: `abort()` models an application crash or a kill, not a power
//! cut, because bytes already handed to the OS still reach the disk afterwards.
//! That only ever makes the journal *more* conservative, so the dangerous
//! direction — over-claiming — is still caught here.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;

use bytes::Bytes;
use idm_core::extent::ByteRange;
use idm_core::journal::{Journal, JournalMeta, JournalStrings, ValidatorKind, VerifyMode};
use idm_core::win::FileIdentity;
use idm_core::win::prealloc::{PreallocOptions, preallocate};
use idm_core::writer::{WriteJob, Writer, WriterConfig};
use idm_core::{RangeSet, pio};

const TOTAL: u64 = 16 << 20;
const CHUNK: u64 = 64 << 10;
/// Deliberately small so a crash is very likely to land between a data write
/// and the journal record that would describe it.
const CHECKPOINT_BYTES: u64 = 1 << 20;

/// Deterministic content: the byte at an offset is a pure function of it, so
/// any misplaced or missing write shows up as a content mismatch rather than
/// merely a wrong length.
fn byte_at(offset: u64) -> u8 {
    let mut x = (offset as u32).wrapping_add(0x9e37_79b9);
    x = (x ^ (x >> 16)).wrapping_mul(0x85eb_ca6b);
    x = (x ^ (x >> 13)).wrapping_mul(0xc2b2_ae35);
    (x ^ (x >> 16)) as u8
}

fn content(range: ByteRange) -> Bytes {
    Bytes::from((range.start()..range.end()).map(byte_at).collect::<Vec<u8>>())
}

/// Reproducible pseudo-randomness, so a failure can be replayed exactly.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn part_path(dir: &Path) -> PathBuf {
    dir.join("payload.bin.idmpart")
}

fn journal_path(dir: &Path) -> PathBuf {
    dir.join("payload.bin.idmjrn")
}

/// One pass of the download: resume whatever is missing, optionally aborting
/// partway through. Returns true when the file is complete.
fn run_pass(dir: &Path, abort_after: Option<u64>, seed: u64) -> bool {
    let part = part_path(dir);
    let journal_file = journal_path(dir);
    let resuming = journal_file.exists();

    let data = File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&part)
        .expect("open part file");

    let mut journal = if resuming {
        let mut journal = Journal::open(&journal_file).expect("open journal");
        // A path is not an identity, and a size is not content.
        journal.check_part_file(&data).expect("part file identity/size");
        let report = journal.verify(&data, VerifyMode::Fast).expect("verify");
        if report.blocks_failed > 0 {
            eprintln!(
                "driver: verification trimmed {} range(s) after {} bad block(s)",
                report.trimmed.len(),
                report.blocks_failed
            );
        }
        journal
    } else {
        preallocate(&data, &part, &PreallocOptions::new(TOTAL)).expect("preallocate");
        let meta = JournalMeta {
            total_len: TOTAL,
            generation: 1,
            flags: 0,
            validator: ValidatorKind::StrongETag,
            part_identity: FileIdentity::of(&data).expect("file identity"),
            strings: JournalStrings {
                request_url: "https://example.invalid/payload.bin".into(),
                pinned_url: "https://example.invalid/payload.bin".into(),
                final_name: "payload.bin".into(),
                etag: "\"crash-test\"".into(),
                ..JournalStrings::default()
            },
        };
        Journal::create(&journal_file, &meta).expect("create journal")
    };

    if journal.is_complete() {
        return true;
    }

    // Everything the journal does not already vouch for, in chunks, shuffled so
    // the writer sees genuinely out-of-order offsets.
    let mut chunks: Vec<ByteRange> = Vec::new();
    for gap in journal.remaining() {
        let mut offset = gap.start();
        while offset < gap.end() {
            let end = (offset + CHUNK).min(gap.end());
            chunks.push(ByteRange::new(offset, end));
            offset = end;
        }
    }
    let mut rng = Rng(seed | 1);
    for index in (1..chunks.len()).rev() {
        chunks.swap(index, (rng.next() % (index as u64 + 1)) as usize);
    }

    let config = WriterConfig::new(1, TOTAL).with_linger(std::time::Duration::from_millis(10));
    let (writer, acks) = Writer::spawn(&part, config).expect("spawn writer");

    let mut written_since_checkpoint = 0u64;
    let mut total_written = 0u64;
    let mut pending_ranges: Vec<ByteRange> = Vec::new();
    let mut pending_crcs: Vec<(u32, u32)> = Vec::new();

    for chunk in chunks {
        writer.write(WriteJob::new(1, chunk.start(), content(chunk))).expect("queue write");
        written_since_checkpoint += chunk.len();
        total_written += chunk.len();

        // Harvest whatever the shards have acknowledged so far.
        while let Ok(ack) = acks.try_recv() {
            pending_ranges.push(ack.range);
            pending_crcs.extend(ack.block_crcs);
        }

        if written_since_checkpoint >= CHECKPOINT_BYTES {
            checkpoint(&writer, &acks, &mut journal, &data, &mut pending_ranges, &mut pending_crcs);
            written_since_checkpoint = 0;

            if let Some(limit) = abort_after
                && total_written >= limit
            {
                // The crash: no destructors, no flush, no unwinding.
                eprintln!("driver: aborting after {total_written} bytes");
                std::process::abort();
            }
        }
    }

    checkpoint(&writer, &acks, &mut journal, &data, &mut pending_ranges, &mut pending_crcs);
    writer.shutdown().expect("writer shutdown");
    journal.checkpoint_now(&data).expect("final checkpoint");
    journal.is_complete()
}

/// The durability fence, in the only order that is safe.
fn checkpoint(
    writer: &Writer,
    acks: &std::sync::mpsc::Receiver<idm_core::WriteAck>,
    journal: &mut Journal,
    data: &File,
    ranges: &mut Vec<ByteRange>,
    crcs: &mut Vec<(u32, u32)>,
) {
    // 1. Take every ack in hand. Each was emitted only after its write had been
    //    issued to the OS.
    while let Ok(ack) = acks.try_recv() {
        ranges.push(ack.range);
        crcs.extend(ack.block_crcs);
    }
    // 2. Flush every shard handle, so those writes are on the device.
    writer.flush_all().expect("flush shards");
    // 3. Only now may the journal claim them. commit() flushes the data file
    //    again itself, so the guarantee does not depend on step 2 happening.
    journal.commit(data, ranges, crcs).expect("journal commit");
    ranges.clear();
    crcs.clear();
}

/// Child-process entry point, reached by re-executing this binary.
#[test]
fn driver_entry() {
    if std::env::var("IDM_CRASH_ROLE").as_deref() != Ok("driver") {
        return; // ordinary test run: do nothing
    }
    let dir = PathBuf::from(std::env::var("IDM_CRASH_DIR").expect("IDM_CRASH_DIR"));
    let abort_after = std::env::var("IDM_CRASH_ABORT_AFTER").ok().and_then(|v| v.parse().ok());
    let seed: u64 = std::env::var("IDM_CRASH_SEED").ok().and_then(|v| v.parse().ok()).unwrap_or(1);

    let complete = run_pass(&dir, abort_after, seed);
    // Communicated by exit code, since stdout is the harness's.
    std::process::exit(if complete { 0 } else { 7 });
}

fn spawn_pass(dir: &Path, abort_after: Option<u64>, seed: u64) -> std::process::ExitStatus {
    let exe = std::env::current_exe().expect("current exe");
    let mut command = Command::new(exe);
    command
        .args(["--exact", "driver_entry", "--nocapture"])
        .env("IDM_CRASH_ROLE", "driver")
        .env("IDM_CRASH_DIR", dir)
        .env("IDM_CRASH_SEED", seed.to_string());
    if let Some(limit) = abort_after {
        command.env("IDM_CRASH_ABORT_AFTER", limit.to_string());
    }
    command.output().expect("spawn driver").status
}

#[test]
fn a_file_survives_repeated_crashes_byte_for_byte() {
    if std::env::var("IDM_CRASH_ROLE").is_ok() {
        return; // never recurse from inside a driver child
    }

    let dir = tempfile::tempdir().expect("tempdir");
    // Fixed by default so a failure is reproducible; overridable so CI and a
    // local soak run can sweep many crash points. A durability test run once
    // proves very little.
    let seed = std::env::var("IDM_CRASH_TEST_SEED")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0x5EED_1234_u64);
    eprintln!("crash-resume seed: {seed:#x}");
    let mut rng = Rng(seed);

    let mut passes = 0;
    let mut crashes = 0;
    let complete = loop {
        passes += 1;
        assert!(passes <= 40, "did not converge after {passes} passes");

        // Crash somewhere in the first ~three quarters, so progress is made but
        // the file is never finished by a crashing pass.
        let abort_after = CHECKPOINT_BYTES + rng.next() % (TOTAL * 3 / 4);
        let status = spawn_pass(dir.path(), Some(abort_after), rng.next());

        if status.success() {
            break true;
        }
        // An abort is the expected outcome; exit code 7 means it ran out of
        // work without finishing, which should not happen here.
        assert_ne!(status.code(), Some(7), "pass ended incomplete without crashing");
        crashes += 1;

        // Let a final, uninterrupted pass finish the job.
        if crashes >= 4 {
            let status = spawn_pass(dir.path(), None, rng.next());
            assert!(status.success(), "the final uninterrupted pass failed: {status:?}");
            break true;
        }
    };
    assert!(complete);
    assert!(crashes >= 1, "the test proved nothing: no pass actually crashed");

    // The journal must now agree the file is done...
    let journal = Journal::open(journal_path(dir.path())).expect("reopen journal");
    assert!(journal.is_complete(), "journal does not consider the file complete");
    assert_eq!(journal.committed_bytes(), TOTAL);
    assert!(journal.remaining().is_empty());

    // ...and, the part that actually matters, every byte must be right.
    let data = File::open(part_path(dir.path())).expect("open part");
    assert_eq!(data.metadata().unwrap().len(), TOTAL);
    let mut got = vec![0u8; TOTAL as usize];
    pio::read_exact_at(&data, 0, &mut got).expect("read back");

    let expected: Vec<u8> = (0..TOTAL).map(byte_at).collect();
    if got != expected {
        let first_bad = got.iter().zip(&expected).position(|(a, b)| a != b).unwrap();
        panic!(
            "content mismatch after {crashes} crash(es) across {passes} pass(es); \
             first wrong byte at offset {first_bad}"
        );
    }

    eprintln!("survived {crashes} crash(es) across {passes} pass(es), {TOTAL} bytes verified");
}

#[test]
fn the_journal_never_claims_more_than_the_file_can_show() {
    // The dangerous direction, checked directly: after a crash the journal's
    // durable set must be a subset of what NTFS actually allocated. Claiming a
    // byte that was never written is what produces a silently corrupt file.
    if std::env::var("IDM_CRASH_ROLE").is_ok() {
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let status = spawn_pass(dir.path(), Some(2 << 20), 99);
    assert!(!status.success(), "expected the pass to crash");

    let journal = Journal::open(journal_path(dir.path())).expect("reopen journal");
    let data = File::open(part_path(dir.path())).expect("open part");
    journal.check_part_file(&data).expect("identity should still match");

    let durable = journal.durable();
    assert!(durable.total_bytes() > 0, "no progress was committed, so nothing is proven");
    assert!(durable.total_bytes() <= TOTAL);

    // Cross-check against the filesystem's own view. It reads no file data, so
    // it is cheap even on a large file.
    let allocated: RangeSet =
        idm_core::win::prealloc::allocated_ranges(&data, ByteRange::new(0, TOTAL))
            .expect("allocated ranges")
            .into_iter()
            .collect();
    if !allocated.is_empty() {
        for extent in durable.iter() {
            assert!(
                allocated.contains_range(extent),
                "journal claims {extent} but NTFS reports it unallocated"
            );
        }
    }

    // And the claimed bytes must actually be the right content.
    for extent in durable.iter() {
        let mut got = vec![0u8; extent.len() as usize];
        pio::read_exact_at(&data, extent.start(), &mut got).expect("read claimed extent");
        let expected: Vec<u8> = (extent.start()..extent.end()).map(byte_at).collect();
        assert_eq!(got, expected, "journal claims {extent} but its content is wrong");
    }
}
