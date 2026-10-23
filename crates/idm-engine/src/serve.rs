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
    /// The destination this job holds, released when it finishes.
    claimed: Option<PathBuf>,
    /// Everything needed to start this download, kept for the job's whole
    /// life rather than handed away when it starts.
    ///
    /// It used to be taken by the runner, which quietly made pausing
    /// one-way: the Cancel button set the state to Paused and kept the
    /// bytes, but Resume then refused with "that download cannot be
    /// resumed" because the config it needed had been moved out. Keeping it
    /// is what makes pause, reconnect and refresh-link all possible.
    ///
    /// Held in memory rather than rebuilt from the stored row because it
    /// carries the cookies the browser handed over, which are deliberately
    /// never written to the database and so could not be recovered from it.
    config: Box<DownloadConfig>,
    /// True while the job is queued and has not yet been handed a slot.
    waiting: bool,
    /// Consecutive failures that looked like the network rather than the
    /// file. Drives how long to wait before trying again, and is cleared the
    /// moment anything succeeds.
    attempts: u32,
    /// When bytes last reached the disk, for the stall watchdog.
    last_progress: std::time::Instant,
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
    /// Destinations a running download has claimed.
    ///
    /// Two downloads that derive the same name — `setup.exe` from two
    /// different sites, or a batch whose URLs differ only in a query
    /// parameter — would otherwise write the same part file and fight over
    /// the rename at the end. One wins, one fails, and which is luck.
    /// Checked against the disk as well, so an existing file is not
    /// overwritten either.
    claimed: Mutex<std::collections::HashSet<PathBuf>>,
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
        let engine = Arc::new(Self {
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
                claimed: Mutex::new(std::collections::HashSet::new()),
                armed: Mutex::new(None),
            }),
        });
        start_stall_watchdog(Arc::clone(&engine.inner));
        engine
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

    /// Queues a request, expanding a `[1-100]` range into one job each.
    ///
    /// Wrapped around `enqueue` rather than folded into it so that a single
    /// download takes exactly the path it always did — the common case does
    /// not go through list handling it has no use for.
    ///
    /// The reply names the first job, because a caller with one id can ask
    /// about it; naming none would leave a browser extension unable to tell
    /// a queued batch from a refused one.
    async fn enqueue_batch(&self, client: &ClientInfo, request: DownloadRequest) -> Response {
        if !crate::batch::is_pattern(&request.url) {
            return self.enqueue(client, request).await;
        }

        let urls = match crate::batch::expand(&request.url) {
            Ok(urls) => urls,
            Err(error) => return Response::Rejected { reason: error.to_string() },
        };

        let mut first = None;
        let mut accepted = 0usize;
        let mut refused: Option<String> = None;

        for url in urls {
            // The suggested name belongs to one file, so it cannot be
            // reused across a series; each URL names its own.
            let one = DownloadRequest { url, filename: None, ..request.clone() };
            match self.enqueue(client, one).await {
                Response::Accepted { id } => {
                    accepted += 1;
                    first.get_or_insert(id);
                }
                Response::Rejected { reason } => refused = Some(reason),
                other => return other,
            }
        }

        match first {
            // Partial failure is reported as success with a note in the log
            // rather than as a refusal: the downloads that were accepted are
            // running, and calling that a failure would be wrong.
            Some(id) => {
                if let Some(reason) = refused {
                    tracing::warn!(accepted, %reason, "some of the batch was refused");
                }
                tracing::info!(accepted, "queued a batch");
                Response::Accepted { id }
            }
            None => Response::Rejected {
                reason: refused.unwrap_or_else(|| "nothing in that range could be queued".into()),
            },
        }
    }

    /// Reserves a destination, adding a counter if it is taken.
    ///
    /// Checks the claims of other running downloads and the disk together,
    /// under one lock: checking only the disk would let two downloads
    /// enqueued moments apart both find the name free, and checking only
    /// the claims would overwrite a file already there. The part file is
    /// checked too, because an interrupted download holds a name it means
    /// to finish using.
    async fn claim_destination(&self, dir: &std::path::Path, filename: &str) -> (String, PathBuf) {
        let mut claimed = self.inner.claimed.lock().await;

        for attempt in 0..1000u32 {
            let candidate =
                if attempt == 0 { filename.to_owned() } else { numbered(filename, attempt) };
            let path = dir.join(&candidate);
            let part = dir.join(format!("{candidate}.idmpart"));

            if !claimed.contains(&path) && !path.exists() && !part.exists() {
                claimed.insert(path.clone());
                return (candidate, path);
            }
        }

        // A thousand files of the same name is not a case worth more code;
        // the download proceeds and the last one wins, as it did before.
        let path = dir.join(filename);
        (filename.to_owned(), path)
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

        // A name the URL cannot supply is asked of the server before anything
        // is decided. See `name_from_server`.
        let filename = if request.filename.is_none() && !has_extension(&filename) {
            name_from_server(&request).await.unwrap_or(filename)
        } else {
            filename
        };

        // Asking for something already here continues it rather than
        // starting a second copy beside it. Without this, downloading the
        // same file again -- the obvious thing to do when one stopped --
        // produced "file (1)" next to a half-finished "file", and the work
        // already done was abandoned rather than used.
        if let Some(existing) = self.unfinished_job_for(&request.url).await {
            return self.resume_or_report(existing).await;
        }

        let id = JobId(self.inner.next_id.fetch_add(1, Ordering::SeqCst));

        // Decided before the download starts, because the destination has
        // to be settled before the first byte is written — there is no
        // moving the file afterwards without racing whatever opened it.
        let category =
            settings.sort_into_folders.then(|| crate::category::category_for(&filename)).flatten();
        // The chosen folder if there is one, else the default the engine was
        // started with. Read now, so a change applies to what is queued next
        // and never moves a download already under way.
        let base_dir = if settings.download_dir.is_empty() {
            self.inner.output_dir.clone()
        } else {
            let chosen = PathBuf::from(&settings.download_dir);
            // Not silently the default: a drive that has been unplugged since
            // the folder was chosen should be said out loud, because files
            // appearing somewhere other than where the person was told would
            // be worse than a refusal. From the browser it falls back to the
            // browser's own downloader, which is the safe direction.
            if let Err(error) = std::fs::create_dir_all(&chosen) {
                return Response::Rejected {
                    reason: format!(
                        "the download folder {} is not available: {error}",
                        chosen.display()
                    ),
                };
            }
            chosen
        };
        let output_dir = match category {
            Some(category) => base_dir.join(category),
            None => base_dir,
        };

        // Settled before anything is written, and held until the job ends.
        let (filename, claimed_path) = self.claim_destination(&output_dir, &filename).await;

        let mut config = DownloadConfig::new(request.url.clone(), output_dir.clone());
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
                output_dir: output_dir.display().to_string(),
                total: request.size_hint,
                referrer: request.referrer.clone(),
                user_agent: request.user_agent.clone(),
                mime: request.mime.clone(),
                category: category.map(str::to_owned),
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
            retry_in_secs: None,
        };

        self.inner.jobs.lock().await.insert(
            id,
            JobRecord {
                status,
                abort: None,
                claimed: Some(claimed_path),
                config: Box::new(config),
                waiting: true,
                attempts: 0,
                last_progress: std::time::Instant::now(),
            },
        );
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
                !job.waiting && matches!(job.status.state, JobState::Queued | JobState::Running)
            })
            .count()
    }

    #[cfg(test)]
    async fn waiting(&self) -> usize {
        self.inner.jobs.lock().await.values().filter(|job| job.waiting).count()
    }

    /// Starts a paused download again.
    async fn resume(&self, id: JobId) -> Response {
        let ready = {
            let mut jobs = self.inner.jobs.lock().await;
            match jobs.get_mut(&id) {
                None => return Response::Rejected { reason: "no such download".into() },
                Some(job)
                    if matches!(
                        job.status.state,
                        JobState::Running | JobState::Queued | JobState::Completed
                    ) =>
                {
                    return Response::Rejected {
                        reason: match job.status.state {
                            JobState::Completed => "that download has already finished",
                            _ => "that download is already going",
                        }
                        .into(),
                    };
                }
                Some(job) => {
                    // Asking for it by hand clears the backoff: the user
                    // plainly believes the connection is back, and making
                    // them wait out a timer they cannot see would be rude.
                    job.attempts = 0;
                    job.status.retry_in_secs = None;
                    job.status.state = JobState::Queued;
                    job.status.error = None;
                    job.waiting = true;
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
                retry_in_secs: None,
            };

            // The destination it was already using, so a new download
            // cannot be given the same name while this one is paused.
            self.inner
                .claimed
                .lock()
                .await
                .insert(PathBuf::from(&row.output_dir).join(&row.filename));
            self.inner.rows.lock().await.insert(id, row.id);
            self.inner.jobs.lock().await.insert(
                id,
                JobRecord {
                    status,
                    abort: None,
                    claimed: Some(PathBuf::from(&row.output_dir).join(&row.filename)),
                    config: Box::new(config),
                    waiting: true,
                    attempts: 0,
                    last_progress: std::time::Instant::now(),
                },
            );
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

    /// An unfinished job already fetching this address, if there is one.
    ///
    /// Matched on the address asked for, not the one redirects landed on: a
    /// signed CDN link is different every time, so comparing those would
    /// never match. A finished download is not a match -- asking again for
    /// something already on disk is a request for another copy, not a
    /// resume.
    async fn unfinished_job_for(&self, url: &str) -> Option<JobId> {
        self.inner
            .jobs
            .lock()
            .await
            .iter()
            .filter(|(_, job)| job.status.state != JobState::Completed)
            .filter(|(_, job)| job.config.url == url || job.status.url == url)
            .map(|(id, _)| *id)
            .max_by_key(|id| id.0)
    }

    /// Continues an existing job, or reports it if it needs no help.
    ///
    /// Either way the answer is the same id, so a caller that asked twice is
    /// pointed at the one download rather than told off.
    async fn resume_or_report(&self, id: JobId) -> Response {
        let already_going = {
            let jobs = self.inner.jobs.lock().await;
            jobs.get(&id)
                .is_some_and(|job| matches!(job.status.state, JobState::Running | JobState::Queued))
        };
        if already_going {
            return Response::Accepted { id };
        }
        match self.resume(id).await {
            // A refusal here means it did not need resuming after all.
            Response::Rejected { .. } => Response::Accepted { id },
            other => other,
        }
    }

    /// Stops a download and keeps everything it has.
    ///
    /// The difference from [`cancel`](Self::cancel) is what happens to the
    /// bytes: pausing keeps the part file and the journal, so resuming
    /// carries on from where it stopped rather than starting again.
    async fn pause(&self, id: JobId) -> Response {
        let outcome = {
            let mut jobs = self.inner.jobs.lock().await;
            match jobs.get_mut(&id) {
                None => return Response::Rejected { reason: format!("no such job: {id}") },
                Some(job) if job.status.state == JobState::Completed => {
                    return Response::Rejected { reason: "that download has finished".into() };
                }
                Some(job) => {
                    if let Some(abort) = job.abort.take() {
                        abort.abort();
                    }
                    job.status.state = JobState::Paused;
                    job.status.bytes_per_sec = 0.0;
                    job.status.connections = 0;
                    job.status.retry_in_secs = None;
                    job.status.error = None;
                    // Stopped by hand, so it is not waiting for a slot; it
                    // waits for the user instead.
                    job.waiting = false;
                    job.attempts = 0;
                    Response::Accepted { id }
                }
            }
        };
        write_state(&self.inner, id, JobState::Paused, None).await;
        // A slot has just come free.
        pump(Arc::clone(&self.inner)).await;
        outcome
    }

    /// Points a download at a new address, keeping the bytes it already has.
    ///
    /// The case this exists for is an expired link. A signed CDN URL stops
    /// working after a few hours, so a download paused overnight cannot be
    /// resumed from the address it was started with -- but the bytes on disk
    /// are still perfectly good, and the engine checks on resume that the new
    /// address serves the same file before appending to them.
    async fn refresh_url(&self, id: JobId, url: String) -> Response {
        // Vetted exactly as a new download would be. A refreshed address is
        // no more trustworthy than the first one.
        let probe = DownloadRequest { url: url.clone(), ..Default::default() };
        if let Err(reason) = probe.validate_with(true) {
            return Response::Rejected { reason };
        }

        {
            let mut jobs = self.inner.jobs.lock().await;
            match jobs.get_mut(&id) {
                None => return Response::Rejected { reason: format!("no such job: {id}") },
                Some(job) if job.status.state == JobState::Completed => {
                    return Response::Rejected { reason: "that download has finished".into() };
                }
                Some(job) => {
                    if let Some(abort) = job.abort.take() {
                        abort.abort();
                    }
                    job.config.url = url.clone();
                    job.status.url = url;
                    job.status.state = JobState::Queued;
                    job.status.error = None;
                    job.status.retry_in_secs = None;
                    job.waiting = true;
                    job.attempts = 0;
                }
            }
        }

        write_state(&self.inner, id, JobState::Queued, None).await;
        pump(Arc::clone(&self.inner)).await;
        Response::Accepted { id }
    }

    /// Stops a download and throws away what it had.
    ///
    /// Only the working files go: the part file and its journal, both named
    /// after the destination and written by us. A finished file is never
    /// touched, because cancelling something already on disk would be
    /// deleting the user's download rather than abandoning a transfer.
    async fn cancel(&self, id: JobId) -> Response {
        let claimed = {
            let mut jobs = self.inner.jobs.lock().await;
            match jobs.get_mut(&id) {
                None => return Response::Rejected { reason: format!("no such job: {id}") },
                Some(record) => {
                    if let Some(abort) = record.abort.take() {
                        abort.abort();
                    }
                    record.status.state = JobState::Failed;
                    record.status.error = Some("cancelled".into());
                    record.waiting = false;
                    record.claimed.clone()
                }
            }
        };

        // After the abort, so the writer is not still holding them.
        if let Some(path) = claimed {
            for suffix in ["idmpart", "idmjrn"] {
                let mut working = path.clone().into_os_string();
                working.push(".");
                working.push(suffix);
                let _ = std::fs::remove_file(PathBuf::from(working));
            }
        }

        write_state(&self.inner, id, JobState::Failed, Some("cancelled")).await;
        release_claim(&self.inner, id).await;
        pump(Arc::clone(&self.inner)).await;
        Response::Accepted { id }
    }
}

/// How long a running download may move no bytes before it is treated as
/// disconnected.
///
/// Generous, because durable progress advances at checkpoints rather than
/// continuously, and a genuinely slow link can go a while between them. Short
/// enough that a user watching a dead download is told within half a minute
/// instead of staring at "running" at 0 B/s indefinitely.
const STALL_AFTER: std::time::Duration = std::time::Duration::from_secs(30);

/// Watches running downloads for ones that have quietly stopped moving.
///
/// Killing the server mid-download does not raise an error in the scheduler:
/// claims are still available, workers take them, fail against a closed port
/// in milliseconds, release them and go round again. The loop is never idle,
/// so the stall detector inside the download never fires, and the job sits in
/// `Running` at 0 B/s for as long as the outage lasts. Correct, in that it
/// recovers the moment the server returns -- but it tells the user nothing
/// and retries without pause.
///
/// So progress is watched from out here, where "no bytes for a while" is
/// visible, and a download that has stopped moving is pushed down the same
/// path as an outright failure: stop, say so, wait, try again.
fn start_stall_watchdog(inner: Arc<Inner>) {
    // An engine can be built outside a runtime -- the synchronous tests do
    // exactly that to check how a request is vetted, which needs no tasks at
    // all. Spawning unconditionally panics there, so a missing runtime is
    // treated as "nothing to watch" rather than an error.
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }

    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            tick.tick().await;

            let stalled: Vec<JobId> = {
                let jobs = inner.jobs.lock().await;
                jobs.iter()
                    .filter(|(_, job)| job.status.state == JobState::Running)
                    // Bytes reaching the disk, and nothing else. The reported
                    // rate is a decaying average, so a download that stopped
                    // dead still shows tens of KB/s a minute later -- which
                    // is both why this cannot test the rate, and why the
                    // window shows a speed for a download that is not moving
                    // until the retry zeroes it.
                    .filter(|(_, job)| job.last_progress.elapsed() >= STALL_AFTER)
                    .map(|(id, _)| *id)
                    .collect()
            };

            for id in stalled {
                // Stop the spinning task before anything else; it is the
                // thing that would otherwise keep re-dialling a dead host.
                if let Some(job) = inner.jobs.lock().await.get_mut(&id)
                    && let Some(abort) = job.abort.take()
                {
                    abort.abort();
                }
                schedule_retry(&inner, id, "the connection went away".to_owned()).await;
            }
        }
    });
}

/// Puts a download into `Reconnecting` and arranges for it to try again.
///
/// This is what makes the network going out a pause rather than a failure.
/// The bytes and the journal are untouched, so when the attempt comes round
/// the download carries on from where it stopped; nothing is re-fetched
/// beyond whatever had not reached the disk.
///
/// The waiting is a task per job rather than one timer over all of them,
/// because there are only ever a handful of downloads and a sleeping task
/// costs nothing. It checks the state again on waking: the user may have
/// paused or cancelled in the meantime, and neither should be overridden by
/// a timer that was already running.
async fn schedule_retry(inner: &Arc<Inner>, id: JobId, message: String) {
    let delay = {
        let mut jobs = inner.jobs.lock().await;
        let Some(job) = jobs.get_mut(&id) else { return };
        job.attempts = job.attempts.saturating_add(1);
        // Reset so the watchdog does not fire again the instant this one
        // starts running, before any bytes have had a chance to arrive.
        job.last_progress = std::time::Instant::now();
        let delay = retry_delay(job.attempts);
        job.status.state = JobState::Reconnecting;
        job.status.bytes_per_sec = 0.0;
        job.status.connections = 0;
        job.status.retry_in_secs = Some(delay.as_secs());
        job.status.error = Some(message.clone());
        job.waiting = false;
        job.abort = None;
        delay
    };

    write_state(inner, id, JobState::Reconnecting, Some(&message)).await;
    // A slot is free while this one waits, so anything queued behind it can
    // get on rather than being held up by a connection that is not there.
    pump(Arc::clone(inner)).await;

    let inner = Arc::clone(inner);
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + delay;
        // Ticks so the window can count down rather than showing a number
        // that sits still for fifteen seconds.
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let mut jobs = inner.jobs.lock().await;
            let Some(job) = jobs.get_mut(&id) else { return };
            if job.status.state != JobState::Reconnecting {
                // Paused, cancelled or resumed by hand while we waited.
                return;
            }
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                job.status.state = JobState::Queued;
                job.status.retry_in_secs = None;
                job.waiting = true;
                break;
            }
            job.status.retry_in_secs = Some(left.as_secs().max(1));
        }
        pump(inner).await;
    });
}

/// Why a download stopped, and whether waiting could fix it.
///
/// The run functions used to flatten every error to a string, which threw
/// away the one thing the engine most needs to know: a 404 and a dropped
/// Wi-Fi look identical once they are both `String`.
struct RunFailure {
    message: String,
    /// True when the network went away rather than the download being
    /// impossible. See [`idm_net::download::DownloadError::is_transient`].
    transient: bool,
}

impl RunFailure {
    /// For the transports whose errors are not `DownloadError`.
    ///
    /// FTP, SFTP and the stream fetcher each have their own error type, and
    /// all three run over a network that can go away. Treating their failures
    /// as worth another try is the same bet as for HTTP; a genuinely
    /// impossible transfer fails again straight away and the attempt costs a
    /// few seconds.
    fn from_transport(error: impl std::fmt::Display) -> Self {
        Self { message: error.to_string(), transient: true }
    }
}

impl From<idm_net::download::DownloadError> for RunFailure {
    fn from(error: idm_net::download::DownloadError) -> Self {
        Self { transient: error.is_transient(), message: error.to_string() }
    }
}

/// How long to wait before trying again, by consecutive failure.
///
/// Backs off so a server refusing everyone is not hammered, but stops at
/// fifteen seconds: this is waiting for a connection to come back, and a
/// laptop rejoining the Wi-Fi should not sit idle for minutes because the
/// failures piled up while it was asleep.
fn retry_delay(attempts: u32) -> std::time::Duration {
    let secs = match attempts {
        0 | 1 => 3,
        2 => 6,
        3 => 10,
        _ => 15,
    };
    std::time::Duration::from_secs(secs)
}

/// Whether a name ends in something that looks like a file extension.
fn has_extension(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(stem, ext)| {
        !stem.is_empty()
            && !ext.is_empty()
            && ext.len() <= 8
            && ext.bytes().all(|b| b.is_ascii_alphanumeric())
    })
}

/// Asks the server what a download is called, when the URL cannot say.
///
/// The destination -- folder and filename -- has to be settled before the
/// first byte is written, so it is decided before the download starts, from
/// the URL. That is fine until the URL's last segment is not a name. A CDN
/// that redirects to a signed object ends in a GUID, and a browser hands over
/// the *redirected* address: `c6451f46-4a95-....`, no extension. The file was
/// then saved under that, outside the folder its type belongs in, while the
/// server had said plainly in Content-Disposition that it was a `.zip`.
///
/// So only when the URL gives no usable name, one ranged request is made and
/// its Content-Disposition read. A URL that already ends in `setup.exe` costs
/// nothing and is trusted as before. Any failure -- a slow server, a refusal,
/// no header -- falls back to the URL's own guess, so this can make a name
/// better and cannot make a download fail.
async fn name_from_server(request: &DownloadRequest) -> Option<String> {
    let mut config = idm_net::download::DownloadConfig::new(request.url.clone(), PathBuf::new());
    config.referrer = request.referrer.clone();
    config.cookies = request.cookies.clone();

    // Bounded: an unresponsive server must not hold up accepting the job.
    let target =
        tokio::time::timeout(std::time::Duration::from_secs(8), idm_net::download::probe(&config))
            .await
            .ok()?
            .ok()?;
    // The probe falls back to the URL's name and then to download.bin; only a
    // name with an extension is better than what we already had.
    has_extension(&target.filename).then_some(target.filename)
}

/// Gives up the destination a finished job was holding.
///
/// After it finishes, not before: releasing early would let a second
/// download with the same name start writing the same part file while the
/// first was still using it.
async fn release_claim(inner: &Arc<Inner>, id: JobId) {
    let path = inner.jobs.lock().await.get_mut(&id).and_then(|job| job.claimed.take());
    if let Some(path) = path {
        inner.claimed.lock().await.remove(&path);
    }
}

/// `report.pdf` and 2 becomes `report (2).pdf`.
///
/// The counter goes before the extension, because a name ending in
/// something other than its extension stops opening in the program that
/// owns it.
fn numbered(filename: &str, attempt: u32) -> String {
    match filename.rsplit_once('.') {
        // A leading dot is the whole name, not an extension.
        Some((stem, extension)) if !stem.is_empty() => format!("{stem} ({attempt}).{extension}"),
        _ => format!("{filename} ({attempt})"),
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
        // Bytes arriving means the connection is back, whatever the recent
        // history. Without this a download that drops once an hour on a long
        // transfer would creep up to the longest backoff and stay there.
        if progress.durable > record.status.downloaded {
            record.attempts = 0;
            record.last_progress = std::time::Instant::now();
        }
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
) -> std::result::Result<Option<u64>, RunFailure> {
    let progress_inner = Arc::clone(inner);
    let report = download_with_progress(config, move |progress: Progress| {
        record_progress(&progress_inner, id, &progress);
    })
    .await?;

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
) -> std::result::Result<Option<u64>, RunFailure> {
    let mut options = idm_net::ftp::FtpOptions::new(config.output_dir.clone());
    options.filename = config.filename.clone();
    options.limiter = config.fetch.limiter.clone();

    let progress_inner = Arc::clone(inner);
    let report = idm_net::ftp::download(&config.url, &options, move |done, total| {
        record_transfer(&progress_inner, id, done, total);
    })
    .await
    .map_err(RunFailure::from_transport)?;
    Ok(Some(report.bytes))
}

/// SFTP. There is nobody to type a password into, so this authenticates the
/// way `ssh` does by default — with the keys in ~/.ssh — and checks the host
/// against known_hosts, refusing one it has not seen.
async fn run_sftp(
    inner: &Arc<Inner>,
    id: JobId,
    config: &DownloadConfig,
) -> std::result::Result<Option<u64>, RunFailure> {
    let mut options = idm_net::sftp::SftpOptions::new(config.output_dir.clone());
    options.filename = config.filename.clone();
    options.limiter = config.fetch.limiter.clone();
    options.use_default_identities = true;

    let progress_inner = Arc::clone(inner);
    let report = idm_net::sftp::download(&config.url, &options, move |done, total| {
        record_transfer(&progress_inner, id, done, total);
    })
    .await
    .map_err(RunFailure::from_transport)?;
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
) -> std::result::Result<Option<u64>, RunFailure> {
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
    .map_err(RunFailure::from_transport)?;

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
                    !job.waiting
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
                .filter(|(_, job)| job.waiting && job.status.state == JobState::Queued)
                .map(|(id, _)| *id)
                .collect();
            waiting.sort_by_key(|id| id.0);

            match waiting.first().copied() {
                // Taken under the same lock that chose it, so two pumps
                // running at once cannot both start the same job.
                Some(id) => jobs.get_mut(&id).map(|job| {
                    job.waiting = false;
                    (id, job.config.clone())
                }),
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
                Err(failure) => {
                    // The partial file and journal are deliberately left in
                    // place, so a retry resumes rather than restarts.
                    if failure.transient {
                        schedule_retry(&inner, id, failure.message).await;
                    } else {
                        finish(&inner, id, JobState::Failed, None, Some(failure.message)).await;
                    }
                }
            }
            forget_row(&inner, id).await;
            release_claim(&inner, id).await;

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
        // Stored as paused: to anything reading this later -- a restart, the
        // history -- a download waiting for the network and one the user
        // held are the same thing, bytes on disk and nothing running.
        JobState::Paused | JobState::Reconnecting => idm_store::JobState::Paused,
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
        // Vetted before anything is stored, while the person is still at the
        // screen: a folder that cannot be written to otherwise fails later,
        // mid-transfer, on a file that may have taken an hour.
        let mut settings = settings;
        match crate::folder::vet_download_dir(&settings.download_dir) {
            Ok(vetted) => settings.download_dir = vetted,
            Err(reason) => return Response::Rejected { reason },
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
            Request::Enqueue(req) => self.enqueue_batch(client, req).await,
            Request::Status { id } => self.status(id).await,
            Request::Cancel { id } => self.cancel(id).await,
            Request::Resume { id } => self.resume(id).await,
            Request::Pause { id } => self.pause(id).await,
            Request::RefreshUrl { id, url } => self.refresh_url(id, url).await,
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

    /// Sets whether finished downloads are sorted into folders.
    async fn set_sorting(engine: &Arc<Engine>, sorting: bool) {
        let mut settings = match engine.handle(&desktop(), Request::GetSettings).await {
            Response::Settings(settings) => settings,
            other => panic!("expected settings, got {other:?}"),
        };
        settings.sort_into_folders = sorting;
        engine.handle(&desktop(), Request::SetSettings(settings)).await;
    }

    /// Queues a job that is going nowhere, to occupy the only slot.
    ///
    /// A job that starts has its config taken by the runner, so the only
    /// way to read the destination it was given is to keep it waiting.
    async fn occupy_the_slot(engine: &Arc<Engine>) {
        set_limit(engine, 1).await;
        engine.enqueue(&desktop(), request("http://127.0.0.1:9/filler.bin")).await;
    }

    /// The directory a queued job will write into.
    async fn output_dir_of(engine: &Arc<Engine>, id: JobId) -> String {
        let jobs = engine.inner.jobs.lock().await;
        let job = jobs.get(&id).expect("the job should exist");
        job.config.output_dir.display().to_string()
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

        // A distinct address each: asking for the same one twice now
        // continues the first rather than starting a second, which is right
        // for a user and wrong for counting slots.
        for n in 0..5 {
            engine.enqueue(&desktop(), request(&format!("http://127.0.0.1:9/big-{n}.bin"))).await;
        }

        assert_eq!(engine.outstanding().await, 2, "only two may hold a slot");
        assert_eq!(engine.waiting().await, 3, "the rest wait their turn");
    }

    #[tokio::test]
    async fn a_waiting_download_starts_when_a_slot_frees() {
        let (_dir, engine) = engine();
        set_limit(&engine, 1).await;

        for n in 0..3 {
            engine.enqueue(&desktop(), request(&format!("http://127.0.0.1:9/big-{n}.bin"))).await;
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
        for n in 0..3 {
            engine.enqueue(&desktop(), request(&format!("http://127.0.0.1:9/big-{n}.bin"))).await;
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

    #[tokio::test]
    async fn a_video_is_sorted_into_its_own_folder() {
        // The wiring, not the classifier: category.rs has its own tests, and
        // they would all pass with nothing calling it.
        let (_dir, engine) = engine();
        occupy_the_slot(&engine).await;
        set_sorting(&engine, true).await;

        let request = DownloadRequest {
            url: "https://example.invalid/holiday.mp4".into(),
            size_hint: Some(10 << 20),
            ..Default::default()
        };
        let Response::Accepted { id } = engine.enqueue(&desktop(), request).await else {
            panic!("expected the job to be accepted");
        };

        let directory = output_dir_of(&engine, id).await;
        assert!(directory.ends_with("Video"), "{directory}");
    }

    /// Sets the download folder through the same path the window uses.
    async fn set_download_dir(engine: &Arc<Engine>, client: &ClientInfo, dir: &str) -> Response {
        let mut settings = match engine.handle(&desktop(), Request::GetSettings).await {
            Response::Settings(settings) => settings,
            other => panic!("expected settings, got {other:?}"),
        };
        settings.download_dir = dir.to_owned();
        engine.handle(client, Request::SetSettings(settings)).await
    }

    #[tokio::test]
    async fn a_chosen_folder_is_where_the_next_download_goes() {
        // The wiring, not the validator: folder.rs has its own tests, and
        // they would all pass with nothing reading the setting.
        let (dir, engine) = engine();
        occupy_the_slot(&engine).await;
        set_sorting(&engine, false).await;

        let chosen = dir.path().join("elsewhere");
        let outcome = set_download_dir(&engine, &desktop(), &chosen.display().to_string()).await;
        assert!(matches!(outcome, Response::Settings(_)), "{outcome:?}");

        let request = DownloadRequest {
            url: "https://example.invalid/setup.bin".into(),
            size_hint: Some(10 << 20),
            ..Default::default()
        };
        let Response::Accepted { id } = engine.enqueue(&desktop(), request).await else {
            panic!("expected the job to be accepted");
        };

        let directory = output_dir_of(&engine, id).await;
        assert_eq!(directory, chosen.display().to_string());
        assert!(!directory.contains("downloads"), "went to the default instead: {directory}");
    }

    #[tokio::test]
    async fn sorting_still_applies_inside_a_chosen_folder() {
        // Two settings that have to compose: the category is a subfolder of
        // whichever folder was chosen, not of the default.
        let (dir, engine) = engine();
        occupy_the_slot(&engine).await;
        set_sorting(&engine, true).await;
        let chosen = dir.path().join("elsewhere");
        set_download_dir(&engine, &desktop(), &chosen.display().to_string()).await;

        let request = DownloadRequest {
            url: "https://example.invalid/holiday.mp4".into(),
            size_hint: Some(10 << 20),
            ..Default::default()
        };
        let Response::Accepted { id } = engine.enqueue(&desktop(), request).await else {
            panic!("expected the job to be accepted");
        };
        let directory = output_dir_of(&engine, id).await;
        assert_eq!(directory, chosen.join("Video").display().to_string());
    }

    #[tokio::test]
    async fn a_download_already_queued_keeps_the_folder_it_was_given() {
        // Its part file and journal are already there; changing the setting
        // must not strand them.
        let (dir, engine) = engine();
        occupy_the_slot(&engine).await;
        set_sorting(&engine, false).await;

        let request = DownloadRequest {
            url: "https://example.invalid/first.bin".into(),
            size_hint: Some(10 << 20),
            ..Default::default()
        };
        let Response::Accepted { id } = engine.enqueue(&desktop(), request).await else {
            panic!("expected the job to be accepted");
        };
        let before = output_dir_of(&engine, id).await;

        set_download_dir(&engine, &desktop(), &dir.path().join("later").display().to_string())
            .await;
        assert_eq!(output_dir_of(&engine, id).await, before);
    }

    #[tokio::test]
    async fn the_browser_cannot_choose_where_downloads_go() {
        // The reason this setting is guarded at all: a page that could pick
        // the folder could pick the Startup folder.
        let (dir, engine) = engine();
        let outcome =
            set_download_dir(&engine, &browser(), &dir.path().join("x").display().to_string())
                .await;
        assert!(matches!(outcome, Response::Rejected { .. }), "{outcome:?}");
        assert!(!dir.path().join("x").exists(), "refused, but the folder was created anyway");
    }

    #[tokio::test]
    async fn an_unusable_folder_is_refused_and_not_stored() {
        let (_dir, engine) = engine();
        let outcome = set_download_dir(&engine, &desktop(), r"not\a\full\path").await;
        let Response::Rejected { reason } = outcome else { panic!("expected a refusal") };
        assert!(reason.contains("full path"), "{reason}");

        // And the stored value is untouched, so one bad entry does not
        // quietly redirect later downloads.
        let Response::Settings(current) = engine.handle(&desktop(), Request::GetSettings).await
        else {
            panic!("expected settings");
        };
        assert_eq!(current.download_dir, "");
    }

    #[tokio::test]
    async fn a_folder_that_has_gone_away_is_a_refusal_not_a_silent_fallback() {
        // A drive unplugged since the folder was chosen. Files appearing
        // somewhere other than where the person was told would be worse.
        let (dir, engine) = engine();
        let chosen = dir.path().join("usb");
        set_download_dir(&engine, &desktop(), &chosen.display().to_string()).await;

        // Replace the folder with a file, so it can neither be used nor
        // recreated, on any platform.
        std::fs::remove_dir_all(&chosen).unwrap();
        std::fs::write(&chosen, b"gone").unwrap();

        let request = DownloadRequest {
            url: "https://example.invalid/a.bin".into(),
            size_hint: Some(10 << 20),
            ..Default::default()
        };
        let outcome = engine.enqueue(&desktop(), request).await;
        let Response::Rejected { reason } = outcome else { panic!("expected a refusal") };
        assert!(reason.contains("not available"), "{reason}");
    }

    #[tokio::test]
    async fn an_old_settings_file_without_the_field_still_loads() {
        // The bug this struct-level default exists for: a file written before
        // a field existed must not be thrown away on every start.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        std::fs::write(&file, r#"{"browser_integration":false,"max_concurrent_downloads":3}"#)
            .unwrap();
        let engine = Engine::with_settings_file(dir.path().join("downloads"), 8, file);

        let Response::Settings(loaded) = engine.handle(&desktop(), Request::GetSettings).await
        else {
            panic!("expected settings");
        };
        assert!(!loaded.browser_integration, "an old choice was discarded");
        assert_eq!(loaded.max_concurrent_downloads, 3);
        assert_eq!(loaded.download_dir, "");
    }

    #[tokio::test]
    async fn an_unrecognised_file_stays_in_the_download_folder() {
        // Not a junk folder: it goes where someone will look for it.
        let (_dir, engine) = engine();
        occupy_the_slot(&engine).await;
        set_sorting(&engine, true).await;

        let request = DownloadRequest {
            url: "https://example.invalid/firmware.rom".into(),
            size_hint: Some(10 << 20),
            ..Default::default()
        };
        let Response::Accepted { id } = engine.enqueue(&desktop(), request).await else {
            panic!("expected the job to be accepted");
        };

        let directory = output_dir_of(&engine, id).await;
        assert!(!directory.ends_with("Video"), "{directory}");
        assert!(directory.ends_with("downloads"), "{directory}");
    }

    #[tokio::test]
    async fn sorting_can_be_switched_off() {
        let (_dir, engine) = engine();
        occupy_the_slot(&engine).await;
        set_sorting(&engine, false).await;

        let request = DownloadRequest {
            url: "https://example.invalid/holiday.mp4".into(),
            size_hint: Some(10 << 20),
            ..Default::default()
        };
        let Response::Accepted { id } = engine.enqueue(&desktop(), request).await else {
            panic!("expected the job to be accepted");
        };

        let directory = output_dir_of(&engine, id).await;
        assert!(directory.ends_with("downloads"), "{directory}");
    }

    #[tokio::test]
    async fn a_numbered_range_becomes_one_job_each() {
        // Again the wiring: batch.rs is tested on its own, and the engine
        // could have ignored it entirely.
        let (_dir, engine) = engine();
        set_limit(&engine, 1).await;

        let request = DownloadRequest {
            url: "https://example.invalid/page-[1-5].jpg".into(),
            size_hint: Some(10 << 20),
            ..Default::default()
        };
        assert!(matches!(
            engine.enqueue_batch(&desktop(), request).await,
            Response::Accepted { .. }
        ));

        let Response::Status { jobs } = engine.status(None).await else {
            panic!("expected a listing");
        };
        assert_eq!(jobs.len(), 5, "one job per number in the range");
        assert!(jobs.iter().any(|job| job.url.ends_with("page-1.jpg")));
        assert!(jobs.iter().any(|job| job.url.ends_with("page-5.jpg")));
    }

    #[tokio::test]
    async fn a_url_with_no_range_still_makes_exactly_one_job() {
        let (_dir, engine) = engine();
        let request = DownloadRequest {
            url: "https://example.invalid/single.bin".into(),
            size_hint: Some(10 << 20),
            ..Default::default()
        };
        engine.enqueue_batch(&desktop(), request).await;

        let Response::Status { jobs } = engine.status(None).await else {
            panic!("expected a listing");
        };
        assert_eq!(jobs.len(), 1);
    }

    #[tokio::test]
    async fn an_oversized_range_is_refused_without_queueing_any_of_it() {
        // Reachable from a browser extension, so a refusal has to leave
        // nothing behind rather than queue until it hits the limit.
        let (_dir, engine) = engine();
        let request = DownloadRequest {
            url: "https://example.invalid/[1-50000].bin".into(),
            size_hint: Some(10 << 20),
            ..Default::default()
        };
        assert!(matches!(
            engine.enqueue_batch(&desktop(), request).await,
            Response::Rejected { .. }
        ));

        let Response::Status { jobs } = engine.status(None).await else {
            panic!("expected a listing");
        };
        assert!(jobs.is_empty(), "a refused batch should queue nothing");
    }

    /// Reads a job's state without going through the status listing.
    async fn state_of(engine: &Arc<Engine>, id: JobId) -> JobState {
        engine.inner.jobs.lock().await.get(&id).expect("the job should exist").status.state
    }

    #[tokio::test]
    async fn a_paused_download_can_be_resumed() {
        // The bug this guards: `pump` used to take the config out of the
        // record when a job started, so pausing left nothing to start again
        // and Resume answered "that download cannot be resumed". The button
        // was there, the bytes were there, and the two could not be joined.
        let (_dir, engine) = engine();
        let Response::Accepted { id } =
            engine.enqueue(&desktop(), request("http://127.0.0.1:9/held.bin")).await
        else {
            panic!("expected the job to be accepted");
        };

        assert!(matches!(engine.pause(id).await, Response::Accepted { .. }), "pause");
        assert_eq!(state_of(&engine, id).await, JobState::Paused);

        let resumed = engine.resume(id).await;
        assert!(matches!(resumed, Response::Accepted { .. }), "resume refused: {resumed:?}");
        assert_ne!(
            state_of(&engine, id).await,
            JobState::Paused,
            "resuming should take it out of paused"
        );
    }

    #[tokio::test]
    async fn pausing_keeps_the_bytes_and_cancelling_throws_them_away() {
        // The whole difference between the two buttons. Pause has to leave
        // the part file so resuming continues; cancel has to remove it so a
        // later download of the same thing starts clean rather than
        // appending to an abandoned stub.
        let (dir, engine) = engine();
        let downloads = dir.path().join("downloads");
        std::fs::create_dir_all(&downloads).unwrap();

        for (label, cancel_it) in [("kept", false), ("gone", true)] {
            let url = format!("http://127.0.0.1:9/{label}.bin");
            let Response::Accepted { id } = engine.enqueue(&desktop(), request(&url)).await else {
                panic!("expected the job to be accepted");
            };
            // Stand in for the writer having started: the engine must not
            // depend on whether bytes happened to land before the click.
            let part = downloads.join(format!("{label}.bin.idmpart"));
            std::fs::write(&part, b"partial").unwrap();

            if cancel_it {
                engine.cancel(id).await;
                assert!(!part.exists(), "cancel should remove the part file");
            } else {
                engine.pause(id).await;
                assert!(part.exists(), "pause must keep the part file");
            }
        }
    }

    #[tokio::test]
    async fn asking_for_the_same_download_again_continues_it() {
        // What a user does when a download stops: go back to the page and
        // click it again. That used to produce "file (1)" beside a
        // half-finished "file" and abandon the work already done.
        let (_dir, engine) = engine();
        let url = "https://example.invalid/big-file.zip";

        let Response::Accepted { id: first } = engine.enqueue(&desktop(), request(url)).await
        else {
            panic!("expected the job to be accepted");
        };
        engine.pause(first).await;
        assert_eq!(state_of(&engine, first).await, JobState::Paused);

        let Response::Accepted { id: again } = engine.enqueue(&desktop(), request(url)).await
        else {
            panic!("the repeat request should be accepted");
        };

        assert_eq!(again, first, "it should be the same download, not a second one");
        assert_eq!(engine.inner.jobs.lock().await.len(), 1, "no second job");
        assert_ne!(state_of(&engine, first).await, JobState::Paused, "and it should be going");
    }

    #[tokio::test]
    async fn a_finished_download_is_not_resumed_by_asking_again() {
        // The other half of the rule: asking for something already on disk
        // is a request for another copy, not a resume of a finished job.
        let (_dir, engine) = engine();
        let url = "https://example.invalid/done.bin";
        let Response::Accepted { id } = engine.enqueue(&desktop(), request(url)).await else {
            panic!("expected the job to be accepted");
        };
        engine.inner.jobs.lock().await.get_mut(&id).unwrap().status.state = JobState::Completed;

        let Response::Accepted { id: second } = engine.enqueue(&desktop(), request(url)).await
        else {
            panic!("expected a fresh job");
        };
        assert_ne!(second, id, "a finished download should not be reopened");
    }

    #[tokio::test]
    async fn refreshing_the_link_keeps_the_download_and_changes_its_address() {
        // For an expired signed URL: the bytes are still good, only the way
        // back to the rest of them has gone stale.
        let (_dir, engine) = engine();
        let Response::Accepted { id } =
            engine.enqueue(&desktop(), request("https://old.invalid/f.bin")).await
        else {
            panic!("expected the job to be accepted");
        };
        engine.pause(id).await;

        let fresh = "https://new.invalid/f.bin?sig=abc";
        assert!(
            matches!(engine.refresh_url(id, fresh.into()).await, Response::Accepted { .. }),
            "refresh"
        );

        let jobs = engine.inner.jobs.lock().await;
        let job = jobs.get(&id).expect("still there");
        assert_eq!(job.config.url, fresh, "the engine should fetch from the new address");
        assert_eq!(job.status.url, fresh, "and the window should show it");
        assert_eq!(job.status.filename, "f.bin", "the destination must not move");
    }

    #[tokio::test]
    async fn a_refreshed_link_is_vetted_like_any_other() {
        // A refreshed address is no more trustworthy than the first one.
        let (_dir, engine) = engine();
        let Response::Accepted { id } =
            engine.enqueue(&desktop(), request("https://ok.invalid/f.bin")).await
        else {
            panic!("expected the job to be accepted");
        };
        for hostile in ["file:///C:/Windows/system.ini", "javascript:alert(1)", "not a url"] {
            assert!(
                matches!(engine.refresh_url(id, hostile.into()).await, Response::Rejected { .. }),
                "{hostile} should be refused"
            );
        }
    }

    #[test]
    fn the_wait_before_reconnecting_grows_but_stays_short() {
        // Long enough not to hammer a server, short enough that a laptop
        // rejoining the Wi-Fi picks up in seconds rather than minutes.
        let delays: Vec<u64> = (1..=6).map(|n| retry_delay(n).as_secs()).collect();
        assert!(delays.windows(2).all(|w| w[1] >= w[0]), "should not go backwards: {delays:?}");
        assert!(delays.iter().all(|d| *d <= 15), "none should be long: {delays:?}");
        assert!(delays[0] <= 5, "the first retry should be quick: {delays:?}");
    }

    #[tokio::test]
    async fn two_downloads_of_the_same_name_do_not_fight_over_it() {
        // Not exotic: setup.exe from two different sites. Before this, both
        // wrote one part file and one of them failed at the rename.
        let (_dir, engine) = engine();
        occupy_the_slot(&engine).await;

        let mut names = Vec::new();
        for site in ["a", "b", "c"] {
            let request = DownloadRequest {
                // Different sites, same filename -- which is what the
                // comment above always described. One address three times
                // is a repeat request, and now continues the first.
                url: format!("https://{site}.invalid/setup.exe"),
                size_hint: Some(10 << 20),
                ..Default::default()
            };
            let Response::Accepted { id } = engine.enqueue(&desktop(), request).await else {
                panic!("expected the job to be accepted");
            };
            let jobs = engine.inner.jobs.lock().await;
            names.push(jobs.get(&id).unwrap().status.filename.clone());
        }

        assert_eq!(names[0], "setup.exe");
        assert_eq!(names[1], "setup (1).exe", "the counter goes before the extension");
        assert_eq!(names[2], "setup (2).exe");
    }

    #[tokio::test]
    async fn a_counter_keeps_the_extension_where_a_program_expects_it() {
        assert_eq!(numbered("report.pdf", 1), "report (1).pdf");
        assert_eq!(numbered("archive.tar.gz", 2), "archive.tar (2).gz");
        // No extension, and a name that is nothing but one.
        assert_eq!(numbered("README", 1), "README (1)");
        assert_eq!(numbered(".gitignore", 1), ".gitignore (1)");
    }

    #[tokio::test]
    async fn a_finished_download_gives_its_name_back() {
        // Otherwise every later download of the same name is numbered for
        // the life of the process, even once the first has finished.
        let (_dir, engine) = engine();
        occupy_the_slot(&engine).await;

        let request = DownloadRequest {
            url: "https://example.invalid/once.bin".into(),
            size_hint: Some(10 << 20),
            ..Default::default()
        };
        let Response::Accepted { id } = engine.enqueue(&desktop(), request).await else {
            panic!("expected the job to be accepted");
        };
        release_claim(&engine.inner, id).await;

        let request = DownloadRequest {
            url: "https://example.invalid/once.bin".into(),
            size_hint: Some(10 << 20),
            ..Default::default()
        };
        let Response::Accepted { id } = engine.enqueue(&desktop(), request).await else {
            panic!("expected the job to be accepted");
        };
        let jobs = engine.inner.jobs.lock().await;
        assert_eq!(jobs.get(&id).unwrap().status.filename, "once.bin");
    }
}
