//! Real downloads through the whole engine, verified byte for byte.
//!
//! Everything until now has been tested in isolation: the extent algebra
//! against a bitmap model, the journal against induced corruption, the writer
//! against synthetic bytes, the scheduler against snapshots. These tests run
//! the actual pipeline — probe, claim, ranged GET, coalescing writer, journal
//! checkpoint — against the adversarial mock server, and compare the finished
//! file against content computed independently.
//!
//! A length check would pass on a file assembled from the right number of
//! wrong bytes, so every assertion here is on content.

mod support;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Receiver;

use idm_core::journal::{Journal, JournalMeta, JournalStrings, ValidatorKind, VerifyMode};
use idm_core::sched::probe::ProbeOutcome;
use idm_core::sched::{ClaimId, LiveClaim, Protocol, WorkLedger};
use idm_core::win::FileIdentity;
use idm_core::win::prealloc::{PreallocOptions, preallocate};
use idm_core::writer::{WriteAck, Writer, WriterConfig};
use idm_core::{ByteRange, pio};
use idm_net::budget::AsyncBudget;
use idm_net::client::{HttpConfig, LanePool};
use idm_net::fetch::{FetchConfig, FetchOutcome, FetchStop, SegmentRequest, fetch_claim};
use support::MockServer;

/// Matches `byteAt` in tests/mock-server/server.mjs. Computed here rather than
/// fetched, so the comparison shares no code with the thing it checks.
fn byte_at(offset: u64) -> u8 {
    let mut x = (offset as u32).wrapping_add(0x9e37_79b9);
    x = (x ^ (x >> 16)).wrapping_mul(0x85eb_ca6b);
    x = (x ^ (x >> 13)).wrapping_mul(0xc2b2_ae35);
    (x ^ (x >> 16)) as u8
}

struct Target {
    _dir: tempfile::TempDir,
    part: PathBuf,
    journal: PathBuf,
}

impl Target {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        Self {
            part: dir.path().join("payload.bin.idmpart"),
            journal: dir.path().join("payload.bin.idmjrn"),
            _dir: dir,
        }
    }

    fn open_part(&self) -> File {
        File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&self.part)
            .expect("open part file")
    }
}

fn start_journal(target: &Target, data: &File, total: u64, url: &str) -> Journal {
    preallocate(data, &target.part, &PreallocOptions::new(total)).expect("preallocate");
    let meta = JournalMeta {
        total_len: total,
        generation: 1,
        flags: 0,
        validator: ValidatorKind::StrongETag,
        part_identity: FileIdentity::of(data).expect("file identity"),
        strings: JournalStrings {
            request_url: url.to_owned(),
            pinned_url: url.to_owned(),
            final_name: "payload.bin".into(),
            ..JournalStrings::default()
        },
    };
    Journal::create(&target.journal, &meta).expect("create journal")
}

/// Drains the writer's acks into the ledger and journal — the durability
/// fence, in the only safe order.
fn checkpoint(
    acks: &Receiver<WriteAck>,
    writer: &Writer,
    journal: &mut Journal,
    ledger: &mut WorkLedger,
    data: &File,
) {
    // 1. Push the reorder buffers out, so acks exist for everything buffered.
    //    Draining first instead would miss exactly these: a short run sits in
    //    the buffer un-issued, so no ack has been emitted yet, and a final
    //    checkpoint would commit nothing at all.
    writer.flush_all().expect("flush shards");

    // 2. Take every ack now in hand. Each was emitted only after its write had
    //    been issued to the OS.
    let mut ranges = Vec::new();
    let mut crcs = Vec::new();
    while let Ok(ack) = acks.try_recv() {
        ledger.mark_acked(ack.range);
        ranges.push(ack.range);
        crcs.extend(ack.block_crcs);
    }

    // 3. Flush again, covering any write issued between the first sync and
    //    now. Cheap -- usually nothing is dirty -- and it makes every range in
    //    hand durable regardless of concurrent fetchers.
    writer.flush_all().expect("flush shards again");

    // 4. Only now may the journal claim them.
    journal.commit(data, &ranges, &crcs).expect("journal commit");
    for range in ranges {
        ledger.mark_durable(range);
    }
    ledger.assert_invariants();
}

/// Runs claims to completion, requeueing whatever a round fails to deliver.
async fn drive(
    url: &str,
    total: u64,
    connections: u32,
    target: &Target,
    rounds: usize,
) -> (Journal, Vec<FetchStop>) {
    let data = target.open_part();
    let mut journal = start_journal(target, &data, total, url);
    let (writer, acks) =
        Writer::spawn(&target.part, WriterConfig::new(1, total)).expect("spawn writer");
    let handle = writer.handle();
    let budget = AsyncBudget::for_connections(connections);
    let pool = Arc::new(LanePool::new(&HttpConfig::default(), 2).expect("lane pool"));
    let mut ledger = WorkLedger::new(total);
    let mut stops = Vec::new();
    let mut next_id = 0u32;

    for _ in 0..rounds {
        // Hand out a round of claims.
        let mut claims = Vec::new();
        for _ in 0..connections {
            match ledger.next_claim(connections, None) {
                Some(range) => {
                    next_id += 1;
                    claims.push(Arc::new(LiveClaim::new(
                        ClaimId(next_id),
                        range,
                        Protocol::Http1,
                        0,
                    )));
                }
                None => break,
            }
        }
        if claims.is_empty() {
            break;
        }

        let mut tasks = tokio::task::JoinSet::new();
        for claim in &claims {
            let claim = Arc::clone(claim);
            let handle = handle.clone();
            let budget = budget.clone();
            let pool = Arc::clone(&pool);
            let url = url.to_owned();
            tasks.spawn(async move {
                let lane = pool.next_lane().clone();
                let request = SegmentRequest { url: &url, generation: 1, ..Default::default() };
                fetch_claim(&lane, &request, &claim, &handle, &budget, &FetchConfig::default())
                    .await
            });
        }

        while let Some(joined) = tasks.join_next().await {
            match joined.expect("fetch task should not panic").expect("fetch should not error") {
                FetchOutcome::Fetched(fetched) => stops.push(fetched.stop),
                FetchOutcome::Refused { outcome, .. } => {
                    panic!("unexpected refusal: {outcome:?}")
                }
            }
        }

        // Return every claim's range to the pool; the acks below re-account
        // for whatever actually arrived, leaving the residue free.
        for claim in &claims {
            ledger.release(ByteRange::new(claim.start(), claim.end_hard()));
        }
        checkpoint(&acks, &writer, &mut journal, &mut ledger, &data);

        if ledger.is_complete() {
            break;
        }
    }

    checkpoint(&acks, &writer, &mut journal, &mut ledger, &data);
    writer.shutdown().expect("writer shutdown");
    journal.checkpoint_now(&data).expect("final checkpoint");
    (journal, stops)
}

/// Compares the whole file against independently computed content.
fn assert_content_matches(part: &Path, total: u64) {
    let data = File::open(part).expect("open finished file");
    assert_eq!(data.metadata().unwrap().len(), total, "file is the wrong size");

    let mut got = vec![0u8; total as usize];
    pio::read_exact_at(&data, 0, &mut got).expect("read back");
    let expected: Vec<u8> = (0..total).map(byte_at).collect();

    if got != expected {
        let at = got.iter().zip(&expected).position(|(a, b)| a != b).unwrap();
        panic!("content differs at offset {at}: got {:#04x}, want {:#04x}", got[at], expected[at]);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_parallel_download_reproduces_the_file_exactly() {
    let total = 8 << 20;
    let server = MockServer::start();
    let target = Target::new();
    let url = server.url(&format!("/honest?size={total}"));

    let (journal, _) = drive(&url, total, 8, &target, 20).await;

    assert!(journal.is_complete(), "journal should consider the file complete");
    assert_eq!(journal.committed_bytes(), total);
    assert!(journal.remaining().is_empty());
    assert_content_matches(&target.part, total);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_server_that_under_delivers_every_range_still_completes() {
    // /short-range serves half of whatever is asked for. The end must be taken
    // from Content-Range and the residue requeued, round after round, or the
    // download would stall at 50% while believing itself finished.
    let total = 4 << 20;
    let server = MockServer::start();
    let target = Target::new();
    let url = server.url(&format!("/short-range?size={total}"));

    let (journal, stops) = drive(&url, total, 4, &target, 40).await;

    assert!(journal.is_complete(), "requeueing the residue should finish the file");
    assert!(
        stops.contains(&FetchStop::ServerEnded),
        "the server ending early is exactly what should have been observed: {stops:?}"
    );
    assert_content_matches(&target.part, total);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_server_without_range_support_is_refused_rather_than_mis_assembled() {
    // /ignores-range answers a ranged request with the whole body. A claim
    // fetcher must refuse it and let the caller fall back to one stream;
    // writing that body at the claim's offset would corrupt the file.
    let total = 1 << 20;
    let server = MockServer::start();
    let target = Target::new();
    let url = server.url(&format!("/ignores-range?size={total}"));

    let data = target.open_part();
    let _journal = start_journal(&target, &data, total, &url);
    let (writer, _acks) =
        Writer::spawn(&target.part, WriterConfig::new(1, total)).expect("spawn writer");
    let pool = LanePool::new(&HttpConfig::default(), 1).unwrap();

    let claim = LiveClaim::new(ClaimId(1), ByteRange::new(total / 2, total), Protocol::Http1, 0);
    let request = SegmentRequest { url: &url, generation: 1, ..Default::default() };
    let outcome = fetch_claim(
        pool.next_lane(),
        &request,
        &claim,
        &writer.handle(),
        &AsyncBudget::for_connections(1),
        &FetchConfig::default(),
    )
    .await
    .expect("request itself should succeed");

    match outcome {
        FetchOutcome::Refused { outcome, .. } => assert!(
            matches!(outcome, ProbeOutcome::SingleStreamFromZero { .. }),
            "expected a single-stream fallback, got {outcome:?}"
        ),
        FetchOutcome::Fetched(fetched) => {
            panic!("wrote {:?} from a server that ignores Range", fetched.written)
        }
    }
    assert_eq!(claim.cursor(), total / 2, "nothing should have been written");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mid_flight_steal_stops_the_donor_and_the_file_still_completes() {
    // The steal mechanism end to end: lower the soft end while a slow transfer
    // is in progress, confirm the donor notices and retires, then fetch the
    // stolen tail and check every byte.
    let total = 2 << 20;
    let server = MockServer::start();
    let target = Target::new();
    // Trickled, so there is time to steal mid-flight.
    let url = server.url(&format!("/slow?size={total}&bps=1500000"));

    let data = target.open_part();
    let mut journal = start_journal(&target, &data, total, &url);
    let (writer, acks) =
        Writer::spawn(&target.part, WriterConfig::new(1, total)).expect("spawn writer");
    let budget = AsyncBudget::for_connections(2);
    let pool = LanePool::new(&HttpConfig::default(), 2).unwrap();

    let claim = Arc::new(LiveClaim::new(ClaimId(1), ByteRange::new(0, total), Protocol::Http1, 0));
    let donor = {
        let claim = Arc::clone(&claim);
        let handle = writer.handle();
        let budget = budget.clone();
        let lane = pool.next_lane().clone();
        let url = url.clone();
        tokio::spawn(async move {
            let request = SegmentRequest { url: &url, generation: 1, ..Default::default() };
            fetch_claim(&lane, &request, &claim, &handle, &budget, &FetchConfig::default()).await
        })
    };

    // Wait until the donor has made real progress, then take its tail.
    let cut = total / 2;
    let mut stolen = None;
    for _ in 0..200 {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        if claim.cursor() > 0 && claim.cursor() < cut {
            stolen = claim.try_steal_at(cut, 1000);
            if stolen.is_some() {
                break;
            }
        }
    }
    let stolen = stolen.expect("should have stolen the tail mid-flight");
    assert_eq!(stolen, ByteRange::new(cut, total));

    let donor_result = donor.await.expect("donor task").expect("donor fetch");
    let FetchOutcome::Fetched(fetched) = donor_result else { panic!("donor was refused") };
    assert_eq!(
        fetched.stop,
        FetchStop::StolenFrom,
        "the donor should have noticed the lowered soft end"
    );
    assert!(fetched.written.end() <= cut + (1 << 20), "donor overran the cut badly");

    // The thief fetches the stolen range.
    let thief = Arc::new(LiveClaim::new(ClaimId(2), stolen, Protocol::Http1, 0));
    let request = SegmentRequest { url: &url, generation: 1, ..Default::default() };
    fetch_claim(
        pool.next_lane(),
        &request,
        &thief,
        &writer.handle(),
        &budget,
        &FetchConfig::default(),
    )
    .await
    .expect("thief fetch");

    // Anything the donor did not reach before retiring.
    let gap_start = fetched.written.end().min(cut);
    if gap_start < cut {
        let filler = Arc::new(LiveClaim::new(
            ClaimId(3),
            ByteRange::new(gap_start, cut),
            Protocol::Http1,
            0,
        ));
        let request = SegmentRequest { url: &url, generation: 1, ..Default::default() };
        fetch_claim(
            pool.next_lane(),
            &request,
            &filler,
            &writer.handle(),
            &budget,
            &FetchConfig::default(),
        )
        .await
        .expect("filler fetch");
    }

    let mut ledger = WorkLedger::new(total);
    checkpoint(&acks, &writer, &mut journal, &mut ledger, &data);
    writer.shutdown().expect("writer shutdown");

    assert_content_matches(&target.part, total);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_download_resumes_from_its_journal() {
    // Larger than a single claim: with one connection `claim_size` is
    // remaining/4 clamped to at least 8 MiB, so an 8 MiB file would be taken
    // whole and the first pass could not be partial.
    let total = 40 << 20;
    let server = MockServer::start();
    let target = Target::new();
    let url = server.url(&format!("/honest?size={total}"));

    // First pass: deliberately too few rounds to finish.
    let partial = {
        let data = target.open_part();
        let mut journal = start_journal(&target, &data, total, &url);
        let (writer, acks) =
            Writer::spawn(&target.part, WriterConfig::new(1, total)).expect("spawn writer");
        let budget = AsyncBudget::for_connections(1);
        let pool = LanePool::new(&HttpConfig::default(), 1).unwrap();
        let mut ledger = WorkLedger::new(total);

        let range = ledger.next_claim(1, None).expect("a first claim");
        let claim = Arc::new(LiveClaim::new(ClaimId(1), range, Protocol::Http1, 0));
        let request = SegmentRequest { url: &url, generation: 1, ..Default::default() };
        fetch_claim(
            pool.next_lane(),
            &request,
            &claim,
            &writer.handle(),
            &budget,
            &FetchConfig::default(),
        )
        .await
        .expect("first fetch");

        ledger.release(range);
        checkpoint(&acks, &writer, &mut journal, &mut ledger, &data);
        writer.shutdown().expect("shutdown");
        journal.checkpoint_now(&data).expect("checkpoint");
        journal.durable().total_bytes()
    };
    assert!(partial > 0 && partial < total, "first pass should be partial, got {partial}");

    // Second pass: reopen the journal and finish only what is missing.
    let data = target.open_part();
    let mut journal = Journal::open(&target.journal).expect("reopen journal");
    journal.check_part_file(&data).expect("identity and size should still match");
    let report = journal.verify(&data, VerifyMode::Full).expect("verify");
    assert_eq!(report.blocks_failed, 0, "the first pass's bytes should verify");

    let resume_from = journal.remaining();
    assert!(!resume_from.is_empty());
    assert_eq!(resume_from[0].start(), partial, "should resume at the first hole");

    let (writer, acks) =
        Writer::spawn(&target.part, WriterConfig::new(1, total)).expect("spawn writer");
    let budget = AsyncBudget::for_connections(4);
    let pool = LanePool::new(&HttpConfig::default(), 2).unwrap();
    let mut ledger = WorkLedger::resumed(total, journal.durable().clone());

    for round in 0..20 {
        let Some(range) = ledger.next_claim(4, None) else { break };
        let claim = Arc::new(LiveClaim::new(ClaimId(100 + round), range, Protocol::Http1, 0));
        let request = SegmentRequest { url: &url, generation: 1, ..Default::default() };
        fetch_claim(
            pool.next_lane(),
            &request,
            &claim,
            &writer.handle(),
            &budget,
            &FetchConfig::default(),
        )
        .await
        .expect("resume fetch");
        ledger.release(range);
        checkpoint(&acks, &writer, &mut journal, &mut ledger, &data);
        if ledger.is_complete() {
            break;
        }
    }

    checkpoint(&acks, &writer, &mut journal, &mut ledger, &data);
    writer.shutdown().expect("shutdown");
    journal.checkpoint_now(&data).expect("checkpoint");

    assert!(journal.is_complete(), "resume should have finished the file");
    assert_content_matches(&target.part, total);
}
