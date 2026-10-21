//! The disk writer: a coalescing, region-sharded positional writer.
//!
//! # Why not the obvious design
//!
//! The intuitive approach is to let every download worker write its own bytes
//! into the preallocated file — embarrassingly parallel, no coordination. It is
//! the wrong choice three times over:
//!
//! * It is exactly the scattered out-of-order access pattern NTFS's Valid Data
//!   Length punishes (see [`crate::win::prealloc`]).
//! * Sixteen workers emitting 32 KiB chunks is a syscall storm and churns NTFS
//!   extents, fragmenting the file.
//! * It leaves nowhere to put the three things that have to live somewhere:
//!   backpressure, the journal's flush fence, and ownership of file metadata.
//!
//! The objection to a single writer is that one thread must be a bottleneck.
//! It is not, at these rates: saturating a gigabit link needs 125 MB/s, and one
//! thread issuing 4 MiB buffered writes to an NVMe device sustains 2–6 GB/s.
//! Even 10 GbE leaves headroom. So writing is funnelled through a small number
//! of shards that each own a file handle, and the parallelism that matters —
//! the network — stays untouched.
//!
//! # Coalescing
//!
//! Each shard holds a reorder buffer keyed by offset, takes the longest
//! contiguous run from the lowest offset, and merges it into one write of up to
//! `io_batch`. Sixteen workers' 32 KiB chunks become 4 MiB near-sequential
//! writes: roughly a hundredth of the syscalls, and far less extent churn.
//! Coalescing is therefore fragmentation control as much as it is a syscall
//! optimisation.
//!
//! # The durability fence
//!
//! An ack is emitted only *after* `seek_write` has returned, so a checkpoint
//! has a simple, airtight ordering available to it:
//!
//! 1. drain the acks received so far,
//! 2. [`Writer::flush_all`] every shard handle,
//! 3. append journal records for the drained ranges.
//!
//! Any ack in hand at step 1 describes a write issued before the flush in
//! step 2, so no sequence-number arithmetic is needed and nothing depends on
//! jobs arriving in order.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::thread::JoinHandle;

use bytes::Bytes;

use crate::extent::{ByteRange, RangeSet};
use crate::journal::BLOCK_SIZE;
use crate::pio;

/// Bytes one backpressure permit stands for.
pub const PERMIT_BYTES: u64 = 64 * 1024;

/// Default coalesced write size. Matches the plan's NVMe figure; callers pick
/// 1 MiB for SATA SSDs and 8 MiB for spinning disks.
pub const DEFAULT_IO_BATCH: usize = 4 << 20;

/// Shard a file by region so each shard's stream stays near-sequential and no
/// two shards ever touch the same 4 KiB block.
pub const DEFAULT_REGION_SIZE: u64 = 256 << 20;

/// Per-shard reorder-buffer ceiling, above which it writes whatever it has
/// rather than holding out for a longer contiguous run.
const DEFAULT_HIGH_WATER: usize = 32 << 20;

/// How long a shard will hold a short run hoping its neighbours arrive.
///
/// Without this a shard would wait for `io_batch` indefinitely, so a file's
/// final partial batch, or a run that simply stops growing, would sit in the
/// reorder buffer holding its budget permits until something else forced a
/// flush. Coalescing has to be an optimisation with a deadline, not a
/// precondition for bytes ever reaching the disk.
const DEFAULT_LINGER: std::time::Duration = std::time::Duration::from_millis(50);

/// An opaque backpressure permit, released when the write is issued.
///
/// Deliberately type-erased: it lets the async download layer hand over a
/// `tokio` semaphore permit without this module depending on tokio, and keeps
/// the writer synchronously testable.
pub type Permit = Box<dyn Send + 'static>;

/// One contiguous run of bytes destined for a file offset.
pub struct WriteJob {
    /// The plan generation these bytes belong to. A job from a superseded
    /// generation is dropped rather than written, which makes "never mix two
    /// server representations in one file" a mechanical guarantee instead of a
    /// discipline the scheduler has to remember.
    pub generation: u64,
    pub offset: u64,
    pub bytes: Bytes,
    pub permit: Option<Permit>,
}

impl WriteJob {
    pub fn new(generation: u64, offset: u64, bytes: Bytes) -> Self {
        Self { generation, offset, bytes, permit: None }
    }

    pub fn with_permit(mut self, permit: Permit) -> Self {
        self.permit = Some(permit);
        self
    }

    pub fn range(&self) -> ByteRange {
        ByteRange::new(self.offset, self.offset + self.bytes.len() as u64)
    }
}

impl std::fmt::Debug for WriteJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriteJob")
            .field("generation", &self.generation)
            .field("range", &self.range())
            .field("has_permit", &self.permit.is_some())
            .finish()
    }
}

/// Emitted once a write has actually been issued to the OS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteAck {
    pub range: ByteRange,
    /// Checksums for blocks this write completed, ready to hand to
    /// [`crate::Journal::commit`].
    pub block_crcs: Vec<(u32, u32)>,
}

#[derive(Debug, Clone)]
pub struct WriterConfig {
    pub generation: u64,
    pub total_len: u64,
    /// Number of shards. One unless the device has no seek penalty — more
    /// shards on a spinning disk is seek thrash, not parallelism.
    pub shards: u8,
    pub io_batch: usize,
    pub region_size: u64,
    pub high_water: usize,
    /// How long to hold a short run waiting for neighbours. See
    /// [`DEFAULT_LINGER`].
    pub linger: std::time::Duration,
}

impl WriterConfig {
    pub fn new(generation: u64, total_len: u64) -> Self {
        Self {
            generation,
            total_len,
            shards: 1,
            io_batch: DEFAULT_IO_BATCH,
            region_size: DEFAULT_REGION_SIZE,
            high_water: DEFAULT_HIGH_WATER,
            linger: DEFAULT_LINGER,
        }
    }

    pub fn with_shards(mut self, shards: u8) -> Self {
        self.shards = shards.max(1);
        self
    }

    pub fn with_io_batch(mut self, io_batch: usize) -> Self {
        self.io_batch = io_batch.max(4096);
        self
    }

    pub fn with_region_size(mut self, region_size: u64) -> Self {
        self.region_size = region_size.max(1 << 20);
        self
    }

    pub fn with_linger(mut self, linger: std::time::Duration) -> Self {
        self.linger = linger;
        self
    }
}

enum ShardMsg {
    Write(WriteJob),
    /// Drain the reorder buffer and flush the handle, then reply.
    Flush(SyncSender<io::Result<()>>),
    Stop,
}

/// A byte-denominated budget that bounds how much downloaded data can be in
/// flight towards the disk.
///
/// When exhausted, a download worker stops reading its socket, the TCP receive
/// window closes and the server slows down. That is correct end-to-end flow
/// control obtained for free, with no timers and no polling.
pub struct ByteBudget {
    state: std::sync::Mutex<u64>,
    available: std::sync::Condvar,
    capacity: u64,
}

/// Releases its bytes back to the budget when dropped.
pub struct BudgetGuard {
    budget: Arc<ByteBudget>,
    bytes: u64,
}

impl Drop for BudgetGuard {
    fn drop(&mut self) {
        let mut used = self.budget.state.lock().unwrap_or_else(|e| e.into_inner());
        *used = used.saturating_sub(self.bytes);
        // One waiter per release is enough; each takes what it needs and the
        // next release wakes the following one.
        self.budget.available.notify_one();
    }
}

impl ByteBudget {
    /// `capacity` should be `min(256 MiB, 12.5% of available RAM, 16 MiB per
    /// connection)`, rounded to whole permits.
    pub fn new(capacity: u64) -> Arc<Self> {
        Arc::new(Self {
            state: std::sync::Mutex::new(0),
            available: std::sync::Condvar::new(),
            capacity: capacity.max(PERMIT_BYTES),
        })
    }

    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    pub fn in_use(&self) -> u64 {
        *self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn try_acquire(self: &Arc<Self>, bytes: u64) -> Option<BudgetGuard> {
        let bytes = bytes.min(self.capacity);
        let mut used = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if *used + bytes > self.capacity {
            return None;
        }
        *used += bytes;
        Some(BudgetGuard { budget: Arc::clone(self), bytes })
    }

    /// Blocks until `bytes` are available. A request larger than the whole
    /// budget is clamped, so a single oversized chunk can never deadlock.
    pub fn acquire_blocking(self: &Arc<Self>, bytes: u64) -> BudgetGuard {
        let bytes = bytes.min(self.capacity);
        let mut used = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while *used + bytes > self.capacity {
            used = self.available.wait(used).unwrap_or_else(|e| e.into_inner());
        }
        *used += bytes;
        BudgetGuard { budget: Arc::clone(self), bytes }
    }
}

struct PendingChunk {
    bytes: Bytes,
    permit: Option<Permit>,
}

/// One shard: its own handle, its own reorder buffer, its own region of the file.
struct Shard {
    file: File,
    pending: BTreeMap<u64, PendingChunk>,
    pending_bytes: usize,
    /// Bytes this shard has issued writes for, used to tell when a block has
    /// become complete and can be checksummed.
    written: RangeSet,
    crc_emitted: BTreeSet<u32>,
    scratch: Vec<u8>,
    read_back: Vec<u8>,
    config: WriterConfig,
    acks: SyncSender<WriteAck>,
    issued_bytes: Arc<AtomicU64>,
    dropped_stale: Arc<AtomicU64>,
}

impl Shard {
    /// Longest contiguous run starting at the lowest pending offset.
    fn contiguous_run(&self) -> Option<(u64, usize)> {
        let (&start, _) = self.pending.iter().next()?;
        let mut end = start;
        while let Some(chunk) = self.pending.get(&end) {
            end += chunk.bytes.len() as u64;
        }
        Some((start, (end - start) as usize))
    }

    fn drain(&mut self, force: bool) -> io::Result<()> {
        while let Some((start, run)) = self.contiguous_run() {
            let worth_writing = force
                || run >= self.config.io_batch
                || self.pending_bytes >= self.config.high_water;
            if !worth_writing {
                return Ok(());
            }
            if !self.issue_from(start)? {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Writes one coalesced batch starting at `start`, returning whether
    /// anything was written.
    fn issue_from(&mut self, start: u64) -> io::Result<bool> {
        let mut buffer = std::mem::take(&mut self.scratch);
        buffer.clear();
        let mut permits: Vec<Option<Permit>> = Vec::new();
        let mut cursor = start;

        // Whole chunks only: splitting one would mean re-slicing Bytes for no
        // gain, and a chunk is already far smaller than io_batch.
        while let Some(next) = self.pending.get(&cursor) {
            if !buffer.is_empty() && buffer.len() + next.bytes.len() > self.config.io_batch {
                break;
            }
            let chunk = self.pending.remove(&cursor).expect("just inspected");
            buffer.extend_from_slice(&chunk.bytes);
            cursor += chunk.bytes.len() as u64;
            self.pending_bytes -= chunk.bytes.len();
            permits.push(chunk.permit);
        }

        if buffer.is_empty() {
            self.scratch = buffer;
            return Ok(false);
        }

        let result = pio::write_all_at(&self.file, start, &buffer);
        self.scratch = buffer;
        result?;

        let range = ByteRange::new(start, cursor);
        self.issued_bytes.fetch_add(range.len(), Ordering::Relaxed);
        self.written.insert(range);

        // The permits are released here and not before: the budget must reflect
        // bytes still owed to the disk, not bytes merely queued.
        drop(permits);

        let block_crcs = self.checksum_completed_blocks(range)?;
        // A closed ack channel means the coordinator is gone; shutting down is
        // not an error.
        let _ = self.acks.send(WriteAck { range, block_crcs });
        Ok(true)
    }

    /// Checksums every block this write completed.
    ///
    /// Blocks are read back rather than checksummed from the write buffer: a
    /// block is often completed by several writes, and the bytes are still in
    /// the OS cache, so this is a memory copy rather than disk I/O.
    fn checksum_completed_blocks(&mut self, range: ByteRange) -> io::Result<Vec<(u32, u32)>> {
        let first = range.start() / BLOCK_SIZE;
        let last = range.end().div_ceil(BLOCK_SIZE);
        let mut out = Vec::new();

        for block in first..last {
            let index = block as u32;
            if self.crc_emitted.contains(&index) {
                continue;
            }
            let start = block * BLOCK_SIZE;
            let end = (start + BLOCK_SIZE).min(self.config.total_len);
            if end <= start {
                continue;
            }
            let block_range = ByteRange::new(start, end);
            // Only a fully written block has a meaningful checksum.
            if !self.written.contains_range(block_range) {
                continue;
            }

            let len = block_range.len() as usize;
            if self.read_back.len() < len {
                self.read_back.resize(len, 0);
            }
            pio::read_exact_at(&self.file, start, &mut self.read_back[..len])?;
            out.push((index, crc32c::crc32c(&self.read_back[..len])));
            self.crc_emitted.insert(index);
        }
        Ok(out)
    }

    fn run(mut self, rx: Receiver<ShardMsg>) {
        use std::sync::mpsc::RecvTimeoutError;

        loop {
            // Block indefinitely while idle, but only up to `linger` while
            // holding data, so a short run always reaches the disk even if no
            // neighbouring chunk ever arrives.
            let message = if self.pending.is_empty() {
                match rx.recv() {
                    Ok(message) => message,
                    Err(_) => break,
                }
            } else {
                match rx.recv_timeout(self.config.linger) {
                    Ok(message) => message,
                    Err(RecvTimeoutError::Timeout) => {
                        if let Err(error) = self.drain(true) {
                            tracing::error!(%error, "writer shard failed to drain on linger");
                        }
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            };

            match message {
                ShardMsg::Write(job) => {
                    if job.generation != self.config.generation {
                        // Stale generation: drop the bytes and the permit. This
                        // is the mechanical half of "never blend two server
                        // representations".
                        self.dropped_stale.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    if job.bytes.is_empty() {
                        continue;
                    }
                    self.pending_bytes += job.bytes.len();
                    self.pending
                        .insert(job.offset, PendingChunk { bytes: job.bytes, permit: job.permit });
                    if let Err(error) = self.drain(false) {
                        tracing::error!(%error, "writer shard failed to drain");
                    }
                }
                ShardMsg::Flush(reply) => {
                    let outcome = self.drain(true).and_then(|()| self.file.sync_data());
                    let _ = reply.send(outcome);
                }
                ShardMsg::Stop => break,
            }
        }
        // Best effort on the way out, so a clean shutdown loses nothing.
        let _ = self.drain(true);
        let _ = self.file.sync_data();
    }
}

struct ShardHandle {
    tx: Sender<ShardMsg>,
    thread: Option<JoinHandle<()>>,
}

/// Which shard owns an offset. Region-based so each shard writes a
/// near-sequential stream and no two shards touch the same 4 KiB block.
fn shard_index(offset: u64, region_size: u64, shards: usize) -> usize {
    ((offset / region_size.max(1)) % shards as u64) as usize
}

/// A cloneable way to queue writes.
///
/// `std::sync::mpsc::Sender` is `Send` but not `Sync`, so a shared `&Writer`
/// cannot be used to queue from several concurrent fetch tasks. Each task takes
/// its own clone of this instead, which costs nothing and keeps the writer's
/// channels lock-free.
#[derive(Clone)]
pub struct WriterHandle {
    shards: Vec<Sender<ShardMsg>>,
    region_size: u64,
}

impl WriterHandle {
    /// Queues a job. Never blocks; memory is bounded by the caller's budget
    /// permit, not by a queue depth.
    pub fn write(&self, job: WriteJob) -> io::Result<()> {
        let index = shard_index(job.offset, self.region_size, self.shards.len());
        self.shards[index]
            .send(ShardMsg::Write(job))
            .map_err(|_| io::Error::other("writer shard has stopped"))
    }

    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }
}

impl std::fmt::Debug for WriterHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriterHandle").field("shards", &self.shards.len()).finish()
    }
}

/// Fans write jobs out to region-owning shards and collects their acks.
pub struct Writer {
    shards: Vec<ShardHandle>,
    config: WriterConfig,
    path: PathBuf,
    issued_bytes: Arc<AtomicU64>,
    dropped_stale: Arc<AtomicU64>,
}

impl Writer {
    /// Spawns the shards, each with its **own** file handle.
    ///
    /// Separate handles are not an optimisation: `seek_write` moves the
    /// handle's shared file pointer as a side effect, so sharing one handle
    /// between shards (or mixing these writes with `Read`/`Write` on the same
    /// handle) would corrupt the file pointer other users depend on.
    pub fn spawn(path: &Path, config: WriterConfig) -> io::Result<(Self, Receiver<WriteAck>)> {
        let (ack_tx, ack_rx) = sync_channel(1024);
        let issued_bytes = Arc::new(AtomicU64::new(0));
        let dropped_stale = Arc::new(AtomicU64::new(0));
        let mut shards = Vec::with_capacity(config.shards as usize);

        for index in 0..config.shards {
            let file = File::options().read(true).write(true).open(path)?;
            // Unbounded in *messages*, because memory is already bounded in
            // *bytes* by the permit budget -- which is the constraint that
            // actually matters. A bounded queue here would instead make
            // `write` block, and an async fetcher blocking on it parks a
            // runtime worker and stalls every other download sharing it.
            let (tx, rx) = channel();
            let shard = Shard {
                file,
                pending: BTreeMap::new(),
                pending_bytes: 0,
                written: RangeSet::new(),
                crc_emitted: BTreeSet::new(),
                scratch: Vec::with_capacity(config.io_batch),
                read_back: Vec::new(),
                config: config.clone(),
                acks: ack_tx.clone(),
                issued_bytes: Arc::clone(&issued_bytes),
                dropped_stale: Arc::clone(&dropped_stale),
            };
            // A dedicated OS thread with blocking receives, deliberately not
            // tokio::fs: that is spawn_blocking per call with an owned-buffer
            // round trip, and several writers through it exhaust the blocking
            // pool and add latency to everything else in the process.
            let thread = std::thread::Builder::new()
                .name(format!("idm-writer-{index}"))
                .spawn(move || shard.run(rx))?;
            shards.push(ShardHandle { tx, thread: Some(thread) });
        }

        Ok((Self { shards, config, path: path.to_path_buf(), issued_bytes, dropped_stale }, ack_rx))
    }

    /// A cloneable handle for queueing writes from concurrent tasks.
    pub fn handle(&self) -> WriterHandle {
        WriterHandle {
            shards: self.shards.iter().map(|shard| shard.tx.clone()).collect(),
            region_size: self.config.region_size,
        }
    }

    fn shard_for(&self, offset: u64) -> usize {
        shard_index(offset, self.config.region_size, self.shards.len())
    }

    /// Queues a job. Never blocks; see the channel note in `spawn`.
    ///
    /// Bounded memory depends on the caller holding a budget permit for the
    /// bytes in the job. Without one, nothing caps how much can be queued.
    pub fn write(&self, job: WriteJob) -> io::Result<()> {
        let shard = self.shard_for(job.offset);
        self.shards[shard]
            .tx
            .send(ShardMsg::Write(job))
            .map_err(|_| io::Error::other("writer shard has stopped"))
    }

    /// Drains and flushes every shard handle.
    ///
    /// Called before appending journal records. Every shard is flushed rather
    /// than just one: although handles to one file share a cache, relying on
    /// that would make the durability argument depend on an implementation
    /// detail instead of an API contract, and a redundant flush finds nothing
    /// dirty and costs nothing.
    pub fn flush_all(&self) -> io::Result<()> {
        let mut replies = Vec::with_capacity(self.shards.len());
        for shard in &self.shards {
            let (tx, rx) = sync_channel(1);
            shard
                .tx
                .send(ShardMsg::Flush(tx))
                .map_err(|_| io::Error::other("writer shard has stopped"))?;
            replies.push(rx);
        }
        // Collected in a second pass so the shards flush concurrently.
        for reply in replies {
            reply.recv().map_err(|_| io::Error::other("writer shard dropped its reply"))??;
        }
        Ok(())
    }

    /// Bytes handed to the OS so far.
    pub fn issued_bytes(&self) -> u64 {
        self.issued_bytes.load(Ordering::Relaxed)
    }

    /// Jobs discarded for belonging to a superseded generation.
    pub fn dropped_stale(&self) -> u64 {
        self.dropped_stale.load(Ordering::Relaxed)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    /// Flushes, stops the shards and waits for them.
    pub fn shutdown(mut self) -> io::Result<()> {
        self.flush_all()?;
        self.stop_shards();
        Ok(())
    }

    fn stop_shards(&mut self) {
        for shard in &mut self.shards {
            let _ = shard.tx.send(ShardMsg::Stop);
        }
        for shard in &mut self.shards {
            if let Some(thread) = shard.thread.take() {
                let _ = thread.join();
            }
        }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // Shards flush on their way out, so dropping a Writer without calling
        // shutdown still cannot lose queued bytes.
        self.stop_shards();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::win::prealloc::{PreallocOptions, preallocate};
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Duration;

    const TOTAL: u64 = 8 * BLOCK_SIZE;

    fn byte_at(offset: u64) -> u8 {
        let mut x = (offset as u32).wrapping_add(0x9e37_79b9);
        x = (x ^ (x >> 16)).wrapping_mul(0x85eb_ca6b);
        x = (x ^ (x >> 13)).wrapping_mul(0xc2b2_ae35);
        (x ^ (x >> 16)) as u8
    }

    fn content(range: ByteRange) -> Bytes {
        Bytes::from((range.start()..range.end()).map(byte_at).collect::<Vec<u8>>())
    }

    struct Harness {
        _dir: tempfile::TempDir,
        path: PathBuf,
    }

    fn harness(total: u64) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.bin.idmpart");
        let file =
            File::options().create(true).truncate(true).read(true).write(true).open(&path).unwrap();
        preallocate(&file, &path, &PreallocOptions::new(total)).unwrap();
        Harness { _dir: dir, path }
    }

    /// Collects acks until the expected byte count has been acknowledged.
    fn collect_acks(rx: &Receiver<WriteAck>, expect_bytes: u64) -> Vec<WriteAck> {
        let mut acks = Vec::new();
        let mut seen = 0u64;
        while seen < expect_bytes {
            match rx.recv_timeout(Duration::from_secs(10)) {
                Ok(ack) => {
                    seen += ack.range.len();
                    acks.push(ack);
                }
                Err(RecvTimeoutError::Timeout) => {
                    panic!("timed out after {seen} of {expect_bytes}")
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        acks
    }

    #[test]
    fn out_of_order_writes_land_at_the_right_offsets() {
        let harness = harness(TOTAL);
        let config = WriterConfig::new(1, TOTAL);
        let (writer, acks) = Writer::spawn(&harness.path, config).unwrap();

        // Deliberately reversed: this is the access pattern the whole design
        // exists to make cheap.
        let chunk = 64 * 1024;
        let mut offsets: Vec<u64> = (0..TOTAL).step_by(chunk as usize).collect();
        offsets.reverse();
        for offset in &offsets {
            let range = ByteRange::new(*offset, (*offset + chunk).min(TOTAL));
            writer.write(WriteJob::new(1, range.start(), content(range))).unwrap();
        }

        collect_acks(&acks, TOTAL);
        writer.shutdown().unwrap();

        // Verify every byte, not just the length.
        let file = File::open(&harness.path).unwrap();
        let mut got = vec![0u8; TOTAL as usize];
        pio::read_exact_at(&file, 0, &mut got).unwrap();
        let want: Vec<u8> = (0..TOTAL).map(byte_at).collect();
        assert_eq!(got, want, "file contents do not match what was written");
    }

    #[test]
    fn small_chunks_are_coalesced_into_far_fewer_writes() {
        let harness = harness(TOTAL);
        let config = WriterConfig::new(1, TOTAL).with_io_batch(4 << 20);
        let (writer, acks) = Writer::spawn(&harness.path, config).unwrap();

        // 32 KiB chunks, in order, as a single fast connection would deliver.
        let chunk = 32 * 1024u64;
        let count = TOTAL / chunk;
        for index in 0..count {
            let range = ByteRange::new(index * chunk, (index + 1) * chunk);
            writer.write(WriteJob::new(1, range.start(), content(range))).unwrap();
        }

        let acks = collect_acks(&acks, TOTAL);
        writer.shutdown().unwrap();

        // The point of coalescing: one ack per issued write, so far fewer acks
        // than chunks submitted.
        assert!(
            acks.len() < count as usize / 8,
            "expected heavy coalescing, got {} writes for {count} chunks",
            acks.len()
        );
        let acked: u64 = acks.iter().map(|ack| ack.range.len()).sum();
        assert_eq!(acked, TOTAL);
    }

    #[test]
    fn a_short_run_reaches_the_disk_without_an_external_flush() {
        // Regression guard. The shard used to write only once a contiguous run
        // reached io_batch, so a run that never grew -- a file's final partial
        // batch, say -- sat in the reorder buffer holding its budget permits
        // until something else happened to force a flush.
        let harness = harness(TOTAL);
        let config = WriterConfig::new(1, TOTAL).with_linger(Duration::from_millis(20));
        let (writer, acks) = Writer::spawn(&harness.path, config).unwrap();

        // Far below the 4 MiB io_batch, and nothing adjacent will follow.
        let range = ByteRange::new(0, 4096);
        writer.write(WriteJob::new(1, range.start(), content(range))).unwrap();

        // No flush_all, no shutdown: the linger alone must get it written.
        let acks = collect_acks(&acks, range.len());
        assert_eq!(acks.len(), 1);
        assert_eq!(acks[0].range, range);

        writer.shutdown().unwrap();
    }

    #[test]
    fn a_gap_does_not_block_the_data_above_it() {
        // One stalled connection must not stop every other connection's bytes
        // from being written.
        let harness = harness(TOTAL);
        let config = WriterConfig::new(1, TOTAL).with_linger(Duration::from_millis(20));
        let (writer, acks) = Writer::spawn(&harness.path, config).unwrap();

        let low = ByteRange::new(0, 64 * 1024);
        let gap = ByteRange::new(64 * 1024, 128 * 1024);
        let high = ByteRange::new(128 * 1024, 192 * 1024);

        // Submit the two outer pieces, leaving a hole between them.
        writer.write(WriteJob::new(1, high.start(), content(high))).unwrap();
        writer.write(WriteJob::new(1, low.start(), content(low))).unwrap();
        let before = collect_acks(&acks, low.len() + high.len());
        assert_eq!(
            before.iter().map(|ack| ack.range.len()).sum::<u64>(),
            low.len() + high.len(),
            "both sides of the gap should be written despite the hole"
        );

        // Filling the hole completes the region.
        writer.write(WriteJob::new(1, gap.start(), content(gap))).unwrap();
        collect_acks(&acks, gap.len());
        writer.shutdown().unwrap();

        let file = File::open(&harness.path).unwrap();
        let mut got = vec![0u8; 192 * 1024];
        pio::read_exact_at(&file, 0, &mut got).unwrap();
        assert_eq!(got, (0..192u64 * 1024).map(byte_at).collect::<Vec<u8>>());
    }

    #[test]
    fn stale_generation_jobs_are_discarded_not_written() {
        let harness = harness(TOTAL);
        let config = WriterConfig::new(7, TOTAL);
        let (writer, acks) = Writer::spawn(&harness.path, config).unwrap();

        // Generation 6 is superseded: writing it would blend two server
        // representations into one file, which passes every length check and
        // is silently corrupt.
        let range = ByteRange::new(0, 64 * 1024);
        writer.write(WriteJob::new(6, range.start(), Bytes::from(vec![0xFFu8; 65536]))).unwrap();
        writer.write(WriteJob::new(7, range.start(), content(range))).unwrap();

        collect_acks(&acks, range.len());
        assert_eq!(writer.dropped_stale(), 1, "the stale job should have been dropped");
        writer.shutdown().unwrap();

        let file = File::open(&harness.path).unwrap();
        let mut got = vec![0u8; 64 * 1024];
        pio::read_exact_at(&file, 0, &mut got).unwrap();
        assert_eq!(
            got,
            (0..64u64 * 1024).map(byte_at).collect::<Vec<u8>>(),
            "stale bytes reached the file"
        );
    }

    #[test]
    fn block_checksums_are_emitted_only_for_complete_blocks() {
        let harness = harness(TOTAL);
        let config = WriterConfig::new(1, TOTAL);
        let (writer, acks) = Writer::spawn(&harness.path, config).unwrap();

        // Half a block: no checksum is computable yet.
        let half = ByteRange::new(0, BLOCK_SIZE / 2);
        writer.write(WriteJob::new(1, half.start(), content(half))).unwrap();
        let partial = collect_acks(&acks, half.len());
        assert!(
            partial.iter().all(|ack| ack.block_crcs.is_empty()),
            "a partial block must not yield a checksum"
        );

        // Completing it must produce exactly one, for block 0.
        let rest = ByteRange::new(BLOCK_SIZE / 2, BLOCK_SIZE);
        writer.write(WriteJob::new(1, rest.start(), content(rest))).unwrap();
        let completing = collect_acks(&acks, rest.len());
        let crcs: Vec<(u32, u32)> =
            completing.iter().flat_map(|ack| ack.block_crcs.clone()).collect();
        assert_eq!(crcs.len(), 1, "expected one checksum, got {crcs:?}");
        assert_eq!(crcs[0].0, 0);

        // And it must match an independent computation over the same bytes.
        let expected: Vec<u8> = (0..BLOCK_SIZE).map(byte_at).collect();
        assert_eq!(crcs[0].1, crc32c::crc32c(&expected));

        writer.shutdown().unwrap();
    }

    #[test]
    fn checksums_are_never_emitted_twice_for_the_same_block() {
        let harness = harness(TOTAL);
        let config = WriterConfig::new(1, TOTAL);
        let (writer, acks) = Writer::spawn(&harness.path, config).unwrap();

        let chunk = 256 * 1024u64;
        let blocks = 4u64;
        for index in 0..(blocks * BLOCK_SIZE / chunk) {
            let range = ByteRange::new(index * chunk, (index + 1) * chunk);
            writer.write(WriteJob::new(1, range.start(), content(range))).unwrap();
        }
        let acks = collect_acks(&acks, blocks * BLOCK_SIZE);
        writer.shutdown().unwrap();

        let mut seen: Vec<u32> =
            acks.iter().flat_map(|a| a.block_crcs.iter().map(|c| c.0)).collect();
        seen.sort_unstable();
        let unique_count = {
            let mut copy = seen.clone();
            copy.dedup();
            copy.len()
        };
        assert_eq!(seen.len(), unique_count, "a block was checksummed twice: {seen:?}");
        assert_eq!(seen, (0..blocks as u32).collect::<Vec<u32>>());
    }

    #[test]
    fn region_sharding_sends_each_offset_to_one_owner() {
        let harness = harness(TOTAL);
        let config = WriterConfig::new(1, TOTAL).with_shards(4);
        let (writer, _acks) = Writer::spawn(&harness.path, config).unwrap();
        assert_eq!(writer.shard_count(), 4);

        // Offsets in one region must all map to the same shard, so no two
        // shards ever write into the same 4 KiB block.
        let region = DEFAULT_REGION_SIZE;
        for offset in [0u64, 4096, region - 1] {
            assert_eq!(writer.shard_for(offset), writer.shard_for(0));
        }
        // And consecutive regions must differ, or sharding buys nothing.
        assert_ne!(writer.shard_for(0), writer.shard_for(region));
    }

    #[test]
    fn multiple_shards_still_reconstruct_the_file_exactly() {
        // A small region size spans several regions across four shards without
        // needing a multi-hundred-megabyte file to do it.
        let region = 4 * BLOCK_SIZE;
        let total = 8 * region;
        let harness = harness(total);
        let config = WriterConfig::new(1, total)
            .with_shards(4)
            .with_region_size(region)
            .with_io_batch(1 << 20)
            .with_linger(Duration::from_millis(20));
        let (writer, acks) = Writer::spawn(&harness.path, config).unwrap();

        let chunk = 256 * 1024u64;
        let mut offsets: Vec<u64> = (0..total).step_by(chunk as usize).collect();
        // Shuffle deterministically so regions interleave across shards.
        offsets.sort_by_key(|offset| (offset / chunk).wrapping_mul(2_654_435_761) % 1_000_003);
        for offset in offsets {
            let range = ByteRange::new(offset, (offset + chunk).min(total));
            writer.write(WriteJob::new(1, range.start(), content(range))).unwrap();
        }

        collect_acks(&acks, total);
        writer.shutdown().unwrap();

        let file = File::open(&harness.path).unwrap();
        let mut got = vec![0u8; total as usize];
        pio::read_exact_at(&file, 0, &mut got).unwrap();
        assert_eq!(got, (0..total).map(byte_at).collect::<Vec<u8>>());
    }

    #[test]
    fn flush_drains_everything_queued() {
        let harness = harness(TOTAL);
        let config = WriterConfig::new(1, TOTAL);
        let (writer, _acks) = Writer::spawn(&harness.path, config).unwrap();

        // A single small chunk is below io_batch, so it would otherwise sit in
        // the reorder buffer waiting for neighbours.
        let range = ByteRange::new(0, 4096);
        writer.write(WriteJob::new(1, range.start(), content(range))).unwrap();
        writer.flush_all().unwrap();

        // After flush_all returns, the bytes must be on disk -- this is the
        // ordering the journal's durability argument rests on.
        assert_eq!(writer.issued_bytes(), 4096);
        let file = File::open(&harness.path).unwrap();
        let mut got = vec![0u8; 4096];
        pio::read_exact_at(&file, 0, &mut got).unwrap();
        assert_eq!(got, (0..4096u64).map(byte_at).collect::<Vec<u8>>());
    }

    #[test]
    fn dropping_the_writer_without_shutdown_still_flushes() {
        let harness = harness(TOTAL);
        let config = WriterConfig::new(1, TOTAL);
        let (writer, acks) = Writer::spawn(&harness.path, config).unwrap();

        let range = ByteRange::new(0, 8192);
        writer.write(WriteJob::new(1, range.start(), content(range))).unwrap();
        drop(writer);
        drop(acks);

        let file = File::open(&harness.path).unwrap();
        let mut got = vec![0u8; 8192];
        pio::read_exact_at(&file, 0, &mut got).unwrap();
        assert_eq!(got, (0..8192u64).map(byte_at).collect::<Vec<u8>>());
    }

    #[test]
    fn the_budget_bounds_bytes_in_flight() {
        let budget = ByteBudget::new(4 * PERMIT_BYTES);
        let first = budget.try_acquire(2 * PERMIT_BYTES).expect("room for the first");
        let _second = budget.try_acquire(2 * PERMIT_BYTES).expect("room for the second");
        assert_eq!(budget.in_use(), 4 * PERMIT_BYTES);

        // Exhausted: a worker that cannot get a permit stops reading its
        // socket, which is what closes the TCP window.
        assert!(budget.try_acquire(PERMIT_BYTES).is_none(), "budget overcommitted");

        drop(first);
        assert_eq!(budget.in_use(), 2 * PERMIT_BYTES);
        assert!(budget.try_acquire(PERMIT_BYTES).is_some(), "release should free capacity");
    }

    #[test]
    fn an_oversized_request_is_clamped_rather_than_deadlocking() {
        let budget = ByteBudget::new(PERMIT_BYTES);
        // Asking for more than the entire budget must not block forever.
        let guard = budget.acquire_blocking(1 << 30);
        assert_eq!(budget.in_use(), PERMIT_BYTES);
        drop(guard);
        assert_eq!(budget.in_use(), 0);
    }

    #[test]
    fn a_permit_is_released_only_once_the_write_is_issued() {
        let harness = harness(TOTAL);
        let budget = ByteBudget::new(16 << 20);
        let config = WriterConfig::new(1, TOTAL);
        let (writer, acks) = Writer::spawn(&harness.path, config).unwrap();

        let range = ByteRange::new(0, 1 << 20);
        let guard = budget.acquire_blocking(range.len());
        assert_eq!(budget.in_use(), range.len());

        writer
            .write(WriteJob::new(1, range.start(), content(range)).with_permit(Box::new(guard)))
            .unwrap();
        writer.flush_all().unwrap();
        collect_acks(&acks, range.len());

        // The budget must reflect bytes still owed to the disk, so it only
        // frees up once the write has actually been issued.
        assert_eq!(budget.in_use(), 0, "permit was not released after the write was issued");
        writer.shutdown().unwrap();
    }
}
