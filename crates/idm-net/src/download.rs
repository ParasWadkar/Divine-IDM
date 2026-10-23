//! The coordinator: one whole download, start to finish.
//!
//! Everything else in the engine is a mechanism; this is the part that drives
//! them. It resolves the URL, probes it, preallocates, opens a journal, spawns
//! the writer, keeps a target number of fetch tasks in flight, decides when to
//! steal and when to hedge, checkpoints on a timer, and finalises.
//!
//! # Ownership
//!
//! The coordinator owns the ledger, the connection controller, the journal and
//! the writer; nothing else may touch them. Fetch tasks get only a cloned
//! writer handle, a cloned budget, a lane and an `Arc<LiveClaim>`. That is what
//! keeps the scheduling state single-threaded and lock-free while the network
//! work is fully concurrent.
//!
//! # The checkpoint order
//!
//! Flush, drain, flush, commit. Draining before the first flush would miss the
//! acks that flush itself produces — a short run sits un-issued in the writer's
//! reorder buffer, so no ack exists yet, and a final checkpoint would commit
//! nothing and leave the journal unaware of the last chunk. The second flush
//! costs almost nothing and makes every drained range durable even if other
//! fetchers wrote concurrently.

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use idm_core::journal::{
    Journal, JournalMeta, JournalStrings, ValidatorKind, VerifyMode, flags as journal_flags,
};
use idm_core::sched::conn::{ConnController, ConnDecision, ConnPolicy, Observation, SettleReason};
use idm_core::sched::probe::{ProbeOutcome, classify};
use idm_core::sched::steal::{Action, StealPolicy, next_action};
use idm_core::sched::{
    ClaimId, ClaimSnapshot, HostKey, LiveClaim, ProfileStore, Protocol, WorkLedger, ledger,
};
use idm_core::win::prealloc::{self, PreallocOptions, WriteMode};
use idm_core::writer::{WriteAck, Writer, WriterConfig};
use idm_core::{ByteRange, ExtentError, FileIdentity};
use reqwest::header::{CONTENT_DISPOSITION, RANGE};

use crate::budget::AsyncBudget;
use crate::client::{HttpConfig, LanePool, lane_topology};
use crate::facts::OwnedFacts;
use crate::fetch::{FetchConfig, FetchOutcome, FetchStop, SegmentRequest, fetch_claim};

/// The probe reads this much, and it doubles as the first segment, so no round
/// trip is wasted learning the file's shape.
const PROBE_BYTES: u64 = 1 << 20;

#[derive(Clone)]
pub struct DownloadConfig {
    pub url: String,
    pub output_dir: PathBuf,
    /// Overrides the server-supplied and URL-derived name.
    pub filename: Option<String>,
    /// Pins the connection count, disabling the adaptive ramp. Useful for
    /// benchmarking; left `None` in normal use, because no fixed number can be
    /// right for every host.
    pub fixed_connections: Option<u32>,
    pub http: HttpConfig,
    pub fetch: FetchConfig,
    pub conn: ConnPolicy,
    pub steal: StealPolicy,
    /// Opt in to `SetFileValidData`. Off by default: it skips zero-filling by
    /// exposing stale on-disk content.
    pub allow_set_valid_data: bool,
    pub checkpoint_interval: Duration,
    /// How often the connection controller is given a measurement window.
    ///
    /// Deliberately separate from the checkpoint cadence. Tying the two
    /// together means the ramp can only act as often as the journal is
    /// flushed, so a 250 ms fast-ramp interval behind a 500 ms checkpoint
    /// never fires and the download spends its first seconds at the starting
    /// connection count.
    pub conn_tick_interval: Duration,
    /// Pin lanes to distinct resolved edge addresses.
    pub pin_edges: bool,
    pub referrer: Option<String>,
    pub cookies: Option<String>,
    /// Where to read and record what this machine has learned about a host.
    ///
    /// Optional because the engine must run without one — a download does
    /// not depend on it, it only starts better informed.
    pub profiles: Option<Arc<dyn ProfileStore>>,
}

impl std::fmt::Debug for DownloadConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hand-written rather than derived: a trait object has no Debug, and
        // the cookies field must never reach a log line.
        f.debug_struct("DownloadConfig")
            .field("url", &self.url)
            .field("output_dir", &self.output_dir)
            .field("filename", &self.filename)
            .field("fixed_connections", &self.fixed_connections)
            .field("conn", &self.conn)
            .field("steal", &self.steal)
            .field("allow_set_valid_data", &self.allow_set_valid_data)
            .field("checkpoint_interval", &self.checkpoint_interval)
            .field("conn_tick_interval", &self.conn_tick_interval)
            .field("pin_edges", &self.pin_edges)
            .field("referrer", &self.referrer)
            .field("cookies", &self.cookies.as_ref().map(|_| "<redacted>"))
            .field("profiles", &self.profiles.as_ref().map(|_| "<store>"))
            .finish_non_exhaustive()
    }
}

impl DownloadConfig {
    pub fn new(url: impl Into<String>, output_dir: impl Into<PathBuf>) -> Self {
        Self {
            url: url.into(),
            output_dir: output_dir.into(),
            filename: None,
            fixed_connections: None,
            http: HttpConfig::default(),
            fetch: FetchConfig::default(),
            conn: ConnPolicy::default(),
            steal: StealPolicy::default(),
            allow_set_valid_data: false,
            checkpoint_interval: Duration::from_millis(500),
            conn_tick_interval: Duration::from_millis(150),
            pin_edges: true,
            referrer: None,
            cookies: None,
            profiles: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct DownloadStats {
    pub requests: u64,
    pub steals: u64,
    pub hedges: u64,
    pub refusals: u64,
    pub retries: u64,
    pub bytes_rewritten: u64,
}

/// A periodic snapshot for the UI or CLI.
#[derive(Debug, Clone, Copy)]
pub struct Progress {
    pub durable: u64,
    pub total: u64,
    pub rate_bps: f64,
    pub connections: u32,
    pub elapsed: Duration,
    /// Set once the ramp has stopped, so a caller can say *why* -- "disk-limited"
    /// is a very different message than "the server is throttling you".
    pub settled: Option<SettleReason>,
}

impl Progress {
    pub fn fraction(&self) -> f64 {
        if self.total == 0 { 0.0 } else { self.durable as f64 / self.total as f64 }
    }
}

#[derive(Debug, Clone)]
pub struct DownloadReport {
    pub path: PathBuf,
    pub http_version: String,
    pub total: u64,
    pub elapsed: Duration,
    pub connections_settled_at: u32,
    pub settle_reason: Option<SettleReason>,
    pub write_mode: WriteMode,
    pub lanes: usize,
    pub stats: DownloadStats,
    pub resumed_bytes: u64,
}

impl DownloadReport {
    pub fn mean_bytes_per_sec(&self) -> f64 {
        let secs = self.elapsed.as_secs_f64();
        if secs <= 0.0 { 0.0 } else { (self.total - self.resumed_bytes) as f64 / secs }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("client setup failed: {0}")]
    Client(#[from] crate::client::ClientError),
    #[error("request failed: {0}")]
    Request(#[source] reqwest::Error),
    #[error("fetch failed: {0}")]
    Fetch(#[from] crate::fetch::FetchError),
    #[error("I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("preallocation failed: {0}")]
    Prealloc(#[from] prealloc::PreallocError),
    #[error("journal failed: {0}")]
    Journal(#[from] idm_core::JournalError),
    #[error("range arithmetic failed: {0}")]
    Extent(#[from] ExtentError),
    #[error("server does not support ranges, so this file cannot be fetched in parallel")]
    NoRangeSupport,
    #[error("the file has no known length, so it cannot be fetched in parallel")]
    UnknownLength,
    #[error("the server's response could not be used: {0:?}")]
    Unusable(ProbeOutcome),
    #[error("the file changed on the server mid-download; the partial file was kept")]
    RepresentationChanged,
    #[error("gave up after {attempts} attempts without finishing")]
    Stalled { attempts: u32 },
}

impl DownloadError {
    /// Whether waiting and trying again could plausibly succeed.
    ///
    /// The question this answers is "is the download dead, or is the network
    /// away?" A dropped Wi-Fi, a laptop lid, a tunnel, a server dropping a
    /// connection mid-body: those come back, and the partial file and journal
    /// are still valid, so the right response is to wait rather than to fail
    /// and make the user start again.
    ///
    /// The opposite answers matter just as much. A 404, a server that will
    /// not do ranges, a file that changed underneath us: retrying those
    /// forever would be a download that never finishes and never admits it.
    pub fn is_transient(&self) -> bool {
        match self {
            // Connecting, timing out, or a body that stopped arriving: the
            // connection went away. `is_request` covers a send that never
            // got off the ground.
            Self::Request(error) => error.is_connect() || error.is_timeout() || error.is_request(),
            Self::Fetch(crate::fetch::FetchError::Request(error)) => {
                error.is_connect() || error.is_timeout() || error.is_request()
            }
            // A body that failed partway is the commonest shape of "the
            // network went out mid-download".
            Self::Fetch(crate::fetch::FetchError::Body { .. }) => true,
            Self::Fetch(crate::fetch::FetchError::Write(error)) => is_transient_io(error),
            Self::Io(error) => is_transient_io(error),
            // Every worker retired without finishing. On a flaky link this is
            // what a stall looks like from the inside.
            Self::Stalled { .. } => true,

            // Nothing about waiting changes these.
            Self::Client(_)
            | Self::Fetch(crate::fetch::FetchError::Range(_))
            | Self::Prealloc(_)
            | Self::Journal(_)
            | Self::Extent(_)
            | Self::NoRangeSupport
            | Self::UnknownLength
            | Self::Unusable(_)
            | Self::RepresentationChanged => false,
        }
    }
}

/// I/O kinds that mean the network rather than the disk.
///
/// A disk that is full or a path that cannot be written is not going to fix
/// itself, and retrying would hide a real problem behind an endless wait.
fn is_transient_io(error: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    matches!(
        error.kind(),
        ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::ConnectionRefused
            | ErrorKind::NotConnected
            | ErrorKind::BrokenPipe
            | ErrorKind::TimedOut
            | ErrorKind::Interrupted
            | ErrorKind::UnexpectedEof
            | ErrorKind::HostUnreachable
            | ErrorKind::NetworkUnreachable
            | ErrorKind::NetworkDown
    )
}

/// What the probe learned about the target.
#[derive(Debug, Clone)]
pub struct Target {
    pub pinned_url: String,
    pub http_version: String,
    pub total: u64,
    pub validator: ValidatorKind,
    pub etag: Option<String>,
    pub filename: String,
    pub facts: OwnedFacts,
    /// Round trip to this host, estimated from the probe.
    ///
    /// The connection controller refuses to probe faster than four round
    /// trips, because a window shorter than that measures noise rather than
    /// throughput. That guard was being fed a hardcoded 20 ms, which made it
    /// inert: every host, satellite link or loopback, got the same floor.
    /// This is the only round-trip measurement taken anywhere on the path,
    /// so it is the one the guard gets.
    pub probe_rtt: Duration,
}

/// Resolves and probes the URL with a single ranged GET.
///
/// Deliberately not a HEAD: HEAD is answered from a different cache tier,
/// returns 405 on some origins, is rejected by some signed URLs, and can report
/// a `Content-Length` that disagrees with the GET that follows.
pub async fn probe(config: &DownloadConfig) -> Result<Target, DownloadError> {
    let resolver = crate::client::redirect_resolver(&config.http)?;
    let requested = ByteRange::try_new(0, PROBE_BYTES)?;

    let mut request = resolver.get(&config.url).header(RANGE, requested.to_range_header()?);
    if let Some(referrer) = &config.referrer {
        request = request.header(reqwest::header::REFERER, referrer);
    }
    if let Some(cookies) = &config.cookies {
        request = request.header(reqwest::header::COOKIE, cookies);
    }

    // Headers back means DNS, the TCP handshake and one request/response
    // exchange have all completed: roughly two round trips for plain HTTP,
    // and the handshake is what makes it two rather than one. Halving it is
    // crude, but an order of magnitude is all the guard needs, and it is
    // the difference between protecting a 600 ms link and not.
    let sent_at = Instant::now();
    let response = request.send().await.map_err(DownloadError::Request)?;
    let probe_rtt = sent_at.elapsed() / 2;
    let facts = OwnedFacts::from_response(&response);
    let outcome = classify(requested, &facts.as_facts());

    let filename = config
        .filename
        .clone()
        .or_else(|| filename_from_disposition(&response))
        .or_else(|| filename_from_url(&facts.final_url))
        .unwrap_or_else(|| "download.bin".to_owned());

    match outcome {
        ProbeOutcome::Ranged { total, validator, .. } => Ok(Target {
            pinned_url: facts.final_url.clone(),
            http_version: facts.http_version.clone(),
            total,
            validator,
            etag: facts.etag.clone(),
            filename,
            facts,
            probe_rtt,
        }),
        ProbeOutcome::SingleStreamFromZero { .. } => Err(DownloadError::NoRangeSupport),
        ProbeOutcome::SingleStreamUnknownLength | ProbeOutcome::NeedsSuffixProbe { .. } => {
            Err(DownloadError::UnknownLength)
        }
        other => Err(DownloadError::Unusable(other)),
    }
}

fn filename_from_disposition(response: &reqwest::Response) -> Option<String> {
    let value = response.headers().get(CONTENT_DISPOSITION)?.to_str().ok()?;
    // `filename*` may carry a path separator or traversal, so only the final
    // component is ever taken and it is sanitised by the caller.
    let raw = value
        .split(';')
        .filter_map(|part| part.trim().strip_prefix("filename="))
        .map(|name| name.trim_matches('"'))
        .next()?;
    sanitise_filename(raw)
}

fn filename_from_url(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next()?;
    sanitise_filename(path.rsplit('/').next()?)
}

/// Strips anything that could escape the output directory or confuse Windows.
///
/// Server-supplied names are untrusted input: traversal, absolute paths,
/// reserved device names and alternate-data-stream colons all have to go.
pub fn sanitise_filename(raw: &str) -> Option<String> {
    let base = raw.rsplit(['/', '\\']).next()?.trim();
    if base.is_empty() || base == "." || base == ".." {
        return None;
    }
    let cleaned: String = base
        .chars()
        .filter(|c| !matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0'))
        .collect();
    // Trailing dots and spaces are silently dropped by Windows, which turns
    // "evil.exe " into "evil.exe".
    let cleaned = cleaned.trim_end_matches(['.', ' ']).to_owned();
    if cleaned.is_empty() {
        return None;
    }
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let stem = cleaned.split('.').next().unwrap_or_default().to_ascii_uppercase();
    if RESERVED.contains(&stem.as_str()) {
        return Some(format!("_{cleaned}"));
    }
    Some(cleaned)
}

struct Tracked {
    claim: Arc<LiveClaim>,
    issued: Instant,
    /// The whole range this claim was reserved for, so the unfetched residue
    /// can be returned to the pool when it retires.
    reserved: ByteRange,
}

impl Tracked {
    fn rate_bps(&self) -> f64 {
        let moved = self.claim.cursor().saturating_sub(self.claim.start());
        let secs = self.issued.elapsed().as_secs_f64();
        if secs <= 0.001 { 0.0 } else { moved as f64 / secs }
    }

    fn snapshot(&self, now_ms: u64) -> ClaimSnapshot {
        self.claim.snapshot(self.rate_bps(), now_ms)
    }
}

/// Runs one download to completion, resuming from an existing journal if there
/// is one.
pub async fn download(config: &DownloadConfig) -> Result<DownloadReport, DownloadError> {
    download_with_progress(config, |_| {}).await
}

/// As [`download`], reporting progress at every checkpoint.
pub async fn download_with_progress<F: FnMut(Progress)>(
    config: &DownloadConfig,
    mut on_progress: F,
) -> Result<DownloadReport, DownloadError> {
    let started = Instant::now();
    let target = probe(config).await?;

    std::fs::create_dir_all(&config.output_dir)?;
    let final_path = config.output_dir.join(&target.filename);
    let part_path = with_extension(&final_path, "idmpart");
    let journal_path = with_extension(&final_path, "idmjrn");

    let data =
        File::options().create(true).truncate(false).read(true).write(true).open(&part_path)?;

    // Resume when a journal describes this very file; otherwise start fresh.
    let (mut journal, write_mode) = match open_resumable(&journal_path, &data, &target) {
        Some(journal) => {
            let mode = mode_from_flags(journal.flags());
            (journal, mode)
        }
        None => {
            let prepared = prealloc::preallocate(
                &data,
                &part_path,
                &PreallocOptions {
                    total_len: target.total,
                    allow_set_valid_data: config.allow_set_valid_data,
                },
            )?;
            let meta = JournalMeta {
                total_len: target.total,
                generation: 1,
                flags: prepared.mode.journal_flags(),
                validator: target.validator,
                part_identity: FileIdentity::of(&data)?,
                strings: JournalStrings {
                    request_url: config.url.clone(),
                    pinned_url: target.pinned_url.clone(),
                    final_name: target.filename.clone(),
                    etag: target.etag.clone().unwrap_or_default(),
                    last_modified: target.facts.last_modified.clone().unwrap_or_default(),
                    content_type: String::new(),
                },
            };
            (Journal::create(&journal_path, &meta)?, prepared.mode)
        }
    };

    let resumed_bytes = journal.durable().total_bytes();
    let mut ledger = WorkLedger::resumed(target.total, journal.durable().clone());

    let bdp = 2 << 20;
    let forced_single = ledger::force_single_connection(target.total, bdp);

    // Identity of what is being downloaded from, for the learned profile.
    // Computed before the connection count is chosen, because a profile that
    // arrives after the lane topology is fixed cannot change it.
    let host = reqwest::Url::parse(&target.pinned_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned));
    let port = reqwest::Url::parse(&target.pinned_url)
        .ok()
        .and_then(|url| url.port_or_known_default())
        .unwrap_or(443);
    let host_key = host
        .as_ref()
        .map(|host| HostKey::new(host.as_str(), port, alpn_label(&target.http_version)));

    let fingerprint = idm_core::win::net_fingerprint();

    // Whether the speed limit held anything back during this download, read
    // as a before-and-after count. If it did, the throughput the controller
    // measured was the limit's, not the host's, and the count it settled on
    // says nothing about what the host can do. Saving it would teach the
    // next unthrottled download from this host to start throttled.
    let waits_before = config.fetch.limiter.as_ref().map_or(0, |limiter| limiter.waits());
    let wall_clock_ms = wall_clock_ms();
    let learned = match (config.profiles.as_ref(), host_key.as_ref()) {
        (Some(store), Some(key)) => store.load(key, fingerprint),
        _ => None,
    };
    // A pinned count and a one-connection file both teach nothing and should
    // learn nothing: the first was not measured, the second was not tried.
    let tuning_applies = config.fixed_connections.is_none() && !forced_single;
    let learned =
        learned.filter(|profile| tuning_applies && profile.is_usable(wall_clock_ms, fingerprint));

    let initial = match config.fixed_connections {
        Some(fixed) => fixed.max(1),
        None if forced_single => 1,
        None => {
            learned.as_ref().map(|profile| profile.best_conn_count).unwrap_or(config.conn.start)
        }
    };
    if let Some(profile) = &learned {
        tracing::debug!(
            host = host.as_deref().unwrap_or("?"),
            connections = profile.best_conn_count,
            "starting from a learned profile instead of ramping"
        );
    }
    // Lanes are sized for where the controller may get to, not where it
    // starts, because the pool is built once and cannot grow afterwards.
    //
    // Sizing from `initial` meant a cold start got 2 lanes and kept them
    // after the controller had ramped to 16 connections -- so the sixteen
    // shared two lanes' worth of independent congestion control for the
    // rest of the transfer, which is exactly the "N streams on one h2
    // connection is not N connections" trap.
    //
    // Honest about the evidence: this is reasoning, not a measured win.
    // The local benchmark server is HTTP/1.1, where reqwest opens extra
    // sockets per client on demand, so lane count barely shows there and
    // fixing it changed nothing measurable. It should matter against a
    // real h2 CDN, where a lane really is one connection. It is kept
    // because the pool cannot grow later, not because a number moved.
    //
    // A lane is a reqwest Client, and one that is never used opens no
    // sockets, so sizing for the ceiling costs nothing when the controller
    // stays low. A pinned count is left alone: it was asked for exactly.
    let lane_basis = if config.fixed_connections.is_some() || forced_single {
        initial
    } else {
        initial.max(config.conn.per_host_max)
    };
    let (lane_count, streams) = lane_topology(lane_basis);
    let mut http = config.http.clone();
    http.streams_per_lane = streams;

    let pool = Arc::new(match (&host, config.pin_edges) {
        (Some(host), true) => LanePool::pinned_to_edges(&http, host, port, lane_count)
            .await
            .unwrap_or(LanePool::new(&http, lane_count)?),
        _ => LanePool::new(&http, lane_count)?,
    });

    let (writer, acks) = Writer::spawn(&part_path, WriterConfig::new(1, target.total))?;
    let handle = writer.handle();
    let budget = AsyncBudget::for_connections(config.conn.per_host_max);

    let mut controller = match &learned {
        Some(profile) => ConnController::from_profile(
            config.conn.clone(),
            profile,
            0,
            wall_clock_ms,
            fingerprint,
        ),
        None => ConnController::new(config.conn.clone(), 0),
    };
    let mut target_connections = initial;
    let mut stats = DownloadStats::default();
    let mut tracked: HashMap<ClaimId, Tracked> = HashMap::new();
    let mut tasks = tokio::task::JoinSet::new();
    let mut next_claim_id = 0u32;
    let mut hedges_used = 0u8;
    let mut steal_tokens = 8u32;
    let mut last_checkpoint = Instant::now();
    let mut last_conn_tick = Instant::now();
    let mut fetched_at_last_tick = 0u64;
    // Bytes fetched by claims that have already retired. Added to the live
    // claims' progress to give a smooth, network-level throughput signal.
    let mut retired_bytes = 0u64;
    let mut idle_rounds = 0u32;

    while !ledger.is_complete() {
        // Top up to the target number of in-flight fetches.
        while tracked.len() < target_connections as usize {
            let now_ms = started.elapsed().as_millis() as u64;
            let snapshots: Vec<ClaimSnapshot> =
                tracked.values().map(|t| t.snapshot(now_ms)).collect();
            // Claims are sized for the count the controller may reach, not
            // the one in use, because a connection it adds can only start
            // if unclaimed bytes are left for it to take.
            //
            // This is what made the adaptive ramp cosmetic. claim_size
            // floors at 8 MiB, shrinking that floor only by the *current*
            // count, so a 32 MiB file at the opening 4 connections handed
            // out four 8 MiB claims -- the whole file. The controller then
            // raised its target to 16 on schedule and nothing happened:
            // every byte was already spoken for, and stealing is gated on
            // saving two seconds, which a download this short can never
            // offer. Measured at exactly the -n 4 time while reporting "16
            // connections", because the target moved and the workers did
            // not. Sizing by the ceiling leaves gaps for them to claim.
            let claim_basis = if config.fixed_connections.is_some() || forced_single {
                target_connections
            } else {
                target_connections.max(config.conn.per_host_max)
            };
            let free = ledger.next_claim(claim_basis, write_mode.claim_window());

            let median_rate = median_rate(&snapshots);
            let action = next_action(
                free,
                &snapshots,
                median_rate,
                ledger.remaining_bytes(),
                target.total,
                hedges_used,
                steal_tokens,
                &config.steal,
            );

            let (range, kind) = match action {
                Action::TakeGap(range) => (range, "gap"),
                Action::Split { donor, at, .. } => {
                    // Free the gap we optimistically took, if any.
                    let Some(stolen) =
                        tracked.get(&donor).and_then(|t| t.claim.try_steal_at(at, now_ms))
                    else {
                        break;
                    };
                    stats.steals += 1;
                    steal_tokens = steal_tokens.saturating_sub(1);
                    ledger.reserve(stolen);
                    (stolen, "steal")
                }
                Action::Hedge { range, .. } => {
                    hedges_used += 1;
                    stats.hedges += 1;
                    // Deliberately not reserved: a hedge duplicates work that
                    // is already claimed, and the writer tolerates the
                    // overlap because both copies are byte-identical.
                    (range, "hedge")
                }
                Action::Idle(_) => break,
            };

            if range.is_empty() {
                break;
            }
            next_claim_id += 1;
            let id = ClaimId(next_claim_id);
            let claim = Arc::new(LiveClaim::new(id, range, Protocol::Http1, now_ms));
            tracked.insert(
                id,
                Tracked { claim: Arc::clone(&claim), issued: Instant::now(), reserved: range },
            );

            let handle = handle.clone();
            let budget = budget.clone();
            let pool = Arc::clone(&pool);
            let url = target.pinned_url.clone();
            let etag =
                target.etag.clone().filter(|_| target.validator == ValidatorKind::StrongETag);
            let referrer = config.referrer.clone();
            let cookies = config.cookies.clone();
            let fetch_config = config.fetch.clone();
            stats.requests += 1;
            tracing::debug!(%id, ?range, kind, "issuing claim");

            tasks.spawn(async move {
                let lane = pool.next_lane().clone();
                let request = SegmentRequest {
                    url: &url,
                    generation: 1,
                    if_match: etag.as_deref(),
                    referrer: referrer.as_deref(),
                    cookies: cookies.as_deref(),
                };
                let outcome =
                    fetch_claim(&lane, &request, &claim, &handle, &budget, &fetch_config).await;
                (id, outcome)
            });
        }

        if tracked.is_empty() {
            // Nothing in flight and nothing to hand out: either done, or the
            // ledger and the journal disagree.
            checkpoint(&acks, &writer, &mut journal, &mut ledger, &data)?;
            if ledger.is_complete() {
                break;
            }
            idle_rounds += 1;
            if idle_rounds > 100 {
                return Err(DownloadError::Stalled { attempts: idle_rounds });
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            continue;
        }
        idle_rounds = 0;

        tokio::select! {
            Some(joined) = tasks.join_next() => {
                let (id, outcome) = joined.expect("fetch task panicked");
                let finished = tracked.remove(&id);
                if let Some(entry) = &finished {
                    retired_bytes += entry.claim.cursor().saturating_sub(entry.claim.start());
                }
                match outcome {
                    Ok(FetchOutcome::Fetched(fetched)) => {
                        if let Some(entry) = &finished {
                            // Return whatever this claim did not deliver.
                            let delivered_to = fetched.written.end().max(entry.reserved.start());
                            if delivered_to < entry.reserved.end() {
                                ledger.release(ByteRange::try_new(delivered_to, entry.reserved.end())?);
                            }
                        }
                        if fetched.stop == FetchStop::StolenFrom {
                            steal_tokens = steal_tokens.saturating_add(1).min(8);
                        }
                    }
                    Ok(FetchOutcome::Refused { outcome, .. }) => {
                        stats.refusals += 1;
                        if let Some(entry) = &finished {
                            ledger.release(entry.reserved);
                        }
                        match outcome {
                            ProbeOutcome::Throttled => {
                                stats.retries += 1;
                                let observation = throttled_observation(target_connections);
                                if let ConnDecision::Reduce { to, .. } =
                                    controller.observe(started.elapsed().as_millis() as u64, &observation, 0)
                                {
                                    target_connections = to.max(1);
                                }
                                tokio::time::sleep(Duration::from_millis(500)).await;
                            }
                            ProbeOutcome::Restart(_) => return Err(DownloadError::RepresentationChanged),
                            other => return Err(DownloadError::Unusable(other)),
                        }
                    }
                    Err(error) => {
                        // A dropped connection is ordinary; keep the bytes that
                        // arrived and requeue the rest.
                        stats.retries += 1;
                        tracing::debug!(%error, "segment failed, requeueing");
                        if let Some(entry) = &finished {
                            let reached = entry.claim.cursor().max(entry.reserved.start());
                            if reached < entry.reserved.end() {
                                ledger.release(ByteRange::try_new(reached, entry.reserved.end())?);
                            }
                        }
                    }
                }
            }
            _ = tokio::time::sleep(config.conn_tick_interval.min(config.checkpoint_interval)) => {}
        }

        // The ramp runs on its own clock, measured from the writer's live
        // issued-byte counter rather than the journal, which only advances at
        // checkpoints and is far too coarse to steer by.
        // `tuning_applies`, not just "no pinned count": a file small enough
        // to be forced down a single connection must not then be ramped back
        // up by the controller.
        //
        // It was. ConnController::new starts at policy.start regardless of
        // what the caller worked out, so a 2 MiB file opened one connection,
        // and 150 ms later the controller raised the target to the cap and
        // the file was fetched sixteen ways in 128 KiB pieces -- measurably
        // slower than the one connection it had decided on, which is the
        // whole reason force_single_connection exists. The variable saying
        // this download is not a tuning candidate was already here and only
        // the profile lookup consulted it.
        if tuning_applies && last_conn_tick.elapsed() >= config.conn_tick_interval {
            let now_ms = started.elapsed().as_millis() as u64;
            // Claim cursors advance as the network delivers, every 1 MiB or
            // 50 ms. The writer's issued-byte counter instead jumps in 4 MiB
            // coalesced steps with flat stretches between, and those flat
            // stretches read as zero throughput -- which the controller scored
            // as weak gains and used to abandon the ramp.
            let fetched: u64 = retired_bytes
                + tracked
                    .values()
                    .map(|t| t.claim.cursor().saturating_sub(t.claim.start()))
                    .sum::<u64>();
            let moved = fetched.saturating_sub(fetched_at_last_tick) as f64;
            let secs = last_conn_tick.elapsed().as_secs_f64().max(0.001);
            let observation = Observation {
                aggregate_bps: moved / secs,
                mean_per_conn_bps: moved / secs / f64::from(target_connections.max(1)),
                per_conn_spread_frac: spread_frac(&tracked, now_ms),
                throttled: false,
                writer_backpressure: budget.available_bytes() == 0,
                interarrival_p95_ratio: 1.0,
                rtt: target.probe_rtt,
            };
            let decision = controller.observe(now_ms, &observation, 0);
            match decision {
                ConnDecision::Add { to } => target_connections = to,
                ConnDecision::Reduce { to, .. } => target_connections = to.max(1),
                ConnDecision::Settle { at, .. } => target_connections = at.max(1),
                ConnDecision::Hold => {}
            }
            // Opt-in trace: speculating about why a ramp is slow is a poor
            // substitute for watching it.
            if std::env::var_os("IDM_TRACE_RAMP").is_some() {
                eprintln!(
                    "ramp t={:>5}ms target={:<3} inflight={:<3} issued={:>9} agg={:>7.1}MB/s settled={:?}",
                    now_ms,
                    target_connections,
                    tracked.len(),
                    fetched,
                    observation.aggregate_bps / 1e6,
                    controller.settled()
                );
                let _ = &decision;
            }
            fetched_at_last_tick = fetched;
            last_conn_tick = Instant::now();
        }

        if last_checkpoint.elapsed() >= config.checkpoint_interval {
            checkpoint(&acks, &writer, &mut journal, &mut ledger, &data)?;
            last_checkpoint = Instant::now();

            let durable = journal.durable().total_bytes();
            let moved = durable.saturating_sub(resumed_bytes) as f64;
            on_progress(Progress {
                durable,
                total: target.total,
                rate_bps: moved / started.elapsed().as_secs_f64().max(0.001),
                connections: target_connections,
                elapsed: started.elapsed(),
                settled: controller.settled(),
            });
        }
    }

    // Drain anything still running before finalising.
    while tasks.join_next().await.is_some() {}
    checkpoint(&acks, &writer, &mut journal, &mut ledger, &data)?;
    writer.shutdown()?;
    journal.checkpoint_now(&data)?;

    // Verify before declaring victory. `SetFileValidData` forces a full pass,
    // because unwritten regions there hold plausible stale content.
    let verify_mode =
        if write_mode.requires_full_verification() { VerifyMode::Full } else { VerifyMode::Fast };
    let report = journal.verify(&data, verify_mode)?;
    if report.blocks_failed > 0 {
        tracing::warn!(
            failed = report.blocks_failed,
            "verification trimmed bad blocks; the file is incomplete"
        );
    }
    if !journal.is_complete() {
        return Err(DownloadError::Stalled { attempts: stats.requests as u32 });
    }

    // Finalise: no holes remain, so sparseness can be dropped safely.
    if write_mode == WriteMode::Sparse {
        let _ = prealloc::clear_sparse(&data);
    }
    data.sync_all()?;
    drop(data);

    prealloc::finalize(
        &part_path,
        &final_path,
        &target.pinned_url,
        config.referrer.as_deref().unwrap_or_default(),
    )?;
    let _ = std::fs::remove_file(&journal_path);

    // Record what this run learned, now that it has finished successfully.
    //
    // Only when the controller settled: an unsettled count is wherever the
    // ramp had got to when the bytes ran out, and storing that would teach a
    // number nothing actually measured.
    if let (Some(store), Some(key), Some(reason)) =
        (config.profiles.as_ref(), host_key.as_ref(), controller.settled())
        && tuning_applies
        && fingerprint != 0
        && !config.fetch.limiter.as_ref().is_some_and(|limiter| limiter.waits() > waits_before)
    {
        // Built by the controller rather than assembled here. The two
        // differ: this loop clamps the connection count in places the
        // controller does not, so a profile written from the loop's copy
        // can disagree with what the controller actually settled on.
        let multiplexing_helped =
            learned.as_ref().map(|profile| profile.multiplexing_helped).unwrap_or(false);
        store.save(key, &controller.to_profile(wall_clock_ms, fingerprint, multiplexing_helped));
        tracing::debug!(
            host = host.as_deref().unwrap_or("?"),
            connections = target_connections,
            ?reason,
            "recorded what this host taught"
        );
    }

    Ok(DownloadReport {
        path: final_path,
        http_version: target.http_version.clone(),
        total: target.total,
        elapsed: started.elapsed(),
        connections_settled_at: target_connections,
        settle_reason: controller.settled(),
        write_mode,
        lanes: pool.len(),
        stats,
        resumed_bytes,
    })
}

/// Wall-clock milliseconds, for judging how old a stored profile is.
fn wall_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or(0)
}

/// Maps hyper's `Version` debug form onto the ALPN name used as a profile key.
///
/// The right connection count differs sharply between these, so an unknown
/// version gets its own bucket rather than being folded in with HTTP/1.1 and
/// inheriting a number learned for a protocol it is not.
fn alpn_label(http_version: &str) -> &'static str {
    match http_version {
        "HTTP/0.9" | "HTTP/1.0" | "HTTP/1.1" => "http/1.1",
        "HTTP/2.0" => "h2",
        "HTTP/3.0" => "h3",
        _ => "unknown",
    }
}

fn median_rate(snapshots: &[ClaimSnapshot]) -> f64 {
    // A brand-new worker is credited with the cohort's median rather than zero;
    // at zero the gain test could never pass on a first steal.
    let mut rates: Vec<f64> = snapshots.iter().map(|s| s.rate_bps).filter(|r| *r > 0.0).collect();
    if rates.is_empty() {
        return 1_000_000.0;
    }
    rates.sort_by(f64::total_cmp);
    rates[rates.len() / 2]
}

fn spread_frac(tracked: &HashMap<ClaimId, Tracked>, now_ms: u64) -> f64 {
    let rates: Vec<f64> =
        tracked.values().map(|t| t.snapshot(now_ms).rate_bps).filter(|r| *r > 0.0).collect();
    if rates.len() < 2 {
        return 1.0;
    }
    let mean = rates.iter().sum::<f64>() / rates.len() as f64;
    if mean <= 0.0 {
        return 1.0;
    }
    let variance = rates.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / rates.len() as f64;
    variance.sqrt() / mean
}

fn throttled_observation(connections: u32) -> Observation {
    Observation { throttled: true, ..Observation::healthy(1.0, connections) }
}

fn mode_from_flags(flags: u32) -> WriteMode {
    if flags & journal_flags::VALID_DATA_SET != 0 {
        WriteMode::ValidDataSet
    } else if flags & journal_flags::ORDERED_MODE != 0 {
        WriteMode::OrderedWindow { window: prealloc::ORDERED_WINDOW }
    } else {
        WriteMode::Sparse
    }
}

/// Opens an existing journal only if it genuinely describes this file.
fn open_resumable(journal_path: &Path, data: &File, target: &Target) -> Option<Journal> {
    let journal = Journal::open(journal_path).ok()?;
    // A path is not an identity, and a size is not content.
    journal.check_part_file(data).ok()?;
    if journal.total_len() != target.total {
        return None;
    }
    // A changed strong validator means the file on the server is not the file
    // on disk; resuming would splice two representations together.
    if let (Some(etag), false) = (&target.etag, journal.strings().etag.is_empty())
        && journal.strings().etag != *etag
    {
        return None;
    }
    Some(journal)
}

fn with_extension(path: &Path, extra: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".");
    name.push(extra);
    PathBuf::from(name)
}

/// Flush, drain, flush, commit. See the module docs for why the order matters.
fn checkpoint(
    acks: &std::sync::mpsc::Receiver<WriteAck>,
    writer: &Writer,
    journal: &mut Journal,
    ledger: &mut WorkLedger,
    data: &File,
) -> Result<(), DownloadError> {
    writer.flush_all()?;

    let mut ranges = Vec::new();
    let mut crcs = Vec::new();
    while let Ok(ack) = acks.try_recv() {
        ledger.mark_acked(ack.range);
        ranges.push(ack.range);
        crcs.extend(ack.block_crcs);
    }

    writer.flush_all()?;
    journal.commit(data, &ranges, &crcs)?;
    for range in ranges {
        ledger.mark_durable(range);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filenames_from_urls_are_sanitised() {
        assert_eq!(filename_from_url("https://x.invalid/a/b/file.zip"), Some("file.zip".into()));
        assert_eq!(
            filename_from_url("https://x.invalid/a/file.zip?token=1#frag"),
            Some("file.zip".into())
        );
        assert_eq!(filename_from_url("https://x.invalid/"), None);
    }

    #[test]
    fn traversal_and_absolute_paths_are_stripped() {
        // Server-supplied names are untrusted: none of these may escape the
        // output directory.
        assert_eq!(sanitise_filename("../../Windows/System32/evil.dll"), Some("evil.dll".into()));
        assert_eq!(sanitise_filename(r"..\..\evil.exe"), Some("evil.exe".into()));
        assert_eq!(sanitise_filename("/etc/passwd"), Some("passwd".into()));
        assert_eq!(sanitise_filename(".."), None);
        assert_eq!(sanitise_filename(""), None);
    }

    #[test]
    fn alternate_data_streams_and_wildcards_are_removed() {
        // "a:b" would write to an alternate data stream rather than a file.
        assert_eq!(sanitise_filename("report:hidden.txt"), Some("reporthidden.txt".into()));
        assert_eq!(sanitise_filename("a*b?c.txt"), Some("abc.txt".into()));
    }

    #[test]
    fn trailing_dots_and_spaces_are_trimmed() {
        // Windows silently drops them, which turns "evil.exe " into "evil.exe".
        assert_eq!(sanitise_filename("evil.exe "), Some("evil.exe".into()));
        assert_eq!(sanitise_filename("evil.exe."), Some("evil.exe".into()));
        assert_eq!(sanitise_filename("..."), None);
    }

    #[test]
    fn reserved_device_names_are_defused() {
        for name in ["CON", "nul", "COM1.txt", "LPT9"] {
            let cleaned = sanitise_filename(name).expect("should produce something");
            assert!(cleaned.starts_with('_'), "{name} became {cleaned}, still a device name");
        }
        // An ordinary name that merely contains one must be left alone.
        assert_eq!(sanitise_filename("console.log"), Some("console.log".into()));
    }

    #[test]
    fn part_and_journal_names_sit_beside_the_output() {
        let final_path = Path::new(r"D:\downloads\file.zip");
        assert_eq!(with_extension(final_path, "idmpart").file_name().unwrap(), "file.zip.idmpart");
        assert_eq!(with_extension(final_path, "idmjrn").file_name().unwrap(), "file.zip.idmjrn");
        // Both on the destination volume, so finalising is a rename rather
        // than a cross-volume copy.
        assert_eq!(with_extension(final_path, "idmpart").parent(), final_path.parent());
    }

    #[test]
    fn write_modes_round_trip_through_journal_flags() {
        assert_eq!(mode_from_flags(journal_flags::SPARSE_USED), WriteMode::Sparse);
        assert_eq!(mode_from_flags(journal_flags::VALID_DATA_SET), WriteMode::ValidDataSet);
        assert!(matches!(
            mode_from_flags(journal_flags::ORDERED_MODE),
            WriteMode::OrderedWindow { .. }
        ));
    }

    #[test]
    fn a_new_worker_is_credited_the_median_not_zero() {
        // At zero the split gain test could never pass on a first steal.
        assert!(median_rate(&[]) > 0.0);
    }
}
