//! Serving mode: the engine behind the local IPC endpoint.
//!
//! This is what the browser extension ultimately talks to, by way of the
//! native messaging host, and what the desktop app will embed. The job
//! registry therefore lives here rather than in argument handling.
//!
//! # The destination is ours, not the caller's
//!
//! A request may suggest a filename; it can never name a directory. The
//! directory comes from this process's own configuration. That is the whole
//! reason the protocol has no path field: a request originating in a web page
//! should not get to choose where bytes land, and "sanitise the path" is a
//! weaker guarantee than having no path to sanitise.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::pairing::{PairingOutcome, PairingStore};
use idm_core::win::power::{ActionOutcome, ArmedAction, Canceller, PostAction, SystemPower};
use idm_ipc::protocol::{
    DownloadRequest, JobId, JobState, JobStatus, PostActionSetting, Request, Response, Settings,
};
use idm_ipc::server::{ClientInfo, Handler};
use idm_ipc::{IpcServer, SessionToken};
use idm_net::download::{DownloadConfig, Progress, download_with_progress, sanitise_filename};
use tokio::sync::Mutex;

struct JobRecord {
    status: JobStatus,
    abort: Option<tokio::task::AbortHandle>,
    /// Set while the job waits for a slot, taken when it starts.
    ///
    /// Held in memory rather than rebuilt from the stored row because it
    /// carries the cookies the browser handed over, which are deliberately
    /// never written to the database and so could not be recovered from it.
    pending: Option<Box<DownloadConfig>>,
}

/// Shared engine state.
///
/// Held behind an `Arc` inside [`Engine`] so that `handle(&self)` can give a
/// clone to a spawned download task. The alternative â€” recovering an
/// `Arc<Self>` from `&self` â€” is only sound while every caller happens to hold
/// one, which is not a property the type can enforce.
struct Inner {
    output_dir: PathBuf,
    max_connections: u32,
    next_id: AtomicU64,
    jobs: Mutex<HashMap<JobId, JobRecord>>,
    settings: Mutex<Settings>,
    settings_file: PathBuf,
    /// Durable state. Optional: the engine runs without it, and everything
    /// it holds is a convenience rather than something a download needs.
    store: Option<Arc<idm_store::Store>>,
    /// Maps a live job to its row, so progress reaches the right one.
    rows: Mutex<HashMap<JobId, i64>>,
    /// One speed limit for every download the engine runs.
    limiter: Arc<idm_net::limiter::RateLimiter>,
    pairings: Mutex<PairingStore>,
    /// Cancels an armed post-download action, if one is counting down.
    armed: Mutex<Option<Canceller>>,
}

/// Runs downloads on behalf of IPC clients.
pub struct Engine {
    inner: Arc<Inner>,
}

impl Engine {
    pub fn new(output_dir: PathBuf, max_connections: u32) -> Arc<Self> {
        let store = crate::open_store();
        if let Some(store) = &store {
            // Anything still marked running belongs to an engine that is no
            // longer there. Left alone it would show the user a download
            // that looks live and will never move again.
            if let Err(error) = store.mark_interrupted_as_paused() {
                tracing::warn!(%error, "could not tidy interrupted downloads");
            }
        }
        Self::with_parts(output_dir, max_connections, settings_path(), store)
    }

    /// An engine with no store, pointed at a chosen settings file.
    /// Tests only: real use goes through `new`, which opens the store.
    #[cfg(test)]
    pub fn with_settings_file(
        output_dir: PathBuf,
        max_connections: u32,
        settings_file: PathBuf,
    ) -> Arc<Self> {
        Self::with_parts(output_dir, max_connections, settings_file, None)
    }

    fn with_parts(
        output_dir: PathBuf,
        max_connections: u32,
        settings_file: PathBuf,
        store: Option<Arc<idm_store::Store>>,
    ) -> Arc<Self> {
        let starting_limit = load_settings(&settings_file).speed_limit_bytes_per_sec;
        Arc::new(Self {
            inner: Arc::new(Inner {
                output_dir,
                max_connections,
                next_id: AtomicU64::new(1),
                jobs: Mutex::new(HashMap::new()),
                settings: Mutex::new(load_settings(&settings_file)),
                pairings: Mutex::new(PairingStore::load(
                    settings_file.with_file_name("pairings.json"),
                )),
                settings_file,
                store,
                rows: Mutex::new(HashMap::new()),
                limiter: Arc::new(idm_net::limiter::RateLimiter::new(starting_limit)),
                armed: Mutex::new(None),
            }),
        })
    }

    /// Decides whether to accept a request, and under what filename.
    ///
    /// Returns a sanitised *leaf* name. The caller joins it to our output
    /// directory; nothing from the request contributes a path component.
    fn vet(
        &self,
        request: &DownloadRequest,
        min_takeover_bytes: u64,
        file_transfer: bool,
    ) -> Result<String, String> {
        request.validate_with(file_transfer)?;

        // The threshold is about whether a download is big enough to be
        // worth taking over. A stream manifest is a few hundred bytes and
        // stands for a video of gigabytes, so its own size says nothing —
        // measuring it would refuse every stream there is.
        let manifest = idm_net::stream::looks_like_manifest(&request.url);
        if let Some(size) = request.size_hint
            && !manifest
            && size < min_takeover_bytes
        {
            return Err(format!(
                "{size} bytes is below the {min_takeover_bytes} byte takeover threshold"
            ));
        }

        // A suggested name goes through the same sanitiser as a
        // Content-Disposition header, because both are untrusted input.
        let name = request
            .filename
            .as_deref()
            .and_then(sanitise_filename)
            .or_else(|| {
                request
                    .url
                    .split(['?', '#'])
                    .next()
                    .and_then(|path| path.rsplit('/').next())
                    .and_then(sanitise_filename)
            })
            .unwrap_or_else(|| "download.bin".to_owned());
        Ok(name)
    }

    async fn enqueue(&self, client: &ClientInfo, request: DownloadRequest) -> Response {
        let settings = self.inner.settings.lock().await.clone();

        // The kill switch, enforced here rather than in the extension. An
        // extension-only switch is cosmetic: anything already holding the
        // token could still queue downloads.
        if client.is_browser() {
            if !settings.browser_integration {
                return Response::Rejected {
                    reason: "browser integration is switched off in Divine IDM".into(),
                };
            }
            // Approved by a human, in the app, before anything is queued.
            let paired = match &client.client_id {
                Some(id) => self.inner.pairings.lock().await.is_paired(id),
                None => false,
            };
            if !paired {
                return Response::Rejected {
                    reason: "this extension is not paired; approve it in Divine IDM".into(),
                };
            }
        }

        // Only the app and the command line may name an ftp or sftp
        // address. A page handing one over would put credentials in a URL
        // and reach servers the browser itself could not.
        let filename = match self.vet(&request, settings.min_takeover_bytes, !client.is_browser()) {
            Ok(name) => name,
            Err(reason) => return Response::Rejected { reason },
        };

        let id = JobId(self.inner.next_id.fetch_add(1, Ordering::SeqCst));
        let mut config = DownloadConfig::new(request.url.clone(), self.inner.output_dir.clone());
        config.filename = Some(filename.clone());
        config.conn.per_host_max = self.inner.max_connections;
        config.referrer = request.referrer.clone();
        config.cookies = request.cookies.clone();
        config.fetch.limiter = Some(Arc::clone(&self.inner.limiter));
        config.profiles =
            self.inner.store.clone().map(|store| store as Arc<dyn idm_core::sched::ProfileStore>);

        // Recorded before the download starts, so a crash one second in
        // still leaves a row saying what was being fetched. Cookies are not
        // among the fields: see the store's own documentation.
        if let Some(store) = &self.inner.store {
            let row = idm_store::NewJob {
                url: request.url.clone(),
                filename: filename.clone(),
                output_dir: self.inner.output_dir.display().to_string(),
                total: request.size_hint,
                referrer: request.referrer.clone(),
                user_agent: request.user_agent.clone(),
                mime: request.mime.clone(),
                // Not the cookies themselves — only that there were some,
                // so a later restart knows this one cannot just be retried.
                needs_browser_auth: request.cookies.is_some(),
                ..Default::default()
            };
            match store.add_job(&row) {
                Ok(row_id) => {
                    self.inner.rows.lock().await.insert(id, row_id);
                }
                Err(error) => tracing::warn!(%error, "could not record the job"),
            }
        }

        let status = JobStatus {
            id,
            url: request.url.clone(),
            filename,
            state: JobState::Queued,
            downloaded: 0,
            total: None,
            bytes_per_sec: 0.0,
            connections: 0,
            error: None,
        };

        self.inner
            .jobs
            .lock()
            .await
            .insert(id, JobRecord { status, abort: None, pending: Some(Box::new(config)) });
        pump(Arc::clone(&self.inner)).await;
        Response::Accepted { id }
    }

    /// How many jobs have been handed a slot and not yet finished.
    ///
    /// The number the limit actually governs. Counting running jobs instead
    /// would miss one that has been started but whose task has not run yet.
    #[cfg(test)]
    async fn outstanding(&self) -> usize {
        self.inner
            .jobs
            .lock()
            .await
            .values()
            .filter(|job| {
                job.pending.is_none()
                    && matches!(job.status.state, JobState::Queued | JobState::Running)
            })
            .count()
    }

    #[cfg(test)]
    async fn waiting(&self) -> usize {
        self.inner.jobs.lock().await.values().filter(|job| job.pending.is_some()).count()
    }

    /// Starts a paused download again.
    async fn resume(&self, id: JobId) -> Response {
        let ready = {
            let mut jobs = self.inner.jobs.lock().await;
            match jobs.get_mut(&id) {
                None => return Response::Rejected { reason: "no such download".into() },
                Some(job) if job.pending.is_none() => {
                    return Response::Rejected {
                        reason: match job.status.state {
                            JobState::Running | JobState::Queued => {
                                "that download is already going"
                            }
                            JobState::Completed => "that download has already finished",
                            _ => "that download cannot be resumed; start it again",
                        }
                        .into(),
                    };
                }
                Some(job) => {
                    job.status.state = JobState::Queued;
                    job.status.error = None;
                    true
                }
            }
        };

        if ready {
            write_state(&self.inner, id, JobState::Queued, None).await;
            pump(Arc::clone(&self.inner)).await;
        }
        Response::Accepted { id }
    }

    /// Brings interrupted downloads back after a restart.
    ///
    /// The bytes and the journal beside them already survive a crash; this
    /// is only the decision to act on that. Anything that was running when
    /// the engine stopped has been marked paused by the store, so what is
    /// restored here is a list of jobs the user can see and resume, with
    /// their progress intact.
    ///
    /// A download that was authorised by browser cookies is restored but
    /// never started automatically. The cookies were deliberately not kept,
    /// so retrying it would produce a refusal from the server and an error
    /// the user has no way to connect to anything they did. Saying so is
    /// more useful than failing.
    pub async fn restore_interrupted(&self) -> usize {
        let Some(store) = self.inner.store.clone() else {
            return 0;
        };
        let rows = match store.unfinished_jobs() {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(%error, "could not read interrupted downloads");
                return 0;
            }
        };
        if rows.is_empty() {
            return 0;
        }

        let auto = self.inner.settings.lock().await.resume_on_start;
        let mut restored = 0;

        for row in rows {
            let id = JobId(self.inner.next_id.fetch_add(1, Ordering::SeqCst));

            let mut config = DownloadConfig::new(row.url.clone(), PathBuf::from(&row.output_dir));
            config.filename = Some(row.filename.clone());
            config.conn.per_host_max = self.inner.max_connections;
            config.referrer = row.referrer.clone();
            config.fetch.limiter = Some(Arc::clone(&self.inner.limiter));
            config.profiles = self
                .inner
                .store
                .clone()
                .map(|store| store as Arc<dyn idm_core::sched::ProfileStore>);

            let blocked = row.needs_browser_auth;
            let status = JobStatus {
                id,
                url: row.url.clone(),
                filename: row.filename.clone(),
                // Queued only when it is actually going to start; otherwise
                // paused, which is what it is.
                state: if auto && !blocked { JobState::Queued } else { JobState::Paused },
                downloaded: row.downloaded,
                total: row.total,
                bytes_per_sec: 0.0,
                connections: 0,
                error: blocked.then(|| {
                    "this download was authorised by your browser; start it there again".to_owned()
                }),
            };

            self.inner.rows.lock().await.insert(id, row.id);
            self.inner
                .jobs
                .lock()
                .await
                .insert(id, JobRecord { status, abort: None, pending: Some(Box::new(config)) });
            restored += 1;
        }

        if auto {
            pump(Arc::clone(&self.inner)).await;
        }
        tracing::info!(restored, auto, "restored interrupted downloads");
        restored
    }

    async fn status(&self, id: Option<JobId>) -> Response {
        let jobs = self.inner.jobs.lock().await;
        let listed = match id {
            Some(wanted) => jobs.get(&wanted).map(|job| job.status.clone()).into_iter().collect(),
            None => jobs.values().map(|job| job.status.clone()).collect(),
        };
        Response::Status { jobs: listed }
    }

    async fn cancel(&self, id: JobId) -> Response {
        let mut jobs = self.inner.jobs.lock().await;
        match jobs.get_mut(&id) {
            Some(record) => {
                if let Some(abort) = record.abort.take() {
                    abort.abort();
                }
                // Paused, not Failed: the part file and journal survive, so
                // the same URL resumes rather than starting over.
                record.status.state = JobState::Paused;
                Response::Accepted { id }
            }
            None => Response::Rejected { reason: format!("no such job: {id}") },
        }
    }
}

/// Records progress from the download's synchronous callback.
///
/// Uses `try_lock` deliberately: progress is advisory, and a checkpoint should
/// never stall because something else is reading the registry.
fn record_progress(inner: &Inner, id: JobId, progress: &Progress) {
    if let Ok(mut jobs) = inner.jobs.try_lock()
        && let Some(record) = jobs.get_mut(&id)
    {
        record.status.downloaded = progress.durable;
        record.status.total = Some(progress.total);
        record.status.bytes_per_sec = progress.rate_bps;
        record.status.connections = progress.connections;
    }
}

/// Segments fetched at once for a stream, mirrored from the stream
/// options so the status can say how many requests are in flight.
const STREAM_CONCURRENCY: usize = 8;

/// An ordinary file, through the scheduler.
async fn run_file(
    inner: &Arc<Inner>,
    id: JobId,
    config: &DownloadConfig,
) -> std::result::Result<Option<u64>, String> {
    let progress_inner = Arc::clone(inner);
    let report = download_with_progress(config, move |progress: Progress| {
        record_progress(&progress_inner, id, &progress);
    })
    .await
    .map_err(|error| error.to_string())?;

    if report.resumed_bytes > 0 {
        // Worth saying out loud: it is the difference between picking a
        // download up and quietly starting it again from nothing.
        eprintln!(
            "resumed {} after {} bytes already on disk",
            report.path.display(),
            report.resumed_bytes
        );
    }
    Ok(Some(report.total))
}

/// Records byte progress for the transfers that report it as (done, total).
fn record_transfer(inner: &Inner, id: JobId, done: u64, total: Option<u64>) {
    if let Ok(mut jobs) = inner.jobs.try_lock()
        && let Some(record) = jobs.get_mut(&id)
    {
        record.status.downloaded = done;
        if total.is_some() {
            record.status.total = total;
        }
        // One connection: FTP servers count logins per user, and SFTP's
        // reads are pipelined on one channel.
        record.status.connections = 1;
    }
}

/// FTP or FTPS. Credentials, if any, come from the address; otherwise the
/// login is anonymous, which is what a bare ftp:// means.
async fn run_ftp(
    inner: &Arc<Inner>,
    id: JobId,
    config: &DownloadConfig,
) -> std::result::Result<Option<u64>, String> {
    let mut options = idm_net::ftp::FtpOptions::new(config.output_dir.clone());
    options.filename = config.filename.clone();
    options.limiter = config.fetch.limiter.clone();

    let progress_inner = Arc::clone(inner);
    let report = idm_net::ftp::download(&config.url, &options, move |done, total| {
        record_transfer(&progress_inner, id, done, total);
    })
    .await
    .map_err(|error| error.to_string())?;
    Ok(Some(report.bytes))
}

/// SFTP. There is nobody to type a password into, so this authenticates the
/// way `ssh` does by default — with the keys in ~/.ssh — and checks the host
/// against known_hosts, refusing one it has not seen.
async fn run_sftp(
    inner: &Arc<Inner>,
    id: JobId,
    config: &DownloadConfig,
) -> std::result::Result<Option<u64>, String> {
    let mut options = idm_net::sftp::SftpOptions::new(config.output_dir.clone());
    options.filename = config.filename.clone();
    options.limiter = config.fetch.limiter.clone();
    options.use_default_identities = true;

    let progress_inner = Arc::clone(inner);
    let report = idm_net::sftp::download(&config.url, &options, move |done, total| {
        record_transfer(&progress_inner, id, done, total);
    })
    .await
    .map_err(|error| error.to_string())?;
    Ok(Some(report.bytes))
}

/// A stream: manifest, rendition, segments, one file.
///
/// Progress is counted in segments rather than in a total that is not known
/// until the last one arrives. The job's byte count still moves, so the
/// window shows something advancing, but its total stays unset rather than
/// being invented from a guess at the segment size.
async fn run_stream(
    inner: &Arc<Inner>,
    id: JobId,
    config: &DownloadConfig,
) -> std::result::Result<Option<u64>, String> {
    let mut options = idm_net::stream::StreamOptions::new(config.output_dir.clone());
    options.filename = config.filename.clone();
    options.referrer = config.referrer.clone();
    options.cookies = config.cookies.clone();
    options.limiter = config.fetch.limiter.clone();

    let progress_inner = Arc::clone(inner);
    let report = idm_net::stream::download(&config.url, &options, move |progress| {
        if let Ok(mut jobs) = progress_inner.jobs.try_lock()
            && let Some(record) = jobs.get_mut(&id)
        {
            record.status.downloaded = progress.bytes;
            // How many segments are still being fetched, capped by how many
            // are fetched at once. The status has no field for segments, and
            // inventing a total from the bytes so far would put a progress
            // bar on a number nobody measured.
            let outstanding = progress.segments_total.saturating_sub(progress.segments_done);
            record.status.connections = outstanding.min(STREAM_CONCURRENCY) as u32;
        }
    })
    .await
    .map_err(|error| error.to_string())?;

    if let Some(note) = &report.note {
        eprintln!("note: {note}");
    }
    Ok(Some(report.bytes))
}

/// Starts waiting downloads while there is room for them.
///
/// Called when a job is queued and again when one finishes, rather than on a
/// timer. A slot opening is an event this code already knows about, so
/// polling for it would only add latency to every handover.
///
/// The limit is about the origin and the link, not the disk. Each running
/// download opens its own fan of connections, so ten at once is not ten times
/// faster -- it is the same bandwidth split ten ways, with ten times the
/// chance of tripping a rate limit. How many connections one download
/// deserves is already measured; this decides how many downloads get to make
/// that decision at the same time.
fn pump(inner: Arc<Inner>) -> BoxFuture {
    Box::pin(async move {
        pump_inner(inner).await;
    })
}

/// A future whose `Send` is stated rather than inferred.
///
/// `pump` starts downloads and each download calls `pump` again as it
/// finishes. That mutual recursion leaves the compiler unable to settle
/// either future's auto traits -- each depends on the other -- and would
/// give the task's future its own type as a field. One boxed trait object
/// in the cycle resolves both.
type BoxFuture = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;

async fn pump_inner(inner: Arc<Inner>) {
    let limit = inner.settings.lock().await.max_concurrent_downloads.max(1);

    loop {
        let next = {
            let mut jobs = inner.jobs.lock().await;
            // Counted by whether the job has been handed out, not by
            // whether its task has got as far as marking itself running.
            // `start` only spawns; the state changes a moment later, so a
            // burst of enqueues would each see nothing running and start
            // everything at once.
            let outstanding = jobs
                .values()
                .filter(|job| {
                    job.pending.is_none()
                        && matches!(job.status.state, JobState::Queued | JobState::Running)
                })
                .count() as u32;
            if outstanding >= limit {
                return;
            }

            // Oldest first: ids are handed out in order, so that is the
            // order they were asked for in.
            let mut waiting: Vec<JobId> = jobs
                .iter()
                .filter(|(_, job)| job.pending.is_some() && job.status.state == JobState::Queued)
                .map(|(id, _)| *id)
                .collect();
            waiting.sort_by_key(|id| id.0);

            match waiting.first().copied() {
                // Taken under the same lock that chose it, so two pumps
                // running at once cannot both start the same job.
                Some(id) => {
                    jobs.get_mut(&id).and_then(|job| job.pending.take()).map(|config| (id, config))
                }
                None => None,
            }
        };

        let Some((id, config)) = next else {
            return;
        };
        start(&inner, id, *config).await;
    }
}

/// Spawns the download task for a job that has just been given a slot.
async fn start(inner: &Arc<Inner>, id: JobId, config: DownloadConfig) {
    let task = {
        let inner = Arc::clone(inner);
        tokio::spawn(async move {
            set_state(&inner, id, JobState::Running).await;

            // A manifest is not a file. Routed here rather than in the
            // caller so every client — the window, the browser extension,
            // the command line — gets streams without each one deciding.
            // Every scheme the engine accepts has to be routed here. Letting
            // a client hand over an ftp:// address and then sending it down
            // the HTTP path accepts the download and fails it with a message
            // about a "builder error" — which is exactly what happened when
            // the schemes were allowed in and this was not updated.
            let outcome = if idm_net::stream::looks_like_manifest(&config.url) {
                run_stream(&inner, id, &config).await
            } else if idm_net::ftp::handles(&config.url) {
                run_ftp(&inner, id, &config).await
            } else if idm_net::sftp::handles(&config.url) {
                run_sftp(&inner, id, &config).await
            } else {
                run_file(&inner, id, &config).await
            };

            match outcome {
                Ok(total) => {
                    finish(&inner, id, JobState::Completed, total, None).await;
                }
                Err(error) => {
                    // The partial file and journal are deliberately left in
                    // place, so a retry resumes rather than restarts.
                    finish(&inner, id, JobState::Failed, None, Some(error)).await;
                }
            }
            forget_row(&inner, id).await;

            // A slot has just opened, so whatever is next can begin.
            pump(Arc::clone(&inner)).await;

            // Every finishing job asks whether the queue has now drained.
            // Doing it here rather than on a timer means the action fires as
            // soon as the last download lands.
            if let Some((armed, all_succeeded)) = maybe_arm(&inner).await {
                tokio::spawn(run_post_action(armed, all_succeeded));
            }
        })
    };

    if let Some(record) = inner.jobs.lock().await.get_mut(&id) {
        record.abort = Some(task.abort_handle());
    }
}

async fn set_state(inner: &Inner, id: JobId, state: JobState) {
    if let Some(record) = inner.jobs.lock().await.get_mut(&id) {
        record.status.state = state;
    }
    write_state(inner, id, state, None).await;
}

/// The row id for a live job, if the store is keeping one.
async fn row_of(inner: &Inner, id: JobId) -> Option<(Arc<idm_store::Store>, i64)> {
    let store = inner.store.clone()?;
    let row = *inner.rows.lock().await.get(&id)?;
    Some((store, row))
}

async fn write_state(inner: &Inner, id: JobId, state: JobState, error: Option<&str>) {
    let Some((store, row)) = row_of(inner, id).await else {
        return;
    };
    let stored = match state {
        JobState::Queued => idm_store::JobState::Queued,
        JobState::Running => idm_store::JobState::Running,
        JobState::Paused => idm_store::JobState::Paused,
        JobState::Completed => idm_store::JobState::Completed,
        JobState::Failed => idm_store::JobState::Failed,
    };
    if let Err(error) = store.set_job_state(row, stored, error) {
        tracing::warn!(%error, "could not record the job state");
    }
}

/// Drops the mapping once a job has finished, so the table does not grow for
/// the life of the process. The row itself stays; it is the history.
async fn forget_row(inner: &Inner, id: JobId) {
    inner.rows.lock().await.remove(&id);
}

async fn finish(
    inner: &Inner,
    id: JobId,
    state: JobState,
    total: Option<u64>,
    error: Option<String>,
) {
    if let Some(record) = inner.jobs.lock().await.get_mut(&id) {
        record.status.state = state;
        record.status.error = error.clone();
        if let Some(total) = total {
            record.status.total = Some(total);
            record.status.downloaded = total;
        }
        record.abort = None;
    }

    if let Some((store, row)) = row_of(inner, id).await
        && let Some(total) = total
        && let Err(error) = store.update_progress(row, total, Some(total))
    {
        tracing::warn!(%error, "could not record the final size");
    }
    write_state(inner, id, state, error.as_deref()).await;
}

impl Engine {
    /// Records a pairing request. Never approves it.
    async fn request_pairing(&self, client: &ClientInfo, label: String) -> Response {
        let Some(client_id) = client.client_id.clone() else {
            return Response::Rejected {
                reason: "this client did not identify itself, so it cannot be paired".into(),
            };
        };
        match self.inner.pairings.lock().await.request(&client_id, &label) {
            PairingOutcome::AlreadyPaired => Response::PairingApproved,
            PairingOutcome::Pending { request_id, fresh } => {
                // Deliberately only a notice: approval has to come from the
                // app, which is a different client on a different connection.
                // Only for a new request -- an extension re-greets on every
                // service worker wake, and repeating the same approval line
                // reads like several browsers are asking.
                if fresh {
                    eprintln!(
                        "Pairing requested by {label:?}. Approve with:  idm --approve {request_id}"
                    );
                }
                Response::PairingPending { request_id }
            }
        }
    }

    /// Managing pairings is refused for browser clients.
    ///
    /// Otherwise an extension could approve itself, or enumerate what else is
    /// connected, which is exactly what pairing exists to prevent.
    fn refuse_browser(client: &ClientInfo) -> Option<Response> {
        client.is_browser().then(|| Response::Rejected {
            reason: "the browser integration cannot manage pairings".into(),
        })
    }

    async fn list_pairings(&self, client: &ClientInfo) -> Response {
        if let Some(refusal) = Self::refuse_browser(client) {
            return refusal;
        }
        let mut store = self.inner.pairings.lock().await;
        Response::Pairings { paired: store.paired(), pending: store.pending() }
    }

    async fn approve_pairing(&self, client: &ClientInfo, request_id: String) -> Response {
        if let Some(refusal) = Self::refuse_browser(client) {
            return refusal;
        }
        match self.inner.pairings.lock().await.approve(&request_id) {
            Some(paired) => {
                eprintln!("Paired {:?}.", paired.label);
                Response::PairingApproved
            }
            None => Response::Rejected { reason: "no such pairing request".into() },
        }
    }

    async fn deny_pairing(&self, client: &ClientInfo, request_id: String) -> Response {
        if let Some(refusal) = Self::refuse_browser(client) {
            return refusal;
        }
        if self.inner.pairings.lock().await.deny(&request_id) {
            Response::Pong
        } else {
            Response::Rejected { reason: "no such pairing request".into() }
        }
    }

    async fn revoke_pairing(&self, client: &ClientInfo, client_id: String) -> Response {
        if let Some(refusal) = Self::refuse_browser(client) {
            return refusal;
        }
        if self.inner.pairings.lock().await.revoke(&client_id) {
            Response::Pong
        } else {
            Response::Rejected { reason: "that client was not paired".into() }
        }
    }

    async fn get_settings(&self) -> Response {
        Response::Settings(self.inner.settings.lock().await.clone())
    }

    async fn set_settings(&self, client: &ClientInfo, settings: Settings) -> Response {
        // Only the desktop app and CLI may change settings. Letting the
        // browser side rewrite them would hand a web page the ability to turn
        // its own kill switch back on.
        if client.is_browser() {
            return Response::Rejected {
                reason: "the browser integration cannot change settings".into(),
            };
        }
        let mut current = self.inner.settings.lock().await;
        let limit_changed = current.speed_limit_bytes_per_sec != settings.speed_limit_bytes_per_sec;
        *current = settings.clone();
        drop(current);
        save_settings(&self.inner.settings_file, &settings);

        // Takes effect on the next read of every running download, not only
        // on the ones started afterwards — the usual reason for changing a
        // limit is that a download already running is in the way.
        if limit_changed {
            self.inner.limiter.set_rate(settings.speed_limit_bytes_per_sec).await;
        }
        Response::Settings(settings)
    }

    /// Cancels an armed shutdown, if one is counting down.
    async fn cancel_post_action(&self) -> Response {
        match self.inner.armed.lock().await.take() {
            Some(canceller) => {
                canceller.cancel();
                Response::Pong
            }
            None => Response::Rejected { reason: "nothing is armed".into() },
        }
    }
}

/// Decides whether the queue has drained and, if so, arms the configured
/// action.
///
/// Only decides *when* to arm. The countdown, the cancellation window and the
/// only-on-success rule all live in `idm_core::win::power`, so the dangerous
/// part has one home and one set of tests.
async fn maybe_arm(inner: &Arc<Inner>) -> Option<(ArmedAction, bool)> {
    let settings = inner.settings.lock().await.clone();
    let action = match settings.post_action {
        PostActionSetting::Nothing => return None,
        PostActionSetting::ExitApp => PostAction::ExitApp,
        PostActionSetting::Sleep => PostAction::Sleep,
        PostActionSetting::Hibernate => PostAction::Hibernate,
        PostActionSetting::Shutdown => PostAction::Shutdown,
    };

    let jobs = inner.jobs.lock().await;
    if jobs.is_empty() {
        return None;
    }
    // Still working: arming now could suspend the machine mid-download.
    let busy =
        jobs.values().any(|job| matches!(job.status.state, JobState::Queued | JobState::Running));
    if busy {
        return None;
    }
    let all_succeeded = jobs.values().all(|job| job.status.state == JobState::Completed);
    drop(jobs);

    let armed = ArmedAction::new(
        action,
        std::time::Duration::from_secs(settings.post_action_countdown_secs),
    );
    *inner.armed.lock().await = Some(armed.canceller());
    Some((armed, all_succeeded))
}

/// Runs an armed action, reporting the countdown so the user can stop it.
async fn run_post_action(armed: ArmedAction, all_succeeded: bool) {
    let action = armed.action();
    eprintln!(
        "All downloads finished. Will {} in {}s unless cancelled.",
        action.describe(),
        armed.countdown().as_secs()
    );

    let mut last_announced = u64::MAX;
    let outcome = armed
        .run(&SystemPower, all_succeeded, |remaining| {
            // Announce each whole second once, not every 250 ms tick.
            let secs = remaining.as_secs();
            if secs != last_announced && secs % 10 == 0 {
                eprintln!("  {} in {secs}s...", action.describe());
                last_announced = secs;
            }
        })
        .await;

    match outcome {
        Ok(ActionOutcome::Performed(PostAction::ExitApp)) => {
            eprintln!("All downloads finished; closing.");
            std::process::exit(0);
        }
        Ok(ActionOutcome::Performed(done)) => tracing::info!(?done, "post-download action taken"),
        Ok(ActionOutcome::SkippedOnFailure) => {
            eprintln!("Not going to {} — something failed.", action.describe());
        }
        Ok(ActionOutcome::Cancelled) => eprintln!("Cancelled; staying on."),
        Ok(ActionOutcome::NotRequested) => {}
        Err(error) => eprintln!("could not {}: {error}", action.describe()),
    }
}

/// Where settings live, beside the endpoint file in the user profile.
pub fn settings_path() -> PathBuf {
    idm_ipc::endpoint_path().with_file_name("settings.json")
}

fn load_settings(path: &PathBuf) -> Settings {
    // A missing file means defaults, which are the safe ones: browser
    // integration on, no post-download action.
    let Ok(bytes) = std::fs::read(path) else {
        return Settings::default();
    };
    match serde_json::from_slice(&bytes) {
        Ok(settings) => settings,
        Err(error) => {
            // Said out loud. Falling back to defaults is the safe choice, but
            // a silent fallback is how this file's own values were discarded
            // on every start without anyone noticing.
            eprintln!(
                "warning: {} could not be read ({error}); using default settings",
                path.display()
            );
            Settings::default()
        }
    }
}

fn save_settings(path: &PathBuf, settings: &Settings) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_vec_pretty(settings) {
        let _ = std::fs::write(path, json);
    }
}

impl Handler for Engine {
    async fn handle(&self, client: &ClientInfo, request: Request) -> Response {
        match request {
            Request::Ping => Response::Pong,
            Request::Enqueue(req) => self.enqueue(client, req).await,
            Request::Status { id } => self.status(id).await,
            Request::Cancel { id } => self.cancel(id).await,
            Request::Resume { id } => self.resume(id).await,
            Request::RequestPairing { label } => self.request_pairing(client, label).await,
            Request::ListPairings => self.list_pairings(client).await,
            Request::ApprovePairing { request_id } => {
                self.approve_pairing(client, request_id).await
            }
            Request::DenyPairing { request_id } => self.deny_pairing(client, request_id).await,
            Request::RevokePairing { client_id } => self.revoke_pairing(client, client_id).await,
            Request::GetSettings => self.get_settings().await,
            Request::SetSettings(settings) => self.set_settings(client, settings).await,
            Request::CancelPostAction => self.cancel_post_action().await,
            Request::Hello { .. } => Response::Error { message: "already authenticated".into() },
        }
    }
}

/// Binds the endpoint and serves until the process ends.
pub async fn run(output_dir: PathBuf, max_connections: u32) -> Result<(), String> {
    std::fs::create_dir_all(&output_dir).map_err(|error| error.to_string())?;
    let server = IpcServer::bind().map_err(|error| error.to_string())?;
    let engine = Engine::new(output_dir.clone(), max_connections);

    let restored = engine.restore_interrupted().await;
    if restored > 0 {
        eprintln!("picked up {restored} interrupted download(s)");
    }

    eprintln!("Divine IDM serving");
    eprintln!("  downloads -> {}", output_dir.display());
    eprintln!("  endpoint  -> {}", idm_ipc::endpoint_path().display());
    // The pipe name and token are deliberately not printed. The endpoint file
    // is readable only by this user; echoing the token to a terminal or a log
    // would widen that for no benefit.
    let _: &SessionToken = server.token();

    server.serve(engine).await.map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> (tempfile::TempDir, Arc<Engine>) {
        let dir = tempfile::tempdir().unwrap();
        let engine = Engine::with_settings_file(
            dir.path().join("downloads"),
            8,
            dir.path().join("settings.json"),
        );
        (dir, engine)
    }

    /// Sets the concurrency limit through the same path the UI uses.
    async fn set_limit(engine: &Arc<Engine>, limit: u32) {
        let mut settings = match engine.handle(&desktop(), Request::GetSettings).await {
            Response::Settings(settings) => settings,
            other => panic!("expected settings, got {other:?}"),
        };
        settings.max_concurrent_downloads = limit;
        engine.handle(&desktop(), Request::SetSettings(settings)).await;
    }

    /// An engine sharing a caller-supplied store, so a restart can be
    /// staged by building a second engine over the same rows.
    fn engine_with_store(dir: &std::path::Path, store: Arc<idm_store::Store>) -> Arc<Engine> {
        Engine::with_parts(dir.join("downloads"), 8, dir.join("settings.json"), Some(store))
    }

    async fn set_resume_on_start(engine: &Arc<Engine>, resume: bool) {
        let mut settings = match engine.handle(&desktop(), Request::GetSettings).await {
            Response::Settings(settings) => settings,
            other => panic!("expected settings, got {other:?}"),
        };
        settings.resume_on_start = resume;
        engine.handle(&desktop(), Request::SetSettings(settings)).await;
    }

    fn browser() -> ClientInfo {
        ClientInfo { name: idm_ipc::BROWSER_CLIENT.to_owned(), client_id: Some("ext-1".to_owned()) }
    }

    fn desktop() -> ClientInfo {
        ClientInfo { name: "desktop".to_owned(), client_id: None }
    }

    /// Walks a browser client through the approval flow.
    async fn pair(engine: &Engine, client: &ClientInfo) {
        let Response::PairingPending { request_id } =
            engine.request_pairing(client, "Test browser".into()).await
        else {
            panic!("a new client should land as pending");
        };
        engine.approve_pairing(&desktop(), request_id).await;
    }

    fn request(url: &str) -> DownloadRequest {
        DownloadRequest { url: url.into(), size_hint: Some(10 << 20), ..Default::default() }
    }

    #[test]
    fn a_suggested_filename_can_never_escape_the_output_directory() {
        let (_dir, engine) = engine();
        for hostile in ["../../evil.exe", r"..\..\evil.exe", "/etc/passwd", r"C:\Windows\x.dll"] {
            let mut req = request("https://example.invalid/a.bin");
            req.filename = Some(hostile.into());
            // Either refused outright, or reduced to a bare leaf name.
            if let Ok(name) = engine.vet(&req, 0, false) {
                assert!(!name.contains('/') && !name.contains('\\'), "{hostile} -> {name}");
                assert!(!name.contains(".."), "{hostile} -> {name}");
            }
        }
    }

    #[test]
    fn reserved_device_names_are_defused() {
        let (_dir, engine) = engine();
        let mut req = request("https://example.invalid/a.bin");
        req.filename = Some("CON".into());
        assert_eq!(engine.vet(&req, 0, false).unwrap(), "_CON");
    }

    #[test]
    fn a_filename_is_derived_from_the_url_when_none_is_given() {
        let (_dir, engine) = engine();
        let name =
            engine.vet(&request("https://example.invalid/path/report.pdf?x=1"), 0, false).unwrap();
        assert_eq!(name, "report.pdf");
    }

    #[test]
    fn small_downloads_are_left_to_the_browser() {
        // Taking over a 100 KiB file costs a handover for no benefit.
        let (_dir, engine) = engine();
        let threshold = Settings::default().min_takeover_bytes;
        let mut req = request("https://example.invalid/small.bin");
        req.size_hint = Some(100 << 10);
        assert!(engine.vet(&req, threshold, false).is_err());

        req.size_hint = Some(50 << 20);
        assert!(engine.vet(&req, threshold, false).is_ok());
        // An unknown size is accepted; the probe will settle it.
        req.size_hint = None;
        assert!(engine.vet(&req, threshold, false).is_ok());
    }

    #[test]
    fn non_http_schemes_never_reach_the_engine() {
        let (_dir, engine) = engine();
        for hostile in ["file:///C:/secret", "data:text/plain,hi", "javascript:alert(1)"] {
            assert!(
                engine.vet(&request(hostile), 0, false).is_err(),
                "{hostile} should be refused"
            );
        }
    }

    #[tokio::test]
    async fn status_reports_nothing_for_an_unknown_job() {
        let (_dir, engine) = engine();
        match engine.status(Some(JobId(999))).await {
            Response::Status { jobs } => assert!(jobs.is_empty()),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_unpaired_browser_is_refused() {
        // The point of pairing: an extension that nobody approved gets
        // nothing, even though it holds a valid token.
        let (_dir, engine) = engine();
        match engine.enqueue(&browser(), request("https://example.invalid/a.bin")).await {
            Response::Rejected { reason } => assert!(reason.contains("not paired"), "{reason}"),
            other => panic!("an unpaired browser should be refused, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_pairing_request_is_never_self_approving() {
        let (_dir, engine) = engine();
        let client = browser();
        let reply = engine.request_pairing(&client, "Evil".into()).await;
        assert!(matches!(reply, Response::PairingPending { .. }), "{reply:?}");

        // Asking did not grant anything.
        match engine.enqueue(&client, request("https://example.invalid/a.bin")).await {
            Response::Rejected { .. } => {}
            other => panic!("still unpaired, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_browser_cannot_approve_or_enumerate_pairings() {
        let (_dir, engine) = engine();
        let client = browser();
        let Response::PairingPending { request_id } =
            engine.request_pairing(&client, "Chrome".into()).await
        else {
            panic!("expected pending")
        };

        // Approving itself would defeat the whole mechanism.
        let self_approval = engine.approve_pairing(&client, request_id.clone()).await;
        assert!(matches!(self_approval, Response::Rejected { .. }), "{self_approval:?}");
        // And it must not get to see what else is connected.
        assert!(matches!(engine.list_pairings(&client).await, Response::Rejected { .. }));

        // The app can.
        assert!(matches!(
            engine.approve_pairing(&desktop(), request_id).await,
            Response::PairingApproved
        ));
    }

    #[tokio::test]
    async fn revoking_stops_an_extension_immediately() {
        let (_dir, engine) = engine();
        let client = browser();
        pair(&engine, &client).await;
        assert!(matches!(
            engine.enqueue(&client, request("https://example.invalid/a.bin")).await,
            Response::Accepted { .. }
        ));

        engine.revoke_pairing(&desktop(), "ext-1".into()).await;
        match engine.enqueue(&client, request("https://example.invalid/a.bin")).await {
            Response::Rejected { reason } => assert!(reason.contains("not paired"), "{reason}"),
            other => panic!("a revoked client should be refused, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_client_that_will_not_identify_itself_cannot_pair() {
        let (_dir, engine) = engine();
        let anonymous = ClientInfo { name: idm_ipc::BROWSER_CLIENT.to_owned(), client_id: None };
        let reply = engine.request_pairing(&anonymous, "Mystery".into()).await;
        assert!(matches!(reply, Response::Rejected { .. }), "{reply:?}");
    }

    #[tokio::test]
    async fn the_kill_switch_blocks_the_browser_but_not_the_app() {
        let (_dir, engine) = engine();
        let req = request("https://example.invalid/big.bin");
        pair(&engine, &browser()).await;

        // On by default, so a paired browser works.
        let accepted = engine.enqueue(&browser(), req.clone()).await;
        assert!(matches!(accepted, Response::Accepted { .. }), "{accepted:?}");

        let off = Settings { browser_integration: false, ..Settings::default() };
        assert!(matches!(engine.set_settings(&desktop(), off).await, Response::Settings(_)));

        match engine.enqueue(&browser(), req.clone()).await {
            Response::Rejected { reason } => assert!(reason.contains("switched off"), "{reason}"),
            other => panic!("the browser should be blocked, got {other:?}"),
        }
        // The app and CLI are unaffected: the switch is about the browser
        // handover, not about downloading.
        let still_works = engine.enqueue(&desktop(), req).await;
        assert!(matches!(still_works, Response::Accepted { .. }), "{still_works:?}");
    }

    #[tokio::test]
    async fn the_browser_cannot_switch_its_own_integration_back_on() {
        // Otherwise the control is worthless: what the user turned off could
        // turn itself back on.
        let (_dir, engine) = engine();
        let off = Settings { browser_integration: false, ..Settings::default() };
        engine.set_settings(&desktop(), off).await;

        let on = Settings { browser_integration: true, ..Settings::default() };
        assert!(matches!(engine.set_settings(&browser(), on).await, Response::Rejected { .. }));

        match engine.get_settings().await {
            Response::Settings(current) => assert!(!current.browser_integration),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn settings_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        {
            let engine = Engine::with_settings_file(dir.path().into(), 8, file.clone());
            let changed = Settings {
                browser_integration: false,
                post_action: PostActionSetting::Shutdown,
                ..Settings::default()
            };
            engine.set_settings(&desktop(), changed).await;
        }
        let reopened = Engine::with_settings_file(dir.path().into(), 8, file);
        match reopened.get_settings().await {
            Response::Settings(current) => {
                assert!(!current.browser_integration);
                assert_eq!(current.post_action, PostActionSetting::Shutdown);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn defaults_are_the_safe_ones() {
        let (_dir, engine) = engine();
        match engine.get_settings().await {
            Response::Settings(settings) => {
                assert!(settings.browser_integration, "integration should work out of the box");
                assert_eq!(
                    settings.post_action,
                    PostActionSetting::Nothing,
                    "nothing should ever shut a machine down unless asked"
                );
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn no_post_action_is_armed_while_work_is_outstanding() {
        let (_dir, engine) = engine();
        let shutdown = Settings { post_action: PostActionSetting::Shutdown, ..Settings::default() };
        engine.set_settings(&desktop(), shutdown).await;

        // Exercises the same function the download task calls, so the test
        // covers the real path rather than a wrapper written for it.
        assert!(maybe_arm(&engine.inner).await.is_none(), "nothing queued, nothing to arm");

        // Queued but unfinished: arming here could suspend the machine
        // mid-download.
        engine.enqueue(&desktop(), request("https://example.invalid/a.bin")).await;
        assert!(
            maybe_arm(&engine.inner).await.is_none(),
            "must not arm while a download is still running"
        );
    }

    #[tokio::test]
    async fn cancelling_a_post_action_with_none_armed_is_refused() {
        let (_dir, engine) = engine();
        assert!(matches!(engine.cancel_post_action().await, Response::Rejected { .. }));
    }

    #[tokio::test]
    async fn cancelling_an_unknown_job_is_refused_not_panicked() {
        let (_dir, engine) = engine();
        assert!(matches!(engine.cancel(JobId(42)).await, Response::Rejected { .. }));
    }

    #[tokio::test]
    async fn no_more_than_the_limit_start_at_once() {
        // The guard is against a burst: `start` only spawns, and the job's
        // state changes a moment later, so a pump that counted running jobs
        // would see nothing running on each of five rapid enqueues and start
        // all five.
        let (_dir, engine) = engine();
        set_limit(&engine, 2).await;

        for _ in 0..5 {
            engine.enqueue(&desktop(), request("http://127.0.0.1:9/big.bin")).await;
        }

        assert_eq!(engine.outstanding().await, 2, "only two may hold a slot");
        assert_eq!(engine.waiting().await, 3, "the rest wait their turn");
    }

    #[tokio::test]
    async fn a_waiting_download_starts_when_a_slot_frees() {
        let (_dir, engine) = engine();
        set_limit(&engine, 1).await;

        for _ in 0..3 {
            engine.enqueue(&desktop(), request("http://127.0.0.1:9/big.bin")).await;
        }
        assert_eq!(engine.outstanding().await, 1);
        assert_eq!(engine.waiting().await, 2);

        // The first one fails -- nothing is listening on port 9 -- which is
        // a finish like any other and must free its slot.
        let started = std::time::Instant::now();
        while engine.waiting().await > 0 && started.elapsed() < std::time::Duration::from_secs(20) {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        assert_eq!(engine.waiting().await, 0, "every job should have had its turn");
    }

    #[tokio::test]
    async fn a_limit_of_zero_still_runs_one() {
        // A queue that runs nothing is a stall with no way out of it from
        // the UI, so the floor is one rather than an honoured zero.
        let (_dir, engine) = engine();
        set_limit(&engine, 0).await;

        engine.enqueue(&desktop(), request("http://127.0.0.1:9/big.bin")).await;
        assert_eq!(engine.outstanding().await, 1);
    }

    #[tokio::test]
    async fn queued_downloads_are_visible_while_they_wait() {
        // A download that is going to happen later should say so, rather
        // than vanish until its turn comes.
        let (_dir, engine) = engine();
        set_limit(&engine, 1).await;
        for _ in 0..3 {
            engine.enqueue(&desktop(), request("http://127.0.0.1:9/big.bin")).await;
        }

        let Response::Status { jobs } = engine.status(None).await else {
            panic!("expected a status listing");
        };
        assert_eq!(jobs.len(), 3, "all three should be listed");
        assert!(
            jobs.iter().any(|job| job.state == JobState::Queued),
            "the waiting ones should read as queued"
        );
    }

    #[tokio::test]
    async fn an_interrupted_download_comes_back_after_a_restart() {
        // What a reboot looks like from the engine's side: rows left behind
        // by a process that is gone, and a new one that has to find them.
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(idm_store::Store::open(&dir.path().join("state.sqlite3")).unwrap());

        let row = store
            .add_job(&idm_store::NewJob {
                url: "http://127.0.0.1:9/big.bin".into(),
                filename: "big.bin".into(),
                output_dir: dir.path().join("downloads").display().to_string(),
                total: Some(1 << 30),
                ..Default::default()
            })
            .unwrap();
        store.update_progress(row, 512 << 20, None).unwrap();
        store.set_job_state(row, idm_store::JobState::Running, None).unwrap();

        // The engine stops. Nothing is working on that row any more.
        store.mark_interrupted_as_paused().unwrap();

        let engine = engine_with_store(dir.path(), Arc::clone(&store));
        set_limit(&engine, 1).await;
        // Restored, but not started, so the assertion is about what came
        // back rather than about a download racing it.
        set_resume_on_start(&engine, false).await;

        assert_eq!(engine.restore_interrupted().await, 1);

        let Response::Status { jobs } = engine.status(None).await else {
            panic!("expected a listing");
        };
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].filename, "big.bin");
        assert_eq!(jobs[0].state, JobState::Paused, "it should be waiting, not running");
        assert_eq!(jobs[0].downloaded, 512 << 20, "progress should come back with it");
    }

    #[tokio::test]
    async fn a_restored_download_can_be_resumed() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(idm_store::Store::open(&dir.path().join("state.sqlite3")).unwrap());
        let row = store
            .add_job(&idm_store::NewJob {
                url: "http://127.0.0.1:9/big.bin".into(),
                filename: "big.bin".into(),
                output_dir: dir.path().join("downloads").display().to_string(),
                ..Default::default()
            })
            .unwrap();
        store.set_job_state(row, idm_store::JobState::Running, None).unwrap();
        store.mark_interrupted_as_paused().unwrap();

        let engine = engine_with_store(dir.path(), Arc::clone(&store));
        set_resume_on_start(&engine, false).await;
        engine.restore_interrupted().await;

        let Response::Status { jobs } = engine.status(None).await else {
            panic!("expected a listing");
        };
        let id = jobs[0].id;

        assert!(matches!(engine.resume(id).await, Response::Accepted { .. }));
        assert_eq!(engine.outstanding().await, 1, "resuming should hand it a slot");
    }

    #[tokio::test]
    async fn a_download_the_browser_authorised_is_never_restarted_on_its_own() {
        // Its cookies were deliberately never stored, so restarting it would
        // produce a refusal from the server and an error the user has no way
        // to connect to anything they did.
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(idm_store::Store::open(&dir.path().join("state.sqlite3")).unwrap());
        let row = store
            .add_job(&idm_store::NewJob {
                url: "http://127.0.0.1:9/paywalled.bin".into(),
                filename: "paywalled.bin".into(),
                output_dir: dir.path().join("downloads").display().to_string(),
                needs_browser_auth: true,
                ..Default::default()
            })
            .unwrap();
        store.set_job_state(row, idm_store::JobState::Running, None).unwrap();
        store.mark_interrupted_as_paused().unwrap();

        let engine = engine_with_store(dir.path(), Arc::clone(&store));
        // Even with automatic resume asked for.
        set_resume_on_start(&engine, true).await;
        engine.restore_interrupted().await;

        let Response::Status { jobs } = engine.status(None).await else {
            panic!("expected a listing");
        };
        assert_eq!(jobs[0].state, JobState::Paused, "it must not start by itself");
        assert_eq!(engine.outstanding().await, 0, "and must not hold a slot");
        let reason = jobs[0].error.clone().unwrap_or_default();
        assert!(reason.contains("browser"), "it should say why: {reason}");
    }

    #[tokio::test]
    async fn resuming_something_already_running_is_refused_rather_than_doubled() {
        let (_dir, engine) = engine();
        let Response::Accepted { id } =
            engine.enqueue(&desktop(), request("http://127.0.0.1:9/big.bin")).await
        else {
            panic!("expected the job to be accepted");
        };
        assert!(matches!(engine.resume(id).await, Response::Rejected { .. }));
    }

    #[tokio::test]
    async fn resuming_an_unknown_download_is_refused() {
        let (_dir, engine) = engine();
        assert!(matches!(engine.resume(JobId(999)).await, Response::Rejected { .. }));
    }
}
